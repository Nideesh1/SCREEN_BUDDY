import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import {
  ActionChip,
  Badge,
  Button,
  Card,
  ConfirmModal,
  IconButton,
  SectionTitle,
  Spinner,
} from '../ui'
import { DeviceDot, deviceLabel, deviceLine, useDevice } from './DeviceRuns'
import {
  ChecklistItem,
  SendBackModal,
  TaskRecord,
  TaskStatusPill,
  VerdictControls,
  newMsgId,
  parseUtcMs,
  patchTaskStatus,
  taskIsTerminal,
  utcRelative,
  QuestionChecklistRows,
  questionChecklist,
  questionTextWithoutChecklist,
} from './Inbox'

// TaskDetail — one task, as a page: #/devices/:deviceId/tasks/:taskId.
//
// The header is the task's intent (title, spec, state); the body is the task's
// slice of the machine's DIARY, rendered as the conversation it actually is —
// the worker's readback question, the operator's verdict, the status beats that
// tie runs in, the nudges and their receipts. The diary is one log per MACHINE,
// so this page reads the whole channel and filters to this task; a message with
// no task_id still belongs here when it replies to one that has it (a nudge's
// receipt carries only in_reply_to).
//
// The operator controls live here because this is where the evidence is:
// `done` is a judgment about the work, and the page putting the button next to
// the diary is the UI half of the backend's "the agent can never mark its own
// work done".
//
// That claim used to be all the page had: the worker said "completed" and the
// operator had to leave for the run page to see a single thing the worker
// actually did. So the RUN now sits on this page — its narration, its actions,
// its frames — directly under the checklist it is evidence FOR, and above the
// diary, which is the conversation about the work rather than the work. The
// order is the order a verdict is reached in: what was asked (spec) → what
// counts as done (checklist) → what the worker asserts and the screen it
// stopped on (claim + frames) → what it actually did (trajectory) → the
// conversation (diary). Nothing on that path is a navigation.

// One diary entry, field-for-field routers/channel.py::_serialize.
interface DiaryMessage {
  msg_id: string
  channel: string
  seq: number
  from: 'admin' | 'worker'
  type: 'goal' | 'nudge' | 'question' | 'verdict' | 'status' | 'receipt'
  task_id: string | null
  in_reply_to: string | null
  requires_reply: boolean
  answered_by: string | null
  client_ts: string | null
  server_ts: string
  payload: Record<string, unknown>
}

// Diary poll cadence while the task is live. Faster than the fleet's 30s
// because this page is watched during the exact minutes a task moves; the read
// is one indexed range query from since_seq, so polling hard costs nothing.
const DIARY_POLL_MS = 5_000
// Page size per diary read. A task's slice of a channel is dozens of messages,
// not hundreds — but the cursor loop below keeps reading until it drains, so
// this is a batch size, not a cap on what the page can show.
const DIARY_PAGE = 500

// Spec longer than this starts collapsed: the spec is the whole ask and can be
// a page of its own, and the diary below is what someone opened this for.
const SPEC_COLLAPSE_CHARS = 400

// ── the embedded run ────────────────────────────────────────────────────────

// How often a LIVE run's timeline is asked for what follows its cursor. A
// computer-use step is seconds; FleetRun's cadence, so a run does not appear to
// move at two different speeds depending on which page is showing it.
const EVENT_POLL_MS = 3_000

// Events per request. The backend's own default page size.
const EVENT_PAGE = 200

// Pages drained in one tick. `has_more` normally ends the loop; this is the
// ceiling that stops a malformed answer spinning — and it is also the honest
// cap on this view: 5,000 events per run. A 300-turn run is well inside it.
const MAX_PAGES_PER_TICK = 25

// Timeline rows actually RENDERED, newest end kept. Fetching a long run is
// cheap; putting several thousand rows in the DOM of a page that also has to
// stay responsive to a verdict is not. Past this the pane says what it dropped
// and points at the full run page, which has the whole thing.
const TRAJECTORY_ROWS = 400

// Thumbnails in the evidence strip beside the promoted final frame. The strip
// is for "what did the screen look like on the way here", not an archive — the
// trajectory below carries every frame in its place in the story.
const STRIP_FRAMES = 12

// Lines of narration or payload before a row collapses. FleetRun's number.
const CLAMP_LINES = 12

// How long a batch of presigned URLs is assumed good when the backend states no
// expiry. Only ever re-signs sooner than needed — the harmless direction.
const URL_ASSUMED_LIFE_MS = 4 * 60_000

// Margin before the stated expiry at which a URL counts as already dead. A
// signature that expires while the full-size image is in flight fails exactly
// like one that expired a minute ago, and looks like a broken frame.
const URL_EXPIRY_MARGIN_MS = 30_000

const RUN_TERMINAL = new Set(['completed', 'failed', 'cancelled', 'error', 'done', 'stopped'])

function runIsTerminal(status: string | null | undefined): boolean {
  return RUN_TERMINAL.has((status || '').toLowerCase())
}

function str(v: unknown): string | null {
  return typeof v === 'string' && v ? v : null
}

function plain(v: unknown): string {
  return v === undefined || v === null ? '' : String(v)
}

function safeJson(input: unknown): string {
  if (input === undefined || input === null) return ''
  try {
    return JSON.stringify(input, null, 2)
  } catch {
    return String(input)
  }
}

function TaskDetail() {
  const navigate = useNavigate()
  const { deviceId = '', taskId = '' } = useParams<{ deviceId: string; taskId: string }>()
  const device = useDevice(deviceId)

  const [task, setTask] = useState<TaskRecord | null>(null)
  const [taskError, setTaskError] = useState<string | null>(null)
  const [messages, setMessages] = useState<DiaryMessage[]>([])
  // The incremental cursor: highest seq this page holds. A ref, not state — the
  // poll reads it without wanting to be rebuilt every time a message lands.
  const sinceSeqRef = useRef(0)
  const [confirming, setConfirming] = useState<'approve' | 'kill' | null>(null)
  const [sendingBack, setSendingBack] = useState(false)
  const [patching, setPatching] = useState(false)
  const [patchError, setPatchError] = useState<string | null>(null)

  // Read the task. Quiet after the first load, Admin's poll contract: a failed
  // re-read never replaces a rendered task with an error card.
  const fetchTask = useCallback(
    async (quiet = false) => {
      try {
        const resp = await fetch(`${CU_BACKEND}/tasks/${encodeURIComponent(taskId)}`, {
          headers: authHeaders(),
        })
        if (!resp.ok) {
          if (!quiet) setTaskError(`Failed to load the task (${resp.status})`)
          return
        }
        setTask((await resp.json()) as TaskRecord)
        setTaskError(null)
      } catch (err) {
        if (!quiet) setTaskError(err instanceof Error ? err.message : 'Network error')
      }
    },
    [taskId],
  )

  // Drain everything newer than the cursor. Seq is the only ordering authority,
  // and reading by it makes every poll — and a reconnect after a laptop lid —
  // the same cheap "what follows N" the worker itself does.
  const fetchDiary = useCallback(async () => {
    try {
      for (;;) {
        const resp = await fetch(
          `${CU_BACKEND}/channel/${encodeURIComponent(deviceId)}/messages?since_seq=${sinceSeqRef.current}&limit=${DIARY_PAGE}`,
          { headers: authHeaders() },
        )
        if (!resp.ok) return
        const batch: DiaryMessage[] = await resp.json()
        if (batch.length === 0) return
        sinceSeqRef.current = batch[batch.length - 1].seq
        setMessages((prev) => {
          // Dedupe by seq at merge time. Seq is the channel's primary key, and
          // two reads CAN return the same rows: StrictMode's doubled mount
          // effect, or an onAnswered/onSent refresh racing the poll — both
          // in flight from the same cursor before either advances it. Keying
          // the merge on seq makes a re-delivered page a no-op whatever the
          // cause, and the sort keeps order with the log even then.
          const bySeq = new Map<number, DiaryMessage>()
          for (const m of [...prev, ...batch]) bySeq.set(m.seq, m)
          const merged = [...bySeq.values()].sort((a, b) => a.seq - b.seq)
          // A verdict landing updates its question's answered_by on a row this
          // page already holds, so re-derive that join locally: the question is
          // answered by whatever verdict names it.
          const answeredBy = new Map<string, string>()
          for (const m of merged) {
            if (m.type === 'verdict' && m.in_reply_to) answeredBy.set(m.in_reply_to, m.msg_id)
          }
          return merged.map((m) =>
            m.type === 'question' && !m.answered_by && answeredBy.has(m.msg_id)
              ? { ...m, answered_by: answeredBy.get(m.msg_id) as string }
              : m,
          )
        })
        if (batch.length < DIARY_PAGE) return
      }
    } catch {
      // A missed poll is corrected by the next one.
    }
  }, [deviceId])

  useEffect(() => {
    fetchTask()
    fetchDiary()
  }, [fetchTask, fetchDiary])

  // Poll while the task can still move; a terminal task's diary is a record,
  // not a feed, and re-reading it forever would be a tab-shaped leak.
  const terminal = task !== null && taskIsTerminal(task.status)
  useEffect(() => {
    if (terminal) return
    const id = setInterval(() => {
      fetchTask(true)
      fetchDiary()
    }, DIARY_POLL_MS)
    return () => clearInterval(id)
  }, [terminal, fetchTask, fetchDiary])

  // This task's slice of the channel: its own messages, plus replies to them
  // (a nudge's receipt has in_reply_to but no task_id of its own).
  // Memoized only so the run/claim derivations below hang off a stable array —
  // they feed a polling component, and a fresh identity every render would
  // restart it.
  const thread = useMemo(() => {
    const taskMsgIds = new Set(messages.filter((m) => m.task_id === taskId).map((m) => m.msg_id))
    return messages.filter(
      (m) => m.task_id === taskId || (m.in_reply_to !== null && taskMsgIds.has(m.in_reply_to)),
    )
  }, [messages, taskId])

  // The runs this task produced, and what the worker asserted about them. Both
  // are read out of the diary the page already holds — no extra request, and no
  // second source of truth about which run belongs to this task.
  const runs = useMemo(() => runsInThread(thread, task), [thread, task])
  const claims = useMemo(() => claimsInThread(thread, runs), [thread, runs])

  // All three verbs go through the shared PATCH (patchTaskStatus, which the
  // Inbox rows use too). A failure re-reads the task — a 409 means it moved
  // under us and the page should show where it actually is.
  const patchStatus = useCallback(
    async (body: { status: 'done' | 'killed' | 'queued'; note?: string }) => {
      setConfirming(null)
      setPatching(true)
      setPatchError(null)
      const out = await patchTaskStatus(taskId, body)
      setPatching(false)
      setSendingBack(false)
      if (!out.ok) {
        setPatchError(out.message)
        fetchTask(true)
        return
      }
      setTask(out.task)
      // A send-back writes into the diary too (the directive is part of the
      // conversation); pick it up now rather than at the next poll tick.
      fetchDiary()
    },
    [taskId, fetchTask, fetchDiary],
  )

  const status = task?.status
  // Kill is reachable from every non-terminal state — but as a tertiary act:
  // ending a task's life is not a verdict on its claim.
  const canKill = task !== null && !taskIsTerminal(status)
  const canJudge = status === 'awaiting_verdict'

  return (
    <div
      style={{
        padding: 'var(--sp-5)',
        maxWidth: 'var(--page-max)',
        margin: '0 auto',
        display: 'flex',
        flexDirection: 'column',
        gap: 'var(--sp-4)',
      }}
    >
      {confirming === 'approve' && task && (
        <ConfirmModal
          title={`Approve “${task.title}”?`}
          body={[
            'Approve is your judgment that the claim holds — the worker says the work is finished, and you agree.',
            'The task closes for good; nothing about the machine changes.',
          ]}
          confirmLabel="Approve"
          busy={patching}
          onConfirm={() => patchStatus({ status: 'done' })}
          onCancel={() => setConfirming(null)}
        />
      )}
      {sendingBack && task && (
        <SendBackModal
          taskTitle={task.title}
          busy={patching}
          onSend={(note) => patchStatus({ status: 'queued', note })}
          onCancel={() => setSendingBack(false)}
        />
      )}
      {confirming === 'kill' && task && (
        <ConfirmModal
          title={`Kill “${task.title}”?`}
          body={[
            'The task ends here without being accepted. The worker stops pursuing it the next time it reads its diary.',
            'This cannot be undone — a killed task never moves again.',
          ]}
          confirmLabel="Kill task"
          danger
          busy={patching}
          onConfirm={() => patchStatus({ status: 'killed' })}
          onCancel={() => setConfirming(null)}
        />
      )}

      {/* Back is a ROUTE, not history.back(): this page is linkable, and someone
          arriving from a pasted URL has nothing behind them. */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
        <Button variant="ghost" size="sm" onClick={() => navigate('/inbox')} style={{ flexShrink: 0 }}>
          ← Inbox
        </Button>
        <div style={{ minWidth: 0, flex: 1 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)', minWidth: 0 }}>
            <h1
              style={{
                margin: 0,
                fontSize: 'var(--fs-2xl)',
                fontWeight: 700,
                color: 'var(--sb-text)',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                whiteSpace: 'nowrap',
              }}
            >
              {task ? task.title : 'Task'}
            </h1>
            {task && <TaskStatusPill status={task.status} />}
          </div>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              marginTop: 2,
              fontSize: 'var(--fs-sm)',
              color: 'var(--sb-text-muted)',
            }}
          >
            {device ? (
              <>
                <DeviceDot device={device} />
                <button
                  onClick={() => navigate('/admin')}
                  title="Open the fleet — this machine's pane"
                  style={{
                    font: 'inherit',
                    color: 'var(--sb-gold)',
                    background: 'none',
                    border: 'none',
                    padding: 0,
                    cursor: 'pointer',
                  }}
                >
                  {deviceLabel(device)}
                </button>
                <span>· {deviceLine(device)}</span>
              </>
            ) : (
              <span style={{ fontFamily: 'var(--font-mono)' }}>{deviceId}</span>
            )}
          </div>
        </div>
        {/* The verdict pair judges the CLAIM: Approve accepts it, Send back
            re-queues the task with a note the worker re-reads against. Kill —
            ending the task's life — is deliberately a small tertiary act off
            to the side, in every non-terminal state: it is not one of the
            claim's answers, and it must never read like one. */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', flexShrink: 0 }}>
          {canJudge && (
            <Button variant="primary" size="sm" onClick={() => setConfirming('approve')} disabled={patching}>
              ✓ Approve
            </Button>
          )}
          {canJudge && (
            <Button variant="secondary" size="sm" onClick={() => setSendingBack(true)} disabled={patching}>
              ✕ Send back
            </Button>
          )}
          {canKill && (
            <Button
              variant="ghost"
              size="sm"
              onClick={() => setConfirming('kill')}
              disabled={patching}
              title="End this task without a verdict — cannot be undone"
              style={{ color: 'var(--sb-text-faint)' }}
            >
              Kill
            </Button>
          )}
        </div>
      </div>

      {/* The standing directive from the last send-back, kept next to the
          status it explains: a re-queued task looks like any queued task, and
          this callout is what says it is on its SECOND lap and why. */}
      {task && task.last_directive && (
        <div
          style={{
            border: '1px solid var(--sb-border-gold)',
            background: 'var(--sb-gold-dim)',
            borderRadius: 'var(--r-md)',
            padding: 'var(--sp-3)',
            display: 'flex',
            flexDirection: 'column',
            gap: 4,
          }}
        >
          <span
            style={{
              fontSize: 'var(--fs-xs)',
              fontWeight: 600,
              textTransform: 'uppercase',
              letterSpacing: '1px',
              color: 'var(--sb-gold)',
            }}
          >
            sent back with
          </span>
          <span
            style={{
              fontSize: 'var(--fs-md)',
              lineHeight: 1.6,
              color: 'var(--sb-text)',
              whiteSpace: 'pre-wrap',
            }}
          >
            {task.last_directive}
          </span>
        </div>
      )}

      {patchError && (
        <Card>
          <div className="error-message">{patchError}</div>
        </Card>
      )}
      {taskError && !task && (
        <Card>
          <div className="error-message">{taskError}</div>
        </Card>
      )}

      {!task && !taskError && (
        <Card>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              gap: 'var(--sp-3)',
              color: 'var(--sb-text-muted)',
              padding: 'var(--sp-6)',
            }}
          >
            <Spinner /> Loading the task…
          </div>
        </Card>
      )}

      {task && <SpecCard task={task} />}

      {/* Between the spec (what was asked) and the diary (what happened): the
          checklist is the bridge — the ask broken into judgeable criteria. */}
      {task && <ChecklistCard task={task} onChanged={() => fetchTask(true)} />}

      {/* THE RUN — directly under the criteria it is evidence for, and above
          the diary. The claim ("the worker says both items are met") is
          worthless without the screen behind it, so the two are adjacent and
          the operator never leaves this page to see either. */}
      {task && <TaskRuns task={task} runs={runs} claims={claims} deviceId={deviceId} />}

      {task && (
        <Card title={<SectionTitle>Diary</SectionTitle>}>
          {thread.length === 0 && (
            <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
              Nothing in the diary for this task yet — the worker writes its readback here the
              moment it picks the task up.
            </span>
          )}
          {thread.length > 0 && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
              {thread.map((m) => (
                <DiaryEntry
                  key={m.msg_id}
                  message={m}
                  deviceId={deviceId}
                  onAnswered={fetchDiary}
                  onOpenRun={(runId) =>
                    navigate(
                      `/devices/${encodeURIComponent(deviceId)}/runs/${encodeURIComponent(runId)}`,
                    )
                  }
                />
              ))}
            </div>
          )}

          {/* Steer a live task without blocking it: a nudge is delivered at the
              worker's next turn boundary and acknowledged with a receipt above. */}
          {task.status === 'running' && (
            <NudgeComposer deviceId={deviceId} taskId={taskId} onSent={fetchDiary} />
          )}
        </Card>
      )}
    </div>
  )
}

// The spec — the whole ask, verbatim. Collapsible past a screenful because the
// diary below is the live half of the page; the spec never changes.
function SpecCard({ task }: { task: TaskRecord }) {
  const long = task.spec.length > SPEC_COLLAPSE_CHARS
  const [open, setOpen] = useState(!long)
  const ws = task.workspace
  return (
    <Card
      title={<SectionTitle>Spec</SectionTitle>}
      actions={
        long ? (
          <Button variant="ghost" size="sm" onClick={() => setOpen((o) => !o)}>
            {open ? 'Collapse' : 'Show full spec'}
          </Button>
        ) : undefined
      }
    >
      <div
        style={{
          fontSize: 'var(--fs-md)',
          lineHeight: 1.6,
          color: 'var(--sb-text)',
          whiteSpace: 'pre-wrap',
        }}
      >
        {open ? task.spec : task.spec.slice(0, SPEC_COLLAPSE_CHARS) + '…'}
      </div>
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--sp-2)',
          flexWrap: 'wrap',
          marginTop: 'var(--sp-3)',
          fontSize: 'var(--fs-sm)',
          color: 'var(--sb-text-muted)',
        }}
      >
        <span>created {utcRelative(task.created_at)}</span>
        {task.started_at && <span>· started {utcRelative(task.started_at)}</span>}
        {task.finished_at && <span>· finished {utcRelative(task.finished_at)}</span>}
        <span>· by {task.started_by}</span>
        {ws && (
          <Badge mono title={ws.branch ? `${ws.repo} @ ${ws.branch}` : ws.repo}>
            {ws.repo}
            {ws.branch ? ` @ ${ws.branch}` : ''} · {ws.mode}
          </Badge>
        )}
      </div>
    </Card>
  )
}

// CHECKLIST — the task's definition of done, one judgeable criterion per row.
//
// Deliberately NO edit-in-place anywhere: an edit is a delete + add, so the
// record never lies about what a criterion said when the worker read it. That
// is also why delete asks for no confirmation — deletion is itself recorded by
// absence, and add/delete are meant to be frictionless enough that reshaping
// the definition of done mid-task costs nothing.
//
// Every mutation POSTs and then re-reads the task (`onChanged`): the backend
// copy is the only copy, and rendering our guess instead of its answer is how
// two open consoles would drift.
function ChecklistCard({ task, onChanged }: { task: TaskRecord; onChanged: () => void }) {
  // Defensive on shape: the checklist fields land with parallel backend work,
  // so an older serializer hands us undefined (or its old opaque slot).
  const items: ChecklistItem[] = Array.isArray(task.checklist) ? task.checklist : []
  const [draft, setDraft] = useState('')
  // The item being mutated ('' while adding) — one at a time keeps the
  // re-read races away without optimistic state to reconcile.
  const [busyId, setBusyId] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  // A finished task's checklist is a record, not a form: the criteria are
  // part of what the verdict judged, so nothing may change after the end.
  const readonly = taskIsTerminal(task.status)

  const call = useCallback(
    async (path: string, init: RequestInit, busy: string) => {
      setBusyId(busy)
      setError(null)
      try {
        const resp = await fetch(
          `${CU_BACKEND}/tasks/${encodeURIComponent(task.task_id)}/checklist${path}`,
          { ...init, headers: { ...authHeaders(), 'content-type': 'application/json' } },
        )
        if (!resp.ok) {
          setError(`The checklist change was refused (${resp.status}).`)
          return false
        }
        onChanged()
        return true
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Network error')
        return false
      } finally {
        setBusyId(null)
      }
    },
    [task.task_id, onChanged],
  )

  const add = useCallback(async () => {
    const text = draft.trim()
    if (!text) return
    if (await call('', { method: 'POST', body: JSON.stringify({ text }) }, '')) setDraft('')
  }, [draft, call])

  const approved = items.filter((i) => i.approved).length

  return (
    <Card
      title={
        <>
          <SectionTitle>Checklist</SectionTitle>
          {/* The count is the card's one-glance answer: how much of the
              definition of done the operator has actually signed off. */}
          {items.length > 0 && (
            <Badge mono tone={approved === items.length ? 'success' : 'neutral'} style={{ marginLeft: 8 }}>
              {approved}/{items.length}
            </Badge>
          )}
        </>
      }
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
        {items.length === 0 && (
          <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
            {readonly
              ? 'This task carried no checklist.'
              : 'No criteria yet — each one you add becomes part of what the worker reads back and what you judge at the verdict.'}
          </span>
        )}
        {items.map((item) => (
          <div
            key={item.item_id}
            style={{ display: 'flex', alignItems: 'flex-start', gap: 'var(--sp-2)' }}
          >
            <button
              onClick={() => call(`/${encodeURIComponent(item.item_id)}/${item.approved ? 'unapprove' : 'approve'}`, { method: 'POST' }, item.item_id)}
              disabled={readonly || busyId !== null}
              aria-pressed={item.approved}
              title={
                readonly
                  ? 'The task is closed — the checklist is a record now'
                  : item.approved
                    ? 'Withdraw approval of this criterion'
                    : 'Approve this criterion as met'
              }
              style={{
                width: 18,
                height: 18,
                flexShrink: 0,
                marginTop: 2,
                display: 'inline-flex',
                alignItems: 'center',
                justifyContent: 'center',
                fontSize: 12,
                lineHeight: 1,
                padding: 0,
                color: '#0A0A0A',
                background: item.approved ? 'var(--sb-gold)' : 'transparent',
                border: `1px solid ${item.approved ? 'var(--sb-gold)' : 'var(--sb-border)'}`,
                borderRadius: 4,
                cursor: readonly ? 'default' : 'pointer',
              }}
            >
              {item.approved ? '✓' : ''}
            </button>
            <span
              title={item.added_at ? `added ${utcRelative(item.added_at)}` : undefined}
              style={{
                flex: 1,
                minWidth: 0,
                fontSize: 'var(--fs-md)',
                lineHeight: 1.5,
                whiteSpace: 'pre-wrap',
                color: item.approved ? 'var(--sb-text-muted)' : 'var(--sb-text)',
                textDecoration: item.approved ? 'line-through' : 'none',
              }}
            >
              {item.text}
            </span>
            {!readonly && (
              <IconButton
                size={22}
                title="Delete this criterion — to reword one, delete it and add it again"
                disabled={busyId !== null}
                onClick={() => call(`/${encodeURIComponent(item.item_id)}`, { method: 'DELETE' }, item.item_id)}
              >
                ✕
              </IconButton>
            )}
          </div>
        ))}

        {!readonly && (
          <div style={{ display: 'flex', gap: 'var(--sp-2)', marginTop: items.length > 0 ? 'var(--sp-2)' : 0 }}>
            <input
              className="agent-input"
              style={{
                flex: 1,
                minWidth: 0,
                boxSizing: 'border-box',
                padding: '8px 11px',
                fontFamily: 'inherit',
                fontSize: 'var(--fs-base)',
                background: 'var(--sb-surface-3)',
                color: 'var(--sb-text)',
                border: '1px solid var(--sb-border)',
                borderRadius: 'var(--r-sm)',
              }}
              placeholder="add a criterion — Enter adds it"
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  e.preventDefault()
                  add()
                }
              }}
              disabled={busyId !== null}
            />
            <Button variant="secondary" size="sm" onClick={add} disabled={busyId !== null || !draft.trim()}>
              {busyId === '' ? 'Adding…' : 'Add'}
            </Button>
          </div>
        )}
        {error && <div className="error-message">{error}</div>}
      </div>
    </Card>
  )
}

// ═══════════════════════════════════════════════════════ the run, on the task
//
// Everything below renders ONE task's runs inline. The visual grammar — gold
// rail, narration as prose, actions as ActionChips, frames enlarging in place —
// is FleetRun's, deliberately: a run must not read as two different things
// depending on which page opened it. It is restated rather than imported
// because FleetRun's pieces are module-private and its shell is the wrong one
// here (that page is a run and can own the viewport; this page is a verdict and
// the run is one card on it).

// One persisted run event. Field-for-field routers/runs.py::RunEventOut.
interface RunEventRow {
  seq: number
  type: string
  data: Record<string, unknown> | null
  artifact_kind: string | null
  /** Presigned GET for a frame; minutes of life, re-signed on demand below. */
  url: string | null
  created_at: string | null
}

// Tolerant on where the two artifact fields sit, the same way ScreenCard's
// normalizeFrame is: a strict read that misses `artifact_url` renders a
// timeline with no screenshots, which looks like a worker that captured none
// rather than like a field read from the wrong place.
function normalizeEvent(raw: unknown): RunEventRow | null {
  if (!raw || typeof raw !== 'object') return null
  const row = raw as Record<string, unknown>
  const seq = typeof row.seq === 'number' ? row.seq : Number(row.seq)
  if (!Number.isFinite(seq)) return null
  const data = (row.data ?? null) as Record<string, unknown> | null
  const pick = (key: string): string | null => {
    const top = row[key]
    if (typeof top === 'string' && top) return top
    const inner = data?.[key]
    if (typeof inner === 'string' && inner) return inner
    return null
  }
  return {
    seq,
    type: plain(row.type),
    data,
    artifact_kind: pick('artifact_kind'),
    url: pick('artifact_url') ?? pick('url'),
    created_at: pick('created_at'),
  }
}

// The worker labels its own uploads `screenshot`; a frame that reached storage
// by another path arrives as a generic event with an image artifact.
function isShot(ev: RunEventRow): boolean {
  return ev.type === 'screenshot' || ev.artifact_kind === 'image'
}

// Time first, seq second. The desktop's `seq` counts loop events while an
// uploaded frame is committed under a separate frame counter, so seq alone
// would rake every frame to the top of the run. `created_at` is stamped by the
// one worker that persists the bus — one clock, the run's real order.
//
// Run event timestamps arrive NAIVE-UTC, like the task's; parseUtcMs is what
// reads them as UTC instead of as the viewer's local time.
function compareEvents(a: RunEventRow, b: RunEventRow): number {
  const at = parseUtcMs(a.created_at)
  const bt = parseUtcMs(b.created_at)
  if (Number.isFinite(at) && Number.isFinite(bt) && at !== bt) return at - bt
  return a.seq - b.seq
}

// ── which runs, and what was claimed about them ─────────────────────────────

interface RunRef {
  run_id: string
  /** What the worker said the run ended as, when it said. */
  outcome: string | null
  error: string | null
}

// A task's runs, oldest first.
//
// The join is the worker's `status` diary entry carrying `{run_id, outcome}` —
// channel.rs posts one on every exit path, and it is the only populated link
// today. `task.run_ids` is the field this is SUPPOSED to come from and it is
// still empty on live records, so both are read and the diary wins on order:
// it has a position in the log, and run_ids has nothing to sort by.
function runsInThread(thread: DiaryMessage[], task: TaskRecord | null): RunRef[] {
  const out: RunRef[] = []
  const seen = new Set<string>()
  for (const m of thread) {
    if (m.from !== 'worker') continue
    const runId = str(m.payload.run_id)
    if (!runId || seen.has(runId)) continue
    seen.add(runId)
    out.push({ run_id: runId, outcome: str(m.payload.outcome), error: str(m.payload.error) })
  }
  for (const runId of Array.isArray(task?.run_ids) ? (task as TaskRecord).run_ids : []) {
    if (typeof runId !== 'string' || !runId || seen.has(runId)) continue
    seen.add(runId)
    out.push({ run_id: runId, outcome: null, error: null })
  }
  return out
}

// One checklist item as the WORKER describes it. Every field is what an agent
// asserted about its own work — never a finding, which is the whole reason a
// human is looking at this page.
interface ClaimItem {
  item_id: string
  /** The criterion as the worker restated it. channel.rs copies it into every
   *  claim row so the claim reads on its own — which is what makes a claim
   *  about a criterion since deleted from the checklist still legible. */
  text: string | null
  /** null when the worker named the item but said nothing either way. */
  satisfied: boolean | null
  evidence_note: string | null
  /** The frame the worker points at, by run event seq. */
  frame_seq: number | null
}

interface TaskClaim {
  summary: string | null
  items: ClaimItem[]
}

// Parse a per-item claim out of a diary payload.
//
// The shape is channel.rs's `claim_status_payload`: a `status` message tagged
// `kind: "done_claim"`, carrying `claims: [{item_id, text, satisfied,
// evidence_note, frame_seq}]`, a `summary`, and the whole thing again as prose
// in `text` for consumers that read no structure.
//
// Read tolerantly all the same, and NOTHING here may be required. A payload
// that is not a claim returns null and the page renders exactly as it did
// before the worker learned to claim — off the trajectory and the frames, which
// are a complete answer on their own. Every run before this shipped, and every
// task with no checklist, is that case.
function parseClaim(payload: Record<string, unknown>): TaskClaim | null {
  const nested = (payload.claim ?? null) as Record<string, unknown> | null
  const root = nested && typeof nested === 'object' && !Array.isArray(nested) ? nested : payload
  const rawItems = root.claims ?? root.items
  if (!Array.isArray(rawItems)) return null
  const items: ClaimItem[] = []
  for (const raw of rawItems) {
    if (!raw || typeof raw !== 'object') continue
    const r = raw as Record<string, unknown>
    const itemId = str(r.item_id) ?? str(r.id)
    if (!itemId) continue
    const sat = r.satisfied ?? r.met
    // `frame_seq` is genuinely optional and arrives as JSON null when the
    // worker cited no frame. Number(null) is 0 and Number('') is 0, and 0 is a
    // real seq — so an absent frame read through Number() claims the run's
    // FIRST frame as the evidence for the item. Only a number or a non-blank
    // string is a citation.
    const rawFrame = r.frame_seq
    const frame =
      typeof rawFrame === 'number' || (typeof rawFrame === 'string' && rawFrame.trim() !== '')
        ? Number(rawFrame)
        : NaN
    items.push({
      item_id: itemId,
      text: str(r.text),
      satisfied: typeof sat === 'boolean' ? sat : null,
      evidence_note: str(r.evidence_note) ?? str(r.note),
      frame_seq: Number.isFinite(frame) ? frame : null,
    })
  }
  // An array that carried no usable row is not a claim — heading a card with
  // "what the worker claims" over nothing would assert something nobody said.
  if (items.length === 0) return null
  return { summary: str(root.summary), items }
}

// Claims by run id. A claim naming no run belongs to the newest run — the
// worker posts it as it stops, and that is the run it just finished.
function claimsInThread(thread: DiaryMessage[], runs: RunRef[]): Map<string, TaskClaim> {
  const out = new Map<string, TaskClaim>()
  const newest = runs.length > 0 ? runs[runs.length - 1].run_id : null
  for (const m of thread) {
    if (m.from !== 'worker' || m.type !== 'status') continue
    const claim = parseClaim(m.payload)
    if (!claim) continue
    const runId = str(m.payload.run_id) ?? newest
    if (!runId) continue
    // Last claim wins: a re-post corrects the one before it.
    out.set(runId, claim)
  }
  return out
}

// ── the feed ────────────────────────────────────────────────────────────────

interface RunFeed {
  events: RunEventRow[]
  status: string | null
  live: boolean
  loading: boolean
  error: string | null
  /** True when the drain hit MAX_PAGES_PER_TICK with more still to come. */
  truncated: boolean
  onBadUrl: (ev: RunEventRow) => void
  onEnlarge: (ev: RunEventRow) => Promise<void>
}

// One run's events, drained incrementally and polled only while it is moving.
//
// GET /runs/{id}/events answers with the run's `status` alongside the page, so
// this never calls GET /runs/{id} at all — that route returns the ENTIRE event
// array and would put the whole history back on the wire just to learn one word.
function useRunEvents(runId: string, active: boolean): RunFeed {
  const [events, setEvents] = useState<RunEventRow[]>([])
  const [status, setStatus] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [truncated, setTruncated] = useState(false)
  // When the URLs currently on screen were minted, and when the backend says
  // they die. Both, because `url_expires_at` is the authority when present and
  // an older build omits it.
  const [signedAt, setSignedAt] = useState(0)
  const expiresAtRef = useRef(0)
  // Highest seq held. A ref: the poll reads it on every tick and must never
  // fire from a stale closure — re-requesting from an old cursor is exactly the
  // whole-history refetch the cursor exists to avoid.
  const sinceRef = useRef(-1)
  // URLs already re-signed once. A second failure is not an expiry (the object
  // is gone) and retrying it is a fetch loop against nothing.
  const resignedRef = useRef<Set<string>>(new Set())

  const merge = useCallback((incoming: RunEventRow[]) => {
    if (incoming.length === 0) return
    setEvents((prev) => {
      // Keyed by seq AND type: the two counters above mean a frame event and a
      // loop event can share a seq, and keying on seq alone drops one of them.
      const byKey = new Map(prev.map((e) => [`${e.seq}:${e.type}`, e]))
      for (const ev of incoming) byKey.set(`${ev.seq}:${ev.type}`, ev)
      return [...byKey.values()].sort(compareEvents)
    })
  }, [])

  const pull = useCallback(async () => {
    for (let page = 0; page < MAX_PAGES_PER_TICK; page++) {
      // No cursor yet is an ABSENT since_seq, not a negative one: the backend
      // validates since_seq >= 0, so the -1 sentinel would 422 the first
      // request of every run — and an empty timeline reads as "this run
      // recorded nothing" rather than as a failed request.
      const cursor = sinceRef.current >= 0 ? `since_seq=${sinceRef.current}&` : ''
      const resp = await fetch(
        `${CU_BACKEND}/runs/${encodeURIComponent(runId)}/events?${cursor}limit=${EVENT_PAGE}`,
        { headers: authHeaders() },
      )
      if (!resp.ok) throw new Error(`Could not load this run's timeline (${resp.status})`)
      const body = (await resp.json()) as Record<string, unknown>
      const rows: unknown[] = Array.isArray(body) ? body : ((body.events as unknown[]) ?? [])
      const batch = rows.map(normalizeEvent).filter((e): e is RunEventRow => e !== null)
      merge(batch)
      setStatus(plain(body.status) || null)
      setSignedAt(Date.now())
      expiresAtRef.current = parseUtcMs(str(body.url_expires_at)) || 0
      // Fresh signatures — whatever failed to load before deserves one more try.
      resignedRef.current = new Set()
      for (const ev of batch) if (ev.seq > sinceRef.current) sinceRef.current = ev.seq
      const more = body.has_more === true
      // A page that advanced nothing would loop whatever `has_more` claims.
      if (!more || batch.length === 0) {
        setTruncated(false)
        return
      }
      if (page === MAX_PAGES_PER_TICK - 1) setTruncated(true)
    }
  }, [runId, merge])

  // First load. Reset on the run id, not on `active`: collapsing a past run
  // must not throw away what it already cost to read.
  useEffect(() => {
    if (!active) return
    let cancelled = false
    sinceRef.current = -1
    setEvents([])
    setStatus(null)
    setError(null)
    setLoading(true)
    ;(async () => {
      try {
        await pull()
      } catch (err) {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Network error')
      } finally {
        if (!cancelled) setLoading(false)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [runId, active, pull])

  const live = status !== null && !runIsTerminal(status)

  // Poll only while the run can still change. A finished run is static, and
  // re-signing a history that cannot move — forever, on a tab someone left
  // open — is the leak this guard exists for. The visibility check is the
  // other half: a backgrounded tab is nobody looking.
  useEffect(() => {
    if (!active || !live) return
    const id = setInterval(() => {
      if (typeof document !== 'undefined' && document.hidden) return
      pull().catch(() => {
        // One dropped poll is corrected by the next; a live run is not worth
        // replacing the timeline on screen with an error card.
      })
    }, EVENT_POLL_MS)
    return () => clearInterval(id)
  }, [active, live, pull])

  // A frame whose URL died. Ask for that ONE event again rather than reloading:
  // `since_seq = seq - 1, limit = 1` is the cheapest re-sign there is and it
  // leaves the cursor alone, so the poll does not re-deliver everything after.
  const onBadUrl = useCallback(
    async (ev: RunEventRow) => {
      if (!ev.url || resignedRef.current.has(ev.url)) return
      resignedRef.current.add(ev.url)
      try {
        const resp = await fetch(
          `${CU_BACKEND}/runs/${encodeURIComponent(runId)}/events?since_seq=${ev.seq - 1}&limit=1`,
          { headers: authHeaders() },
        )
        if (!resp.ok) return
        const body = (await resp.json()) as Record<string, unknown>
        const rows: unknown[] = Array.isArray(body) ? body : ((body.events as unknown[]) ?? [])
        merge(rows.map(normalizeEvent).filter((e): e is RunEventRow => e !== null))
        setSignedAt(Date.now())
        expiresAtRef.current = parseUtcMs(str(body.url_expires_at)) || 0
      } catch {
        // The frame renders its "would not load" marker either way.
      }
    },
    [runId, merge],
  )

  // Enlarging refetches the same URL at full size — and a thumbnail the browser
  // already decoded keeps rendering long after its signature died, which makes
  // an expired link invisible until exactly this moment. Re-sign first when the
  // batch is old enough to be a risk.
  const onEnlarge = useCallback(
    async (ev: RunEventRow) => {
      const dead =
        expiresAtRef.current > 0
          ? Date.now() >= expiresAtRef.current - URL_EXPIRY_MARGIN_MS
          : Date.now() - signedAt > URL_ASSUMED_LIFE_MS
      if (!dead) return
      resignedRef.current.delete(ev.url ?? '')
      await onBadUrl(ev)
    },
    [signedAt, onBadUrl],
  )

  return { events, status, live, loading, error, truncated, onBadUrl, onEnlarge }
}

// ── the cards ───────────────────────────────────────────────────────────────

// Every run this task produced, NEWEST FIRST and stacked — not tabs.
//
// A send-back means the task runs again, and the second lap is the one being
// judged, so it leads and opens expanded. The earlier laps stay on the page,
// collapsed, because "what did it do differently this time" is the actual
// question a send-back creates and tabs answer it by hiding one side of the
// comparison. Collapsed is also the cost control: a past run's events are not
// fetched until it is opened.
function TaskRuns({
  task,
  runs,
  claims,
  deviceId,
}: {
  task: TaskRecord
  runs: RunRef[]
  claims: Map<string, TaskClaim>
  deviceId: string
}) {
  if (runs.length === 0) {
    return (
      <Card title={<SectionTitle>The run</SectionTitle>}>
        <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
          {taskIsTerminal(task.status)
            ? 'This task closed without a run — nothing was executed for it.'
            : 'No run yet. The worker posts its run into the diary the moment it starts one, and the trajectory appears here as it happens.'}
        </span>
      </Card>
    )
  }
  const newest = runs.length - 1
  return (
    <>
      {runs
        .map((run, i) => ({ run, i }))
        .reverse()
        .map(({ run, i }) => (
          <RunCard
            key={run.run_id}
            run={run}
            ordinal={i + 1}
            total={runs.length}
            latest={i === newest}
            claim={claims.get(run.run_id) ?? null}
            checklist={Array.isArray(task.checklist) ? task.checklist : []}
            deviceId={deviceId}
          />
        ))}
    </>
  )
}

function RunCard({
  run,
  ordinal,
  total,
  latest,
  claim,
  checklist,
  deviceId,
}: {
  run: RunRef
  ordinal: number
  total: number
  latest: boolean
  claim: TaskClaim | null
  checklist: ChecklistItem[]
  deviceId: string
}) {
  const navigate = useNavigate()
  const [open, setOpen] = useState(latest)
  const feed = useRunEvents(run.run_id, open)

  const frames = useMemo(() => feed.events.filter(isShot), [feed.events])
  // The frame the claim rests on: the LAST one the worker managed to put in
  // front of us. A frame with no signed URL is a frame nobody can look at, so
  // it cannot be the one promoted — fall back past it.
  const finalFrame = useMemo(() => {
    for (let i = frames.length - 1; i >= 0; i--) if (frames[i].url) return frames[i]
    return frames.length > 0 ? frames[frames.length - 1] : null
  }, [frames])
  const bySeq = useMemo(() => new Map(frames.map((f) => [f.seq, f])), [frames])

  const label = total > 1 ? `Run ${ordinal} of ${total}` : 'The run'
  const shots = frames.length

  return (
    <Card
      title={
        <>
          <SectionTitle>{label}</SectionTitle>
          {latest && total > 1 && (
            <Badge tone="neutral" style={{ marginLeft: 8 }}>
              latest
            </Badge>
          )}
          {run.outcome && (
            <Badge
              tone={run.outcome === 'completed' ? 'success' : 'neutral'}
              style={{ marginLeft: 8 }}
            >
              {run.outcome}
            </Badge>
          )}
          {feed.live && (
            <Badge tone="neutral" style={{ marginLeft: 8 }}>
              in flight
            </Badge>
          )}
        </>
      }
      actions={
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>
          {open && (
            <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
              {feed.events.length} events{shots ? ` · ${shots} frames` : ''}
            </span>
          )}
          <Button variant="ghost" size="sm" onClick={() => setOpen((o) => !o)}>
            {open ? 'Collapse' : 'Show this run'}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            title="The run on its own page — the same timeline, full width"
            onClick={() =>
              navigate(
                `/devices/${encodeURIComponent(deviceId)}/runs/${encodeURIComponent(run.run_id)}`,
              )
            }
          >
            Full run →
          </Button>
        </div>
      }
    >
      {!open ? (
        <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
          An earlier lap of this task — nothing is read from the backend for it until you open it.
          Open it to compare what the worker did then with what it did on the run above.
        </span>
      ) : (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
          {run.error && <div className="error-message">{run.error}</div>}
          {feed.error && <div className="error-message">{feed.error}</div>}
          {feed.loading && feed.events.length === 0 && (
            <div
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 'var(--sp-3)',
                color: 'var(--sb-text-muted)',
                padding: 'var(--sp-4)',
              }}
            >
              <Spinner /> Reading the run…
            </div>
          )}

          <ClaimRows claim={claim} checklist={checklist} frames={bySeq} feed={feed} />

          <FinalFrame frame={finalFrame} frames={frames} live={feed.live} feed={feed} />

          <Trajectory feed={feed} />
        </div>
      )}
    </Card>
  )
}

// WHAT THE WORKER SAYS IT DID, one row per criterion.
//
// Every word here is hedged on purpose. An agent asserting that it satisfied a
// criterion is precisely the thing the human is on this page to check, so the
// UI never says "item met" — it says who says so. The operator's own approval
// of a criterion stays where it was, in the checklist card above: this card
// reports, it does not decide.
//
// Renders nothing at all when no claim was posted. Older runs and the worker
// build shipping today post none, and the frames and trajectory below are a
// complete answer without it — the page must not degrade into a blank space
// waiting for a payload.
function ClaimRows({
  claim,
  checklist,
  frames,
  feed,
}: {
  claim: TaskClaim | null
  checklist: ChecklistItem[]
  frames: Map<number, RunEventRow>
  feed: RunFeed
}) {
  if (!claim) {
    if (checklist.length === 0) return null
    return (
      <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)', lineHeight: 1.5 }}>
        This run made no per-item claim about the checklist. Judge it from the screen it stopped on
        and the trajectory below.
      </div>
    )
  }

  // Claim order follows the CHECKLIST, so the rows line up with the card above
  // one-for-one. An item the worker said nothing about is a row too — silence
  // about a criterion is a finding, and dropping it would hide one.
  const byItem = new Map(claim.items.map((c) => [c.item_id, c]))
  const rows = checklist.length > 0 ? checklist : []
  const orphans = claim.items.filter((c) => !rows.some((r) => r.item_id === c.item_id))

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
      <div
        style={{
          fontSize: 'var(--fs-xs)',
          fontWeight: 600,
          textTransform: 'uppercase',
          letterSpacing: '1px',
          color: 'var(--sb-gold)',
        }}
      >
        what the worker claims
      </div>
      {rows.map((item) => (
        <ClaimRow
          key={item.item_id}
          text={item.text}
          claim={byItem.get(item.item_id) ?? null}
          frames={frames}
          feed={feed}
        />
      ))}
      {orphans.map((c) => (
        <ClaimRow
          key={c.item_id}
          // A claim about a criterion that is no longer on the list: the
          // checklist was reshaped after the worker read it. The claim carries
          // its own copy of the wording, so say what it was about rather than
          // drop the assertion.
          text={c.text ?? '(a criterion no longer on the checklist)'}
          stale
          claim={c}
          frames={frames}
          feed={feed}
        />
      ))}
      {claim.summary && (
        <div
          style={{
            fontSize: 'var(--fs-md)',
            lineHeight: 1.6,
            color: 'var(--sb-text-muted)',
            whiteSpace: 'pre-wrap',
            borderTop: '1px solid var(--sb-border)',
            paddingTop: 'var(--sp-3)',
          }}
        >
          {claim.summary}
        </div>
      )}
    </div>
  )
}

function ClaimRow({
  text,
  claim,
  frames,
  feed,
  stale,
}: {
  text: string
  claim: ClaimItem | null
  frames: Map<number, RunEventRow>
  feed: RunFeed
  /** The criterion this claim names is no longer on the checklist. */
  stale?: boolean
}) {
  const frame = claim?.frame_seq != null ? (frames.get(claim.frame_seq) ?? null) : null
  const asserted =
    claim === null
      ? { text: 'the worker said nothing about this one', tone: 'var(--sb-text-faint)' }
      : claim.satisfied === true
        ? { text: 'the worker claims this is met', tone: 'var(--sb-gold)' }
        : claim.satisfied === false
          ? { text: 'the worker says this is NOT met', tone: 'var(--sb-danger-bright)' }
          : { text: 'the worker named this one without saying either way', tone: 'var(--sb-text-faint)' }

  return (
    <div
      style={{
        display: 'flex',
        gap: 'var(--sp-3)',
        alignItems: 'flex-start',
        border: '1px solid var(--sb-border)',
        borderRadius: 'var(--r-md)',
        padding: 'var(--sp-3)',
      }}
    >
      <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 4 }}>
        <span
          style={{
            fontSize: 'var(--fs-md)',
            lineHeight: 1.5,
            color: 'var(--sb-text)',
            whiteSpace: 'pre-wrap',
          }}
        >
          {text}
        </span>
        {stale && (
          <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--sb-text-faint)' }}>
            no longer on the checklist — it was reworded or removed after the worker read it
          </span>
        )}
        <span style={{ fontSize: 'var(--fs-sm)', color: asserted.tone }}>{asserted.text}</span>
        {claim?.evidence_note && (
          <span
            style={{
              fontSize: 'var(--fs-sm)',
              lineHeight: 1.5,
              color: 'var(--sb-text-muted)',
              whiteSpace: 'pre-wrap',
            }}
          >
            “{claim.evidence_note}”
          </span>
        )}
        {claim?.frame_seq != null && !frame && (
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
            points at frame #{claim.frame_seq}, which is not in this run's timeline
          </span>
        )}
      </div>
      {/* The frame the worker points at, beside the thing it is offered as
          proof of — the join the operator would otherwise do by hand, scrolling
          a strip of fourteen thumbnails looking for the right one. */}
      {frame && <Shot ev={frame} width={200} feed={feed} caption={`frame #${frame.seq}`} />}
    </div>
  )
}

// THE LAST FRAME, large and first.
//
// Fourteen thumbnails in a row make the final one the fourteenth, and the final
// one is the only one the claim actually rests on: it is the state of the
// machine at the moment the worker decided it was finished. Everything else is
// how it got there. So it is promoted, said out loud, and the rest of the
// frames follow it as a strip.
function FinalFrame({
  frame,
  frames,
  live,
  feed,
}: {
  frame: RunEventRow | null
  frames: RunEventRow[]
  live: boolean
  feed: RunFeed
}) {
  if (!frame) {
    if (feed.events.length === 0) return null
    return (
      <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
        This run reached the console with no screenshot — there is no picture of the screen behind
        its claim, only the trajectory below.
      </span>
    )
  }
  // Newest-first, and never the promoted frame twice.
  const strip = frames.filter((f) => f !== frame).reverse()
  const shown = strip.slice(0, STRIP_FRAMES)
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--sp-2)', flexWrap: 'wrap' }}>
        <span
          style={{
            fontSize: 'var(--fs-xs)',
            fontWeight: 600,
            textTransform: 'uppercase',
            letterSpacing: '1px',
            color: 'var(--sb-gold)',
          }}
        >
          {live ? 'the screen right now' : 'the screen when the worker stopped'}
        </span>
        <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
          frame #{frame.seq} · {utcRelative(frame.created_at)}
        </span>
      </div>
      <Shot ev={frame} width={860} feed={feed} />
      {shown.length > 0 && (
        <>
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
            how it got there — newest first
            {strip.length > shown.length
              ? ` · showing ${shown.length} of ${strip.length} earlier frames, the rest are in the trajectory below`
              : ''}
          </span>
          <div style={{ display: 'flex', gap: 'var(--sp-2)', flexWrap: 'wrap' }}>
            {shown.map((f) => (
              <Shot key={`${f.seq}:strip`} ev={f} width={150} feed={feed} caption={`#${f.seq}`} />
            ))}
          </div>
        </>
      )}
    </div>
  )
}

// The run, step by step — the same rail RunDetail and FleetRun draw.
function Trajectory({ feed }: { feed: RunFeed }) {
  const [open, setOpen] = useState(true)
  const all = feed.events
  // The TAIL is kept when a run is longer than the pane will render: the end of
  // a run is what a verdict is about, and the beginning of one is the part the
  // full run page exists for.
  const shown = all.length > TRAJECTORY_ROWS ? all.slice(-TRAJECTORY_ROWS) : all
  if (all.length === 0 && !feed.loading) {
    return (
      <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
        {feed.live
          ? 'The machine has taken the run but has not reported a step yet.'
          : 'This run recorded no timeline — usually a worker that lost its connection before its first turn.'}
      </span>
    )
  }
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>
        <Button variant="ghost" size="sm" onClick={() => setOpen((o) => !o)}>
          {open ? '▾ Trajectory' : `▸ Trajectory · ${all.length} steps`}
        </Button>
        {feed.live && (
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
            following · every {Math.round(EVENT_POLL_MS / 1000)}s
          </span>
        )}
      </div>
      {open && (shown.length < all.length || feed.truncated) && (
        <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)', lineHeight: 1.5 }}>
          {shown.length < all.length
            ? `Showing the last ${shown.length} of ${all.length} steps. `
            : ''}
          {feed.truncated
            ? `This run is longer than ${MAX_PAGES_PER_TICK * EVENT_PAGE} events, which is as much as this card reads. `
            : ''}
          Open the full run for everything.
        </span>
      )}
      {open && (
        <div
          style={{
            maxHeight: '52vh',
            overflowY: 'auto',
            paddingRight: 'var(--sp-2)',
            display: 'flex',
            flexDirection: 'column',
            gap: 'var(--sp-3)',
            borderLeft: '2px solid var(--sb-gold-line)',
            paddingLeft: 'var(--sp-4)',
          }}
        >
          {shown.map((ev) => (
            <EventRow key={`${ev.seq}:${ev.type}`} ev={ev} feed={feed} />
          ))}
        </div>
      )}
    </div>
  )
}

// One event, rendered by type — RunDetail's shapes, so a run reads the same
// whichever page is showing it.
function EventRow({ ev, feed }: { ev: RunEventRow; feed: RunFeed }) {
  const d = (ev.data ?? {}) as Record<string, unknown>

  if (isShot(ev)) return <Shot ev={ev} width={220} feed={feed} />

  switch (ev.type) {
    // `model_delta` is the streaming form of `text`; the desktop posts whole
    // turns today, and reading both costs one label.
    case 'text':
    case 'model_delta': {
      const text = plain(d.text ?? d.delta)
      if (!text.trim()) return null
      return <Clamped text={text} prose />
    }
    case 'action':
    case 'tool_use':
      return (
        <div>
          <ActionChip name={plain(d.name)} input={d.input} />
        </div>
      )
    case 'tool_result': {
      const body = plain(d.content ?? d.output ?? d.text) || safeJson(d)
      if (!body.trim()) return null
      return <Clamped text={body} />
    }
    case 'status': {
      // One per turn, carrying { turn, state } — the only marker of where one
      // turn ends and the next begins, so it is a rule rather than a line.
      if (d.turn != null) {
        return (
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              marginTop: 'var(--sp-2)',
            }}
          >
            <SectionTitle>Turn {plain(d.turn)}</SectionTitle>
            <div style={{ flex: 1, height: 1, background: 'var(--sb-gold-line)' }} />
          </div>
        )
      }
      return <Marker text={plain(d.state ?? d.status ?? d.message)} tone="muted" />
    }
    case 'done':
      return <Marker icon="✓" text={`done${d.reason ? ` (${plain(d.reason)})` : ''}`} tone="gold" />
    case 'error':
      return <Marker icon="✕" text={`error: ${plain(d.error ?? d.message)}`} tone="danger" />
    default:
      return <Clamped text={`${ev.type}: ${safeJson(ev.data)}`} />
  }
}

// A frame. It enlarges IN PLACE and never opens a tab: a presigned URL that
// died while this page sat open fails silently in a new window, where nothing
// can catch the error or re-sign it — here the failure lands on an onError this
// component owns.
function Shot({
  ev,
  width,
  feed,
  caption,
}: {
  ev: RunEventRow
  /** Collapsed width in px; clicking toggles to the card's full width. */
  width: number
  feed: RunFeed
  caption?: string
}) {
  const [big, setBig] = useState(false)
  const [broken, setBroken] = useState(false)
  // One re-sign per frame, ever. An expired signature is fixed by a fresh URL;
  // a frame whose object is gone from the bucket fails again on the new one.
  const triedRef = useRef(0)

  // No signed URL: the frame stayed on the worker's own disk, or the backend
  // answered without one. Say a frame exists rather than render a broken tile —
  // and never try a local path, which belongs to another machine entirely.
  if (!ev.url) {
    return <Marker icon="📸" text="a frame was captured, with no link this console can open" tone="muted" />
  }
  if (broken) {
    return <Marker icon="📸" text="this frame would not load, even with a fresh link" tone="muted" />
  }

  return (
    <figure style={{ margin: 0, minWidth: 0 }}>
      <img
        src={ev.url}
        alt={caption ? `screen at ${caption}` : `screen at step ${ev.seq}`}
        loading="lazy"
        onClick={async () => {
          if (!big) await feed.onEnlarge(ev)
          setBig((v) => !v)
        }}
        onError={() => {
          // Nearly always a signature that expired while this page sat open, so
          // the first failure buys one fresh URL and the second is reported.
          if (triedRef.current >= 1) {
            setBroken(true)
            return
          }
          triedRef.current += 1
          feed.onBadUrl(ev)
        }}
        style={{
          display: 'block',
          width: big ? '100%' : width,
          maxWidth: '100%',
          height: 'auto',
          borderRadius: 'var(--r-sm)',
          border: `1px solid ${big ? 'var(--sb-border-gold)' : 'var(--sb-border)'}`,
          background: 'var(--sb-surface-2)',
          boxShadow: big ? 'var(--shadow-2)' : undefined,
          cursor: big ? 'zoom-out' : 'zoom-in',
        }}
      />
      {caption && (
        <figcaption
          style={{
            marginTop: 2,
            fontSize: 'var(--fs-xs)',
            fontFamily: 'var(--font-mono)',
            color: 'var(--sb-text-faint)',
          }}
        >
          {caption}
        </figcaption>
      )}
    </figure>
  )
}

// Long bodies collapse. `prose` is the model's own narration, set in the
// reading face; everything else is a payload and stays mono, because what is
// being looked for in a tool result is usually a literal.
function Clamped({ text, prose }: { text: string; prose?: boolean }) {
  const [open, setOpen] = useState(false)
  const lines = useMemo(() => text.split('\n'), [text])
  const long = lines.length > CLAMP_LINES || text.length > 1200
  const shown = open || !long ? text : lines.slice(0, CLAMP_LINES).join('\n')
  return (
    <div style={{ minWidth: 0 }}>
      <div
        style={{
          margin: 0,
          fontFamily: prose ? 'var(--font-sans)' : 'var(--font-mono)',
          fontSize: prose ? 'var(--fs-md)' : 'var(--fs-sm)',
          lineHeight: prose ? 1.6 : 1.5,
          color: prose ? 'var(--sb-text)' : 'var(--sb-text-muted)',
          whiteSpace: 'pre-wrap',
          // Long unbroken tokens — a base64 blob, a URL — are what actually
          // widen a flex column past its container.
          overflowWrap: 'anywhere',
          wordBreak: 'break-word',
        }}
      >
        {shown}
        {long && !open && '…'}
      </div>
      {long && (
        <Button variant="ghost" size="sm" onClick={() => setOpen((v) => !v)} style={{ marginTop: 4 }}>
          {open ? 'Show less' : `Show all ${lines.length} lines`}
        </Button>
      )}
    </div>
  )
}

// A compact single-line marker on the rail — RunDetail's PillLine.
function Marker({ icon, text, tone }: { icon?: string; text: string; tone: 'muted' | 'gold' | 'danger' }) {
  if (!text.trim()) return null
  const color =
    tone === 'danger'
      ? 'var(--sb-danger-bright)'
      : tone === 'gold'
        ? 'var(--sb-gold)'
        : 'var(--sb-text-muted)'
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'baseline',
        gap: 6,
        fontSize: 'var(--fs-sm)',
        color,
        wordBreak: 'break-word',
      }}
    >
      {icon && <span aria-hidden>{icon}</span>}
      <span>{text}</span>
    </div>
  )
}

// Wall-clock + relative, off the server timestamp. Server order (seq) decides
// position; time is only decoration here.
function whenLine(m: DiaryMessage): string {
  const ms = parseUtcMs(m.server_ts)
  if (!Number.isFinite(ms)) return ''
  return `${new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })} · ${relativeTime(ms)}`
}

// One diary entry, rendered by type. Questions are the loudest thing in the
// thread (they froze the machine); verdicts wear their decision's color;
// status carries the run join; nudges and receipts are bookkeeping and stay
// small — receipts collapsed entirely until asked.
function DiaryEntry({
  message: m,
  deviceId,
  onAnswered,
  onOpenRun,
}: {
  message: DiaryMessage
  deviceId: string
  onAnswered: () => void
  onOpenRun: (runId: string) => void
}) {
  const text = str(m.payload.text)
  const who = m.from === 'admin' ? 'operator' : 'worker'
  const meta = (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-2)',
        fontSize: 'var(--fs-xs)',
        color: 'var(--sb-text-faint)',
      }}
    >
      <span style={{ textTransform: 'uppercase', letterSpacing: '0.6px' }}>
        {who} · {m.type}
      </span>
      <span>{whenLine(m)}</span>
      <span style={{ fontFamily: 'var(--font-mono)' }}>#{m.seq}</span>
    </div>
  )

  if (m.type === 'question') {
    const answered = !!m.answered_by
    const qRows = questionChecklist(m.payload)
    return (
      <div
        style={{
          border: '1px solid var(--sb-border-gold)',
          borderRadius: 'var(--r-md)',
          background: 'var(--sb-surface-2)',
          padding: 'var(--sp-3)',
          display: 'flex',
          flexDirection: 'column',
          gap: 'var(--sp-2)',
        }}
      >
        {meta}
        <div
          style={{
            fontSize: 'var(--fs-md)',
            lineHeight: 1.6,
            color: 'var(--sb-text)',
            whiteSpace: 'pre-wrap',
          }}
        >
          {questionTextWithoutChecklist(text ?? '', qRows.length > 0) || '(no text)'}
        </div>
        <QuestionChecklistRows items={qRows} />
        {!answered && m.requires_reply && (
          <VerdictControls
            deviceId={deviceId}
            questionMsgId={m.msg_id}
            taskId={m.task_id}
            onAnswered={onAnswered}
          />
        )}
        {answered && (
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
            answered — the verdict follows below
          </span>
        )}
      </div>
    )
  }

  if (m.type === 'verdict') {
    const rejected = str(m.payload.decision) === 'rejected'
    const tone = rejected
      ? { color: 'var(--sb-danger-bright)', border: 'rgba(192, 57, 43, 0.40)' }
      : { color: 'var(--sb-success)', border: 'rgba(111, 184, 122, 0.30)' }
    return (
      <div
        style={{
          // Indented under the question it settles — a verdict is a reply, and
          // the thread should read that way even with rows between them.
          marginLeft: 'var(--sp-5)',
          border: `1px solid ${tone.border}`,
          borderRadius: 'var(--r-md)',
          padding: 'var(--sp-3)',
          display: 'flex',
          flexDirection: 'column',
          gap: 'var(--sp-2)',
        }}
      >
        {meta}
        <div style={{ fontSize: 'var(--fs-md)', fontWeight: 700, color: tone.color }}>
          {rejected ? '✕ Rejected' : '✓ Approved'}
        </div>
        {text && (
          <div
            style={{
              fontSize: 'var(--fs-md)',
              lineHeight: 1.6,
              color: 'var(--sb-text)',
              whiteSpace: 'pre-wrap',
            }}
          >
            {text}
          </div>
        )}
      </div>
    )
  }

  if (m.type === 'status' || m.type === 'goal') {
    const runId = str(m.payload.run_id)
    const outcome = str(m.payload.outcome)
    // A claim arrives as a status entry whose `text` restates every item in
    // prose (channel.rs writes it twice on purpose, for consumers that read no
    // structure). This page IS such a consumer of the structure — the claim is
    // rendered item by item up beside the checklist — so the diary shows the
    // one-line fact that it was posted instead of the whole thing again.
    const claim = m.type === 'status' ? parseClaim(m.payload) : null
    const claimLine = claim
      ? `claimed the checklist item by item — ${
          claim.items.filter((c) => c.satisfied === true).length
        } of ${claim.items.length} asserted satisfied, shown with the run above`
      : null
    return (
      <div
        style={{
          border: '1px solid var(--sb-border)',
          borderRadius: 'var(--r-md)',
          padding: 'var(--sp-3)',
          display: 'flex',
          flexDirection: 'column',
          gap: 'var(--sp-2)',
        }}
      >
        {meta}
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-3)',
            flexWrap: 'wrap',
          }}
        >
          <span
            style={{
              flex: 1,
              minWidth: 200,
              fontSize: 'var(--fs-md)',
              lineHeight: 1.5,
              color: 'var(--sb-text)',
              whiteSpace: 'pre-wrap',
            }}
          >
            {claimLine ?? text ?? '(no text)'}
          </span>
          {outcome && <Badge tone={outcome === 'completed' ? 'success' : 'neutral'}>{outcome}</Badge>}
          {runId && (
            <Button variant="secondary" size="sm" onClick={() => onOpenRun(runId)}>
              Open run →
            </Button>
          )}
        </div>
      </div>
    )
  }

  if (m.type === 'receipt') {
    return <ReceiptRow message={m} meta={meta} />
  }

  // nudge — the operator steering mid-flight. Small and muted: it matters to
  // the story, not to the eye.
  return (
    <div
      style={{
        marginLeft: 'var(--sp-5)',
        display: 'flex',
        flexDirection: 'column',
        gap: 2,
        fontSize: 'var(--fs-sm)',
        color: 'var(--sb-text-muted)',
      }}
    >
      {meta}
      <span style={{ whiteSpace: 'pre-wrap' }}>{text || '(no text)'}</span>
    </div>
  )
}

// A receipt is bookkeeping ("your nudge was injected at the turn boundary") —
// one collapsed line, the payload behind a click for the rare day it matters.
function ReceiptRow({ message: m, meta }: { message: DiaryMessage; meta: React.ReactNode }) {
  const [open, setOpen] = useState(false)
  const disposition = str(m.payload.disposition) ?? str(m.payload.note) ?? 'receipt'
  return (
    <div style={{ marginLeft: 'var(--sp-5)', fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
      <button
        onClick={() => setOpen((o) => !o)}
        style={{
          font: 'inherit',
          color: 'inherit',
          background: 'none',
          border: 'none',
          padding: 0,
          cursor: 'pointer',
        }}
        title={open ? 'Collapse this receipt' : 'Show what this receipt carried'}
      >
        {open ? '▾' : '▸'} receipt · {disposition} · {whenLine(m)}
      </button>
      {open && (
        <pre
          style={{
            margin: 'var(--sp-2) 0 0',
            padding: 'var(--sp-2)',
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--fs-xs)',
            lineHeight: 1.5,
            color: 'var(--sb-text-muted)',
            background: 'var(--sb-surface-2)',
            border: '1px solid var(--sb-border)',
            borderRadius: 'var(--r-sm)',
            overflowX: 'auto',
          }}
        >
          {JSON.stringify(m.payload, null, 2)}
        </pre>
      )}
      {open && <div>{meta}</div>}
    </div>
  )
}

// Send one nudge into a running task's diary. The worker takes it at its next
// turn boundary and answers with a receipt, which the poll renders above.
function NudgeComposer({
  deviceId,
  taskId,
  onSent,
}: {
  deviceId: string
  taskId: string
  onSent: () => void
}) {
  const [text, setText] = useState('')
  const [sending, setSending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const send = useCallback(async () => {
    const body = text.trim()
    if (!body) return
    setSending(true)
    setError(null)
    try {
      const resp = await fetch(`${CU_BACKEND}/channel/${encodeURIComponent(deviceId)}/messages`, {
        method: 'POST',
        headers: { ...authHeaders(), 'content-type': 'application/json' },
        body: JSON.stringify({
          msg_id: newMsgId(),
          type: 'nudge',
          task_id: taskId,
          payload: { text: body },
        }),
      })
      if (!resp.ok) {
        setError(`The nudge was refused (${resp.status}).`)
        return
      }
      setText('')
      onSent()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Network error')
    } finally {
      setSending(false)
    }
  }, [deviceId, taskId, text, onSent])

  return (
    <div style={{ marginTop: 'var(--sp-4)', display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
      <div style={{ display: 'flex', gap: 'var(--sp-2)' }}>
        <input
          className="agent-input"
          style={{
            flex: 1,
            minWidth: 0,
            boxSizing: 'border-box',
            padding: '9px 11px',
            fontFamily: 'inherit',
            fontSize: 'var(--fs-base)',
            background: 'var(--sb-surface-3)',
            color: 'var(--sb-text)',
            border: '1px solid var(--sb-border)',
            borderRadius: 'var(--r-sm)',
          }}
          placeholder="nudge the worker — delivered at its next turn boundary"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              send()
            }
          }}
          disabled={sending}
        />
        <Button variant="primary" size="sm" onClick={send} disabled={sending || !text.trim()}>
          {sending ? 'Sending…' : 'Send nudge'}
        </Button>
      </div>
      {error && <div className="error-message">{error}</div>}
    </div>
  )
}

export default TaskDetail

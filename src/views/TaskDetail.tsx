import { useCallback, useEffect, useRef, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import { Badge, Button, Card, ConfirmModal, SectionTitle, Spinner } from '../ui'
import { DeviceDot, deviceLabel, deviceLine, useDevice } from './DeviceRuns'
import {
  TaskRecord,
  TaskStatusPill,
  VerdictControls,
  newMsgId,
  parseUtcMs,
  taskIsTerminal,
  utcRelative,
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

function str(v: unknown): string | null {
  return typeof v === 'string' && v ? v : null
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
  const [confirming, setConfirming] = useState<'done' | 'kill' | null>(null)
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
          // A verdict landing updates its question's answered_by on a row this
          // page already holds, so re-derive that join locally: the question is
          // answered by whatever verdict names it.
          const merged = [...prev, ...batch]
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
  const taskMsgIds = new Set(messages.filter((m) => m.task_id === taskId).map((m) => m.msg_id))
  const thread = messages.filter(
    (m) => m.task_id === taskId || (m.in_reply_to !== null && taskMsgIds.has(m.in_reply_to)),
  )

  const patchStatus = useCallback(
    async (status: 'done' | 'killed') => {
      setConfirming(null)
      setPatching(true)
      setPatchError(null)
      try {
        const resp = await fetch(`${CU_BACKEND}/tasks/${encodeURIComponent(taskId)}`, {
          method: 'PATCH',
          headers: { ...authHeaders(), 'content-type': 'application/json' },
          body: JSON.stringify({ status }),
        })
        if (!resp.ok) {
          // A 409 names both states ("illegal transition: X -> Y") — worth
          // surfacing verbatim, because it means the task moved under us.
          let detail = `(${resp.status})`
          try {
            const body = await resp.json()
            if (typeof body?.detail === 'string') detail = body.detail
          } catch {
            // keep the status code
          }
          setPatchError(`Could not update the task: ${detail}`)
          fetchTask(true)
          return
        }
        setTask((await resp.json()) as TaskRecord)
      } catch (err) {
        setPatchError(err instanceof Error ? err.message : 'Network error')
      } finally {
        setPatching(false)
      }
    },
    [taskId, fetchTask],
  )

  const status = task?.status
  const canKill = status === 'queued' || status === 'readback' || status === 'running'
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
      {confirming === 'done' && task && (
        <ConfirmModal
          title={`Mark “${task.title}” done?`}
          body={[
            'Done is your judgment that the work is actually finished — the worker can claim it, but only you can say it.',
            'The task closes for good; nothing about the machine changes.',
          ]}
          confirmLabel="Mark done"
          busy={patching}
          onConfirm={() => patchStatus('done')}
          onCancel={() => setConfirming(null)}
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
          onConfirm={() => patchStatus('killed')}
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
        <div style={{ display: 'flex', gap: 'var(--sp-2)', flexShrink: 0 }}>
          {canJudge && (
            <Button variant="primary" size="sm" onClick={() => setConfirming('done')} disabled={patching}>
              ✓ Mark done
            </Button>
          )}
          {(canJudge || canKill) && (
            <Button variant="danger" size="sm" onClick={() => setConfirming('kill')} disabled={patching}>
              ✕ Kill
            </Button>
          )}
        </div>
      </div>

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
          {text || '(no text)'}
        </div>
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
            {text || '(no text)'}
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

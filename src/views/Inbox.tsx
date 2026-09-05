import { useCallback, useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import { Button, Card, ConfirmModal, Divider, EmptyState, SectionTitle, Spinner, StatusPill } from '../ui'

// Inbox — the "what needs me" page.
//
// Everything on it is a machine standing still until a human acts: a worker
// blocked on an unanswered question is frozen real hardware, and a task at
// awaiting_verdict is work somebody claimed finished that no one has judged.
// Both lists come straight from the operator queries the backend built for
// exactly this (`/channel/inbox/awaiting-reply`, `/tasks?status=awaiting_verdict`),
// so this page never derives urgency itself — it renders what the server says
// is waiting, oldest first.

// ───────────────────────────────────────────────────────── shared task bits
//
// The task layer's API shapes and helpers live here rather than in a lib file
// because this page is the task layer's front door — TaskDetail and the Devices
// pane import from it the way Admin imports RunRow from DeviceRuns.

// One criterion of a task's definition of done. Items are append-and-delete
// only — there is deliberately no edit anywhere: a criterion the worker read
// must stay verbatim in the record, and a change of mind is a delete + add.
export interface ChecklistItem {
  item_id: string
  text: string
  approved: boolean
  added_at: string | null
}

// One task, field-for-field the backend contract (routers/tasks.py::_serialize).
export interface TaskRecord {
  task_id: string
  device_id: string
  title: string
  spec: string
  workspace: {
    repo: string
    mode: string
    branch: string | null
    baseline_commit: string | null
    subdir: string | null
    auth_ref: string | null
  } | null
  status: string
  started_by: string
  // Optional because the definition-of-done fields land with the checklist
  // backend work: an older serializer (or an older record) simply omits them,
  // and the UI must read that as "no checklist" rather than crash.
  checklist?: ChecklistItem[] | null
  // The operator's note from the last send-back — the standing directive the
  // worker re-reads the task against. Present only after a send-back.
  last_directive?: string | null
  run_ids: string[]
  created_at: string | null
  started_at: string | null
  finished_at: string | null
}

// The states nothing moves out of. Mirrors TERMINAL_STATUSES server-side; used
// to decide when a page can stop polling.
export function taskIsTerminal(status: string | undefined): boolean {
  return status === 'done' || status === 'killed' || status === 'abandoned'
}

// The backend serializes task timestamps naive ("2026-09-04T01:11:17.703000",
// no zone) even though they are UTC, and Date.parse reads a zoneless ISO string
// as LOCAL time — which would age every task by the viewer's UTC offset. Treat
// a marker-less string as UTC. Channel messages carry +00:00 and pass through.
export function parseUtcMs(value: string | null | undefined): number {
  if (!value) return NaN
  const hasZone = /(?:Z|[+-]\d\d:?\d\d)$/.test(value)
  return Date.parse(hasZone ? value : value + 'Z')
}

export function utcRelative(value: string | null | undefined): string {
  const ms = parseUtcMs(value)
  return Number.isFinite(ms) ? relativeTime(ms) : '—'
}

// Task lifecycle → the shared pill vocabulary. The task states don't map onto
// run states one-for-one, so each gets an explicit reading: awaiting_verdict
// keeps the pulsing gold dot on purpose — it is the state that is waiting on a
// person, and this pill is how that person's eye is caught.
export function TaskStatusPill({ status }: { status?: string }) {
  switch (status) {
    case 'queued':
      return <StatusPill status="pending" label="Queued" />
    case 'readback':
      return <StatusPill status="pending" label="Readback" />
    case 'running':
      return <StatusPill status="running" />
    case 'awaiting_verdict':
      return <StatusPill status="running" label="Awaiting verdict" />
    case 'done':
      return <StatusPill status="completed" label="Done" />
    case 'killed':
      return <StatusPill status="failed" label="Killed" />
    case 'abandoned':
      return <StatusPill status="cancelled" label="Abandoned" />
    default:
      return <StatusPill status={status} />
  }
}

// Sender-minted idempotency key for a diary append. randomUUID needs a secure
// context, which localhost and the Tauri webview both are — the fallback only
// exists so a plain-http deployment degrades to a still-unique id rather than
// a crash at the exact moment someone tries to unblock a machine.
export function newMsgId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID()
  }
  return `msg-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`
}

export type VerdictOutcome =
  | { ok: true }
  | { ok: false; conflict: boolean; message: string }

// Post one verdict into a machine's diary. The payload convention is the
// worker's contract: `decision` is the marker it looks for, and anything
// without an explicit rejection reads as approval — so `decision` is always
// sent rather than left to be inferred from the free text.
//
// 409 is not a failure to retry: it means another verdict already won the CAS
// and the question is settled. The caller gets that as `conflict` so it can be
// SAID rather than rendered as a generic error someone would try again.
export async function postVerdict(
  deviceId: string,
  questionMsgId: string,
  taskId: string | null,
  decision: 'approved' | 'rejected',
  text: string,
): Promise<VerdictOutcome> {
  const payload: Record<string, unknown> = { decision }
  if (text.trim()) payload.text = text.trim()
  try {
    const resp = await fetch(`${CU_BACKEND}/channel/${encodeURIComponent(deviceId)}/messages`, {
      method: 'POST',
      headers: { ...authHeaders(), 'content-type': 'application/json' },
      body: JSON.stringify({
        msg_id: newMsgId(),
        type: 'verdict',
        in_reply_to: questionMsgId,
        task_id: taskId,
        payload,
      }),
    })
    if (resp.status === 409) {
      return {
        ok: false,
        conflict: true,
        message:
          'Someone already answered this question — first verdict wins, and theirs stands. It will drop off this list on the next refresh.',
      }
    }
    if (!resp.ok) {
      return { ok: false, conflict: false, message: `The verdict was refused (${resp.status}).` }
    }
    return { ok: true }
  } catch (err) {
    return {
      ok: false,
      conflict: false,
      message: err instanceof Error ? err.message : 'Network error',
    }
  }
}

// Inline answer controls for one unanswered question: Approve, Reject, and one
// optional directive box that rides along with EITHER — the worker reads the
// decision from the payload marker, the text is instruction, not the verdict.
// Shared with TaskDetail so a question answers identically wherever it is met.
export function VerdictControls({
  deviceId,
  questionMsgId,
  taskId,
  onAnswered,
}: {
  deviceId: string
  questionMsgId: string
  taskId: string | null
  onAnswered: () => void
}) {
  const [text, setText] = useState('')
  const [busy, setBusy] = useState<'approved' | 'rejected' | null>(null)
  const [notice, setNotice] = useState<string | null>(null)

  const send = useCallback(
    async (decision: 'approved' | 'rejected') => {
      setBusy(decision)
      setNotice(null)
      const out = await postVerdict(deviceId, questionMsgId, taskId, decision, text)
      setBusy(null)
      if (out.ok) {
        onAnswered()
        return
      }
      setNotice(out.message)
      // A conflict means the question IS answered — refresh so the standing
      // verdict replaces these controls rather than leaving a dead form up.
      if (out.conflict) onAnswered()
    },
    [deviceId, questionMsgId, taskId, text, onAnswered],
  )

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>
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
          placeholder="optional directive — sent with either answer"
          value={text}
          onChange={(e) => setText(e.target.value)}
          disabled={busy !== null}
        />
        <Button variant="primary" size="sm" disabled={busy !== null} onClick={() => send('approved')}>
          {busy === 'approved' ? 'Sending…' : '✓ Approve'}
        </Button>
        <Button variant="danger" size="sm" disabled={busy !== null} onClick={() => send('rejected')}>
          {busy === 'rejected' ? 'Sending…' : '✕ Reject'}
        </Button>
      </div>
      {notice && <div className="error-message">{notice}</div>}
    </div>
  )
}

export type TaskPatchOutcome = { ok: true; task: TaskRecord } | { ok: false; message: string }

// Move a task through its lifecycle. `note` rides along only on the send-back
// edge (awaiting_verdict -> queued, a backend edge landing in parallel with
// this UI; the agreed body is `{status: "queued", note}`): the backend records
// it as the task's `last_directive`, and the worker reads the task back again
// against the updated checklist with the note in hand. Shared between the
// Inbox rows and TaskDetail so the same verb PATCHes identically everywhere.
export async function patchTaskStatus(
  taskId: string,
  body: { status: 'done' | 'killed' | 'queued'; note?: string },
): Promise<TaskPatchOutcome> {
  try {
    const resp = await fetch(`${CU_BACKEND}/tasks/${encodeURIComponent(taskId)}`, {
      method: 'PATCH',
      headers: { ...authHeaders(), 'content-type': 'application/json' },
      body: JSON.stringify(body),
    })
    if (!resp.ok) {
      // A 409 names both states ("illegal transition: X -> Y") — worth
      // surfacing verbatim, because it means the task moved under us.
      let detail = `(${resp.status})`
      try {
        const parsed = await resp.json()
        if (typeof parsed?.detail === 'string') detail = parsed.detail
      } catch {
        // keep the status code
      }
      return { ok: false, message: `Could not update the task: ${detail}` }
    }
    return { ok: true, task: (await resp.json()) as TaskRecord }
  } catch (err) {
    return { ok: false, message: err instanceof Error ? err.message : 'Network error' }
  }
}

// The "send back" half of the verdict pair. Not ConfirmModal, because the note
// is the point: a task sent back without a directive hands the worker the same
// checklist it already failed — the note is what changes on the re-read, so it
// is required. Shared with TaskDetail so the verb behaves identically wherever
// the claim is judged.
export function SendBackModal({
  taskTitle,
  busy,
  onSend,
  onCancel,
}: {
  taskTitle: string
  busy?: boolean
  onSend: (note: string) => void
  onCancel: () => void
}) {
  const [note, setNote] = useState('')
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onCancel()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onCancel])

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={`Send “${taskTitle}” back`}
      onClick={onCancel}
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 2000,
        background: 'rgba(0, 0, 0, 0.72)',
        backdropFilter: 'blur(2px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        padding: 'var(--sp-6)',
      }}
    >
      <div
        // The backdrop closes; the card must not close through it.
        onClick={(e) => e.stopPropagation()}
        style={{
          width: '100%',
          maxWidth: 460,
          background: 'var(--sb-surface-1)',
          border: '1px solid var(--sb-border-gold)',
          borderRadius: 'var(--r-lg)',
          boxShadow: 'var(--shadow-2)',
          overflow: 'hidden',
        }}
      >
        <div
          style={{
            padding: '14px 20px',
            borderBottom: '1px solid var(--sb-border)',
            fontSize: 'var(--fs-lg)',
            fontWeight: 700,
            color: 'var(--sb-text)',
          }}
        >
          Send “{taskTitle}” back?
        </div>
        <div style={{ padding: 'var(--sp-5) 20px', display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
          <p style={{ margin: 0, fontSize: 'var(--fs-md)', lineHeight: 1.6, color: 'var(--sb-text-muted)' }}>
            The task returns to the worker&rsquo;s queue. It will read the task back to you again —
            against the updated checklist, with your note in hand.
          </p>
          <textarea
            className="agent-input"
            autoFocus
            style={{
              boxSizing: 'border-box',
              width: '100%',
              minHeight: 80,
              resize: 'vertical',
              padding: '9px 11px',
              fontFamily: 'inherit',
              fontSize: 'var(--fs-base)',
              lineHeight: 1.5,
              background: 'var(--sb-surface-3)',
              color: 'var(--sb-text)',
              border: '1px solid var(--sb-border)',
              borderRadius: 'var(--r-sm)',
            }}
            placeholder="what to change before it tries again"
            value={note}
            onChange={(e) => setNote(e.target.value)}
            disabled={busy}
          />
        </div>
        <div
          style={{
            display: 'flex',
            justifyContent: 'flex-end',
            gap: 'var(--sp-3)',
            padding: '12px 20px',
            borderTop: '1px solid var(--sb-border)',
          }}
        >
          <Button variant="secondary" onClick={onCancel} disabled={busy}>
            Cancel
          </Button>
          <Button variant="primary" onClick={() => onSend(note.trim())} disabled={busy || !note.trim()}>
            {busy ? 'Sending…' : '✕ Send back'}
          </Button>
        </div>
      </div>
    </div>
  )
}

// ───────────────────────────────────────────────────────── inbox data

// One row of GET /channel/inbox/awaiting-reply.
interface InboxQuestion {
  device_id: string
  msg_id: string
  seq: number
  task_id: string | null
  asked_at: string
  blocking_for_seconds: number
  payload: { text?: string } & Record<string, unknown>
}

// The two fields this page needs off GET /devices, for naming machines.
interface DeviceName {
  device_id: string
  hostname: string
  name: string | null
}

type Load =
  | { state: 'loading' }
  | { state: 'error'; message: string }
  | { state: 'ready'; questions: InboxQuestion[]; tasks: TaskRecord[] }

// The badge and the page share one cadence. 15s: a blocked worker is frozen
// hardware, so this polls twice as hard as the fleet list — and still stays
// far from being load anyone notices (three indexed reads).
const POLL_MS = 15_000

// Count for the NavRail badge: open questions + tasks awaiting verdict. Null
// until the first answer, and quiet forever after — a failed poll keeps the
// last count rather than flickering the badge off a live queue. `enabled`
// gates the polling so a rail that doesn't show the item doesn't poll for it.
export function useInboxCount(enabled: boolean): number | null {
  const [count, setCount] = useState<number | null>(null)
  useEffect(() => {
    if (!enabled) return
    let alive = true
    const load = async () => {
      try {
        const [qResp, tResp] = await Promise.all([
          fetch(`${CU_BACKEND}/channel/inbox/awaiting-reply`, { headers: authHeaders() }),
          fetch(`${CU_BACKEND}/tasks?status=awaiting_verdict&limit=200`, {
            headers: authHeaders(),
          }),
        ])
        if (!qResp.ok || !tResp.ok) return
        const questions = await qResp.json()
        const tasks = await tResp.json()
        if (!alive) return
        setCount(
          (Array.isArray(questions) ? questions.length : 0) +
            (Array.isArray(tasks) ? tasks.length : 0),
        )
      } catch {
        // Quiet — see above.
      }
    }
    load()
    const id = setInterval(load, POLL_MS)
    return () => {
      alive = false
      clearInterval(id)
    }
  }, [enabled])
  return count
}

// How loudly to say how long a machine has been blocked. Minutes are normal
// operation (someone will get to it), an hour is a machine nobody noticed —
// the color escalates so the oldest row cannot read like the newest.
function blockingStyle(seconds: number): { label: string; color: string } {
  const mins = Math.floor(seconds / 60)
  const label =
    mins < 1
      ? 'blocked for under a minute'
      : mins < 60
        ? `blocked for ${mins}m`
        : `blocked for ${Math.floor(mins / 60)}h ${mins % 60}m`
  const color =
    mins < 5 ? 'var(--sb-text-muted)' : mins < 60 ? 'var(--sb-gold-bright)' : 'var(--sb-danger-bright)'
  return { label, color }
}

// ───────────────────────────────────────────────────────── the page

function Inbox() {
  const navigate = useNavigate()
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const [deviceNames, setDeviceNames] = useState<Map<string, DeviceName>>(new Map())
  // Task titles for the questions' task_ids, fetched one task at a time and
  // cached across polls — a title never changes, so re-asking would be waste.
  const [taskTitles, setTaskTitles] = useState<Map<string, string>>(new Map())
  const titlesRef = useRef(taskTitles)
  titlesRef.current = taskTitles

  // Same poll-quietly contract as Admin's fetchDevices: only the first load and
  // an explicit refresh may show the spinner, and a failed poll leaves the last
  // good lists on screen rather than replacing a live queue with an error card.
  const fetchAll = useCallback(async (quiet = false) => {
    if (!quiet) setLoad({ state: 'loading' })
    try {
      const [qResp, tResp, dResp] = await Promise.all([
        fetch(`${CU_BACKEND}/channel/inbox/awaiting-reply`, { headers: authHeaders() }),
        fetch(`${CU_BACKEND}/tasks?status=awaiting_verdict&limit=200`, { headers: authHeaders() }),
        fetch(`${CU_BACKEND}/devices`, { headers: authHeaders() }),
      ])
      if (!qResp.ok || !tResp.ok) {
        if (!quiet) {
          setLoad({
            state: 'error',
            message: `Failed to load the inbox (${qResp.ok ? tResp.status : qResp.status})`,
          })
        }
        return
      }
      const questions: InboxQuestion[] = await qResp.json()
      const tasksBody = await tResp.json()
      const tasks: TaskRecord[] = Array.isArray(tasksBody) ? tasksBody : (tasksBody.tasks ?? [])
      // Device names are decoration: a failed read degrades rows to bare ids.
      if (dResp.ok) {
        const body = await dResp.json()
        const rows: DeviceName[] = Array.isArray(body) ? body : (body.devices ?? [])
        setDeviceNames(new Map(rows.map((d) => [d.device_id, d])))
      }
      setLoad({ state: 'ready', questions, tasks })

      // Resolve question → task title for any task not seen before.
      const missing = questions
        .map((q) => q.task_id)
        .filter((id): id is string => !!id && !titlesRef.current.has(id))
      if (missing.length > 0) {
        const found = await Promise.all(
          [...new Set(missing)].map(async (id) => {
            try {
              const resp = await fetch(`${CU_BACKEND}/tasks/${encodeURIComponent(id)}`, {
                headers: authHeaders(),
              })
              if (!resp.ok) return null
              const task: TaskRecord = await resp.json()
              return [id, task.title] as const
            } catch {
              return null
            }
          }),
        )
        setTaskTitles((prev) => {
          const next = new Map(prev)
          for (const entry of found) if (entry) next.set(entry[0], entry[1])
          return next
        })
      }
    } catch (err) {
      if (!quiet) {
        setLoad({ state: 'error', message: err instanceof Error ? err.message : 'Network error' })
      }
    }
  }, [])

  useEffect(() => {
    fetchAll()
    const id = setInterval(() => fetchAll(true), POLL_MS)
    return () => clearInterval(id)
  }, [fetchAll])

  const deviceName = useCallback(
    (id: string): string => {
      const d = deviceNames.get(id)
      return d ? d.name?.trim() || d.hostname : id
    },
    [deviceNames],
  )

  const openTask = useCallback(
    (deviceId: string, taskId: string) =>
      navigate(`/devices/${encodeURIComponent(deviceId)}/tasks/${encodeURIComponent(taskId)}`),
    [navigate],
  )

  const questions = load.state === 'ready' ? load.questions : []
  const tasks = load.state === 'ready' ? load.tasks : []

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
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--sp-3)' }}>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
          Inbox
        </h1>
        {load.state === 'ready' && (
          <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
            {questions.length + tasks.length === 0
              ? 'nothing is waiting on you'
              : `${questions.length + tasks.length} waiting on you`}
          </span>
        )}
        <div style={{ marginLeft: 'auto' }}>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => fetchAll()}
            disabled={load.state === 'loading'}
          >
            ↻ Refresh
          </Button>
        </div>
      </div>

      {load.state === 'loading' && (
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
            <Spinner /> Loading the inbox…
          </div>
        </Card>
      )}

      {load.state === 'error' && (
        <Card>
          <div className="error-message">{load.message}</div>
        </Card>
      )}

      {load.state === 'ready' && (
        <>
          {/* ── Questions waiting: each one is a frozen machine. ── */}
          <Card title={<SectionTitle>Questions waiting</SectionTitle>}>
            {questions.length === 0 && (
              <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
                No machine is blocked on an answer.
              </span>
            )}
            {questions.length > 0 && (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
                {questions.map((q) => (
                  <QuestionCard
                    key={q.msg_id}
                    question={q}
                    deviceName={deviceName(q.device_id)}
                    taskTitle={q.task_id ? taskTitles.get(q.task_id) ?? null : null}
                    onOpenTask={q.task_id ? () => openTask(q.device_id, q.task_id as string) : null}
                    onAnswered={() => fetchAll(true)}
                  />
                ))}
              </div>
            )}
          </Card>

          {/* ── Work claimed done: judge it. ── */}
          <Card title={<SectionTitle>Tasks awaiting verdict</SectionTitle>} padded={tasks.length === 0}>
            {tasks.length === 0 && (
              <EmptyState
                icon="⚖"
                title="Nothing is awaiting a verdict"
                hint="When a worker finishes a task it lands here for you to judge — done is your call, never the agent's."
              />
            )}
            {tasks.map((task, i) => (
              <div key={task.task_id}>
                {i > 0 && <Divider style={{ margin: 0 }} />}
                <VerdictTaskRow
                  task={task}
                  deviceName={deviceName(task.device_id)}
                  onOpen={() => openTask(task.device_id, task.task_id)}
                  onChanged={() => fetchAll(true)}
                />
              </div>
            ))}
          </Card>
        </>
      )}
    </div>
  )
}

// One task claiming to be finished, judged from the row itself. The verdict
// pair is Approve / Send back — the operator is judging a CLAIM, and those are
// the claim's two answers ("yes, it holds" / "no, go again with this note").
//
// Kill is here too, because "open the task page to abandon it" is not an answer
// on the page whose whole job is clearing what is waiting. It is NOT a third
// verdict, so it is not shaped like one: a small faint ghost the same size and
// colour as the task page's Kill, pushed past a vertical rule and a full gap
// from the pair, with no icon to catch the eye — the two things a fast clicker
// aims at (the gold primary and the outlined secondary) both read as buttons,
// this reads as text. It is also the only control on the row that opens a
// danger-red modal, so a mis-click still has to be confirmed against the task's
// own name before anything dies.
function VerdictTaskRow({
  task,
  deviceName,
  onOpen,
  onChanged,
}: {
  task: TaskRecord
  deviceName: string
  onOpen: () => void
  onChanged: () => void
}) {
  const [confirming, setConfirming] = useState<'approve' | 'kill' | null>(null)
  const [sendingBack, setSendingBack] = useState(false)
  const [busy, setBusy] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)

  const patch = useCallback(
    async (body: { status: 'done' | 'killed' | 'queued'; note?: string }) => {
      setBusy(true)
      setNotice(null)
      const out = await patchTaskStatus(task.task_id, body)
      setBusy(false)
      setConfirming(null)
      setSendingBack(false)
      if (!out.ok) setNotice(out.message)
      // Refresh either way: success moves the task off this list, and a 409
      // means it moved under us — both are answered by re-reading the queue.
      onChanged()
    },
    [task.task_id, onChanged],
  )

  return (
    <div>
      {confirming === 'approve' && (
        <ConfirmModal
          title={`Approve “${task.title}”?`}
          body={[
            'Approve is your judgment that the claim holds — the worker says the work is finished, and you agree.',
            'The task closes for good; nothing about the machine changes.',
          ]}
          confirmLabel="Approve"
          busy={busy}
          onConfirm={() => patch({ status: 'done' })}
          onCancel={() => setConfirming(null)}
        />
      )}
      {/* The title is in the dialog because the risk this whole affordance
          carries is killing the wrong row, and the row it came from is off
          screen behind the backdrop by the time anyone reads it. */}
      {confirming === 'kill' && (
        <ConfirmModal
          title={`Kill “${task.title}”?`}
          body={[
            'The task ends here without being judged — neither accepted nor sent back — and it drops off this list.',
            `No hardware changes: ${deviceName} finished this task and moved on when it reached awaiting verdict. This only abandons the claim.`,
            'This cannot be undone — a killed task never moves again.',
          ]}
          confirmLabel="Kill task"
          danger
          busy={busy}
          onConfirm={() => patch({ status: 'killed' })}
          onCancel={() => setConfirming(null)}
        />
      )}
      {sendingBack && (
        <SendBackModal
          taskTitle={task.title}
          busy={busy}
          onSend={(note) => patch({ status: 'queued', note })}
          onCancel={() => setSendingBack(false)}
        />
      )}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--sp-3)',
          padding: '10px 16px',
        }}
      >
        <TaskStatusPill status={task.status} />
        {/* The title is the door to the full page (diary, checklist, history);
            the verbs live beside it so judging never needs a second screen. */}
        <button
          onClick={onOpen}
          title="Open this task — its diary, checklist and full controls"
          style={{
            flex: 1,
            minWidth: 0,
            textAlign: 'left',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
            fontSize: 'var(--fs-md)',
            color: 'var(--sb-text)',
            background: 'none',
            border: 'none',
            padding: 0,
            cursor: 'pointer',
            font: 'inherit',
          }}
          onMouseEnter={(e) => (e.currentTarget.style.color = 'var(--sb-gold)')}
          onMouseLeave={(e) => (e.currentTarget.style.color = 'var(--sb-text)')}
        >
          {task.title} <span style={{ color: 'var(--sb-text-muted)' }}>→</span>
        </button>
        <span
          style={{
            fontSize: 'var(--fs-sm)',
            color: 'var(--sb-text-muted)',
            whiteSpace: 'nowrap',
          }}
        >
          {deviceName} · started {utcRelative(task.started_at)}
        </span>
        <Button variant="primary" size="sm" disabled={busy} onClick={() => setConfirming('approve')}>
          ✓ Approve
        </Button>
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => setSendingBack(true)}>
          ✕ Send back
        </Button>
        {/* The rule is the point: everything left of it is a verdict on the
            claim, everything right of it is not. */}
        <div
          aria-hidden
          style={{ width: 1, height: 18, background: 'var(--sb-border)', margin: '0 var(--sp-2)' }}
        />
        <Button
          variant="ghost"
          size="sm"
          disabled={busy}
          onClick={() => setConfirming('kill')}
          title="End this task without a verdict — cannot be undone"
          style={{ color: 'var(--sb-text-faint)' }}
        >
          Kill
        </Button>
      </div>
      {notice && (
        <div className="error-message" style={{ margin: '0 16px 10px' }}>
          {notice}
        </div>
      )}
    </div>
  )
}

// One blocked machine's question, with the answer controls right on it — the
// whole point of this page is that unblocking never needs a second screen.
// One checklist row inside a question card: read-only, mirroring the payload's
// structured `checklist` array. Questions used to flatten these into the text
// blob as "[ ] …" lines; the payload now carries them typed, and renderers use
// the rows — parsing prose back into structure was the confusion this removes.
export interface QuestionChecklistItem {
  item_id: string
  text: string
  approved: boolean
}

export function QuestionChecklistRows({ items }: { items: QuestionChecklistItem[] }) {
  if (items.length === 0) return null
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
      {items.map((it) => (
        <div
          key={it.item_id}
          style={{
            display: 'flex',
            alignItems: 'baseline',
            gap: 'var(--sp-2)',
            fontSize: 'var(--fs-md)',
            color: it.approved ? 'var(--sb-text-muted)' : 'var(--sb-text)',
          }}
        >
          <span aria-hidden style={{ fontFamily: 'var(--font-mono)' }}>
            {it.approved ? '\u2611' : '\u2610'}
          </span>
          <span style={{ textDecoration: it.approved ? 'line-through' : 'none' }}>{it.text}</span>
        </div>
      ))}
    </div>
  )
}

// A question payload's structured checklist, tolerantly parsed ([] when absent
// or malformed — older questions carry only the text blob).
export function questionChecklist(payload: unknown): QuestionChecklistItem[] {
  const raw = (payload as { checklist?: unknown } | null)?.checklist
  if (!Array.isArray(raw)) return []
  return raw
    .filter((r): r is Record<string, unknown> => !!r && typeof r === 'object')
    .map((r) => ({
      item_id: String(r.item_id ?? ''),
      text: String(r.text ?? ''),
      approved: r.approved === true,
    }))
    .filter((r) => r.item_id && r.text)
}

// The same items also sit inside the text blob as "[ ] ..." lines (kept there
// so a consumer without the array loses nothing). Rendering both would show
// every criterion twice, so the prose copy — the lines plus their "Definition
// of done" header — is stripped for display whenever the rows will be shown.
export function questionTextWithoutChecklist(text: string, hasRows: boolean): string {
  if (!hasRows) return text
  return text
    .split('\n')
    .filter((line) => {
      const t = line.trim()
      if (t.startsWith('[ ]') || t.startsWith('[done]')) return false
      if (t.startsWith('Definition of done')) return false
      return true
    })
    .join('\n')
}

function QuestionCard({
  question,
  deviceName,
  taskTitle,
  onOpenTask,
  onAnswered,
}: {
  question: InboxQuestion
  deviceName: string
  taskTitle: string | null
  onOpenTask: (() => void) | null
  onAnswered: () => void
}) {
  const blocking = blockingStyle(question.blocking_for_seconds)
  const rawText = typeof question.payload?.text === 'string' ? question.payload.text : ''
  const rows = questionChecklist(question.payload)
  const text = questionTextWithoutChecklist(rawText, rows.length > 0)
  const [killing, setKilling] = useState(false)
  const [busy, setBusy] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)
  // A question is not always a task's readback (the channel allows one with a
  // null task_id). There is nothing to kill then, so the control is simply not
  // offered — guessing which task a loose question belongs to is exactly the
  // mistake this whole affordance is designed against.
  const taskId = question.task_id
  const killLabel = taskTitle ?? taskId ?? ''

  // Abandoning a BLOCKED worker's task takes two writes, and both are needed.
  //
  // Killing the task does NOT wake the worker: `ask_operator` in the Rust
  // (src-tauri/src/channel.rs) blocks in a loop that scans the device's log for
  // a `verdict` whose `in_reply_to` is this question, and it never re-reads the
  // task's status — deliberately, so a backend blip can't be mistaken for an
  // answer. Left alone it waits forever. So the kill is followed by a rejection
  // verdict, which is what the wait is actually watching for: the worker sees a
  // non-approval, stops pursuing the task, and returns to picking up work.
  //
  // Kill first, verdict second, on purpose: the reverse order gives the worker
  // an answer while the task is still alive, and an approval racing in from a
  // second console would then start the run we are trying to prevent.
  const killAndRelease = useCallback(async () => {
    if (!taskId) return
    setBusy(true)
    setNotice(null)
    const killed = await patchTaskStatus(taskId, { status: 'killed' })
    if (!killed.ok) {
      setBusy(false)
      setKilling(false)
      setNotice(killed.message)
      onAnswered()
      return
    }
    const released = await postVerdict(
      question.device_id,
      question.msg_id,
      taskId,
      'rejected',
      'Task killed by the operator — stand down and pick up other work.',
    )
    setBusy(false)
    setKilling(false)
    // A conflict means the question was already answered, so the wait is over
    // either way. Any other failure leaves the machine frozen, which is the one
    // outcome the operator must not be allowed to assume away.
    if (!released.ok && !released.conflict) {
      setNotice(
        `The task is killed, but ${deviceName} is still blocked on this question: ${released.message} Answer it here to release the machine.`,
      )
    }
    onAnswered()
  }, [taskId, question.device_id, question.msg_id, deviceName, onAnswered])

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
      {killing && taskId && (
        <ConfirmModal
          title={`Kill “${killLabel}” and release ${deviceName}?`}
          body={[
            `${deviceName} is frozen on this question and will wait for an answer forever — the worker's wait has no timeout, and killing the task on its own does not end it.`,
            'So this does both: the task is killed, and the question is answered with a rejection. The worker stands down and goes back to picking up work.',
            'This cannot be undone — a killed task never moves again.',
          ]}
          confirmLabel="Kill and release"
          danger
          busy={busy}
          onConfirm={killAndRelease}
          onCancel={() => setKilling(false)}
        />
      )}
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', flexWrap: 'wrap' }}>
        <span style={{ fontSize: 'var(--fs-base)', fontWeight: 600, color: 'var(--sb-text)' }}>
          {deviceName}
        </span>
        {taskTitle && (
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
            · {taskTitle}
          </span>
        )}
        {onOpenTask && (
          <Button variant="ghost" size="sm" onClick={onOpenTask}>
            Open task →
          </Button>
        )}
        {/* The blocking time is the loudest thing on the row on purpose: this
            machine is doing NOTHING until the buttons below are pressed. */}
        <span
          style={{
            marginLeft: 'auto',
            fontSize: 'var(--fs-md)',
            fontWeight: 700,
            color: blocking.color,
            whiteSpace: 'nowrap',
          }}
        >
          ⏸ {blocking.label}
        </span>
        {/* Kill lives up here in the header, not down with the answer buttons:
            the question text, the checklist and the directive box all sit
            between it and Approve, so no fast click on the answer pair can land
            on it — and it is faint, unlabelled by an icon, and the only control
            on the card that stops to confirm. */}
        {taskId && (
          <Button
            variant="ghost"
            size="sm"
            disabled={busy}
            onClick={() => setKilling(true)}
            title="End this task and release the machine — cannot be undone"
            style={{ color: 'var(--sb-text-faint)' }}
          >
            Kill &amp; release
          </Button>
        )}
      </div>

      <div
        style={{
          fontSize: 'var(--fs-md)',
          lineHeight: 1.6,
          color: 'var(--sb-text)',
          whiteSpace: 'pre-wrap',
          background: 'var(--sb-surface-1)',
          border: '1px solid var(--sb-border)',
          borderRadius: 'var(--r-sm)',
          padding: 'var(--sp-3)',
        }}
      >
        {text || '(the question carried no text)'}
      </div>

      <QuestionChecklistRows items={rows} />

      <VerdictControls
        deviceId={question.device_id}
        questionMsgId={question.msg_id}
        taskId={question.task_id}
        onAnswered={onAnswered}
      />

      {notice && <div className="error-message">{notice}</div>}
    </div>
  )
}

export default Inbox

import { useCallback, useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import { Button, Card, Divider, EmptyState, SectionTitle, Spinner, StatusPill } from '../ui'

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
                <button
                  onClick={() => openTask(task.device_id, task.task_id)}
                  title="Open this task — its diary, its runs, and the verdict controls"
                  style={{
                    display: 'flex',
                    alignItems: 'center',
                    gap: 'var(--sp-3)',
                    width: '100%',
                    textAlign: 'left',
                    padding: '10px 16px',
                    background: 'transparent',
                    border: 'none',
                    cursor: 'pointer',
                    color: 'var(--sb-text)',
                    font: 'inherit',
                  }}
                  onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--sb-gold-dim)')}
                  onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
                >
                  <TaskStatusPill status={task.status} />
                  <span
                    style={{
                      flex: 1,
                      minWidth: 0,
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                      fontSize: 'var(--fs-md)',
                    }}
                  >
                    {task.title}
                  </span>
                  <span
                    style={{
                      fontSize: 'var(--fs-sm)',
                      color: 'var(--sb-text-muted)',
                      whiteSpace: 'nowrap',
                    }}
                  >
                    {deviceName(task.device_id)} · started {utcRelative(task.started_at)}
                  </span>
                  <span style={{ color: 'var(--sb-text-muted)' }}>→</span>
                </button>
              </div>
            ))}
          </Card>
        </>
      )}
    </div>
  )
}

// One blocked machine's question, with the answer controls right on it — the
// whole point of this page is that unblocking never needs a second screen.
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
  const text = typeof question.payload?.text === 'string' ? question.payload.text : ''
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

      <VerdictControls
        deviceId={question.device_id}
        questionMsgId={question.msg_id}
        taskId={question.task_id}
        onAnswered={onAnswered}
      />
    </div>
  )
}

export default Inbox

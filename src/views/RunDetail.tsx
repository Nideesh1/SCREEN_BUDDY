import { useEffect, useState, useCallback, useRef, type ReactNode } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import { convertFileSrc } from '@tauri-apps/api/core'
import { useActiveRun } from '../activeRun'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import { formatTokens } from './History'
import AgentRunPanel from '../AgentRunPanel'
import {
  Card,
  SectionTitle,
  StatusPill,
  StatChip,
  Chip,
  Button,
  ActionChip,
} from '../ui'

// One persisted trajectory event, ordered by seq. `data` is type-dependent and
// loose, so we read whatever the type implies and fall back to JSON.
interface RunEvent {
  seq: number
  type: string
  data?: Record<string, unknown> | null
  artifact_object?: string
  artifact_kind?: string
}

// The full run record returned by GET /runs/:id.
interface RunRecord {
  run_id: string
  task?: string
  model?: string
  status?: string
  num_steps?: number
  total_input_tokens?: number
  total_output_tokens?: number
  total_cache_creation_input_tokens?: number
  total_cache_read_input_tokens?: number
  created_at?: string | number
  started_at?: string | number
  completed_at?: string | number
  result?: unknown
  error_message?: unknown
}

// Coerce any value to a display string. Backend may store result/error_message
// as objects (e.g. { ok: true }) or null; rendering those raw as React children
// throws and white-screens the view, so always stringify first.
function asText(v: unknown): string {
  if (v == null) return ''
  if (typeof v === 'string') return v
  try {
    return JSON.stringify(v, null, 2)
  } catch {
    return String(v)
  }
}

interface RunDetailResponse {
  run: RunRecord
  events: RunEvent[]
}

type Load =
  | { state: 'loading' }
  | { state: 'error'; message: string }
  | { state: 'ready'; data: RunDetailResponse }

// Unified detail view: serves a LIVE run (embeds AgentRunPanel's stream) and a
// COMPLETED run (replays persisted events). The run id is the route param; the
// live-run hint comes from shared context; mode is decided from that hint + the
// fetched status.
function RunDetail() {
  const navigate = useNavigate()
  const { runId = '' } = useParams<{ runId: string }>()
  const { activeRun, setActiveRun } = useActiveRun()

  // Real browser back; fall back to History when there's no entry to pop.
  const onBack = useCallback(() => {
    if (window.history.length > 1) navigate(-1)
    else navigate('/history')
  }, [navigate])

  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  const fetchRun = useCallback(
    async (quiet = false) => {
      if (!quiet) setLoad({ state: 'loading' })
      try {
        const resp = await fetch(`${CU_BACKEND}/runs/${encodeURIComponent(runId)}`, {
          headers: authHeaders(),
        })
        if (!resp.ok) {
          if (!quiet) setLoad({ state: 'error', message: `Failed to load run (${resp.status})` })
          return
        }
        const data = (await resp.json()) as RunDetailResponse
        setLoad({ state: 'ready', data })
      } catch (err) {
        if (!quiet) {
          setLoad({
            state: 'error',
            message: err instanceof Error ? err.message : 'Network error',
          })
        }
      }
    },
    [runId],
  )

  useEffect(() => {
    fetchRun()
  }, [fetchRun])

  const run = load.state === 'ready' ? load.data.run : undefined
  const events = load.state === 'ready' ? (load.data.events ?? []) : []

  // LIVE when the Shell's active run matches and is running, or the fetched
  // record itself still reports running; otherwise REPLAY.
  const isLive =
    (activeRun?.id === runId && activeRun.status === 'running') || run?.status === 'running'

  // While the run is still running but we're rendering replay (e.g. opened from
  // History), poll so telemetry/events stay fresh. AgentRunPanel owns the live
  // stream, so no poll is needed when it is mounted.
  useEffect(() => {
    const stillRunning = run?.status === 'running'
    if (stillRunning && !isLive) {
      pollRef.current = setInterval(() => fetchRun(true), 3000)
      return () => {
        if (pollRef.current) clearInterval(pollRef.current)
      }
    }
  }, [run?.status, isLive, fetchRun])

  return (
    <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max-wide)', margin: '0 auto' }}>
      <Header run={run} runId={runId} onBack={onBack} />

      {load.state === 'loading' && (
        <p style={{ color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)' }}>Loading run…</p>
      )}
      {load.state === 'error' && <div className="error-message">{load.message}</div>}

      {load.state === 'ready' && isLive && (
        <Card padded={false} style={{ overflow: 'hidden' }}>
          <div
            style={{
              height: 'calc(100vh - 180px)',
              minHeight: 420,
            }}
          >
            {/* AgentRunPanel owns the live agent:// stream, latest screenshot and Stop.
                Wiring its onStatus into the shared context is what flips activeRun to
                'done'/'error' when the run ends (previously it only ever read 'running'). */}
            <AgentRunPanel
              runId={runId}
              initialPrompt={run?.task}
              attached
              onStatus={(status, id) => setActiveRun(id ? { id, status } : null)}
            />
          </div>
        </Card>
      )}

      {load.state === 'ready' && !isLive && <Replay run={load.data.run} events={events} />}
    </div>
  )
}

function Header({
  run,
  runId,
  onBack,
}: {
  run: RunRecord | undefined
  runId: string
  onBack: () => void
}) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-3)',
        marginBottom: 'var(--sp-5)',
      }}
    >
      <Button variant="ghost" size="sm" onClick={onBack} style={{ flexShrink: 0 }}>
        ← Back
      </Button>
      <div style={{ minWidth: 0, flex: 1 }}>
        <div
          style={{
            fontSize: 'var(--fs-xl)',
            fontWeight: 600,
            color: 'var(--sb-gold-bright)',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
        >
          {run?.task || '(untitled task)'}
        </div>
        <div
          style={{
            fontSize: 'var(--fs-xs)',
            fontFamily: 'var(--font-mono)',
            color: 'var(--sb-text-faint)',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
        >
          {run?.run_id ?? runId}
        </div>
      </div>
      {run?.model && (
        <Chip mono tone="neutral" title={run.model} style={{ flexShrink: 0 }}>
          {run.model}
        </Chip>
      )}
      <StatusPill status={run?.status} />
    </div>
  )
}

// Convert a local screenshot path through the Tauri asset protocol. Returns null
// if convertFileSrc is unavailable or throws (so callers can fall back to the
// raw path).
function shotSrc(path: string): string | null {
  try {
    return convertFileSrc(path)
  } catch {
    return null
  }
}

// REPLAY: full-width panels stacked row by row — Summary, Telemetry, the
// trajectory timeline (built from persisted events), then a screenshot gallery.
// The gallery + timeline thumbnails feed a shared lightbox.
function Replay({ run, events }: { run: RunRecord; events: RunEvent[] }) {
  const shots = events.filter(
    (e) => e.artifact_kind === 'screenshot_local' && e.artifact_object,
  )
  const [lightbox, setLightbox] = useState<number | null>(null)

  // Full-width panels stacked row by row. The tall ones (Trajectory, Screenshots)
  // cap at a decent height and scroll internally; the page scrolls in Layout main.
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
      <Summary run={run} />
      <Telemetry run={run} />

      {/* Trajectory title stays pinned; its body scrolls at a capped height. */}
      <Card title="Trajectory">
        <div style={{ maxHeight: '60vh', overflowY: 'auto', paddingRight: 'var(--sp-2)' }}>
          <Timeline events={events} shots={shots} onOpenShot={setLightbox} />
        </div>
      </Card>

      <Gallery shots={shots} onOpenShot={setLightbox} />

      {lightbox != null && shots[lightbox] && (
        <Lightbox
          shots={shots}
          index={lightbox}
          onClose={() => setLightbox(null)}
          onNavigate={setLightbox}
        />
      )}
    </div>
  )
}

// ─────────────────────────────────────────────────────────── Timeline

// Group ordered events into turns, then render each turn as a block on a gold
// timeline rail. The first events before any explicit `turn` marker fall into an
// implicit opening group.
function Timeline({
  events,
  shots,
  onOpenShot,
}: {
  events: RunEvent[]
  shots: RunEvent[]
  onOpenShot: (index: number) => void
}) {
  if (events.length === 0) {
    return (
      <p style={{ color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)', margin: 0 }}>
        No trajectory events recorded for this run.
      </p>
    )
  }

  // Map a screenshot artifact's path back to its index in the gallery so a
  // timeline thumbnail and the gallery open the same lightbox slide.
  const shotIndexByObject = new Map<string, number>()
  shots.forEach((s, i) => {
    if (s.artifact_object) shotIndexByObject.set(s.artifact_object, i)
  })

  // Partition into turn groups, preserving order.
  type Group = { turn: string | null; rows: RunEvent[] }
  const groups: Group[] = []
  let current: Group | null = null
  // Fallback ordinal for bare `screenshot` events that carry no artifact object.
  let screenshotOrdinal = 0

  for (const ev of events) {
    if (ev.type === 'turn') {
      const d = (ev.data ?? {}) as Record<string, unknown>
      current = { turn: str(d.turn) || String(groups.length + 1), rows: [] }
      groups.push(current)
      continue
    }
    if (!current) {
      current = { turn: null, rows: [] }
      groups.push(current)
    }
    current.rows.push(ev)
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
      {groups.map((g, gi) => (
        <div key={gi}>
          {g.turn != null && (
            <div
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 'var(--sp-2)',
                marginBottom: 'var(--sp-2)',
              }}
            >
              <SectionTitle>Turn {g.turn}</SectionTitle>
              <div style={{ flex: 1, height: 1, background: 'var(--sb-gold-line)' }} />
            </div>
          )}
          <div
            style={{
              display: 'flex',
              flexDirection: 'column',
              gap: 'var(--sp-3)',
              borderLeft: '2px solid var(--sb-gold-line)',
              paddingLeft: 'var(--sp-4)',
            }}
          >
            {g.rows.map((ev) => {
              // Resolve a screenshot slide index for screenshot rows.
              let shotIndex: number | undefined
              const isShot =
                ev.type === 'screenshot' || ev.artifact_kind === 'screenshot_local'
              if (isShot) {
                if (ev.artifact_object && shotIndexByObject.has(ev.artifact_object)) {
                  shotIndex = shotIndexByObject.get(ev.artifact_object)
                } else if (screenshotOrdinal < shots.length) {
                  shotIndex = screenshotOrdinal
                }
                screenshotOrdinal += 1
              }
              return (
                <TimelineRow
                  key={ev.seq}
                  ev={ev}
                  shotIndex={shotIndex}
                  shots={shots}
                  onOpenShot={onOpenShot}
                />
              )
            })}
          </div>
        </div>
      ))}
    </div>
  )
}

function TimelineRow({
  ev,
  shotIndex,
  shots,
  onOpenShot,
}: {
  ev: RunEvent
  shotIndex: number | undefined
  shots: RunEvent[]
  onOpenShot: (index: number) => void
}) {
  const d = (ev.data ?? {}) as Record<string, unknown>

  // Screenshot rows (either a bare `screenshot` event or any artifact-bearing
  // screenshot_local) render an inline clickable thumbnail.
  if (ev.type === 'screenshot' || ev.artifact_kind === 'screenshot_local') {
    if (shotIndex != null && shots[shotIndex]) {
      return (
        <Thumbnail
          path={shots[shotIndex].artifact_object as string}
          width={140}
          onClick={() => onOpenShot(shotIndex)}
        />
      )
    }
    return <PillLine icon="📸" text="screenshot" tone="muted" />
  }

  switch (ev.type) {
    case 'text': {
      const text = str(d.delta ?? d.text)
      if (!text.trim()) return null
      return (
        <p
          style={{
            margin: 0,
            fontFamily: 'var(--font-sans)',
            fontSize: 'var(--fs-md)',
            lineHeight: 1.6,
            color: 'var(--sb-text)',
            whiteSpace: 'pre-wrap',
            wordBreak: 'break-word',
          }}
        >
          {text}
        </p>
      )
    }
    // Live runs emit `action`; persisted runs store the same as `tool_use`
    // with data = { name: "computer", input: { action, ... } }. prettyAction
    // unwraps the computer tool, so both render as a clean ActionChip.
    case 'action':
    case 'tool_use':
      return (
        <div>
          <ActionChip name={str(d.name)} input={d.input} />
        </div>
      )
    case 'done':
      return (
        <PillLine
          icon="✓"
          text={`done${d.reason ? ` (${str(d.reason)})` : ''}`}
          tone="gold"
        />
      )
    case 'error':
      return <PillLine icon="✕" text={`error: ${str(d.error ?? d.message)}`} tone="danger" />
    case 'status':
      return <PillLine text={str(d.status ?? d.message)} tone="muted" />
    default:
      return <PillLine text={`${ev.type}: ${safeJson(ev.data)}`} tone="muted" />
  }
}

// A compact single-line status marker on the timeline.
function PillLine({
  icon,
  text,
  tone,
}: {
  icon?: string
  text: string
  tone: 'muted' | 'gold' | 'danger'
}) {
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

// Inline clickable thumbnail; falls back to a tiny marker when the path can't be
// resolved through the asset protocol.
function Thumbnail({
  path,
  width,
  onClick,
}: {
  path: string
  width: number
  onClick: () => void
}) {
  const src = shotSrc(path)
  if (!src) {
    return <PillLine icon="📸" text="screenshot" tone="muted" />
  }
  return (
    <button
      onClick={onClick}
      title="Open screenshot"
      style={{
        display: 'inline-block',
        padding: 0,
        width,
        maxWidth: '100%',
        border: '1px solid var(--sb-border)',
        borderRadius: 'var(--r-sm)',
        overflow: 'hidden',
        cursor: 'pointer',
        background: 'var(--sb-surface-2)',
        lineHeight: 0,
      }}
    >
      <img
        src={src}
        alt="run screenshot"
        style={{ display: 'block', width: '100%', height: 'auto' }}
      />
    </button>
  )
}

// ─────────────────────────────────────────────────────────── Telemetry

function Telemetry({ run }: { run: RunRecord }) {
  const errText = run.error_message != null ? asText(run.error_message) : ''
  return (
    <Card title="Telemetry">
      <div style={{ display: 'flex', alignItems: 'center', marginBottom: 'var(--sp-4)' }}>
        <StatusPill status={run.status} />
      </div>
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(96px, 1fr))',
          gap: 'var(--sp-4)',
        }}
      >
        <StatChip label="Steps" value={String(run.num_steps ?? 0)} />
        <StatChip label="Input tok" value={formatTokens(run.total_input_tokens)} />
        <StatChip label="Output tok" value={formatTokens(run.total_output_tokens)} />
        {run.total_cache_creation_input_tokens != null && (
          <StatChip
            label="Cache write"
            value={formatTokens(run.total_cache_creation_input_tokens)}
          />
        )}
        {run.total_cache_read_input_tokens != null && (
          <StatChip label="Cache read" value={formatTokens(run.total_cache_read_input_tokens)} />
        )}
        <StatChip label="Started" value={relativeTime(run.started_at ?? run.created_at)} />
        {run.completed_at && (
          <StatChip label="Completed" value={relativeTime(run.completed_at)} />
        )}
      </div>

      {errText && (
        <div className="error-message" style={{ marginTop: 'var(--sp-4)' }}>
          {errText}
        </div>
      )}
    </Card>
  )
}

// ─────────────────────────────────────────────────────────── Summary

// Extract the run summary text. The backend stores result as { summary: "<md>" }
// (preferred), but tolerate a plain-string result too. Anything else → no card.
function summaryText(result: unknown): string {
  if (result == null) return ''
  if (typeof result === 'string') return result.trim()
  if (typeof result === 'object') {
    const s = (result as { summary?: unknown }).summary
    if (typeof s === 'string') return s.trim()
  }
  return ''
}

// Render a single line of light markdown: **bold** spans become <strong>.
function renderInline(text: string, keyBase: string): ReactNode[] {
  const out: ReactNode[] = []
  const re = /\*\*([^*]+)\*\*/g
  let last = 0
  let m: RegExpExecArray | null
  let i = 0
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) out.push(text.slice(last, m.index))
    out.push(<strong key={`${keyBase}-b${i++}`}>{m[1]}</strong>)
    last = m.index + m[0].length
  }
  if (last < text.length) out.push(text.slice(last))
  return out
}

// Tiny inline markdown renderer (no dependency): ## headings, - / * bullets,
// **bold**, preserved line breaks. Emojis pass through unchanged.
function Markdown({ text }: { text: string }) {
  const lines = text.split('\n')
  return (
    <div
      style={{
        fontFamily: 'var(--font-sans)',
        fontSize: 'var(--fs-md)',
        lineHeight: 1.65,
        color: 'var(--sb-text)',
        wordBreak: 'break-word',
      }}
    >
      {lines.map((raw, i) => {
        const line = raw.replace(/\s+$/, '')
        const heading = line.match(/^#{1,6}\s+(.*)$/)
        if (heading) {
          return (
            <div
              key={i}
              style={{
                fontWeight: 600,
                fontSize: 'var(--fs-lg)',
                color: 'var(--sb-gold-bright)',
                margin: i === 0 ? '0 0 4px' : '12px 0 4px',
              }}
            >
              {renderInline(heading[1], `h${i}`)}
            </div>
          )
        }
        const bullet = line.match(/^\s*[-*]\s+(.*)$/)
        if (bullet) {
          return (
            <div key={i} style={{ display: 'flex', gap: 8, margin: '2px 0' }}>
              <span aria-hidden style={{ color: 'var(--sb-gold)', flexShrink: 0 }}>
                •
              </span>
              <span style={{ minWidth: 0 }}>{renderInline(bullet[1], `li${i}`)}</span>
            </div>
          )
        }
        if (line.trim() === '') return <div key={i} style={{ height: 8 }} />
        return (
          <div key={i} style={{ margin: '2px 0' }}>
            {renderInline(line, `p${i}`)}
          </div>
        )
      })}
    </div>
  )
}

function Summary({ run }: { run: RunRecord }) {
  const text = summaryText(run.result)
  if (!text) return null
  return (
    <Card title="Summary">
      <Markdown text={text} />
    </Card>
  )
}

// ─────────────────────────────────────────────────────────── Gallery

function Gallery({
  shots,
  onOpenShot,
}: {
  shots: RunEvent[]
  onOpenShot: (index: number) => void
}) {
  return (
    <Card title={`Screenshots${shots.length ? ` · ${shots.length}` : ''}`}>
      {shots.length === 0 ? (
        <p style={{ color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)', margin: 0 }}>
          No screenshots.
        </p>
      ) : (
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fill, minmax(180px, 1fr))',
            gap: 'var(--sp-3)',
            maxHeight: '55vh',
            overflowY: 'auto',
            paddingRight: 'var(--sp-2)',
          }}
        >
          {shots.map((ev, i) => (
            <Thumbnail
              key={ev.seq}
              path={ev.artifact_object as string}
              width={9999}
              onClick={() => onOpenShot(i)}
            />
          ))}
        </div>
      )}
    </Card>
  )
}

// ─────────────────────────────────────────────────────────── Lightbox

function Lightbox({
  shots,
  index,
  onClose,
  onNavigate,
}: {
  shots: RunEvent[]
  index: number
  onClose: () => void
  onNavigate: (index: number) => void
}) {
  const count = shots.length
  const go = useCallback(
    (delta: number) => onNavigate((index + delta + count) % count),
    [index, count, onNavigate],
  )

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
      else if (e.key === 'ArrowRight') go(1)
      else if (e.key === 'ArrowLeft') go(-1)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose, go])

  const ev = shots[index]
  const src = ev?.artifact_object ? shotSrc(ev.artifact_object) : null

  return (
    <div
      onClick={onClose}
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 1000,
        background: 'rgba(0, 0, 0, 0.86)',
        backdropFilter: 'blur(2px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        padding: 'var(--sp-6)',
      }}
    >
      {/* Close */}
      <Button
        variant="secondary"
        size="sm"
        onClick={(e) => {
          e.stopPropagation()
          onClose()
        }}
        style={{ position: 'absolute', top: 'var(--sp-4)', right: 'var(--sp-4)' }}
      >
        ✕ Close
      </Button>

      {count > 1 && (
        <NavArrow side="left" onClick={() => go(-1)} />
      )}

      {src ? (
        <img
          src={src}
          alt="run screenshot"
          onClick={(e) => e.stopPropagation()}
          style={{
            maxWidth: '90vw',
            maxHeight: '86vh',
            objectFit: 'contain',
            borderRadius: 'var(--r-md)',
            border: '1px solid var(--sb-border-gold)',
            boxShadow: 'var(--shadow-2)',
          }}
        />
      ) : (
        <code
          onClick={(e) => e.stopPropagation()}
          style={{
            color: 'var(--sb-text-muted)',
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--fs-sm)',
            wordBreak: 'break-all',
            maxWidth: '80vw',
          }}
        >
          {ev?.artifact_object}
        </code>
      )}

      {count > 1 && <NavArrow side="right" onClick={() => go(1)} />}

      {count > 1 && (
        <div
          style={{
            position: 'absolute',
            bottom: 'var(--sp-4)',
            left: '50%',
            transform: 'translateX(-50%)',
            fontSize: 'var(--fs-sm)',
            fontFamily: 'var(--font-mono)',
            color: 'var(--sb-text-muted)',
          }}
        >
          {index + 1} / {count}
        </div>
      )}
    </div>
  )
}

function NavArrow({ side, onClick }: { side: 'left' | 'right'; onClick: () => void }) {
  return (
    <button
      onClick={(e) => {
        e.stopPropagation()
        onClick()
      }}
      aria-label={side === 'left' ? 'Previous' : 'Next'}
      style={{
        position: 'absolute',
        left: side === 'left' ? 'var(--sp-4)' : undefined,
        right: side === 'right' ? 'var(--sp-4)' : undefined,
        top: '50%',
        transform: 'translateY(-50%)',
        width: 44,
        height: 44,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        fontSize: 20,
        color: 'var(--sb-text)',
        background: 'var(--sb-surface-2)',
        border: '1px solid var(--sb-border)',
        borderRadius: 'var(--r-pill)',
        cursor: 'pointer',
      }}
    >
      {side === 'left' ? '‹' : '›'}
    </button>
  )
}

function str(v: unknown): string {
  if (v === undefined || v === null) return ''
  return String(v)
}

function safeJson(input: unknown): string {
  if (input === undefined || input === null) return ''
  try {
    return JSON.stringify(input)
  } catch {
    return String(input)
  }
}

export default RunDetail

import { useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { useActiveRun } from '../activeRun'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import { Card, SectionTitle, StatusPill, StatChip, Button, EmptyState, Spinner, Divider } from '../ui'

// Best-effort run summary as returned by GET /runs. Every field is optional so
// we degrade gracefully when the backend omits some.
interface RunSummary {
  run_id: string
  task?: string
  model?: string
  status?: string
  num_steps?: number
  total_input_tokens?: number
  total_output_tokens?: number
  created_at?: string | number
  started_at?: string | number
  completed_at?: string | number
  thumbnail?: string
}

type Load =
  | { state: 'loading' }
  | { state: 'error'; message: string }
  | { state: 'ready'; runs: RunSummary[] }

// Landing view. Polls GET /runs every ~3s, surfaces the in-progress run as a
// prominent live card (or an empty-state hero), then lists the recent runs.
// Navigation is via the router; the live-run hint comes from shared context.
function Dashboard() {
  const navigate = useNavigate()
  const { activeRun } = useActiveRun()
  const onSelectRun = (runId: string) => navigate('/runs/' + runId)
  const onNewRun = () => navigate('/runs')
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  // Keep a ref so the interval callback can avoid flipping back to a full-screen
  // "Loading…" on every poll once we already have data.
  const hasData = useRef(false)

  useEffect(() => {
    let cancelled = false

    const fetchRuns = async () => {
      try {
        const resp = await fetch(`${CU_BACKEND}/runs`, { headers: authHeaders() })
        if (!resp.ok) {
          if (!cancelled && !hasData.current) {
            setLoad({ state: 'error', message: `Failed to load runs (${resp.status})` })
          }
          return
        }
        const data = await resp.json()
        const runs: RunSummary[] = Array.isArray(data) ? data : (data.runs ?? [])
        if (!cancelled) {
          hasData.current = true
          setLoad({ state: 'ready', runs })
        }
      } catch (err) {
        if (!cancelled && !hasData.current) {
          setLoad({
            state: 'error',
            message: err instanceof Error ? err.message : 'Network error',
          })
        }
      }
    }

    fetchRuns()
    const timer = setInterval(fetchRuns, 3000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [])

  const runs = load.state === 'ready' ? load.runs : []

  // Prefer the locally-tracked live run id, else the first running run.
  const liveRun =
    (activeRun && activeRun.status === 'running'
      ? runs.find((r) => r.run_id === activeRun.id)
      : undefined) ?? runs.find((r) => isRunning(r.status))

  // Synthesize a minimal live card from activeRun even if the backend hasn't
  // listed it yet (just started).
  const liveSynthetic: RunSummary | null =
    !liveRun && activeRun && activeRun.status === 'running'
      ? { run_id: activeRun.id, status: 'running' }
      : null

  const live = liveRun ?? liveSynthetic
  const recent = [...runs].sort(byRecency).slice(0, 8)

  return (
    <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max)', margin: '0 auto' }}>
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--sp-3)', marginBottom: 'var(--sp-5)' }}>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
          Dashboard
        </h1>
      </div>

      {load.state === 'loading' && !hasData.current && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-3)',
            color: 'var(--sb-text-muted)',
            fontSize: 'var(--fs-base)',
            padding: 'var(--sp-4) 0',
          }}
        >
          <Spinner /> Loading runs…
        </div>
      )}

      {load.state === 'error' && !hasData.current && <div className="error-message">{load.message}</div>}

      {(load.state === 'ready' || hasData.current) && (
        <>
          {live ? (
            <LiveCard run={live} onClick={() => onSelectRun(live.run_id)} />
          ) : (
            <Card padded>
              <EmptyState
                icon="✦"
                title="No runs in progress"
                hint="Kick off a computer-use run to watch it work, live, right here."
                action={
                  <Button variant="primary" onClick={onNewRun}>
                    Start a run
                  </Button>
                }
              />
            </Card>
          )}

          <div style={{ margin: 'var(--sp-6) 0 var(--sp-3)' }}>
            <SectionTitle>Recent runs</SectionTitle>
          </div>

          {recent.length === 0 ? (
            <p style={{ color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)' }}>No runs yet.</p>
          ) : (
            <Card padded={false}>
              {recent.map((run, i) => (
                <div key={run.run_id}>
                  {i > 0 && <Divider style={{ margin: 0 }} />}
                  <RunRow run={run} onClick={() => onSelectRun(run.run_id)} />
                </div>
              ))}
            </Card>
          )}
        </>
      )}
    </div>
  )
}

// Prominent in-progress card at the top of the dashboard. Whole card is a
// button → the run's route.
function LiveCard({ run, onClick }: { run: RunSummary; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      style={{
        display: 'block',
        width: '100%',
        textAlign: 'left',
        padding: 0,
        background: 'transparent',
        border: 'none',
        cursor: 'pointer',
      }}
    >
      <Card
        style={{
          border: '1px solid var(--sb-border-gold)',
          background:
            'linear-gradient(135deg, var(--sb-gold-dim) 0%, var(--sb-surface-1) 70%)',
          boxShadow: 'var(--shadow-2)',
        }}
      >
        <div style={{ display: 'flex', gap: 'var(--sp-4)' }}>
          {run.thumbnail && (
            <img
              src={run.thumbnail}
              alt=""
              style={{
                width: 96,
                height: 60,
                objectFit: 'cover',
                borderRadius: 'var(--r-sm)',
                border: '1px solid var(--sb-border)',
                flexShrink: 0,
              }}
            />
          )}
          <div style={{ minWidth: 0, flex: 1 }}>
            <div
              style={{
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'space-between',
                gap: 'var(--sp-3)',
                marginBottom: 'var(--sp-3)',
              }}
            >
              <SectionTitle>Live</SectionTitle>
              <StatusPill status="running" />
            </div>
            <div
              style={{
                fontSize: 'var(--fs-lg)',
                fontWeight: 600,
                color: 'var(--sb-text)',
                marginBottom: 'var(--sp-4)',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                whiteSpace: 'nowrap',
              }}
            >
              {run.task || '(untitled task)'}
            </div>
            <div style={{ display: 'flex', gap: 'var(--sp-6)', flexWrap: 'wrap' }}>
              <StatChip label="Step" value={String(run.num_steps ?? 0)} />
              <StatChip label="Elapsed" value={elapsed(run.started_at)} />
              <StatChip
                label="Tokens"
                value={<span style={{ fontFamily: 'var(--font-mono)' }}>{formatTokens(tokensOf(run))}</span>}
              />
              {run.model && <StatChip label="Model" value={run.model} />}
            </div>
          </div>
        </div>
      </Card>
    </button>
  )
}

function RunRow({ run, onClick }: { run: RunSummary; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      className="sb-run-row"
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-4)',
        width: '100%',
        textAlign: 'left',
        padding: '12px 16px',
        background: 'transparent',
        border: 'none',
        cursor: 'pointer',
        color: 'var(--sb-text)',
        transition: 'background 0.15s ease',
      }}
      onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--sb-gold-dim)')}
      onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
    >
      <StatusPill status={run.status} />
      <span
        style={{
          flex: 1,
          minWidth: 0,
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
          fontSize: 'var(--fs-base)',
        }}
      >
        {run.task || '(untitled task)'}
      </span>
      <Meta>{run.num_steps ?? 0} steps</Meta>
      <Meta mono>{formatTokens(tokensOf(run))} tok</Meta>
      <Meta>{relativeTime(run.completed_at ?? run.started_at ?? run.created_at)}</Meta>
    </button>
  )
}

function Meta({ children, mono }: { children: React.ReactNode; mono?: boolean }) {
  return (
    <span
      style={{
        fontSize: 'var(--fs-sm)',
        color: 'var(--sb-text-muted)',
        whiteSpace: 'nowrap',
        fontFamily: mono ? 'var(--font-mono)' : undefined,
      }}
    >
      {children}
    </span>
  )
}

function isRunning(status?: string): boolean {
  const s = (status || '').toLowerCase()
  return s === 'running' || s === 'in_progress'
}

function tokensOf(run: RunSummary): number {
  return (run.total_input_tokens ?? 0) + (run.total_output_tokens ?? 0)
}

function formatTokens(tokens?: number): string {
  if (!tokens || tokens <= 0) return '0'
  if (tokens >= 1000) return `${(tokens / 1000).toFixed(1)}k`
  return String(tokens)
}

function elapsed(start?: string | number | null): string {
  if (start === null || start === undefined) return '—'
  const ms = typeof start === 'number' ? start : Date.parse(start)
  if (!Number.isFinite(ms)) return '—'
  const sec = Math.max(0, Math.round((Date.now() - ms) / 1000))
  if (sec < 60) return `${sec}s`
  const min = Math.floor(sec / 60)
  const rem = sec % 60
  if (min < 60) return `${min}m ${rem}s`
  const hr = Math.floor(min / 60)
  return `${hr}h ${min % 60}m`
}

function recencyKey(run: RunSummary): number {
  const v = run.completed_at ?? run.started_at ?? run.created_at
  if (v === null || v === undefined) return 0
  const ms = typeof v === 'number' ? v : Date.parse(v)
  return Number.isFinite(ms) ? ms : 0
}

function byRecency(a: RunSummary, b: RunSummary): number {
  return recencyKey(b) - recencyKey(a)
}

export default Dashboard

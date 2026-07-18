import { useEffect, useState, useCallback } from 'react'
import { useNavigate } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import { Card, SectionTitle, StatusPill, Button, EmptyState, Spinner, Divider } from '../ui'
import RunsTabs from './RunsTabs'

// One row in the runs list. The backend shape is best-effort — we read the
// common fields and degrade gracefully when any are missing. The canonical id
// field is `run_id` (string uuid).
export interface RunSummary {
  run_id: string
  task?: string
  created_at?: string | number
  status?: string
  // Step / token counts arrive under a couple of names depending on endpoint.
  num_steps?: number
  steps?: number
  total_input_tokens?: number
  total_output_tokens?: number
  tokens?: number
}

type Load =
  | { state: 'loading' }
  | { state: 'error'; message: string }
  | { state: 'ready'; runs: RunSummary[] }

// History view. Owns only the list-load state; a row click drills down into the
// run-detail route, which decides live vs replay.
function History() {
  const navigate = useNavigate()
  const onSelectRun = (runId: string) => navigate('/runs/' + runId)
  const [load, setLoad] = useState<Load>({ state: 'loading' })

  const fetchRuns = useCallback(async () => {
    setLoad({ state: 'loading' })
    try {
      const resp = await fetch(`${CU_BACKEND}/runs`, { headers: authHeaders() })
      if (!resp.ok) {
        setLoad({ state: 'error', message: `Failed to load runs (${resp.status})` })
        return
      }
      const data = await resp.json()
      // Accept either a bare array or { runs: [...] }.
      const runs: RunSummary[] = Array.isArray(data) ? data : (data.runs ?? [])
      setLoad({ state: 'ready', runs })
    } catch (err) {
      setLoad({
        state: 'error',
        message: err instanceof Error ? err.message : 'Network error',
      })
    }
  }, [])

  useEffect(() => {
    fetchRuns()
  }, [fetchRuns])

  return (
    <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max)', margin: '0 auto' }}>
      <div style={{ marginBottom: 'var(--sp-5)' }}>
        <RunsTabs />
      </div>
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--sp-3)', marginBottom: 'var(--sp-5)' }}>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
          History
        </h1>
        <div style={{ marginLeft: 'auto' }}>
          <Button variant="secondary" size="sm" onClick={fetchRuns} disabled={load.state === 'loading'}>
            ↻ Refresh
          </Button>
        </div>
      </div>

      <Card
        title={<SectionTitle>All runs</SectionTitle>}
        padded={false}
      >
        {load.state === 'loading' && (
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
            <Spinner /> Loading runs…
          </div>
        )}

        {load.state === 'error' && (
          <div style={{ padding: 'var(--sp-4)' }}>
            <div className="error-message">{load.message}</div>
          </div>
        )}

        {load.state === 'ready' && load.runs.length === 0 && (
          <EmptyState
            icon="✦"
            title="No runs yet"
            hint="Start one from New Run to see its history here."
          />
        )}

        {load.state === 'ready' && load.runs.length > 0 && (
          <div>
            {load.runs.map((run, i) => (
              <div key={run.run_id}>
                {i > 0 && <Divider style={{ margin: 0 }} />}
                <RunRow run={run} onClick={() => onSelectRun(run.run_id)} />
              </div>
            ))}
          </div>
        )}
      </Card>
    </div>
  )
}

function RunRow({ run, onClick }: { run: RunSummary; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
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
      <Meta>{run.num_steps ?? run.steps ?? 0} steps</Meta>
      <Meta mono>{formatTokens(runTokens(run))} tok</Meta>
      <Meta>{relativeTime(run.created_at)}</Meta>
    </button>
  )
}

// Total tokens for the row: explicit `tokens` if the list endpoint provides it,
// otherwise input + output.
function runTokens(run: RunSummary): number {
  if (typeof run.tokens === 'number') return run.tokens
  return (run.total_input_tokens ?? 0) + (run.total_output_tokens ?? 0)
}

// Kept exported: RunDetail imports statusIcon from this file. Even though the
// list now renders status via the ui.tsx StatusPill, this emoji mapping stays.
export function statusIcon(status?: string): string {
  switch ((status || '').toLowerCase()) {
    case 'done':
    case 'success':
    case 'completed':
      return '✅'
    case 'error':
    case 'failed':
      return '❌'
    case 'running':
    case 'in_progress':
      return '⏳'
    case 'stopped':
    case 'cancelled':
      return '⏹'
    default:
      return '•'
  }
}

// Kept exported: RunDetail imports formatTokens from this file.
export function formatTokens(tokens?: number): string {
  if (!tokens || tokens <= 0) return '0'
  if (tokens >= 1000) return `${(tokens / 1000).toFixed(1)}k`
  return String(tokens)
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

export default History

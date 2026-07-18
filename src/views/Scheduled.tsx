import { useCallback, useEffect, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { relativeTime } from '../lib'
import { Card, SectionTitle, Button, EmptyState, Spinner, Divider, Badge, IconButton, TrashIcon } from '../ui'
import {
  listSchedules,
  patchSchedule,
  deleteSchedule,
  cronLabel,
  type Schedule,
} from '../schedules'
import RunsTabs from './RunsTabs'

type Load =
  | { state: 'loading' }
  | { state: 'error'; message: string }
  | { state: 'ready'; schedules: Schedule[] }

// Scheduled (future) view: every schedule, with its cron label, next fire, an
// enabled toggle (PATCH), and delete. A row click drills into ScheduleDetail.
function Scheduled() {
  const navigate = useNavigate()
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const [busyId, setBusyId] = useState<string | null>(null)

  const fetchAll = useCallback(async () => {
    setLoad({ state: 'loading' })
    try {
      const schedules = await listSchedules()
      setLoad({ state: 'ready', schedules })
    } catch (err) {
      setLoad({ state: 'error', message: err instanceof Error ? err.message : 'Network error' })
    }
  }, [])

  useEffect(() => {
    fetchAll()
  }, [fetchAll])

  const toggle = useCallback(
    async (s: Schedule) => {
      setBusyId(s.schedule_id)
      try {
        const updated = await patchSchedule(s.schedule_id, { enabled: !s.enabled })
        setLoad((prev) =>
          prev.state === 'ready'
            ? {
                state: 'ready',
                schedules: prev.schedules.map((x) => (x.schedule_id === updated.schedule_id ? updated : x)),
              }
            : prev,
        )
      } catch (err) {
        console.error('[scheduled] toggle failed', err)
      } finally {
        setBusyId(null)
      }
    },
    [],
  )

  const remove = useCallback(async (s: Schedule) => {
    setBusyId(s.schedule_id)
    try {
      await deleteSchedule(s.schedule_id)
      setLoad((prev) =>
        prev.state === 'ready'
          ? { state: 'ready', schedules: prev.schedules.filter((x) => x.schedule_id !== s.schedule_id) }
          : prev,
      )
    } catch (err) {
      console.error('[scheduled] delete failed', err)
    } finally {
      setBusyId(null)
    }
  }, [])

  return (
    <div
      style={{
        maxWidth: 'var(--page-max-narrow)',
        margin: '0 auto',
        padding: 'var(--sp-6) var(--sp-5)',
        display: 'flex',
        flexDirection: 'column',
        gap: 'var(--sp-5)',
        boxSizing: 'border-box',
      }}
    >
      <RunsTabs />

      <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--sp-3)' }}>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
          Scheduled
        </h1>
        <div style={{ marginLeft: 'auto' }}>
          <Button variant="secondary" size="sm" onClick={fetchAll} disabled={load.state === 'loading'}>
            ↻ Refresh
          </Button>
        </div>
      </div>

      <Card title={<SectionTitle>Upcoming schedules</SectionTitle>} padded={false}>
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
            <Spinner /> Loading schedules…
          </div>
        )}

        {load.state === 'error' && (
          <div style={{ padding: 'var(--sp-4)' }}>
            <div className="error-message">{load.message}</div>
          </div>
        )}

        {load.state === 'ready' && load.schedules.length === 0 && (
          <EmptyState
            icon="⏰"
            title="No schedules yet"
            hint="Create one from New run — toggle “Schedule instead of running now”."
          />
        )}

        {load.state === 'ready' && load.schedules.length > 0 && (
          <div>
            {load.schedules.map((s, i) => (
              <div key={s.schedule_id}>
                {i > 0 && <Divider style={{ margin: 0 }} />}
                <ScheduleRow
                  schedule={s}
                  busy={busyId === s.schedule_id}
                  onOpen={() => navigate('/scheduled/' + s.schedule_id)}
                  onToggle={() => toggle(s)}
                  onDelete={() => remove(s)}
                />
              </div>
            ))}
          </div>
        )}
      </Card>
    </div>
  )
}

function ScheduleRow({
  schedule,
  busy,
  onOpen,
  onToggle,
  onDelete,
}: {
  schedule: Schedule
  busy: boolean
  onOpen: () => void
  onToggle: () => void
  onDelete: () => void
}) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-4)',
        padding: '12px 16px',
        transition: 'background 0.15s ease',
      }}
      onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--sb-gold-dim)')}
      onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
    >
      <button
        onClick={onOpen}
        style={{
          flex: 1,
          minWidth: 0,
          textAlign: 'left',
          background: 'transparent',
          border: 'none',
          cursor: 'pointer',
          color: 'var(--sb-text)',
          display: 'flex',
          flexDirection: 'column',
          gap: 4,
        }}
      >
        <span
          style={{
            fontSize: 'var(--fs-base)',
            fontWeight: 600,
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
        >
          {schedule.name}
        </span>
        <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
          {cronLabel(schedule.cron)}
        </span>
      </button>

      <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)', whiteSpace: 'nowrap' }}>
        {schedule.next_fire_at ? `next ${relativeTime(schedule.next_fire_at)}` : '—'}
      </span>

      <button
        onClick={onToggle}
        disabled={busy}
        title={schedule.enabled ? 'Enabled — click to pause' : 'Paused — click to enable'}
        style={{ background: 'transparent', border: 'none', cursor: busy ? 'default' : 'pointer', padding: 0 }}
      >
        <Badge tone={schedule.enabled ? 'success' : 'neutral'}>
          {schedule.enabled ? '● Enabled' : '○ Paused'}
        </Badge>
      </button>

      <IconButton
        onClick={onDelete}
        disabled={busy}
        aria-label="Delete schedule"
        title="Delete schedule"
        style={{ color: 'var(--sb-danger-bright)' }}
      >
        <TrashIcon />
      </IconButton>
    </div>
  )
}

export default Scheduled

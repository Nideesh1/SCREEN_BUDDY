import { useCallback, useEffect, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import { safeInvoke } from '../lib'
import { Card, SectionTitle, Button, Spinner, Badge, Divider } from '../ui'
import {
  getSchedule,
  patchSchedule,
  deleteSchedule,
  type ScheduleDetail as ScheduleDetailData,
} from '../schedules'
import CronBuilder from './CronBuilder'

interface PinnedSet {
  id: string
  name: string
  count: number
}

type Load =
  | { state: 'loading' }
  | { state: 'error'; message: string }
  | { state: 'ready'; schedule: ScheduleDetailData }

// ScheduleDetail drill-down: the full (editable) config for one schedule, the
// next 5 occurrences (from GET /schedules/{id}), and delete. Edits PATCH the
// changed fields; the local copy is replaced with the server's response.
function ScheduleDetail() {
  const { id } = useParams<{ id: string }>()
  const navigate = useNavigate()
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const [sets, setSets] = useState<PinnedSet[]>([])
  const [saving, setSaving] = useState(false)
  const [savedAt, setSavedAt] = useState<number | null>(null)

  // Editable draft fields.
  const [name, setName] = useState('')
  const [cron, setCron] = useState('')
  const [task, setTask] = useState('')
  const [model, setModel] = useState('claude-sonnet-5')
  const [pinnedSetId, setPinnedSetId] = useState<string>('')
  const [requireConfirmation, setRequireConfirmation] = useState(true)
  const [snoozeMinutes, setSnoozeMinutes] = useState(5)

  const hydrate = useCallback((s: ScheduleDetailData) => {
    setName(s.name)
    setCron(s.cron)
    setTask(s.task)
    setModel(s.model)
    setPinnedSetId(s.pinned_set_id ?? '')
    setRequireConfirmation(s.require_confirmation)
    setSnoozeMinutes(s.snooze_minutes)
  }, [])

  const fetchOne = useCallback(async () => {
    if (!id) return
    setLoad({ state: 'loading' })
    try {
      const schedule = await getSchedule(id)
      hydrate(schedule)
      setLoad({ state: 'ready', schedule })
    } catch (err) {
      setLoad({ state: 'error', message: err instanceof Error ? err.message : 'Network error' })
    }
  }, [id, hydrate])

  useEffect(() => {
    fetchOne()
    ;(async () => {
      const res = await safeInvoke<PinnedSet[]>('pinned_list')
      if (res.ok) setSets(res.data ?? [])
    })()
  }, [fetchOne])

  const save = useCallback(async () => {
    if (!id || saving) return
    setSaving(true)
    try {
      const updated = await patchSchedule(id, {
        name: name.trim(),
        cron: cron.trim(),
        task,
        model,
        pinned_set_id: pinnedSetId || null,
        require_confirmation: requireConfirmation,
        snooze_minutes: snoozeMinutes,
      })
      // Merge the fresh core fields with the existing occurrences (PATCH returns
      // a Schedule without next_occurrences).
      setLoad((prev) =>
        prev.state === 'ready'
          ? { state: 'ready', schedule: { ...prev.schedule, ...updated } }
          : { state: 'ready', schedule: { ...updated, next_occurrences: [] } },
      )
      setSavedAt(Date.now())
    } catch (err) {
      setLoad({ state: 'error', message: err instanceof Error ? err.message : 'Save failed' })
    } finally {
      setSaving(false)
    }
  }, [id, saving, name, cron, task, model, pinnedSetId, requireConfirmation, snoozeMinutes])

  const remove = useCallback(async () => {
    if (!id) return
    try {
      await deleteSchedule(id)
      navigate('/scheduled')
    } catch (err) {
      setLoad({ state: 'error', message: err instanceof Error ? err.message : 'Delete failed' })
    }
  }, [id, navigate])

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
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
        <Button variant="ghost" size="sm" onClick={() => navigate('/scheduled')}>
          ← Back
        </Button>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
          Schedule
        </h1>
      </div>

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
          <Spinner /> Loading…
        </div>
      )}

      {load.state === 'error' && (
        <div style={{ padding: 'var(--sp-4)' }}>
          <div className="error-message">{load.message}</div>
        </div>
      )}

      {load.state === 'ready' && (
        <>
          <Card style={{ boxShadow: 'var(--shadow-2)' }}>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-5)' }}>
              <Field label="Name">
                <input className="agent-input" value={name} onChange={(e) => setName(e.target.value)} style={inputStyle} />
              </Field>

              <Field label="Schedule">
                <CronBuilder value={cron} onChange={setCron} timezone={load.schedule.timezone} />
              </Field>

              <Field label="Task">
                <textarea
                  className="agent-input"
                  value={task}
                  onChange={(e) => setTask(e.target.value)}
                  rows={5}
                  style={{ ...inputStyle, resize: 'vertical', lineHeight: 1.55, minHeight: 110 }}
                />
              </Field>

              <div
                style={{
                  display: 'grid',
                  gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))',
                  gap: 'var(--sp-4)',
                }}
              >
                <Field label="Model">
                  <select value={model} onChange={(e) => setModel(e.target.value)} style={{ ...inputStyle, cursor: 'pointer' }}>
                    <option value="claude-sonnet-5">Claude Sonnet 5</option>
                    <option value="claude-opus-4-8">Claude Opus 4.8</option>
                  </select>
                </Field>

                <Field label="Pinned set">
                  <select
                    value={pinnedSetId}
                    onChange={(e) => setPinnedSetId(e.target.value)}
                    style={{ ...inputStyle, cursor: 'pointer' }}
                  >
                    <option value="">None</option>
                    {sets.map((s) => (
                      <option key={s.id} value={s.id}>
                        {s.name} · {s.count} {s.count === 1 ? 'file' : 'files'}
                      </option>
                    ))}
                    {/* Preserve an id not in the local library so it isn't silently dropped. */}
                    {pinnedSetId && !sets.some((s) => s.id === pinnedSetId) && (
                      <option value={pinnedSetId}>{pinnedSetId}</option>
                    )}
                  </select>
                </Field>

                <Field label="Snooze minutes">
                  <input
                    type="number"
                    min={1}
                    className="agent-input"
                    value={snoozeMinutes}
                    onChange={(e) => setSnoozeMinutes(Math.max(1, Number(e.target.value) || 1))}
                    style={inputStyle}
                  />
                </Field>
              </div>

              <label
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: 'var(--sp-2)',
                  cursor: 'pointer',
                  fontSize: 'var(--fs-md)',
                  color: 'var(--sb-text-muted)',
                }}
              >
                <input
                  type="checkbox"
                  checked={requireConfirmation}
                  onChange={(e) => setRequireConfirmation(e.target.checked)}
                />
                Require confirmation before each fire
              </label>

              <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
                <Button variant="primary" size="md" onClick={save} disabled={saving} style={{ minWidth: 140 }}>
                  {saving ? (
                    <>
                      <Spinner size={14} style={{ borderTopColor: '#0A0A0A', borderColor: 'rgba(0,0,0,0.25)' }} />
                      Saving…
                    </>
                  ) : (
                    'Save changes'
                  )}
                </Button>
                {savedAt && (
                  <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-success)' }}>Saved.</span>
                )}
                <div style={{ marginLeft: 'auto' }}>
                  <Button variant="danger" size="md" onClick={remove}>
                    Delete
                  </Button>
                </div>
              </div>
            </div>
          </Card>

          {/* Next occurrences */}
          <Card title={<SectionTitle>Next occurrences</SectionTitle>}>
            {load.schedule.next_occurrences.length === 0 ? (
              <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
                {load.schedule.enabled ? 'None computed.' : 'Schedule is paused.'}
              </span>
            ) : (
              <div style={{ display: 'flex', flexDirection: 'column' }}>
                {load.schedule.next_occurrences.slice(0, 5).map((iso, i) => (
                  <div key={iso}>
                    {i > 0 && <Divider style={{ margin: '8px 0' }} />}
                    <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
                      <Badge tone="gold">{i + 1}</Badge>
                      <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text)' }}>
                        {formatOccurrence(iso)}
                      </span>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </Card>
        </>
      )}
    </div>
  )
}

function formatOccurrence(iso: string): string {
  const ms = Date.parse(iso)
  if (!Number.isFinite(ms)) return iso
  return new Date(ms).toLocaleString(undefined, { dateStyle: 'full', timeStyle: 'short' })
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
      <SectionTitle>{label}</SectionTitle>
      {children}
    </label>
  )
}

const inputStyle: React.CSSProperties = {
  width: '100%',
  boxSizing: 'border-box',
  fontFamily: 'var(--font-sans)',
  fontSize: 'var(--fs-base)',
  color: 'var(--sb-text)',
  background: 'var(--sb-surface-3)',
  border: '1px solid var(--sb-border)',
  borderRadius: 'var(--r-md)',
  padding: '10px 12px',
}

export default ScheduleDetail

import { useEffect, useState } from 'react'
import { Button, SectionTitle, Badge, Spinner } from '../ui'
import { safeInvoke } from '../lib'
import type { FireItem } from '../useScheduler'

// The fire modal: shown (globally, above any view) for the head-of-queue owed
// occurrence. Presents the schedule name, human cron label, occurrence time
// (localized, with a "was due Xm ago" hint when in the past), the task summary
// and target pinned set, and three actions: Accept / Snooze (n min) / Skip.
//
// Matches the app's modal house style (fixed backdrop + surface-1 card + CSS
// vars), mirroring RunDetail's lightbox / the shared ui.tsx kit.
interface ScheduleFireModalProps {
  item: FireItem
  busy: boolean
  onAccept: () => void
  onSnooze: () => void
  onSkip: () => void
}

interface PinnedSet {
  id: string
  name: string
  count: number
}

// Localized absolute time, e.g. "Jul 18, 2026, 09:00".
function formatOccurrence(iso: string): string {
  const ms = Date.parse(iso)
  if (!Number.isFinite(ms)) return iso
  return new Date(ms).toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  })
}

// "was due Xm ago" / "due in Xm" relative to now, or null if it's basically now.
function dueHint(iso: string): string | null {
  const ms = Date.parse(iso)
  if (!Number.isFinite(ms)) return null
  const diffMin = Math.round((Date.now() - ms) / 60_000)
  if (diffMin >= 1) {
    if (diffMin < 60) return `was due ${diffMin}m ago`
    const hr = Math.round(diffMin / 60)
    if (hr < 24) return `was due ${hr}h ago`
    return `was due ${Math.round(hr / 24)}d ago`
  }
  if (diffMin <= -1) return `due in ${-diffMin}m`
  return null
}

function ScheduleFireModal({ item, busy, onAccept, onSnooze, onSkip }: ScheduleFireModalProps) {
  const { schedule, occurrenceTs } = item
  const [setName, setSetName] = useState<string | null>(null)

  // Resolve the target pinned set's display name from the LOCAL library (Rust
  // pinned_list), same source NewRun reads. Best-effort — fall back to the id.
  useEffect(() => {
    let active = true
    const id = schedule.pinned_set_id
    if (!id) {
      setSetName(null)
      return
    }
    ;(async () => {
      const res = await safeInvoke<PinnedSet[]>('pinned_list')
      if (!active) return
      if (res.ok) {
        const match = (res.data ?? []).find((s) => s.id === id)
        setSetName(match ? match.name : id)
      } else {
        setSetName(id)
      }
    })()
    return () => {
      active = false
    }
  }, [schedule.pinned_set_id])

  const hint = dueHint(occurrenceTs)
  const snoozeMin = schedule.snooze_minutes > 0 ? schedule.snooze_minutes : 5

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Scheduled run"
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
        style={{
          width: '100%',
          maxWidth: 480,
          background: 'var(--sb-surface-1)',
          border: '1px solid var(--sb-border-gold)',
          borderRadius: 'var(--r-lg)',
          boxShadow: 'var(--shadow-2)',
          overflow: 'hidden',
        }}
      >
        {/* Header */}
        <div
          style={{
            padding: '14px 20px',
            borderBottom: '1px solid var(--sb-border)',
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-2)',
          }}
        >
          <span aria-hidden style={{ fontSize: 16 }}>
            ⏰
          </span>
          <SectionTitle>Scheduled run due</SectionTitle>
        </div>

        {/* Body */}
        <div style={{ padding: '20px', display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
          <div>
            <div
              style={{
                fontSize: 'var(--fs-xl)',
                fontWeight: 700,
                color: 'var(--sb-gold-bright)',
                marginBottom: 4,
              }}
            >
              {schedule.name}
            </div>
            <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
              Scheduled task due to run
            </div>
          </div>

          {/* Occurrence time */}
          <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--sp-2)', flexWrap: 'wrap' }}>
            <span style={{ fontSize: 'var(--fs-base)', color: 'var(--sb-text)' }}>
              {formatOccurrence(occurrenceTs)}
            </span>
            {hint && (
              <Badge tone={hint.startsWith('was due') ? 'gold' : 'neutral'}>{hint}</Badge>
            )}
          </div>

          {/* Task summary */}
          <div>
            <SectionTitle>Task</SectionTitle>
            <div
              style={{
                marginTop: 6,
                padding: '10px 12px',
                background: 'var(--sb-surface-3)',
                border: '1px solid var(--sb-border)',
                borderRadius: 'var(--r-md)',
                fontSize: 'var(--fs-md)',
                color: 'var(--sb-text)',
                lineHeight: 1.5,
                maxHeight: 140,
                overflowY: 'auto',
                whiteSpace: 'pre-wrap',
              }}
            >
              {schedule.task || '(no task)'}
            </div>
          </div>

          {/* Model + pinned set */}
          <div style={{ display: 'flex', gap: 'var(--sp-2)', flexWrap: 'wrap' }}>
            <Badge mono>{schedule.model}</Badge>
            {schedule.pinned_set_id && (
              <Badge tone="gold">📌 {setName ?? '…'}</Badge>
            )}
          </div>

          {/* Actions */}
          <div style={{ display: 'flex', gap: 'var(--sp-2)', justifyContent: 'flex-end', marginTop: 'var(--sp-2)' }}>
            <Button variant="ghost" size="md" onClick={onSkip} disabled={busy}>
              Skip
            </Button>
            <Button variant="secondary" size="md" onClick={onSnooze} disabled={busy}>
              Snooze ({snoozeMin} min)
            </Button>
            <Button variant="primary" size="md" onClick={onAccept} disabled={busy}>
              {busy ? (
                <>
                  <Spinner size={14} style={{ borderTopColor: '#0A0A0A', borderColor: 'rgba(0,0,0,0.25)' }} />
                  Starting…
                </>
              ) : (
                <>▶ Accept</>
              )}
            </Button>
          </div>
        </div>
      </div>
    </div>
  )
}

export default ScheduleFireModal

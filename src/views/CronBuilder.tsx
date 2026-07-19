import { useEffect, useRef, useState } from 'react'
import { SectionTitle } from '../ui'
import { cronLabel } from '../schedules'

// A controlled, descriptive cron builder. Friendly controls (frequency + time +
// day pickers) generate a standard 5-field cron string (min hour dom month dow)
// which is what the backend (cronsim) expects. Invalid/incomplete states emit
// onChange('') so callers can gate submit on a non-empty cron.
//
// It also best-effort PARSES an incoming `value` back into matching controls —
// but only the shapes it itself produces:
//   `MM * * * *`      → Hourly (at minute :MM)
//   `0 */N * * *`     → Hourly (every N hours)
//   `MM HH * * *`     → Daily
//   `MM HH * * days`  → Weekly (days = comma list of 0-6)
//   `MM HH DOM * *`   → Monthly
// Anything else falls back to Custom mode showing the raw cron verbatim.

type Mode = 'hourly' | 'daily' | 'weekly' | 'monthly' | 'custom'

const DOW: { value: number; label: string }[] = [
  { value: 0, label: 'Sun' },
  { value: 1, label: 'Mon' },
  { value: 2, label: 'Tue' },
  { value: 3, label: 'Wed' },
  { value: 4, label: 'Thu' },
  { value: 5, label: 'Fri' },
  { value: 6, label: 'Sat' },
]

const MODES: { id: Mode; label: string }[] = [
  { id: 'hourly', label: 'Hourly' },
  { id: 'daily', label: 'Daily' },
  { id: 'weekly', label: 'Weekly' },
  { id: 'monthly', label: 'Monthly' },
  { id: 'custom', label: 'Custom' },
]

// The full internal control state. Time (hour/minute) is shared across the
// daily/weekly/monthly modes; hourly has its own at-minute + every-N controls.
interface BuilderState {
  mode: Mode
  minute: number // 0-59 — daily/weekly/monthly time + hourly at-minute
  hour: number // 0-23
  hourlyMode: 'atMinute' | 'everyN'
  hourlyMinute: number // 0-59, the :MM for "every hour at minute"
  everyN: number // 1-23
  days: number[] // weekly day-of-week (0-6)
  dom: number // monthly day-of-month (1-31)
  custom: string // raw cron for power users
}

const DEFAULT_STATE: BuilderState = {
  mode: 'daily',
  minute: 0,
  hour: 9,
  hourlyMode: 'atMinute',
  hourlyMinute: 0,
  everyN: 2,
  days: [1], // Mon
  dom: 1,
  custom: '',
}

const clamp = (n: number, lo: number, hi: number): number =>
  Number.isFinite(n) ? Math.min(hi, Math.max(lo, Math.trunc(n))) : lo

const pad2 = (n: number): string => String(n).padStart(2, '0')

// Compute the 5-field cron from the current controls. Returns '' for
// invalid/incomplete states (weekly with no days) so callers can gate submit.
function computeCron(s: BuilderState): string {
  switch (s.mode) {
    case 'hourly':
      if (s.hourlyMode === 'everyN') {
        const n = clamp(s.everyN, 1, 23)
        return n === 1 ? '0 * * * *' : `0 */${n} * * *`
      }
      return `${clamp(s.hourlyMinute, 0, 59)} * * * *`
    case 'daily':
      return `${clamp(s.minute, 0, 59)} ${clamp(s.hour, 0, 23)} * * *`
    case 'weekly': {
      if (s.days.length === 0) return '' // require ≥1 day
      const days = [...new Set(s.days)].sort((a, b) => a - b).join(',')
      return `${clamp(s.minute, 0, 59)} ${clamp(s.hour, 0, 23)} * * ${days}`
    }
    case 'monthly':
      return `${clamp(s.minute, 0, 59)} ${clamp(s.hour, 0, 23)} ${clamp(s.dom, 1, 31)} * *`
    case 'custom':
      return s.custom.trim()
  }
}

const isNum = (t: string): boolean => /^\d+$/.test(t)

// Best-effort parse of `value` into controls, recognizing ONLY the shapes this
// builder emits. Returns a full BuilderState (merged over defaults). Unknown
// shapes → Custom mode holding the raw string.
function parseCron(value: string): BuilderState {
  const raw = value.trim()
  const base = { ...DEFAULT_STATE }
  if (!raw) return base // empty → leave defaults (Daily 09:00), emit nothing until touched
  const parts = raw.split(/\s+/)
  if (parts.length !== 5) return { ...base, mode: 'custom', custom: raw }
  const [mi, hh, dom, mon, dow] = parts

  // Hourly — every N hours: `0 */N * * *`
  const everyMatch = /^\*\/(\d+)$/.exec(hh)
  if (mi === '0' && everyMatch && dom === '*' && mon === '*' && dow === '*') {
    return { ...base, mode: 'hourly', hourlyMode: 'everyN', everyN: clamp(Number(everyMatch[1]), 1, 23) }
  }
  // Hourly — at minute: `MM * * * *`
  if (isNum(mi) && hh === '*' && dom === '*' && mon === '*' && dow === '*') {
    return { ...base, mode: 'hourly', hourlyMode: 'atMinute', hourlyMinute: clamp(Number(mi), 0, 59) }
  }
  // The remaining recognized shapes all have numeric minute + hour.
  if (isNum(mi) && isNum(hh) && mon === '*') {
    const minute = clamp(Number(mi), 0, 59)
    const hour = clamp(Number(hh), 0, 23)
    // Daily: `MM HH * * *`
    if (dom === '*' && dow === '*') {
      return { ...base, mode: 'daily', minute, hour }
    }
    // Weekly: `MM HH * * days` (days = comma list of 0-6)
    if (dom === '*' && dow !== '*') {
      const toks = dow.split(',')
      if (toks.every((t) => isNum(t) && Number(t) >= 0 && Number(t) <= 6)) {
        const days = [...new Set(toks.map(Number))].sort((a, b) => a - b)
        return { ...base, mode: 'weekly', minute, hour, days }
      }
    }
    // Monthly: `MM HH DOM * *`
    if (dow === '*' && isNum(dom)) {
      const d = Number(dom)
      if (d >= 1 && d <= 31) {
        return { ...base, mode: 'monthly', minute, hour, dom: d }
      }
    }
  }
  // Unrecognized → Custom, verbatim.
  return { ...base, mode: 'custom', custom: raw }
}

export default function CronBuilder({
  value,
  onChange,
  timezone,
}: {
  value: string
  onChange: (cron: string) => void
  timezone: string
}) {
  const [state, setState] = useState<BuilderState>(() => parseCron(value))

  // Re-parse when `value` changes EXTERNALLY (mount / seeding an edit form). We
  // skip re-parsing when the incoming value already matches what our current
  // controls produce — that's the echo of our own onChange, and re-parsing it
  // would clobber controls that don't round-trip (e.g. weekly-with-no-days → '').
  const lastEmitted = useRef<string>(computeCron(state))
  useEffect(() => {
    if (value !== lastEmitted.current) {
      const parsed = parseCron(value)
      setState(parsed)
      lastEmitted.current = computeCron(parsed)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value])

  // Apply a control change, recompute the cron, and notify the caller.
  const update = (patch: Partial<BuilderState>) => {
    setState((prev) => {
      const next = { ...prev, ...patch }
      const cron = computeCron(next)
      lastEmitted.current = cron
      onChange(cron)
      return next
    })
  }

  const cron = computeCron(state)

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
      {/* Frequency selector */}
      <Field label="Frequency">
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--sp-2)' }}>
          {MODES.map((m) => (
            <button
              key={m.id}
              type="button"
              onClick={() => update({ mode: m.id })}
              style={chipStyle(m.id === state.mode)}
            >
              {m.label}
            </button>
          ))}
        </div>
      </Field>

      {/* Hourly */}
      {state.mode === 'hourly' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--sp-2)' }}>
            <button
              type="button"
              onClick={() => update({ hourlyMode: 'atMinute' })}
              style={chipStyle(state.hourlyMode === 'atMinute')}
            >
              At minute
            </button>
            <button
              type="button"
              onClick={() => update({ hourlyMode: 'everyN' })}
              style={chipStyle(state.hourlyMode === 'everyN')}
            >
              Every N hours
            </button>
          </div>
          {state.hourlyMode === 'atMinute' ? (
            <Field label="Minute of the hour">
              <select
                value={state.hourlyMinute}
                onChange={(e) => update({ hourlyMinute: Number(e.target.value) })}
                style={selectStyle}
              >
                {Array.from({ length: 60 }, (_, i) => (
                  <option key={i} value={i}>
                    :{pad2(i)}
                  </option>
                ))}
              </select>
            </Field>
          ) : (
            <Field label="Interval (hours)">
              <select value={state.everyN} onChange={(e) => update({ everyN: Number(e.target.value) })} style={selectStyle}>
                {Array.from({ length: 23 }, (_, i) => i + 1).map((n) => (
                  <option key={n} value={n}>
                    Every {n} {n === 1 ? 'hour' : 'hours'}
                  </option>
                ))}
              </select>
            </Field>
          )}
        </div>
      )}

      {/* Daily */}
      {state.mode === 'daily' && (
        <TimeField
          hour={state.hour}
          minute={state.minute}
          onChange={(hour, minute) => update({ hour, minute })}
        />
      )}

      {/* Weekly */}
      {state.mode === 'weekly' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
          <Field label="Days of week">
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--sp-2)' }}>
              {DOW.map((d) => {
                const on = state.days.includes(d.value)
                return (
                  <button
                    key={d.value}
                    type="button"
                    onClick={() =>
                      update({
                        days: on ? state.days.filter((x) => x !== d.value) : [...state.days, d.value],
                      })
                    }
                    style={chipStyle(on)}
                  >
                    {d.label}
                  </button>
                )
              })}
            </div>
            {state.days.length === 0 && (
              <span style={{ ...hintStyle, color: 'var(--sb-danger-bright)' }}>Select at least one day.</span>
            )}
          </Field>
          <TimeField
            hour={state.hour}
            minute={state.minute}
            onChange={(hour, minute) => update({ hour, minute })}
          />
        </div>
      )}

      {/* Monthly */}
      {state.mode === 'monthly' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
          <Field label="Day of month">
            <select value={state.dom} onChange={(e) => update({ dom: Number(e.target.value) })} style={selectStyle}>
              {Array.from({ length: 31 }, (_, i) => i + 1).map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
            </select>
          </Field>
          <TimeField
            hour={state.hour}
            minute={state.minute}
            onChange={(hour, minute) => update({ hour, minute })}
          />
        </div>
      )}

      {/* Custom */}
      {state.mode === 'custom' && (
        <Field label="Cron expression">
          <input
            className="agent-input"
            value={state.custom}
            onChange={(e) => update({ custom: e.target.value })}
            placeholder="e.g. 0 9 * * 1-5"
            spellCheck={false}
            style={{ ...selectStyle, fontFamily: 'var(--font-mono)', cursor: 'text' }}
          />
        </Field>
      )}

      {/* Live preview */}
      <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
        {cron ? (
          <>
            <span style={{ color: 'var(--sb-gold)' }}>{cronLabel(cron)}</span>
            <span style={{ color: 'var(--sb-text-faint)' }}> · {timezone}</span>
            <div style={{ marginTop: 'var(--sp-1)', fontFamily: 'var(--font-mono)', color: 'var(--sb-text-faint)' }}>
              {cron}
            </div>
          </>
        ) : (
          <span style={{ color: 'var(--sb-text-faint)' }}>Finish configuring the schedule to preview it.</span>
        )}
      </div>
    </div>
  )
}

// A shared HH:MM time picker used by daily/weekly/monthly.
function TimeField({
  hour,
  minute,
  onChange,
}: {
  hour: number
  minute: number
  onChange: (hour: number, minute: number) => void
}) {
  return (
    <Field label="Time">
      <input
        type="time"
        className="agent-input"
        value={`${pad2(hour)}:${pad2(minute)}`}
        onChange={(e) => {
          const [h, m] = e.target.value.split(':')
          const hh = clamp(Number(h), 0, 23)
          const mm = clamp(Number(m), 0, 59)
          onChange(hh, mm)
        }}
        style={{ ...selectStyle, cursor: 'text', maxWidth: 160 }}
      />
    </Field>
  )
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
      <SectionTitle>{label}</SectionTitle>
      {children}
    </label>
  )
}

// Selectable pill, matching NewRun's templateChipStyle conventions.
function chipStyle(selected: boolean): React.CSSProperties {
  return {
    fontFamily: 'var(--font-sans)',
    fontSize: 'var(--fs-md)',
    fontWeight: 600,
    padding: '7px 14px',
    borderRadius: 'var(--r-pill)',
    cursor: 'pointer',
    whiteSpace: 'nowrap',
    transition: 'all 0.15s ease',
    color: selected ? 'var(--sb-gold-bright)' : 'var(--sb-text-muted)',
    background: selected ? 'var(--sb-gold-dim)' : 'var(--sb-surface-3)',
    border: `1px solid ${selected ? 'var(--sb-border-gold)' : 'var(--sb-border)'}`,
  }
}

const selectStyle: React.CSSProperties = {
  width: '100%',
  boxSizing: 'border-box',
  fontFamily: 'var(--font-sans)',
  fontSize: 'var(--fs-base)',
  color: 'var(--sb-text)',
  background: 'var(--sb-surface-3)',
  border: '1px solid var(--sb-border)',
  borderRadius: 'var(--r-md)',
  padding: '10px 12px',
  cursor: 'pointer',
}

const hintStyle: React.CSSProperties = {
  fontSize: 'var(--fs-sm)',
  color: 'var(--sb-text-faint)',
}

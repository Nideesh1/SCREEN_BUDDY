// ScreenBuddy — shared UI kit.
//
// Reusable, presentational components for the refined luxe gold-on-black look.
// Everything is inline-styled off the CSS custom properties defined in
// index.css, so these components automatically track theme tokens. Gold is used
// strictly as an accent (borders, key text, active state); surfaces are layered
// dark neutrals. Pure/presentational — no data fetching, no side effects.
//
// Restyling agents will import from here; existing views are untouched.

import React from 'react'

// ───────────────────────────────────────────────────────── Card

interface CardProps {
  title?: React.ReactNode
  actions?: React.ReactNode
  children?: React.ReactNode
  /** When false, removes inner padding (e.g. for flush log wells). Default true. */
  padded?: boolean
  style?: React.CSSProperties
  className?: string
}

/** A raised --sb-surface-1 panel with a subtle border, large radius and a soft
 *  shadow. Optional header row: gold uppercase-tracked title on the left,
 *  right-aligned `actions`. */
export function Card({ title, actions, children, padded = true, style, className }: CardProps) {
  const showHeader = title != null || actions != null
  return (
    <div
      className={className}
      style={{
        background: 'var(--sb-surface-1)',
        border: '1px solid var(--sb-border)',
        borderRadius: 'var(--r-lg)',
        boxShadow: 'var(--shadow-1)',
        overflow: 'hidden',
        ...style,
      }}
    >
      {showHeader && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            gap: 'var(--sp-3)',
            padding: '12px 16px',
            borderBottom: '1px solid var(--sb-border)',
          }}
        >
          {title != null ? <SectionTitle>{title}</SectionTitle> : <span />}
          {actions != null && (
            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>{actions}</div>
          )}
        </div>
      )}
      <div style={{ padding: padded ? 'var(--sp-4)' : 0 }}>{children}</div>
    </div>
  )
}

// ───────────────────────────────────────────────────────── SectionTitle

/** Uppercase, letter-tracked, gold micro-label used for section headers. */
export function SectionTitle({ children, style }: { children: React.ReactNode; style?: React.CSSProperties }) {
  return (
    <span
      style={{
        fontSize: 'var(--fs-xs)',
        fontWeight: 600,
        textTransform: 'uppercase',
        letterSpacing: '1px',
        color: 'var(--sb-gold)',
        ...style,
      }}
    >
      {children}
    </span>
  )
}

// ───────────────────────────────────────────────────────── StatusPill

type StatusKind = 'running' | 'completed' | 'failed' | 'cancelled' | 'pending' | 'error'

interface StatusMeta {
  label: string
  icon: string
  color: string
  bg: string
  border: string
  pulse?: boolean
}

// Normalize the many backend status spellings onto our canonical set.
function normalizeStatus(status?: string): StatusKind {
  switch ((status || '').toLowerCase()) {
    case 'running':
    case 'in_progress':
      return 'running'
    case 'done':
    case 'success':
    case 'completed':
      return 'completed'
    case 'failed':
      return 'failed'
    case 'error':
      return 'error'
    case 'stopped':
    case 'cancelled':
    case 'canceled':
      return 'cancelled'
    case 'pending':
    case 'queued':
    case 'idle':
    default:
      return 'pending'
  }
}

const STATUS_META: Record<StatusKind, StatusMeta> = {
  running: {
    label: 'Running',
    icon: '●',
    color: 'var(--sb-gold-bright)',
    bg: 'var(--sb-gold-dim)',
    border: 'var(--sb-border-gold)',
    pulse: true,
  },
  completed: {
    label: 'Completed',
    icon: '✓',
    color: 'var(--sb-success)',
    bg: 'rgba(111, 184, 122, 0.12)',
    border: 'rgba(111, 184, 122, 0.30)',
  },
  failed: {
    label: 'Failed',
    icon: '✕',
    color: 'var(--sb-danger-bright)',
    bg: 'rgba(192, 57, 43, 0.12)',
    border: 'rgba(192, 57, 43, 0.40)',
  },
  error: {
    label: 'Error',
    icon: '✕',
    color: 'var(--sb-danger-bright)',
    bg: 'rgba(192, 57, 43, 0.12)',
    border: 'rgba(192, 57, 43, 0.40)',
  },
  cancelled: {
    label: 'Cancelled',
    icon: '⊘',
    color: 'var(--sb-text-muted)',
    bg: 'rgba(255, 255, 255, 0.05)',
    border: 'var(--sb-border)',
  },
  pending: {
    label: 'Pending',
    icon: '○',
    color: 'var(--sb-text-muted)',
    bg: 'rgba(255, 255, 255, 0.05)',
    border: 'var(--sb-border)',
  },
}

/** Colored status pill mapping run states to icon + tone. `running` softly
 *  pulses; completed reads green-gold; failed/error read danger. */
export function StatusPill({ status, label }: { status?: string; label?: string }) {
  const kind = normalizeStatus(status)
  const meta = STATUS_META[kind]
  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        fontSize: 'var(--fs-sm)',
        fontWeight: 600,
        color: meta.color,
        background: meta.bg,
        border: `1px solid ${meta.border}`,
        borderRadius: 'var(--r-pill)',
        padding: '4px 12px',
        whiteSpace: 'nowrap',
        lineHeight: 1.2,
      }}
    >
      <span
        aria-hidden
        style={{
          fontSize: 10,
          animation: meta.pulse ? 'pulse 1.4s ease-in-out infinite' : undefined,
        }}
      >
        {meta.icon}
      </span>
      {label ?? meta.label}
    </span>
  )
}

// ───────────────────────────────────────────────────────── StatChip

/** Compact labeled stat for telemetry strips: tiny uppercase label over value. */
export function StatChip({ label, value }: { label: React.ReactNode; value: React.ReactNode }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 2, minWidth: 0 }}>
      <span
        style={{
          fontSize: 'var(--fs-xs)',
          textTransform: 'uppercase',
          letterSpacing: '0.8px',
          color: 'var(--sb-text-faint)',
        }}
      >
        {label}
      </span>
      <span style={{ fontSize: 'var(--fs-base)', color: 'var(--sb-text)' }}>{value}</span>
    </div>
  )
}

// ───────────────────────────────────────────────────────── Badge / Chip

type Tone = 'neutral' | 'gold' | 'danger' | 'success'

const TONE_STYLE: Record<Tone, { color: string; bg: string; border: string }> = {
  neutral: {
    color: 'var(--sb-text-muted)',
    bg: 'rgba(255, 255, 255, 0.04)',
    border: 'var(--sb-border)',
  },
  gold: {
    color: 'var(--sb-gold-bright)',
    bg: 'var(--sb-gold-dim)',
    border: 'var(--sb-border-gold)',
  },
  danger: {
    color: 'var(--sb-danger-bright)',
    bg: 'rgba(192, 57, 43, 0.12)',
    border: 'rgba(192, 57, 43, 0.40)',
  },
  success: {
    color: 'var(--sb-success)',
    bg: 'rgba(111, 184, 122, 0.12)',
    border: 'rgba(111, 184, 122, 0.30)',
  },
}

interface BadgeProps {
  children: React.ReactNode
  tone?: Tone
  /** Render in --font-mono (e.g. model ids, counts). */
  mono?: boolean
  title?: string
  style?: React.CSSProperties
}

/** Small pill — model chips, counts, tags. */
export function Badge({ children, tone = 'neutral', mono, title, style }: BadgeProps) {
  const t = TONE_STYLE[tone]
  return (
    <span
      title={title}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 4,
        fontSize: 'var(--fs-sm)',
        fontWeight: 500,
        fontFamily: mono ? 'var(--font-mono)' : undefined,
        color: t.color,
        background: t.bg,
        border: `1px solid ${t.border}`,
        borderRadius: 'var(--r-pill)',
        padding: '3px 10px',
        whiteSpace: 'nowrap',
        lineHeight: 1.2,
        ...style,
      }}
    >
      {children}
    </span>
  )
}

/** Alias of Badge for callers that prefer the "Chip" name. */
export const Chip = Badge

// ───────────────────────────────────────────────────────── Button

type ButtonVariant = 'primary' | 'secondary' | 'danger' | 'ghost'
type ButtonSize = 'sm' | 'md'

interface ButtonProps extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant
  size?: ButtonSize
}

const BTN_VARIANT: Record<ButtonVariant, React.CSSProperties> = {
  primary: {
    background: 'linear-gradient(135deg, var(--sb-gold) 0%, var(--sb-gold-deep) 100%)',
    color: '#0A0A0A',
    border: '1px solid transparent',
  },
  secondary: {
    background: 'var(--sb-surface-3)',
    color: 'var(--sb-text)',
    border: '1px solid var(--sb-border)',
  },
  danger: {
    background: 'var(--sb-danger)',
    color: '#fff',
    border: '1px solid transparent',
  },
  ghost: {
    background: 'transparent',
    color: 'var(--sb-text-muted)',
    border: '1px solid transparent',
  },
}

/** Themed button. Variants: primary (gold), secondary (neutral surface),
 *  danger, ghost. Sizes: sm | md. */
export function Button({ variant = 'secondary', size = 'md', style, children, ...rest }: ButtonProps) {
  const pad = size === 'sm' ? '6px 12px' : '10px 18px'
  const fs = size === 'sm' ? 'var(--fs-sm)' : 'var(--fs-base)'
  return (
    <button
      {...rest}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 8,
        padding: pad,
        fontSize: fs,
        fontWeight: 600,
        fontFamily: 'var(--font-sans)',
        borderRadius: 'var(--r-sm)',
        cursor: 'pointer',
        transition: 'all 0.18s ease',
        ...BTN_VARIANT[variant],
        ...style,
      }}
    >
      {children}
    </button>
  )
}

interface IconButtonProps extends React.ButtonHTMLAttributes<HTMLButtonElement> {
  size?: number
}

/** Square, borderless icon button with a gold hover wash (via the .nav-btn /
 *  .btn-ghost CSS hover, applied inline here for standalone use). */
export function IconButton({ size = 32, style, children, ...rest }: IconButtonProps) {
  return (
    <button
      {...rest}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        justifyContent: 'center',
        width: size,
        height: size,
        padding: 0,
        fontSize: Math.round(size * 0.5),
        color: 'var(--sb-text-muted)',
        background: 'transparent',
        border: '1px solid transparent',
        borderRadius: 'var(--r-sm)',
        cursor: 'pointer',
        transition: 'all 0.18s ease',
        ...style,
      }}
    >
      {children}
    </button>
  )
}

// ───────────────────────────────────────────────────────── Divider

/** Hairline rule. `vertical` for inline separators. */
export function Divider({ vertical, style }: { vertical?: boolean; style?: React.CSSProperties }) {
  return (
    <div
      style={
        vertical
          ? { width: 1, alignSelf: 'stretch', background: 'var(--sb-border)', ...style }
          : { height: 1, width: '100%', background: 'var(--sb-border)', margin: 'var(--sp-3) 0', ...style }
      }
    />
  )
}

// ───────────────────────────────────────────────────────── EmptyState

interface EmptyStateProps {
  icon?: React.ReactNode
  title: React.ReactNode
  hint?: React.ReactNode
  action?: React.ReactNode
}

/** Centered placeholder for empty lists / no-data views. */
export function EmptyState({ icon, title, hint, action }: EmptyStateProps) {
  return (
    <div
      style={{
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        textAlign: 'center',
        gap: 'var(--sp-3)',
        padding: 'var(--sp-6)',
        color: 'var(--sb-text-muted)',
      }}
    >
      {icon != null && <div style={{ fontSize: 32, opacity: 0.7 }}>{icon}</div>}
      <div style={{ fontSize: 'var(--fs-lg)', color: 'var(--sb-text)', fontWeight: 600 }}>{title}</div>
      {hint != null && (
        <div style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)', maxWidth: 360 }}>{hint}</div>
      )}
      {action != null && <div style={{ marginTop: 'var(--sp-1)' }}>{action}</div>}
    </div>
  )
}

// ───────────────────────────────────────────────────────── Spinner

/** Gold ring spinner. `size` in px. */
export function Spinner({ size = 18, style }: { size?: number; style?: React.CSSProperties }) {
  return (
    <span
      role="status"
      aria-label="Loading"
      style={{
        display: 'inline-block',
        width: size,
        height: size,
        borderRadius: '50%',
        border: `2px solid var(--sb-gold-dim)`,
        borderTopColor: 'var(--sb-gold)',
        animation: 'sb-spin 0.7s linear infinite',
        ...style,
      }}
    />
  )
}

// ───────────────────────────────────────────────────────── Icons
//
// Monochrome line-icons matching the NavRail house style: no fill, stroke
// currentColor (so they inherit text color), ~1.7 stroke, round caps. Sized via
// a `size` prop (default 16). IconBase is a small local duplicate of NavRail's
// wrapper on purpose — keeps these reusable without cross-importing.

function IconBase({ size = 16, children }: { size?: number; children: React.ReactNode }) {
  return (
    <svg
      aria-hidden="true"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {children}
    </svg>
  )
}

/** Bookmark/pin tag — mirrors NavRail's PinIcon. */
export function PinIcon({ size }: { size?: number }) {
  return (
    <IconBase size={size}>
      <path d="M6 3h12a1 1 0 0 1 1 1v17l-7-5-7 5V4a1 1 0 0 1 1-1z" />
    </IconBase>
  )
}

/** Trash can — lid line, hinge, body, and two vertical slats. */
export function TrashIcon({ size }: { size?: number }) {
  return (
    <IconBase size={size}>
      <path d="M4 7h16" />
      <path d="M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2" />
      <path d="M6 7l1 13a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-13" />
      <path d="M10 11v6" />
      <path d="M14 11v6" />
    </IconBase>
  )
}

/** Framed photo — frame, sun, mountain range. */
export function ImageIcon({ size }: { size?: number }) {
  return (
    <IconBase size={size}>
      <path d="M3 5h18v14H3z" />
      <circle cx="8.5" cy="10" r="1.5" />
      <path d="M21 16l-5-5-4 4-2-2-7 7" />
    </IconBase>
  )
}

/** Film strip — frame with sprocket perforations down both edges. Marks the
 *  "new set from video" action, distinct from the image/photo set action. */
export function FilmIcon({ size }: { size?: number }) {
  return (
    <IconBase size={size}>
      <rect x="3" y="4" width="18" height="16" rx="1.5" />
      <path d="M7 4v16" />
      <path d="M17 4v16" />
      <path d="M3 8h4" />
      <path d="M3 12h4" />
      <path d="M3 16h4" />
      <path d="M17 8h4" />
      <path d="M17 12h4" />
      <path d="M17 16h4" />
    </IconBase>
  )
}

/** Page with a folded corner. */
export function DocIcon({ size }: { size?: number }) {
  return (
    <IconBase size={size}>
      <path d="M6 3h8l4 4v14H6z" />
      <path d="M14 3v4h4" />
    </IconBase>
  )
}

// ───────────────────────────────────────────────────────── Trajectory helpers

/** Truncate a string to `max` chars with an ellipsis. */
function truncate(s: string, max = 32): string {
  const t = s.replace(/\s+/g, ' ').trim()
  return t.length > max ? t.slice(0, max - 1) + '…' : t
}

// Read a value off a loose input bag by trying several likely keys.
function pick(input: unknown, ...keys: string[]): unknown {
  if (input == null || typeof input !== 'object') return undefined
  const obj = input as Record<string, unknown>
  for (const k of keys) {
    if (obj[k] != null) return obj[k]
  }
  return undefined
}

// Format a [x, y] coordinate pair (array or {x,y}) as "x,y".
function coord(input: unknown): string {
  const c = pick(input, 'coordinate', 'coord', 'position', 'point')
  if (Array.isArray(c) && c.length >= 2) return `${c[0]},${c[1]}`
  const x = pick(input, 'x')
  const y = pick(input, 'y')
  if (x != null && y != null) return `${x},${y}`
  if (Array.isArray(input) && input.length >= 2) return `${input[0]},${input[1]}`
  return ''
}

/** Map a computer-use action name + input to a clean icon + human label.
 *  Pure: presentational only. Unknown actions fall back to the raw name. */
export function prettyAction(name: string, input: unknown): { icon: string; label: string } {
  const n = (name || '').toLowerCase()
  // Unwrap the computer-use tool: it's emitted/stored as the tool name
  // "computer" with the real action nested in input.action. Recurse on the
  // inner action so the cases below match (input still carries coordinate/text).
  if (
    (n === 'computer' || n === 'computer_20251124') &&
    input &&
    typeof input === 'object' &&
    typeof (input as { action?: unknown }).action === 'string'
  ) {
    return prettyAction(String((input as { action: string }).action), input)
  }
  switch (n) {
    case 'screenshot':
      return { icon: '📸', label: 'Screenshot' }
    case 'left_click':
    case 'click': {
      const c = coord(input)
      return { icon: '🖱', label: c ? `Click ${c}` : 'Click' }
    }
    case 'double_click': {
      const c = coord(input)
      return { icon: '🖱', label: c ? `Double-click ${c}` : 'Double-click' }
    }
    case 'right_click': {
      const c = coord(input)
      return { icon: '🖱', label: c ? `Right-click ${c}` : 'Right-click' }
    }
    case 'middle_click': {
      const c = coord(input)
      return { icon: '🖱', label: c ? `Middle-click ${c}` : 'Middle-click' }
    }
    case 'triple_click': {
      const c = coord(input)
      return { icon: '🖱', label: c ? `Triple-click ${c}` : 'Triple-click' }
    }
    case 'left_click_drag':
    case 'drag': {
      const c = coord(input)
      return { icon: '✥', label: c ? `Drag to ${c}` : 'Drag' }
    }
    case 'mouse_move': {
      const c = coord(input)
      return { icon: '↗', label: c ? `Move to ${c}` : 'Move' }
    }
    case 'type': {
      const text = pick(input, 'text', 'value')
      return { icon: '⌨', label: text != null ? `Type "${truncate(String(text))}"` : 'Type' }
    }
    case 'key':
    case 'keypress': {
      const key = pick(input, 'text', 'key', 'keys')
      const ks = key != null ? String(key) : ''
      const pretty = ks.toLowerCase() === 'return' || ks.toLowerCase() === 'enter' ? 'Return' : ks
      return { icon: '↵', label: pretty ? `Press ${pretty}` : 'Press key' }
    }
    case 'scroll': {
      const dir = pick(input, 'scroll_direction', 'direction')
      const amt = pick(input, 'scroll_amount', 'amount', 'clicks')
      const d = dir != null ? String(dir) : 'down'
      return { icon: '⇅', label: amt != null ? `Scroll ${d} ×${amt}` : `Scroll ${d}` }
    }
    case 'wait':
      return { icon: '⏲', label: 'Wait' }
    case 'cursor_position':
      return { icon: '⌖', label: 'Cursor position' }
    case 'left_mouse_down':
      return { icon: '🖱', label: 'Mouse down' }
    case 'left_mouse_up':
      return { icon: '🖱', label: 'Mouse up' }
    case 'hold_key': {
      const key = pick(input, 'text', 'key')
      return { icon: '⌨', label: key != null ? `Hold ${String(key)}` : 'Hold key' }
    }
    default:
      return { icon: '•', label: name || 'action' }
  }
}

interface ActionChipProps {
  name: string
  input?: unknown
  style?: React.CSSProperties
}

/** Tidy inline chip rendering prettyAction's icon + label. The dynamic
 *  coords/values portion is rendered in --font-mono; the rest stays sans. */
export function ActionChip({ name, input, style }: ActionChipProps) {
  const { icon, label } = prettyAction(name, input)
  // Split label into a leading word (verb) and a trailing mono part (coords/text).
  const m = label.match(/^(\D*?)\s*("?.*"?|[\d,]+(?:\s*×\d+)?|[A-Za-z]+)?$/)
  const head = m ? m[1].trim() : label
  const tail = m && m[2] ? m[2] : ''
  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        fontSize: 'var(--fs-sm)',
        color: 'var(--sb-text)',
        background: 'var(--sb-surface-2)',
        border: '1px solid var(--sb-border)',
        borderRadius: 'var(--r-sm)',
        padding: '3px 9px',
        lineHeight: 1.3,
        whiteSpace: 'nowrap',
        ...style,
      }}
    >
      <span aria-hidden style={{ color: 'var(--sb-gold)', fontSize: 13 }}>
        {icon}
      </span>
      <span>{head || label}</span>
      {tail && <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--sb-text-muted)' }}>{tail}</span>}
    </span>
  )
}

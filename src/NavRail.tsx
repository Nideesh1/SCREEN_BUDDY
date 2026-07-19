import { useEffect, useState } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'
import { listen } from '@tauri-apps/api/event'
import sbLogo from './assets/sb-logo.svg'

// Live remote-channel indicator. Subscribes to the Rust-emitted `remote://status`
// event and shows a small dot — gold/lit when the always-on WebSocket is
// connected (the backend can push runs here), dim when not. Unobtrusive, sits
// just above Sign out on the rail.
function RemoteIndicator() {
  const [connected, setConnected] = useState(false)
  useEffect(() => {
    const unlisten = listen<{ connected: boolean }>('remote://status', (e) => {
      setConnected(!!e.payload?.connected)
    })
    return () => {
      unlisten.then((un) => un())
    }
  }, [])
  return (
    <div
      className="nav-btn"
      role="status"
      aria-label={connected ? 'Remote connected' : 'Remote offline'}
      style={{
        position: 'relative',
        width: 46,
        height: 46,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        cursor: 'default',
      }}
    >
      <span
        style={{
          width: 9,
          height: 9,
          borderRadius: '50%',
          background: connected ? 'var(--sb-gold)' : 'var(--sb-text-muted)',
          boxShadow: connected ? '0 0 6px var(--sb-gold)' : 'none',
          opacity: connected ? 1 : 0.5,
          transition: 'all 0.2s ease',
        }}
      />
      <span className="nav-tooltip" role="tooltip">
        {connected ? 'Remote ● connected' : 'Remote ● offline'}
      </span>
    </div>
  )
}

// The route segment each nav item points at (also its ViewId). Exported so any
// caller that still thinks in view ids has one source of truth.
export type ViewId =
  | 'dashboard'
  | 'artifacts'
  | 'pinned'
  | 'runs'
  | 'scheduled'
  | 'history'
  | 'credentials'
  | 'settings'

interface NavItem {
  id: ViewId
  Icon: () => React.JSX.Element
  label: string
}

const ITEMS: NavItem[] = [
  { id: 'dashboard', Icon: HomeIcon, label: 'Dashboard' },
  { id: 'artifacts', Icon: ArtifactIcon, label: 'Artifacts' },
  { id: 'pinned', Icon: PinIcon, label: 'Pinned library' },
  { id: 'runs', Icon: PlusIcon, label: 'Runs' },
  { id: 'scheduled', Icon: CalendarIcon, label: 'Scheduled' },
  { id: 'history', Icon: ClockIcon, label: 'History' },
  { id: 'credentials', Icon: KeyIcon, label: 'Credentials' },
  { id: 'settings', Icon: GearIcon, label: 'Settings' },
]

interface NavRailProps {
  userEmail: string | null
  onSignOut: () => void
}

// Which nav item the current path highlights. Matched by path prefix so a
// drilldown like /runs/:id keeps the "Runs" item lit (the section that led
// there). Returns null when nothing matches (e.g. an unknown route).
function activeIdFor(pathname: string): ViewId | null {
  const seg = pathname.replace(/^\/+/, '').split('/')[0]
  // Templates is a Runs sub-view (reached from the Runs sub-nav), so keep the
  // "Runs" rail item lit while it's open — it has no rail icon of its own.
  if (seg === 'templates') return 'runs'
  const match = ITEMS.find((item) => item.id === seg)
  return match ? match.id : null
}

// Full-height left icon rail (no top bar): SB brand mark pinned to the top, the
// nav icons in the middle, and a Sign out control pushed to the very bottom via
// a flex spacer. Route-aware: the active item derives from the current path and
// a click navigates to that route. Sign out still calls the passed callback.
function NavRail({ userEmail, onSignOut }: NavRailProps) {
  const navigate = useNavigate()
  const location = useLocation()
  const active = activeIdFor(location.pathname)
  return (
    <nav
      style={{
        width: 68,
        flexShrink: 0,
        height: '100vh',
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        gap: 'var(--sp-2)',
        padding: '16px 0 18px',
        background: 'var(--sb-surface-1)',
        borderRight: '1px solid var(--sb-border)',
        boxSizing: 'border-box',
      }}
    >
      {/* Brand mark — top of the rail. */}
      <img
        src={sbLogo}
        alt="ScreenBuddy"
        title="ScreenBuddy"
        style={{ display: 'block', height: 26, width: 'auto', marginBottom: 'var(--sp-3)' }}
      />

      {/* Hairline under the brand mark. */}
      <div
        style={{
          width: 32,
          height: 1,
          background: 'var(--sb-border)',
          marginBottom: 'var(--sp-2)',
        }}
      />

      {/* Nav icons. */}
      {ITEMS.map((item) => {
        const isActive = item.id === active
        return (
          <button
            key={item.id}
            className="nav-btn"
            aria-label={item.label}
            aria-current={isActive ? 'page' : undefined}
            onClick={() => navigate('/' + item.id)}
            style={navButtonStyle(isActive)}
          >
            <item.Icon />
            {/* Styled gold/black tooltip, revealed on hover to the right of the
                icon (left rail). Pure CSS — see .nav-tooltip in index.css. */}
            <span className="nav-tooltip" role="tooltip">
              {item.label}
            </span>
          </button>
        )
      })}

      {/* Remote-channel status dot — pushed toward the bottom by the spacer,
          sitting just above Sign out. */}
      <div style={{ marginTop: 'auto' }}>
        <RemoteIndicator />
      </div>

      {/* Sign out — pinned to the very bottom of the rail. */}
      <button
        className="nav-btn"
        aria-label="Sign out"
        onClick={onSignOut}
        style={navButtonStyle(false)}
      >
        <SignOutIcon />
        <span className="nav-tooltip" role="tooltip">
          Sign out
          {userEmail && <span className="nav-tooltip-sub">{userEmail}</span>}
        </span>
      </button>
    </nav>
  )
}

// Shared icon-button styling. `isActive` only applies to the nav items.
// Active item reads as a gold-dim pill with a gold accent line and gold icon;
// idle items are muted neutral, lightening to gold on hover (.nav-btn:hover).
function navButtonStyle(isActive: boolean): React.CSSProperties {
  return {
    position: 'relative',
    width: 46,
    height: 46,
    display: 'flex',
    flexDirection: 'column',
    alignItems: 'center',
    justifyContent: 'center',
    gap: 2,
    border: '1px solid transparent',
    borderRadius: 'var(--r-md)',
    cursor: 'pointer',
    fontSize: 18,
    lineHeight: 1,
    background: isActive ? 'var(--sb-gold-dim)' : 'transparent',
    borderColor: isActive ? 'var(--sb-border-gold)' : 'transparent',
    // Icons use stroke="currentColor", so this drives their color: muted by
    // default, gold when active. Hover gold comes from .nav-btn:hover in CSS.
    color: isActive ? 'var(--sb-gold)' : 'var(--sb-text-muted)',
    transition: 'all 0.15s ease',
  }
}

// Monochrome line-icons. All share one stroked wrapper (currentColor, no fill,
// ~1.75 stroke, 20px) so they inherit the rail's muted/gold/hover color states.
function IconBase({ children }: { children: React.ReactNode }) {
  return (
    <svg
      aria-hidden="true"
      width="20"
      height="20"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.75"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {children}
    </svg>
  )
}

function HomeIcon() {
  return (
    <IconBase>
      <path d="M3 11l9-7 9 7" />
      <path d="M5 10v9a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-9" />
      <path d="M9 20v-6h6v6" />
    </IconBase>
  )
}

function PlusIcon() {
  return (
    <IconBase>
      <line x1="12" y1="5" x2="12" y2="19" />
      <line x1="5" y1="12" x2="19" y2="12" />
    </IconBase>
  )
}

function CalendarIcon() {
  return (
    <IconBase>
      <rect x="3" y="4" width="18" height="17" rx="2" />
      <line x1="3" y1="9" x2="21" y2="9" />
      <line x1="8" y1="2" x2="8" y2="6" />
      <line x1="16" y1="2" x2="16" y2="6" />
    </IconBase>
  )
}

function ClockIcon() {
  return (
    <IconBase>
      <circle cx="12" cy="12" r="9" />
      <polyline points="12 7 12 12 16 14" />
    </IconBase>
  )
}

function ArtifactIcon() {
  // Stacked layers — a library of media sitting on top of each other.
  return (
    <IconBase>
      <path d="M12 3 3 7.5l9 4.5 9-4.5L12 3z" />
      <path d="M3 12.5 12 17l9-4.5" />
      <path d="M3 17 12 21.5 21 17" />
    </IconBase>
  )
}

function PinIcon() {
  // A bookmark/pin tag.
  return (
    <IconBase>
      <path d="M6 3h12a1 1 0 0 1 1 1v17l-7-5-7 5V4a1 1 0 0 1 1-1z" />
    </IconBase>
  )
}

function KeyIcon() {
  return (
    <IconBase>
      <circle cx="8" cy="15" r="4" />
      <path d="M10.85 12.15 21 2" />
      <path d="M18 5l3 3" />
      <path d="M15 8l2.5 2.5" />
    </IconBase>
  )
}

function GearIcon() {
  return (
    <IconBase>
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
    </IconBase>
  )
}

// Logout / door-with-arrow.
function SignOutIcon() {
  return (
    <IconBase>
      <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" />
      <polyline points="16 17 21 12 16 7" />
      <line x1="21" y1="12" x2="9" y2="12" />
    </IconBase>
  )
}

export default NavRail

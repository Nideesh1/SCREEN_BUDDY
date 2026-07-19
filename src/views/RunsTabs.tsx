import { useLocation, useNavigate } from 'react-router-dom'

// Sub-navigation strip shared across the three Runs sub-views: New run,
// Scheduled (future), and History (past). Rendered at the top of each so the
// group reads as one area. Route-aware: the active tab derives from the current
// path prefix (so /scheduled/:id keeps "Scheduled" lit).
type TabId = 'runs' | 'scheduled' | 'history' | 'templates'

const TABS: { id: TabId; label: string }[] = [
  { id: 'runs', label: 'New run' },
  { id: 'scheduled', label: 'Scheduled' },
  { id: 'history', label: 'History' },
  { id: 'templates', label: 'Templates' },
]

function activeTab(pathname: string): TabId {
  const seg = pathname.replace(/^\/+/, '').split('/')[0]
  const match = TABS.find((t) => t.id === seg)
  return match ? match.id : 'runs'
}

function RunsTabs() {
  const navigate = useNavigate()
  const location = useLocation()
  const active = activeTab(location.pathname)
  return (
    <div
      style={{
        display: 'flex',
        gap: 'var(--sp-2)',
        justifyContent: 'center',
        marginBottom: 'var(--sp-2)',
      }}
    >
      {TABS.map((t) => {
        const selected = t.id === active
        return (
          <button
            key={t.id}
            type="button"
            onClick={() => navigate('/' + t.id)}
            style={{
              fontFamily: 'var(--font-sans)',
              fontSize: 'var(--fs-md)',
              fontWeight: 600,
              padding: '7px 16px',
              borderRadius: 'var(--r-pill)',
              cursor: 'pointer',
              whiteSpace: 'nowrap',
              transition: 'all 0.15s ease',
              color: selected ? 'var(--sb-gold-bright)' : 'var(--sb-text-muted)',
              background: selected ? 'var(--sb-gold-dim)' : 'var(--sb-surface-3)',
              border: `1px solid ${selected ? 'var(--sb-border-gold)' : 'var(--sb-border)'}`,
            }}
          >
            {t.label}
          </button>
        )
      })}
    </div>
  )
}

export default RunsTabs

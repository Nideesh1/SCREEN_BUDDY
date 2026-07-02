import { Outlet } from 'react-router-dom'
import NavRail from './NavRail'

interface LayoutProps {
  userEmail: string | null
  onSignOut: () => void
}

// Context handed to child routes via <Outlet/>. Leaf routes that need the
// signed-in account (Settings) read it with useOutletContext<LayoutContext>().
export interface LayoutContext {
  userEmail: string | null
  onSignOut: () => void
}

// The authenticated "inside": a full-height left icon nav rail (which also
// carries the brand mark + Sign out) and a routed main content area beside it.
// No top bar — each view supplies its own heading. This is the router parent
// layout: <NavRail/> stays put while <Outlet/> swaps the active route's view.
function Layout({ userEmail, onSignOut }: LayoutProps) {
  return (
    <div
      style={{
        height: '100vh',
        width: '100vw',
        display: 'flex',
        background: 'var(--sb-bg)',
        color: 'var(--sb-text)',
        fontFamily: 'var(--font-sans)',
        boxSizing: 'border-box',
        overflow: 'hidden',
      }}
    >
      <NavRail userEmail={userEmail} onSignOut={onSignOut} />
      <main
        style={{
          flex: 1,
          minWidth: 0,
          minHeight: 0,
          height: '100vh',
          overflowY: 'auto',
          // Subtle gold-tinted depth from the top so content lifts off the canvas.
          background:
            'radial-gradient(120% 80% at 50% -10%, rgba(212,175,55,0.04) 0%, transparent 55%), var(--sb-bg)',
        }}
      >
        <Outlet context={{ userEmail, onSignOut } satisfies LayoutContext} />
      </main>
    </div>
  )
}

export default Layout

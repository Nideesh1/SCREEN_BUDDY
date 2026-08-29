import { useEffect, useRef } from 'react'
import { HashRouter, Routes, Route, Navigate } from 'react-router-dom'
import { useGoogleAuth } from './hooks/useGoogleAuth'
import { ActiveRunProvider } from './activeRun'
import { CU_BACKEND, safeInvoke, reconcileOrphanedRuns, isTauri } from './lib'
import SplashLogin from './SplashLogin'
import Layout from './Layout'
import Dashboard from './views/Dashboard'
import NewRun from './views/NewRun'
import History from './views/History'
import RunDetail from './views/RunDetail'
import PinnedLibrary from './views/PinnedLibrary'
import Artifacts from './views/Artifacts'
import Credentials from './views/Credentials'
import Settings from './views/Settings'
import Scheduled from './views/Scheduled'
import Templates from './views/Templates'
import ScheduleDetail from './views/ScheduleDetail'
import ScheduleFireModal from './views/ScheduleFireModal'
import Admin from './views/Admin'
import { useScheduler } from './useScheduler'
import { ModeProvider, homeRouteFor, useMode } from './mode'
import ModePicker from './ModePicker'

// App is the auth gate (single source of truth for auth state). It calls
// useGoogleAuth() ONCE. Not authenticated -> splash. Authenticated -> the
// hash-routed run manager: a HashRouter (so a webview reload restores the
// route) wrapping the NavRail Layout + its child routes, all inside the shared
// ActiveRunProvider so the live-run hint survives navigation.
function App() {
  const { isAuthenticated, userEmail, isLoading, error, login, logout, checkAuth } =
    useGoogleAuth()

  // Restore any existing backend session on mount.
  useEffect(() => {
    checkAuth()
  }, [checkAuth])

  // Once authenticated, reconcile orphaned "running" runs exactly ONCE per app
  // launch. A mid-run restart/rebuild kills the executor process without it ever
  // PATCHing a terminal status, so the backend leaves that run stuck at
  // "running" — a ghost that shows as live across restarts. Since the local
  // executor is single (one run at a time), any run still "running" at startup
  // is by definition orphaned. Best effort: never blocks or crashes the UI.
  const reconciledRef = useRef(false)
  useEffect(() => {
    if (!isAuthenticated || reconciledRef.current) return
    const token = localStorage.getItem('screen_buddy_session_token')
    if (!token) return
    reconciledRef.current = true
    reconcileOrphanedRuns()
  }, [isAuthenticated])

  // Request OS notification permission once after auth so the Rust-sent
  // run-complete / run-failed notifications can actually display. Best-effort.
  // Desktop only: the plugin talks to the Tauri IPC bridge, and this same bundle
  // is served in a plain browser for the admin panel — hence the isTauri() gate
  // and the dynamic import (a static one would pull the plugin into the web
  // chunk for a call that can never run there).
  useEffect(() => {
    if (!isAuthenticated || !isTauri()) return
    ;(async () => {
      try {
        const { isPermissionGranted, requestPermission } = await import(
          '@tauri-apps/plugin-notification'
        )
        if (!(await isPermissionGranted())) {
          await requestPermission()
        }
      } catch {
        // notifications are non-essential — ignore
      }
    })()
  }, [isAuthenticated])

  // Once authenticated, open the always-on remote channel so the backend can
  // push run commands to this desktop. The session token doubles as the WS auth
  // and the started run's bearer; `start_remote_listener` is idempotent (it
  // cancels any prior socket), so re-running on token change is safe. Best
  // effort — a missing token or not-yet-built command never breaks the UI. The
  // listener is a Rust-side socket, so there is nothing to start in a browser:
  // safeInvoke already refuses outside Tauri, and skipping here keeps the
  // no-op out of the console entirely.
  useEffect(() => {
    if (!isAuthenticated || !isTauri()) return
    const token = localStorage.getItem('screen_buddy_session_token')
    if (!token) return
    safeInvoke('start_remote_listener', { token, backend: CU_BACKEND })
    return () => {
      safeInvoke('stop_remote_listener')
    }
  }, [isAuthenticated])

  if (!isAuthenticated) {
    return <SplashLogin login={login} isLoading={isLoading} error={error} />
  }

  return (
    <ModeProvider>
      <ModedShell userEmail={userEmail} onSignOut={logout} />
    </ModeProvider>
  )
}

// The authenticated inside, once a shell mode is settled. Splitting this out of
// App is what lets it consume useMode() — App owns the ModeProvider, so it can't
// read from it itself.
//
// EVERY route is registered in EVERY mode, on purpose. Mode chooses what the nav
// offers and where home points; it is not a permission (the backend authorizes
// each request off the session token regardless), so a shared or bookmarked link
// must keep working after a mode switch instead of bouncing to a 404.
function ModedShell({
  userEmail,
  onSignOut,
}: {
  userEmail: string | null
  onSignOut: () => void
}) {
  const { mode } = useMode()

  // No stored choice and nothing safe to infer — ask, once.
  if (!mode) return <ModePicker />

  const home = homeRouteFor(mode)

  return (
    <ActiveRunProvider>
      {/* Global cron firing engine + fire modal — mounted above the router so
          the modal appears no matter which view is active. Not in admin mode:
          it fires runs on THIS machine, and admin is the one shell that may be
          a phone browser with no executor behind it. */}
      {mode !== 'admin' && <SchedulerHost />}
      <HashRouter>
        <Routes>
          <Route element={<Layout userEmail={userEmail} onSignOut={onSignOut} mode={mode} />}>
            {/* / -> the mode's home. `replace` so it doesn't pollute history. */}
            <Route index element={<Navigate to={home} replace />} />
            <Route path="dashboard" element={<Dashboard />} />
            {/* The launcher. */}
            <Route path="runs" element={<NewRun />} />
            {/* Drilldown: live or replay, decided inside RunDetail. */}
            <Route path="runs/:runId" element={<RunDetail />} />
            {/* Scheduled (future) runs + drilldown. */}
            <Route path="scheduled" element={<Scheduled />} />
            <Route path="scheduled/:id" element={<ScheduleDetail />} />
            <Route path="history" element={<History />} />
            {/* Templates manager — a Runs sub-view alongside New / Scheduled / History. */}
            <Route path="templates" element={<Templates />} />
            {/* Devices — the fleet supervisor, and admin mode's home. Served in
                a plain browser as well as the desktop app, so nothing in it may
                depend on the Tauri bridge. */}
            <Route path="admin" element={<Admin />} />
            <Route path="artifacts" element={<Artifacts />} />
            <Route path="pinned" element={<PinnedLibrary />} />
            <Route path="credentials" element={<Credentials />} />
            <Route path="settings" element={<Settings />} />
            {/* Unknown route -> the mode's home. */}
            <Route path="*" element={<Navigate to={home} replace />} />
          </Route>
        </Routes>
      </HashRouter>
    </ActiveRunProvider>
  )
}

// Drives the cron firing engine (useScheduler) and renders the fire modal for
// the head-of-queue owed occurrence. Rendered once, globally, so the modal
// floats above whichever view is active. require_confirmation defaults true, so
// every owed occurrence surfaces here as a modal.
function SchedulerHost() {
  const { current, accept, snooze, skip, busy } = useScheduler()
  if (!current) return null
  return (
    <ScheduleFireModal
      item={current}
      busy={busy}
      onAccept={accept}
      onSnooze={snooze}
      onSkip={skip}
    />
  )
}

export default App

import { useEffect, useRef } from 'react'
import { HashRouter, Routes, Route, Navigate } from 'react-router-dom'
import { isPermissionGranted, requestPermission } from '@tauri-apps/plugin-notification'
import { useGoogleAuth } from './hooks/useGoogleAuth'
import { ActiveRunProvider } from './activeRun'
import { CU_BACKEND, safeInvoke, reconcileOrphanedRuns } from './lib'
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
import ScheduleDetail from './views/ScheduleDetail'
import ScheduleFireModal from './views/ScheduleFireModal'
import { useScheduler } from './useScheduler'

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

  // Request macOS notification permission once after auth so the Rust-sent
  // run-complete / run-failed notifications can actually display. Best-effort.
  useEffect(() => {
    if (!isAuthenticated) return
    ;(async () => {
      try {
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
  // effort — a missing token or not-yet-built command never breaks the UI.
  useEffect(() => {
    if (!isAuthenticated) return
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
    <ActiveRunProvider>
      {/* Global cron firing engine + fire modal — mounted above the router so
          the modal appears no matter which view is active. */}
      <SchedulerHost />
      <HashRouter>
        <Routes>
          <Route element={<Layout userEmail={userEmail} onSignOut={logout} />}>
            {/* / -> /dashboard. `replace` so it doesn't pollute history. */}
            <Route index element={<Navigate to="/dashboard" replace />} />
            <Route path="dashboard" element={<Dashboard />} />
            {/* The launcher. */}
            <Route path="runs" element={<NewRun />} />
            {/* Drilldown: live or replay, decided inside RunDetail. */}
            <Route path="runs/:runId" element={<RunDetail />} />
            {/* Scheduled (future) runs + drilldown. */}
            <Route path="scheduled" element={<Scheduled />} />
            <Route path="scheduled/:id" element={<ScheduleDetail />} />
            <Route path="history" element={<History />} />
            <Route path="artifacts" element={<Artifacts />} />
            <Route path="pinned" element={<PinnedLibrary />} />
            <Route path="credentials" element={<Credentials />} />
            <Route path="settings" element={<Settings />} />
            {/* Unknown route -> dashboard. */}
            <Route path="*" element={<Navigate to="/dashboard" replace />} />
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

import { useCallback, useEffect, useRef, useState } from 'react'
import { HashRouter, Routes, Route, Navigate } from 'react-router-dom'
import { useGoogleAuth } from './hooks/useGoogleAuth'
import { ActiveRunProvider } from './activeRun'
import { CU_BACKEND, safeInvoke, reconcileOrphanedRuns, isTauri, unenrollMachine } from './lib'
import SplashLogin from './SplashLogin'
import DeviceRevoked from './DeviceRevoked'
import { ConfirmModal } from './ui'
import Layout from './Layout'
import Dashboard from './views/Dashboard'
import NewRun from './views/NewRun'
import History from './views/History'
import RunDetail from './views/RunDetail'
import FleetRun, { FleetRunRedirect } from './views/FleetRun'
import DeviceRuns from './views/DeviceRuns'
import PinnedLibrary from './views/PinnedLibrary'
import Artifacts from './views/Artifacts'
import Credentials from './views/Credentials'
import Settings from './views/Settings'
import Scheduled from './views/Scheduled'
import Templates from './views/Templates'
import ScheduleDetail from './views/ScheduleDetail'
import ScheduleFireModal from './views/ScheduleFireModal'
import Admin from './views/Admin'
import Machine from './views/Machine'
import { useScheduler } from './useScheduler'
import { ModeProvider, homeRouteFor, useCredentialClass, useDeviceRejected, useMode } from './mode'
import ModePicker from './ModePicker'

// App is the auth gate (single source of truth for auth state). It calls
// useGoogleAuth() ONCE. The question it asks is not "authenticated?" but
// "authenticated as WHAT": a Google session is an operator and gets the mode
// picker, a device token is an enrolled worker and goes straight to the worker
// shell, and neither gets the splash. Past the gate it is the hash-routed run
// manager: a HashRouter (so a webview reload restores the route) wrapping the
// NavRail Layout + its child routes, all inside the shared ActiveRunProvider so
// the live-run hint survives navigation.
function App() {
  const { isAuthenticated, userEmail, isLoading, error, login, logout, checkAuth } =
    useGoogleAuth()
  const { credential, refresh: refreshCredential } = useCredentialClass()

  // An enrolled machine is "signed in" without ever having signed in: it holds a
  // device token in the Rust credential store and no Google session at all. Both
  // states get the inside of the app; they differ only in which shell.
  const enrolled = credential === 'device'
  const inside = isAuthenticated || enrolled

  // Only a worker can be told its credential is dead; see useDeviceRejected.
  const { rejected, clear: clearRejection } = useDeviceRejected(enrolled)
  const [confirmUnenrol, setConfirmUnenrol] = useState(false)

  // Sign out has to mean the thing the machine can actually stop being. On an
  // admin machine that is the Google session in localStorage, which is all
  // `logout` has ever cleared — and on an enrolled worker there is no session
  // there at all, so the control used to do nothing whatsoever while the machine
  // stayed in the fleet. Un-enrolling is the equivalent act, and it is one-way
  // (rejoining needs a key only the operator can mint), which is what the
  // confirm is for.
  const signOut = useCallback(async () => {
    if (!enrolled) {
      logout()
      return
    }
    setConfirmUnenrol(true)
  }, [enrolled, logout])

  const confirmUnenrolNow = useCallback(async () => {
    setConfirmUnenrol(false)
    await unenrollMachine()
    clearRejection()
    // Whether or not the clear worked: re-reading is what turns the answer into
    // the right screen, and a token still present simply lands back here.
    refreshCredential()
  }, [clearRejection, refreshCredential])

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
  //
  // Session machines only: reconciliation is a plain fetch, and authHeaders()
  // has nothing to send on an enrolled worker (its device token never leaves
  // Rust). A worker's ghosts have to be reconciled from the Rust side instead.
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
    if (!inside || !isTauri()) return
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
  }, [inside])

  // Once authenticated, open the always-on remote channel so the backend can
  // push run commands to this desktop. The session token doubles as the WS auth
  // and the started run's bearer; `start_remote_listener` is idempotent (it
  // cancels any prior socket), so re-running on token change is safe. Best
  // effort — a missing token or not-yet-built command never breaks the UI. The
  // listener is a Rust-side socket, so there is nothing to start in a browser:
  // safeInvoke already refuses outside Tauri, and skipping here keeps the
  // no-op out of the console entirely.
  useEffect(() => {
    if (!inside || !isTauri()) return
    const token = localStorage.getItem('screen_buddy_session_token')
    if (!enrolled && !token) return
    // An enrolled worker has no session token to hand over: remote.rs opens the
    // socket with whichever credential the machine holds, so passing none is how
    // we say "use your own" rather than handing it an empty bearer.
    safeInvoke('start_remote_listener', enrolled ? { backend: CU_BACKEND } : { token, backend: CU_BACKEND })
    return () => {
      safeInvoke('stop_remote_listener')
    }
  }, [inside, enrolled])

  // The credential class decides which of the three screens below renders, so
  // there is nothing correct to show until it resolves — a splash flashed at a
  // worker for one frame is worse than a frame of nothing.
  if (credential === null) return null

  // A worker the backend has stopped accepting is a state of its own: it is not
  // signed out (it still holds a token) and it is not working (nothing it sends
  // is accepted). Rendering the worker shell over that would show a machine
  // quietly failing every call with no explanation anywhere on screen.
  if (enrolled && rejected) {
    return (
      <DeviceRevoked
        onCredentialChanged={() => {
          clearRejection()
          refreshCredential()
        }}
        onDismiss={clearRejection}
      />
    )
  }

  if (!inside) {
    return (
      <SplashLogin
        login={login}
        isLoading={isLoading}
        error={error}
        onEnrolled={refreshCredential}
      />
    )
  }

  return (
    <ModeProvider credential={credential}>
      {confirmUnenrol && (
        <ConfirmModal
          title="Sign this machine out of the fleet?"
          body={[
            'It stops running agents and leaves the fleet.',
            'Rejoining needs a new enrollment key, which only the operator can mint.',
          ]}
          confirmLabel="Sign out of fleet"
          danger
          onConfirm={confirmUnenrolNow}
          onCancel={() => setConfirmUnenrol(false)}
        />
      )}
      <ModedShell userEmail={userEmail} onSignOut={signOut} />
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
            {/* One machine's runs, and one of its runs — both real pages under
                the machine they belong to, not cards inside the Devices pane.
                The nesting is what lets each of them say whose work it is
                showing and where "back" goes when it is opened cold from a
                bookmark, which history.back() cannot.

                A FLEET run is read entirely over HTTP, and is separate from
                runs/:runId because that view embeds the local agent:// stream
                and resolves screenshots off this machine's disk, neither of
                which exists for a run executed elsewhere. */}
            <Route path="devices/:deviceId/runs" element={<DeviceRuns />} />
            <Route path="devices/:deviceId/runs/:runId" element={<FleetRun />} />
            {/* Where the fleet run view used to live. Resolves the run's machine
                and forwards to the nested address, so links minted before the
                move keep landing on the run. */}
            <Route path="fleet/runs/:runId" element={<FleetRunRedirect />} />
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
            {/* This machine — worker mode's home. Reads only local Tauri
                commands and events, so it is the one fleet screen an enrolled
                worker can render at all (its device token never reaches the
                webview). Registered in every mode: a personal or admin user
                visiting it sees their own machine, which is correct. */}
            <Route path="machine" element={<Machine />} />
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

import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from 'react'
import { listen } from '@tauri-apps/api/event'
import { DEVICE_REJECTED_EVENT, credentialClass, isTauri, type CredentialClass } from './lib'

// The shell the signed-in user gets. This is a VIEW choice and nothing else:
// it decides which routes the nav offers and where "home" points, and it is
// never sent to the backend or consulted before doing anything. Every request
// is authorized server-side off the session token, so picking "admin" here
// grants exactly nothing — a user who switches modes gains no capability they
// didn't already have. Keep it that way: no `if (mode === 'admin')` guard may
// ever stand in for an authorization check.
//
//   admin    — supervises the fleet: which machines are up, what they are
//              running, and how to take one over. Works in a plain browser.
//   worker   — this machine as a fleet node. Sparse: what's running, whether
//              it's connected, whether it has the permissions it needs.
//   consumer — the full desktop ScreenBuddy: launch, history, templates,
//              artifacts, credentials.
export type AppMode = 'admin' | 'worker' | 'consumer'

export const MODES: { id: AppMode; label: string; blurb: string; icon: string }[] = [
  {
    id: 'admin',
    label: 'Admin',
    blurb: 'Supervise the fleet: which machines are up, what each is running, and remote access to any of them.',
    icon: '✓',
  },
  {
    id: 'worker',
    label: 'Worker',
    blurb: 'This machine runs agents for the fleet. Shows what it is doing and whether it is ready.',
    icon: '⛭',
  },
  {
    id: 'consumer',
    label: 'Personal',
    blurb: 'The full ScreenBuddy desktop: launch runs, browse history, manage templates and credentials.',
    icon: '✦',
  },
]

const STORAGE_KEY = 'screen_buddy_mode'

function isMode(v: unknown): v is AppMode {
  return v === 'admin' || v === 'worker' || v === 'consumer'
}

// The remembered choice, or null when the user has never picked one.
export function loadMode(): AppMode | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    return isMode(raw) ? raw : null
  } catch {
    // Private-mode browsers throw on localStorage access; asking again beats
    // failing to render a shell at all.
    return null
  }
}

// The mode to assume when nothing is stored, or null to ask. Only one case is
// unambiguous: a phone-width BROWSER can't run worker or consumer at all (both
// assume a desktop window and the Tauri command bridge), so skip the picker and
// go straight to the one shell that works there.
export function inferMode(): AppMode | null {
  if (!isTauri() && typeof window !== 'undefined' && window.innerWidth <= 560) return 'admin'
  return null
}

// Where each mode lands, and what "unknown route" falls back to. Worker gets
// /machine rather than the generic Dashboard: the Dashboard reads the backend
// over `authHeaders()`, which an enrolled machine has nothing to put in, so it
// was landing every worker in the fleet on a screen that could only ever be
// empty.
export function homeRouteFor(mode: AppMode): string {
  if (mode === 'admin') return '/admin'
  if (mode === 'worker') return '/machine'
  return '/dashboard'
}

interface ModeContextValue {
  mode: AppMode | null
  setMode: (mode: AppMode) => void
  /**
   * True when the mode was decided by the credential rather than by the user,
   * so the picker and the Settings switcher stay out of the way. Not a guard:
   * the backend refuses a device token on admin routes whether or not this
   * renders a button.
   */
  locked: boolean
}

const ModeContext = createContext<ModeContextValue | null>(null)

// Holds the chosen shell. `mode === null` means "not chosen yet" — App renders
// the picker for that, rather than guessing on the user's behalf.
//
// A DEVICE credential removes the choice. An enrolled machine holds a worker
// token and nothing else, so admin and personal are shells with no credential
// behind them: offering them would only produce a screenful of 403s. A session
// credential behaves exactly as it always has.
export function ModeProvider({
  credential,
  children,
}: {
  credential: CredentialClass
  children: ReactNode
}) {
  const locked = credential === 'device'
  const [mode, setModeState] = useState<AppMode | null>(() =>
    locked ? 'worker' : (loadMode() ?? inferMode()),
  )

  // Persist whatever we end up on — including an inferred mode, so the phone
  // browser case is asked zero times rather than decided fresh on every load.
  // A locked mode is deliberately NOT persisted: it is re-derived from the
  // credential on every launch anyway, and writing it would silently overwrite
  // the choice this machine had before it was enrolled.
  useEffect(() => {
    if (!mode || locked) return
    try {
      localStorage.setItem(STORAGE_KEY, mode)
    } catch {
      // Non-persistent storage just means we ask again next launch.
    }
  }, [mode, locked])

  const setMode = useCallback(
    (next: AppMode) => {
      if (locked) return
      setModeState(next)
    },
    [locked],
  )

  return <ModeContext.Provider value={{ mode, setMode, locked }}>{children}</ModeContext.Provider>
}

// This machine's credential class, resolved once at startup. `null` while the
// answer is still outstanding — which shell renders depends on it, so showing
// the Google splash first and swapping it for the worker shell a beat later
// would flash the wrong screen at every worker in the fleet.
//
// `refresh` is for the one moment the answer changes under us: a successful
// enrollment turns a 'none' machine into a 'device' one without a reload.
export function useCredentialClass(): {
  credential: CredentialClass | null
  refresh: () => void
} {
  const [credential, setCredential] = useState<CredentialClass | null>(null)
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    let alive = true
    credentialClass().then((cls) => {
      if (alive) setCredential(cls)
    })
    return () => {
      alive = false
    }
  }, [attempt])

  const refresh = useCallback(() => setAttempt((n) => n + 1), [])
  return { credential, refresh }
}

// Whether the backend has refused this machine's device token since launch —
// the enrollment was revoked (or the device forgotten) and every authenticated
// call this machine makes from here on will fail the same way. Rust says so once
// and takes no action; this is where a worker gets to hear it.
//
// `active` gates the SUBSCRIPTION, not the render, because the event only means
// this on a worker: a 401 on an admin machine is an expired Google session, and
// the sign-in flow already handles that one.
//
// `clear` exists for the far side of the recovery — a machine that re-enrols has
// a live token again, and the notice must not outlive the problem it describes.
export function useDeviceRejected(active: boolean): { rejected: boolean; clear: () => void } {
  const [rejected, setRejected] = useState(false)

  useEffect(() => {
    // Nothing emits this in a browser tab (the same bundle serves the admin
    // panel), and `listen` rejects there — the same reason NavRail's remote
    // indicator stays dim rather than subscribing.
    if (!active || !isTauri()) return
    const unlisten = listen(DEVICE_REJECTED_EVENT, () => setRejected(true))
    return () => {
      unlisten.then((un) => un())
    }
  }, [active])

  const clear = useCallback(() => setRejected(false), [])
  return { rejected, clear }
}

export function useMode(): ModeContextValue {
  const ctx = useContext(ModeContext)
  if (!ctx) {
    throw new Error('useMode must be used within a ModeProvider')
  }
  return ctx
}

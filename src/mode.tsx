import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from 'react'
import { isTauri } from './lib'

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

// Where each mode lands, and what "unknown route" falls back to.
export function homeRouteFor(mode: AppMode): string {
  return mode === 'admin' ? '/admin' : '/dashboard'
}

interface ModeContextValue {
  mode: AppMode | null
  setMode: (mode: AppMode) => void
}

const ModeContext = createContext<ModeContextValue | null>(null)

// Holds the chosen shell. `mode === null` means "not chosen yet" — App renders
// the picker for that, rather than guessing on the user's behalf.
export function ModeProvider({ children }: { children: ReactNode }) {
  const [mode, setModeState] = useState<AppMode | null>(() => loadMode() ?? inferMode())

  // Persist whatever we end up on — including an inferred mode, so the phone
  // browser case is asked zero times rather than decided fresh on every load.
  useEffect(() => {
    if (!mode) return
    try {
      localStorage.setItem(STORAGE_KEY, mode)
    } catch {
      // Non-persistent storage just means we ask again next launch.
    }
  }, [mode])

  const setMode = useCallback((next: AppMode) => setModeState(next), [])

  return <ModeContext.Provider value={{ mode, setMode }}>{children}</ModeContext.Provider>
}

export function useMode(): ModeContextValue {
  const ctx = useContext(ModeContext)
  if (!ctx) {
    throw new Error('useMode must be used within a ModeProvider')
  }
  return ctx
}

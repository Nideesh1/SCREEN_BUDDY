import { createContext, useContext, useState, type ReactNode } from 'react'

// The single live-run hint shared across routed views. Status is coarse-grained;
// the detail/panel views own the fine-grained streaming state. Previously this
// lived as a useState in Shell and was only ever set to 'running' (it never
// flipped to 'done'/'error' because AgentRunPanel's onStatus wasn't wired back
// up). Lifting it into context lets RunDetail update it from the panel, so the
// Dashboard live card and the live-vs-replay decision stay correct.
export type ActiveRun =
  | { id: string; status: 'idle' | 'running' | 'done' | 'error' }
  | null

interface ActiveRunContextValue {
  activeRun: ActiveRun
  setActiveRun: (run: ActiveRun) => void
}

const ActiveRunContext = createContext<ActiveRunContextValue | null>(null)

export function ActiveRunProvider({ children }: { children: ReactNode }) {
  const [activeRun, setActiveRun] = useState<ActiveRun>(null)
  return (
    <ActiveRunContext.Provider value={{ activeRun, setActiveRun }}>
      {children}
    </ActiveRunContext.Provider>
  )
}

export function useActiveRun(): ActiveRunContextValue {
  const ctx = useContext(ActiveRunContext)
  if (!ctx) {
    throw new Error('useActiveRun must be used within an ActiveRunProvider')
  }
  return ctx
}

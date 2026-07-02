import { useState, useEffect, useRef, useCallback, type ReactNode } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { safeInvoke } from './lib'
import { Card, SectionTitle, StatusPill, Chip, Button, EmptyState, ActionChip } from './ui'

// Event payload shapes — must match how src-tauri/src/agent.rs emits them.
//   agent://turn       -> { turn: number }
//   agent://text       -> { delta: string }
//   agent://action     -> { name: string, input: unknown }
//   agent://screenshot -> { jpeg_base64, sent_w, sent_h, screen_w, screen_h }
//   agent://done       -> { reason: string, turns?: number }
//   agent://error      -> { error: string }
interface TurnPayload { turn: number }
interface TextPayload { delta: string }
interface ActionPayload { name: string; input: unknown }
interface ScreenshotPayload {
  jpeg_base64: string
  sent_w: number
  sent_h: number
  screen_w: number
  screen_h: number
}
interface DonePayload { reason: string; turns?: number }
interface ErrorPayload { error: string }

type AgentStatus = 'idle' | 'running' | 'done' | 'error'

interface LogLine {
  id: number
  kind: 'turn' | 'text' | 'action' | 'done' | 'error' | 'info'
  text: string
  ts: number
  // For 'action' lines: the structured name/input so the timeline can render a
  // clean ActionChip (icon + label) instead of raw JSON. Presentational only —
  // the legacy `text` is still populated so behavior is unchanged.
  name?: string
  input?: unknown
}

// Format an epoch-ms timestamp as a muted HH:MM:SS clock prefix.
function clock(ts: number): string {
  const d = new Date(ts)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}

interface PinnedSet {
  id: string
  name: string
  count: number
}

interface AgentRunPanelProps {
  // Optional seed task (e.g. from the New Run form). The user can still edit it
  // in the task box before running.
  initialPrompt?: string
  // Optional pinned reference set attached to this run. Sent to the agent as a
  // token-cache-optimized prefix (billed once per run, not every turn).
  pinnedSetId?: string
  // Pre-minted run id (created by the launcher via `POST /runs`). Threaded into
  // start_agent_task so the Rust side stamps THIS row instead of creating one.
  runId?: string
  // Surfaces lifecycle transitions to a host (e.g. the run-detail view): running
  // on start, done on agent://done, error on agent://error/start failure.
  onStatus?: (status: 'idle' | 'running' | 'done' | 'error', runId?: string) => void
  // ATTACHED (live) mode: the run was already started elsewhere (the launcher).
  // Hide the idle task box + Run button and start in the "running" state so this
  // panel only mirrors the in-flight agent:// stream (and can Stop it).
  attached?: boolean
}

function AgentRunPanel({ initialPrompt = '', pinnedSetId, runId, onStatus, attached = false }: AgentRunPanelProps) {
  const [prompt, setPrompt] = useState(initialPrompt)
  const [status, setStatus] = useState<AgentStatus>(attached ? 'running' : 'idle')
  const [log, setLog] = useState<LogLine[]>([])
  const [screenshot, setScreenshot] = useState<string | null>(null)
  const [pinnedSet, setPinnedSet] = useState<PinnedSet | null>(null)

  // Resolve the attached set's name/count for the chip.
  useEffect(() => {
    if (!pinnedSetId) {
      setPinnedSet(null)
      return
    }
    let active = true
    safeInvoke<PinnedSet[]>('pinned_list').then((res) => {
      if (!active) return
      setPinnedSet(res.ok ? res.data?.find((s) => s.id === pinnedSetId) ?? null : null)
    })
    return () => {
      active = false
    }
  }, [pinnedSetId])

  // Keep the latest onStatus/runId in refs so the (one-shot) event subscription
  // effect always calls the current handler without re-subscribing.
  const onStatusRef = useRef(onStatus)
  onStatusRef.current = onStatus
  const runIdRef = useRef(runId)
  runIdRef.current = runId

  const logRef = useRef<HTMLDivElement>(null)
  const lineId = useRef(0)
  // Track whether the last appended line was an agent://text delta so streamed
  // text accumulates on one line instead of spamming a line per token.
  const textLineId = useRef<number | null>(null)

  // Run-start clock (set at mount, reset on handleRun) and per-turn timing so
  // the log can show how long each turn took and the total run duration.
  const runStart = useRef(Date.now())
  const firstTurnTs = useRef<number | null>(null)
  const lastTurnTs = useRef<number | null>(null)

  const append = useCallback((kind: LogLine['kind'], text: string, extra?: Partial<LogLine>) => {
    lineId.current += 1
    setLog((prev) => [...prev, { id: lineId.current, kind, text, ts: Date.now(), ...extra }])
  }, [])

  // Subscribe to all agent events for the lifetime of the component.
  useEffect(() => {
    let active = true
    const unlisteners: UnlistenFn[] = []

    const subscribe = async () => {
      const reg = async <T,>(event: string, handler: (payload: T) => void) => {
        const un = await listen<T>(event, (e) => handler(e.payload))
        if (active) unlisteners.push(un)
        else un()
      }

      await reg<TurnPayload>('agent://turn', (p) => {
        textLineId.current = null
        const now = Date.now()
        // First turn: elapsed since run start. Later turns: delta from the
        // previous turn so the user can see where time goes.
        const prevTs = lastTurnTs.current ?? runStart.current
        if (firstTurnTs.current === null) firstTurnTs.current = now
        lastTurnTs.current = now
        const delta = ((now - prevTs) / 1000).toFixed(1)
        append('turn', `--- turn ${p.turn} --- (+${delta}s)`)
      })

      await reg<TextPayload>('agent://text', (p) => {
        // Accumulate streamed deltas onto a single "text" line.
        setLog((prev) => {
          if (textLineId.current !== null) {
            return prev.map((l) =>
              l.id === textLineId.current ? { ...l, text: l.text + p.delta } : l,
            )
          }
          lineId.current += 1
          textLineId.current = lineId.current
          return [...prev, { id: lineId.current, kind: 'text', text: p.delta, ts: Date.now() }]
        })
      })

      await reg<ActionPayload>('agent://action', (p) => {
        textLineId.current = null
        let inputStr = ''
        try {
          inputStr = JSON.stringify(p.input)
        } catch {
          inputStr = String(p.input)
        }
        append('action', `→ ${p.name}(${inputStr})`, { name: p.name, input: p.input })
      })

      await reg<ScreenshotPayload>('agent://screenshot', (p) => {
        textLineId.current = null
        setScreenshot(`data:image/jpeg;base64,${p.jpeg_base64}`)
        append('info', `📸 screenshot ${p.sent_w}×${p.sent_h} (screen ${p.screen_w}×${p.screen_h})`)
      })

      await reg<DonePayload>('agent://done', (p) => {
        textLineId.current = null
        setStatus('done')
        onStatusRef.current?.('done', runIdRef.current)
        const total = ((Date.now() - (firstTurnTs.current ?? runStart.current)) / 1000).toFixed(1)
        append('done', `✓ done (${p.reason}${p.turns ? `, ${p.turns} turns` : ''}) — total ${total}s`)
        // Native completion notification is sent from Rust (agent.rs) so it
        // fires even when this panel isn't mounted / the app is backgrounded.
      })

      await reg<ErrorPayload>('agent://error', (p) => {
        textLineId.current = null
        setStatus('error')
        onStatusRef.current?.('error', runIdRef.current)
        append('error', `✗ error: ${p.error}`)
        // Native failure notification is sent from Rust (agent.rs).
      })
    }

    subscribe()

    return () => {
      active = false
      for (const un of unlisteners) un()
    }
  }, [append])

  // Auto-scroll the log.
  useEffect(() => {
    if (logRef.current) {
      logRef.current.scrollTop = logRef.current.scrollHeight
    }
  }, [log])

  const handleRun = async () => {
    if (!prompt.trim() || status === 'running') return
    setLog([])
    setScreenshot(null)
    textLineId.current = null
    runStart.current = Date.now()
    firstTurnTs.current = null
    lastTurnTs.current = null
    setStatus('running')
    onStatus?.('running', runId)
    try {
      await invoke('start_agent_task', {
        prompt,
        auth: localStorage.getItem('screen_buddy_session_token') ?? undefined,
        pinnedSetId,
        runId,
      })
    } catch (err) {
      setStatus('error')
      onStatus?.('error', runId)
      append('error', `✗ failed to start: ${String(err)}`)
    }
  }

  const handleStop = async () => {
    try {
      await invoke('stop_agent_task')
      append('info', 'stop requested')
    } catch (err) {
      append('error', `✗ stop failed: ${String(err)}`)
    }
  }

  // Live timing readout. Recomputed each render (the log stream drives frequent
  // re-renders while running); freezes naturally once the stream stops.
  const elapsed = ((Date.now() - runStart.current) / 1000).toFixed(1)
  const turnCount = log.reduce((n, l) => (l.kind === 'turn' ? n + 1 : n), 0)
  const isRunning = status === 'running'

  return (
    <div
      style={{
        padding: 'var(--sp-4)',
        gap: 'var(--sp-3)',
        display: 'flex',
        flexDirection: 'column',
        height: '100%',
        boxSizing: 'border-box',
        fontFamily: 'var(--font-sans)',
      }}
    >
      {/* ── Status header ── */}
      <Card padded={false} style={{ flexShrink: 0 }}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-3)',
            flexWrap: 'wrap',
            padding: '12px 16px',
          }}
        >
          <StatusPill status={status} />

          {/* per-turn / total timing readout (mono values) */}
          <div
            style={{
              display: 'inline-flex',
              alignItems: 'baseline',
              gap: 6,
              fontSize: 'var(--fs-sm)',
              color: 'var(--sb-text-muted)',
            }}
          >
            <span style={{ color: 'var(--sb-text-faint)' }}>⏱</span>
            <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--sb-text)' }}>{elapsed}s</span>
            <span style={{ color: 'var(--sb-text-faint)' }}>·</span>
            <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--sb-text)' }}>{turnCount}</span>
            <span>{turnCount === 1 ? 'turn' : 'turns'}</span>
          </div>

          {pinnedSet && (
            <Chip tone="gold" title={`Reference set "${pinnedSet.name}" attached`}>
              📌 {pinnedSet.name} · {pinnedSet.count} {pinnedSet.count === 1 ? 'file' : 'files'}
            </Chip>
          )}

          <div style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>
            {!attached && (
              <Button variant="primary" onClick={handleRun} disabled={isRunning || !prompt.trim()}>
                ▶ Run
              </Button>
            )}
            <Button variant="danger" onClick={handleStop} disabled={!isRunning}>
              ■ Stop
            </Button>
          </div>
        </div>

        {/* idle task box (non-attached only) */}
        {!attached && (
          <div style={{ padding: '0 16px 16px' }}>
            <textarea
              className="agent-input"
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              placeholder="Describe the task for ScreenBuddy to perform on this Mac…"
              rows={3}
              style={{
                width: '100%',
                boxSizing: 'border-box',
                resize: 'vertical',
                padding: 'var(--sp-3)',
                fontFamily: 'var(--font-sans)',
                fontSize: 'var(--fs-base)',
                lineHeight: 1.5,
                background: 'var(--sb-surface-3)',
                color: 'var(--sb-text)',
                border: '1px solid var(--sb-border)',
                borderRadius: 'var(--r-sm)',
              }}
            />
          </div>
        )}
      </Card>

      {/* ── Timeline + screenshot ── */}
      <div style={{ display: 'flex', gap: 'var(--sp-3)', flex: 1, minHeight: 0 }}>
        {/* Streaming timeline */}
        <Card
          padded={false}
          title="Live timeline"
          style={{ flex: 1.4, minWidth: 0, display: 'flex', flexDirection: 'column' }}
        >
          <div
            ref={logRef}
            className="agent-log"
            style={{
              flex: 1,
              minHeight: 0,
              overflowY: 'auto',
              background: 'var(--sb-surface-2)',
              padding: 'var(--sp-3)',
              display: 'flex',
              flexDirection: 'column',
              gap: 6,
            }}
          >
            {log.length === 0 ? (
              <span style={{ color: 'var(--sb-text-faint)', fontSize: 'var(--fs-md)' }}>
                Waiting for the agent stream…
              </span>
            ) : (
              log.map((l) => <TimelineRow key={l.id} line={l} />)
            )}
          </div>
        </Card>

        {/* Latest screenshot */}
        <Card
          padded={false}
          title="Latest screenshot"
          style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column' }}
        >
          <div
            style={{
              flex: 1,
              minHeight: 0,
              display: 'flex',
              alignItems: 'flex-start',
              justifyContent: 'center',
              background: 'var(--sb-surface-2)',
              padding: 'var(--sp-3)',
              overflow: 'auto',
            }}
          >
            {screenshot ? (
              <img
                src={screenshot}
                alt="latest screenshot"
                style={{
                  maxWidth: '100%',
                  height: 'auto',
                  borderRadius: 'var(--r-md)',
                  border: '1px solid var(--sb-border-gold)',
                  boxShadow: 'var(--shadow-2)',
                }}
              />
            ) : (
              <div style={{ margin: 'auto' }}>
                <EmptyState icon="🖥" title="No screenshot yet" hint="Frames appear here as the agent captures the screen." />
              </div>
            )}
          </div>
        </Card>
      </div>
    </div>
  )
}

// A single timeline row: a faint mono HH:MM:SS prefix followed by kind-specific
// content (turn divider, sans text paragraph, ActionChip, screenshot marker, or
// done/error pill). Pure presentation over an already-built LogLine.
function TimelineRow({ line }: { line: LogLine }) {
  const time = (
    <span
      style={{
        fontFamily: 'var(--font-mono)',
        fontSize: 'var(--fs-xs)',
        color: 'var(--sb-text-faint)',
        flexShrink: 0,
        paddingTop: 2,
      }}
    >
      {clock(line.ts)}
    </span>
  )

  if (line.kind === 'turn') {
    // Strip the "--- turn N --- (+Xs)" wrapping into a clean label + delta.
    const m = line.text.match(/turn\s+(\d+).*?(\(\+[\d.]+s\))?$/i)
    const label = m ? `Turn ${m[1]}` : line.text
    const delta = m && m[2] ? m[2].replace(/[()]/g, '') : ''
    return (
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', margin: '6px 0 2px' }}>
        {time}
        <span style={{ height: 1, flex: 1, background: 'var(--sb-border-gold)' }} />
        <SectionTitle style={{ color: 'var(--sb-gold-bright)' }}>{label}</SectionTitle>
        {delta && (
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-xs)', color: 'var(--sb-text-faint)' }}>
            {delta}
          </span>
        )}
        <span style={{ height: 1, flex: 1, background: 'var(--sb-border-gold)' }} />
      </div>
    )
  }

  let content: ReactNode
  if (line.kind === 'action' && line.name) {
    content = <ActionChip name={line.name} input={line.input} />
  } else if (line.kind === 'text') {
    content = (
      <span style={{ color: 'var(--sb-text)', fontSize: 'var(--fs-md)', lineHeight: 1.55, whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}>
        {line.text}
      </span>
    )
  } else if (line.kind === 'done') {
    content = <StatusPill status="done" label={line.text.replace(/^✓\s*/, '')} />
  } else if (line.kind === 'error') {
    content = <StatusPill status="error" label={line.text.replace(/^✗\s*/, '')} />
  } else {
    // 'info' (screenshot marker, stop requested, …)
    content = (
      <span style={{ color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)', whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}>
        {line.text}
      </span>
    )
  }

  return (
    <div style={{ display: 'flex', alignItems: 'flex-start', gap: 'var(--sp-2)' }}>
      {time}
      <div style={{ minWidth: 0, flex: 1 }}>{content}</div>
    </div>
  )
}

export default AgentRunPanel

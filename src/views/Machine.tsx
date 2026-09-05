import { useCallback, useEffect, useState } from 'react'
import { convertFileSrc } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { CU_BACKEND, isTauri, safeInvoke, type CredentialClass } from '../lib'
import { Badge, Button, Card, EmptyState, SectionTitle, Spinner, StatusPill } from '../ui'
import { PermissionsCard } from './Settings'

// THIS MACHINE — worker mode's home, and the one screen a fleet node has.
//
// Everything here is read from THIS machine: Tauri commands and Tauri events,
// never the backend. That is not a style preference. An enrolled worker's
// credential is a device token held in the Rust credential store, so it never
// reaches the webview and `authHeaders()` has nothing to send — a `fetch` from
// here would go out unauthenticated. `GET /devices` refuses a device token
// outright besides, by design: a machine that has been compromised must not be
// able to read back the fleet's names and RustDesk ids. So if something on this
// screen wants a backend read, the answer is a new Rust command, not a fetch.
//
// The route is registered in every mode (mode picks the nav and the home route,
// never which routes exist), and it reads correctly on a personal or admin
// install too — it is simply that machine's own facts.

// What `device_info` answers with (src-tauri/src/device.rs `DeviceInfo`).
interface DeviceInfo {
  device_id: string
  hostname: string
  /** "macos" | "windows" | "linux" — `std::env::consts::OS` verbatim. */
  os: string
  os_version: string
  app_version: string
}

// Where this machine's model calls go (src-tauri/src/agent.rs `ModelEndpoint`).
// Same shape the launcher's onboarding reads.
interface ModelEndpoint {
  base: string
  is_anthropic: boolean
  model: string
  /** Who chose it — see `agent.rs::EndpointSource`. */
  source: 'fleet' | 'env' | 'default'
  /** The guard would refuse the next dispatched run. */
  blocked: boolean
}

// How long "Reconnecting…" stays up when nothing on the socket answers. The
// listener's own connect attempt is bounded by the TCP/TLS handshake, not by us,
// so this is only a floor for the button's busy state: it must not spin forever
// on a machine whose backend is a black hole, and it must not clear so fast that
// the click looks like it did nothing.
const RECONNECT_BUSY_MS = 6000

function Machine() {
  // Bumping this re-runs every card's read effect. A nonce rather than a set of
  // callbacks because the cards are independent and each already knows how to
  // read itself once — the button's job is to say "again", not to re-implement
  // five reads at the top level.
  const [nonce, setNonce] = useState(0)
  const [reconnecting, setReconnecting] = useState(false)
  const [reconnectError, setReconnectError] = useState<string | null>(null)

  // While a reconnect is in flight, ANY link state change is the answer — clear
  // the busy state on the real `remote://status` event rather than on the invoke
  // returning, which only means the Rust task spawned.
  useEffect(() => {
    if (!isTauri()) return
    let alive = true
    const unlisten = listen('remote://status', () => {
      if (alive) setReconnecting(false)
    })
    return () => {
      alive = false
      unlisten.then((un) => un())
    }
  }, [])

  // REFRESH — re-read the screen, and force the command channel to retry now.
  //
  // The reads are all cheap local commands. The reconnect is the part that
  // earns the button: the listener backs off up to 30s between attempts, so a
  // machine whose network just came back sits unreachable for up to half a
  // minute with nothing to do about it but restart the app — over a laggy
  // remote-desktop session, from someone who opened this screen precisely
  // because the machine was not taking work.
  //
  // Safe against a live run: `start_remote_listener` cancels and respawns only
  // the socket task (RemoteState). A run's cancellation token lives in a
  // separate AgentState and `run_agent` is its own spawned task holding a
  // RunLease, so dropping the socket neither cancels nor orphans it — the run
  // keeps streaming and keeps persisting through `backend`, which is a different
  // server from the model endpoint anyway.
  const refresh = useCallback(async () => {
    setReconnectError(null)
    setNonce((n) => n + 1)
    if (!isTauri()) return

    setReconnecting(true)
    // Mirror App.tsx exactly. An enrolled worker has NO session token to hand
    // over — its credential lives in the Rust store — so it passes `backend`
    // alone and lets remote.rs pick. Passing `token: null` here instead is what
    // once left an enrolled Windows machine silently unreachable.
    const hasSession = !!localStorage.getItem('screen_buddy_session_token')
    const cls = await safeInvoke<CredentialClass>('credential_class', { hasSession })
    const enrolled = cls.ok && cls.data === 'device'
    const token = localStorage.getItem('screen_buddy_session_token')
    const res = await safeInvoke(
      'start_remote_listener',
      enrolled ? { backend: CU_BACKEND } : { token, backend: CU_BACKEND },
    )
    if (!res.ok) {
      setReconnecting(false)
      setReconnectError(res.error)
      return
    }
    // The socket may connect, fail, or hang. Only the event above can say which,
    // so this is purely a ceiling on the spinner.
    setTimeout(() => setReconnecting(false), RECONNECT_BUSY_MS)
  }, [])

  return (
    <div
      style={{
        padding: 'var(--sp-5)',
        maxWidth: 'var(--page-max-narrow)',
        margin: '0 auto',
        display: 'flex',
        flexDirection: 'column',
        gap: 'var(--sp-4)',
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
        <h1
          style={{
            margin: 0,
            fontSize: 'var(--fs-2xl)',
            fontWeight: 700,
            color: 'var(--sb-gold-bright)',
          }}
        >
          This machine
        </h1>
        <div style={{ marginLeft: 'auto' }}>
          <Button variant="secondary" size="sm" onClick={refresh} disabled={reconnecting}>
            {reconnecting ? 'Reconnecting…' : '↻ Refresh'}
          </Button>
        </div>
      </div>

      {reconnectError && (
        <p style={{ margin: 0, fontSize: 'var(--fs-md)', color: 'var(--sb-danger-bright)' }}>
          Could not restart the command channel. {reconnectError}
        </p>
      )}

      <IdentityCard refreshKey={nonce} />
      <LinkCard refreshKey={nonce} />
      <NowCard />
      <LocalRunsCard refreshKey={nonce} />
      {/* Readiness. The same card Settings shows, deliberately not a variant of
          it: a worker missing Screen Recording looks exactly like a worker with
          nothing to do, and nobody is sitting at it to tell the difference. */}
      <PermissionsCard refreshKey={nonce} />
      <ModelEndpointCard refreshKey={nonce} />
      <UiaProbeCard />
      <EnrollmentCard refreshKey={nonce} />
    </div>
  )
}

// ───────────────────────────────────────────────────────── identity

// IDENTITY — which machine this is, in the same words the admin's device list
// uses for it, so a name read here and a row read there are recognisably the
// same box.
function IdentityCard({ refreshKey }: { refreshKey: number }) {
  const [info, setInfo] = useState<DeviceInfo | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let active = true
    safeInvoke<DeviceInfo>('device_info').then((res) => {
      if (!active) return
      if (res.ok) setInfo(res.data)
      else setError(res.error)
    })
    return () => {
      active = false
    }
  }, [refreshKey])

  return (
    <>
      <Card>
        {!info && !error && (
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              color: 'var(--sb-text-muted)',
              fontSize: 'var(--fs-md)',
            }}
          >
            <Spinner size={14} /> Reading this machine…
          </div>
        )}

        {/* Outside the Tauri webview there is no machine to describe — the same
            bundle serves the admin panel in a plain browser, where every command
            on this screen is unavailable. Say which machine the answer would be
            about rather than rendering six empty cards. */}
        {error && (
          <p style={{ margin: 0, fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
            {isTauri()
              ? `Could not read this machine's details. ${error}`
              : 'Open ScreenBuddy on the machine itself — this page reads facts only the desktop app can see.'}
          </p>
        )}

        {info && (
          <>
            <div style={{ fontSize: 'var(--fs-lg)', fontWeight: 600, color: 'var(--sb-text)' }}>
              {info.hostname}
            </div>
            <div
              style={{
                marginTop: 'var(--sp-2)',
                fontSize: 'var(--fs-md)',
                color: 'var(--sb-text-muted)',
              }}
            >
              {prettyOs(info.os)} {info.os_version} · ScreenBuddy v{info.app_version}
            </div>
            {/* The device id is what an operator matches this machine to a row
                in the fleet list by, and it is the only handle they have when a
                machine's name has not been filled in. */}
            <div style={{ marginTop: 'var(--sp-3)' }}>
              <Badge mono title={info.device_id}>
                {info.device_id}
              </Badge>
            </div>
          </>
        )}
      </Card>
    </>
  )
}

// `std::env::consts::OS` is lowercase and unbranded; these three are the builds
// that exist. Anything else is shown as Rust reported it rather than guessed at.
function prettyOs(os: string): string {
  if (os === 'macos') return 'macOS'
  if (os === 'windows') return 'Windows'
  if (os === 'linux') return 'Linux'
  return os
}

// ───────────────────────────────────────────────────────── link

// LINK — whether the always-on channel to the backend is up. This is the single
// most load-bearing line on the screen: work reaches a worker by being pushed
// down that socket, so a disconnected machine is not idle by choice, it is
// unreachable, and no amount of staring at the Now card below will say so.
//
// Read once on mount and then followed by `remote://status`, which Rust emits on
// every connect and drop. Both are needed: the event alone leaves this reading
// "checking" until the link next CHANGES, and a machine that has been happily
// connected for an hour is precisely the case that never fires one. The state
// stays null until the first answer arrives rather than defaulting to either —
// claiming "not connected" about a machine that is fine sends someone to the
// wrong box.
function LinkCard({ refreshKey }: { refreshKey: number }) {
  const [connected, setConnected] = useState<boolean | null>(null)

  useEffect(() => {
    if (!isTauri()) return
    let alive = true
    // A refresh drops back to "Checking…" first. Holding the previous answer
    // while re-reading would let the card say "Connected" about a socket that is
    // at that moment being torn down and rebuilt.
    if (refreshKey > 0) setConnected(null)
    // Subscribe BEFORE the read, so a change landing between the two is not
    // missed — an event arriving first simply gets overwritten by a read of the
    // same value, which is harmless.
    const unlisten = listen<{ connected: boolean }>('remote://status', (e) => {
      if (alive) setConnected(!!e.payload?.connected)
    })
    safeInvoke<boolean>('remote_status').then((res) => {
      // Only seed: a live event that already answered outranks this.
      if (alive && res.ok) setConnected((prev) => (prev === null ? res.data : prev))
    })
    return () => {
      alive = false
      unlisten.then((un) => un())
    }
  }, [refreshKey])

  return (
    <Card title={<SectionTitle>Link</SectionTitle>}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
        <span
          aria-hidden
          style={{
            flexShrink: 0,
            boxSizing: 'border-box',
            width: 10,
            height: 10,
            borderRadius: '50%',
            background:
              connected === null
                ? 'transparent'
                : connected
                  ? 'var(--sb-success)'
                  : 'var(--sb-text-faint)',
            border: connected === null ? '1.5px solid var(--sb-text-muted)' : undefined,
          }}
        />
        <span style={{ fontSize: 'var(--fs-base)', color: 'var(--sb-text)' }}>
          {connected === null ? 'Checking…' : connected ? 'Connected' : 'Not connected'}
        </span>
      </div>
      <p
        style={{
          margin: 'var(--sp-3) 0 0',
          fontSize: 'var(--fs-md)',
          lineHeight: 1.5,
          color: 'var(--sb-text-muted)',
        }}
      >
        {connected
          ? 'The fleet can push work to this machine.'
          : 'Runs reach this machine over this channel. While it is down nothing can be dispatched here, however healthy the rest of the screen looks. It retries on its own.'}
      </p>
    </Card>
  )
}

// ───────────────────────────────────────────────────────── now

// The live run as this machine can see it. Assembled from the `agent://*` stream
// rather than polled from the backend — the events are the same ones the run
// panel already renders, they arrive as they happen, and they are the only
// account of the run a worker has.
//
// It keeps the LAST run after it ends instead of clearing to empty. Whoever
// opens this screen has usually opened it because something looked wrong, and
// arriving to a blank Now card cannot distinguish "nothing has run today" from
// "the run failed a minute ago".
type MachineRun = {
  runId: string | null
  status: 'running' | 'done' | 'error'
  startedAt: number
  endedAt: number | null
  /** Turns seen so far. The agent's own step counter, 0 before the first turn. */
  step: number
  /** What the machine was asked to do. Null when the panel was opened mid-run. */
  task: string | null
  /** The last tool call, so the card says what it is doing and not only that it is. */
  lastAction: string | null
  /** Why it stopped — the `agent://done` reason or the `agent://error` message. */
  outcome: string | null
}

function useMachineRun(): MachineRun | null {
  const [run, setRun] = useState<MachineRun | null>(null)

  useEffect(() => {
    if (!isTauri()) return
    let active = true
    const unlisteners: UnlistenFn[] = []

    const subscribe = async () => {
      const reg = async <T,>(event: string, handler: (payload: T) => void) => {
        const un = await listen<T>(event, (e) => handler(e.payload))
        if (active) unlisteners.push(un)
        else un()
      }

      // A run starting replaces whatever the previous one left behind.
      await reg<{ run_id: string; task?: string }>('agent://run_started', (p) => {
        setRun({
          runId: p.run_id,
          status: 'running',
          startedAt: Date.now(),
          endedAt: null,
          step: 0,
          // The task rides on this event because a worker cannot ask the backend
          // for it — its device token never leaves Rust. A run already in flight
          // when this panel opened has no task to show; the card falls back to
          // the run id rather than inventing one.
          task: p.task ?? null,
          lastAction: null,
          outcome: null,
        })
      })

      // Every later event describes the run already in flight. Each one also
      // SEEDS a run when none is known: this screen can be opened mid-run (a
      // navigation, a webview reload), and the start event is long gone by then.
      const advance = (patch: (prev: MachineRun) => MachineRun) => {
        setRun((prev) =>
          patch(
            prev ?? {
              runId: null,
              status: 'running',
              startedAt: Date.now(),
              endedAt: null,
              step: 0,
              task: null,
              lastAction: null,
              outcome: null,
            },
          ),
        )
      }

      await reg<{ turn: number }>('agent://turn', (p) => {
        advance((prev) => ({ ...prev, status: 'running', endedAt: null, step: p.turn }))
      })

      await reg<{ name: string }>('agent://action', (p) => {
        advance((prev) => ({ ...prev, status: 'running', endedAt: null, lastAction: p.name }))
      })

      await reg<{ reason: string; turns?: number }>('agent://done', (p) => {
        advance((prev) => ({
          ...prev,
          status: 'done',
          endedAt: Date.now(),
          step: p.turns ?? prev.step,
          outcome: p.reason,
        }))
      })

      await reg<{ error: string }>('agent://error', (p) => {
        advance((prev) => ({ ...prev, status: 'error', endedAt: Date.now(), outcome: p.error }))
      })
    }

    subscribe()

    return () => {
      active = false
      for (const un of unlisteners) un()
    }
  }, [])

  return run
}

// NOW — what this machine is doing this second, and the control that stops it.
//
// Idle is the NORMAL state and gets the same care as the live one: a fleet node
// spends most of its life here, so this is the version of the card someone
// actually stares at.
function NowCard() {
  const run = useMachineRun()
  const running = run?.status === 'running'
  const [stopping, setStopping] = useState(false)
  const [stopError, setStopError] = useState<string | null>(null)

  // A running clock needs a tick of its own: the agent:// stream is bursty and a
  // turn can take a minute, during which a re-render-derived elapsed would sit
  // frozen and read as a hung run.
  const [, setNow] = useState(Date.now())
  useEffect(() => {
    if (!running) return
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [running])

  // Stop is the same command the run panel's Stop button calls — it cancels the
  // single in-flight run, whoever started it. Not an authorization boundary:
  // cancelling is local to this machine by nature.
  const stop = useCallback(async () => {
    setStopping(true)
    setStopError(null)
    const res = await safeInvoke<null>('stop_agent_task')
    if (!res.ok) setStopError(res.error)
    setStopping(false)
  }, [])

  // A failed stop describes one run; carrying its message into the next one
  // would report a problem that is no longer on screen.
  const runId = run?.runId ?? null
  useEffect(() => {
    setStopError(null)
  }, [runId])

  if (!run) {
    return (
      <Card title={<SectionTitle>Now</SectionTitle>}>
        <EmptyState icon="○" title="Idle" hint="Nothing has run on this machine since it started." />
      </Card>
    )
  }

  if (!running) {
    return (
      <Card title={<SectionTitle>Now</SectionTitle>}>
        <div style={{ fontSize: 'var(--fs-base)', color: 'var(--sb-text)' }}>
          Idle — last run {run.status === 'error' ? 'failed' : 'finished'}
        </div>
        <div
          style={{
            marginTop: 'var(--sp-2)',
            fontSize: 'var(--fs-md)',
            lineHeight: 1.5,
            color: run.status === 'error' ? 'var(--sb-danger-bright)' : 'var(--sb-text-muted)',
          }}
        >
          {run.outcome || (run.status === 'error' ? 'no reason given' : 'completed')}
        </div>
        <div
          style={{
            marginTop: 'var(--sp-2)',
            fontSize: 'var(--fs-sm)',
            color: 'var(--sb-text-faint)',
          }}
        >
          {run.step > 0 && `${run.step} ${run.step === 1 ? 'step' : 'steps'} · `}
          ran for {formatElapsed((run.endedAt ?? Date.now()) - run.startedAt)}
        </div>
      </Card>
    )
  }

  return (
    <Card title={<SectionTitle>Now</SectionTitle>}>
      <div style={{ display: 'flex', alignItems: 'flex-start', gap: 'var(--sp-4)' }}>
        <div style={{ flex: 1, minWidth: 0 }}>
          {/* The task first, because it is the question someone walking up to
              this machine is actually asking. The tool call is the answer to a
              narrower one ("is it stuck?") and sits below in the detail line. */}
          <div
            style={{
              fontSize: 'var(--fs-base)',
              color: 'var(--sb-text)',
              lineHeight: 1.5,
            }}
          >
            {run.task ?? 'Running'}
          </div>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              marginTop: 'var(--sp-2)',
              flexWrap: 'wrap',
              fontSize: 'var(--fs-sm)',
              color: 'var(--sb-text-muted)',
            }}
          >
            {run.lastAction && (
              <>
                <span>{run.lastAction}</span>
                <span>·</span>
              </>
            )}
            <span>step {run.step}</span>
            <span>·</span>
            <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--sb-text)' }}>
              {formatElapsed(Date.now() - run.startedAt)}
            </span>
            {run.runId && (
              <Badge mono title={run.runId}>
                {run.runId.slice(0, 8)}
              </Badge>
            )}
          </div>
        </div>
        <Button size="sm" variant="danger" onClick={stop} disabled={stopping}>
          {stopping ? 'Stopping…' : 'Stop'}
        </Button>
      </div>
      {stopError && (
        <div className="error-message" style={{ marginTop: 'var(--sp-3)' }}>
          {stopError}
        </div>
      )}
    </Card>
  )
}

// ───────────────────────────────────────────────────────── recent runs (local)

// RECENT RUNS (LOCAL) — the machine's own memory of past runs, read from its
// own disk and nowhere else.
//
// A worker cannot show backend run history here: its device token never enters
// the webview, and the run-history routes refuse device tokens by design. What
// it CAN show honestly is what already lives under app_data_dir/runs/<id>/ —
// every frame the agent saved, plus outcome.json where a run finalized on a
// build that writes it. That record is deliberately presented as what it is:
// timestamps are file times, older runs have no outcome at all, and the card
// says so instead of dressing local traces up as fleet metadata.

// What `local_runs` answers with (src-tauri/src/runs_local.rs `LocalRun`).
interface LocalRun {
  run_id: string
  /** RFC3339, from the earliest frame's mtime. Null when no frame was saved. */
  started_at: string | null
  /** outcome.json's finished_at, else the latest frame's mtime. */
  finished_at: string | null
  /** Terminal status if the machine recorded one; null for older runs. */
  outcome: string | null
  error_message: string | null
  frame_count: number
  first_frame: string | null
  last_frame: string | null
}

// Same conversion RunDetail.tsx uses for run screenshots: local absolute path →
// asset-protocol URL. Null when convertFileSrc is unavailable or throws, so the
// caller can drop the thumbnail rather than render a broken img.
function frameSrc(path: string): string | null {
  try {
    return convertFileSrc(path)
  } catch {
    return null
  }
}

// "When" for a row: date + time in the reader's locale, or a dash. These are
// file mtimes, so minute precision is already flattering them.
function formatWhen(ts: string | null): string {
  if (!ts) return '—'
  const ms = Date.parse(ts)
  if (Number.isNaN(ms)) return '—'
  return new Date(ms).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  })
}

function LocalRunsCard({ refreshKey }: { refreshKey: number }) {
  const [runs, setRuns] = useState<LocalRun[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  // One row open at a time: the strip below it is the detail view, and two open
  // rows of thumbnails turn a glanceable card into a scroll.
  const [openId, setOpenId] = useState<string | null>(null)
  const [frames, setFrames] = useState<Record<string, string[] | 'loading'>>({})

  useEffect(() => {
    let active = true
    safeInvoke<LocalRun[]>('local_runs').then((res) => {
      if (!active) return
      if (res.ok) setRuns(res.data)
      else setError(res.error)
    })
    return () => {
      active = false
    }
  }, [refreshKey])

  const toggle = useCallback(
    async (runId: string) => {
      if (openId === runId) {
        setOpenId(null)
        return
      }
      setOpenId(runId)
      // Frames are fetched once per run and kept: the paths are immutable (a
      // finished run's directory only ever gains an outcome.json).
      if (frames[runId]) return
      setFrames((prev) => ({ ...prev, [runId]: 'loading' }))
      const res = await safeInvoke<string[]>('local_run_frames', { runId })
      setFrames((prev) => ({ ...prev, [runId]: res.ok ? res.data : [] }))
    },
    [openId, frames],
  )

  // Outside the desktop app there is no disk to read — and unlike the identity
  // card, which explains itself, an always-empty history card would only add
  // noise to the browser-served admin panel. Render nothing.
  if (!isTauri()) return null

  return (
    <Card title={<SectionTitle>Recent runs (local)</SectionTitle>}>
      {/* The honesty line, before any rows: this is the machine testifying
          about itself, not the fleet's ledger. */}
      <p
        style={{
          margin: '0 0 var(--sp-3)',
          fontSize: 'var(--fs-sm)',
          lineHeight: 1.5,
          color: 'var(--sb-text-faint)',
        }}
      >
        What this machine remembers on its own disk — frames saved as runs happened, and
        outcomes where it recorded one. The fleet's authoritative run history lives in the
        operator's console.
      </p>

      {!runs && !error && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-2)',
            color: 'var(--sb-text-muted)',
            fontSize: 'var(--fs-md)',
          }}
        >
          <Spinner size={14} /> Reading local records…
        </div>
      )}

      {error && (
        <p style={{ margin: 0, fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
          Could not read this machine's local run records. {error}
        </p>
      )}

      {runs && runs.length === 0 && (
        <EmptyState icon="○" title="Nothing recorded" hint="No run has left frames on this machine's disk yet." />
      )}

      {runs && runs.length > 0 && (
        <div style={{ display: 'flex', flexDirection: 'column' }}>
          {runs.map((run, i) => {
            const open = openId === run.run_id
            const runFrames = frames[run.run_id]
            return (
              <div
                key={run.run_id}
                style={{ borderTop: i > 0 ? '1px solid var(--sb-border)' : undefined }}
              >
                <button
                  onClick={() => toggle(run.run_id)}
                  title={run.run_id}
                  style={{
                    display: 'flex',
                    alignItems: 'center',
                    gap: 'var(--sp-3)',
                    width: '100%',
                    padding: 'var(--sp-2) 0',
                    border: 'none',
                    background: 'none',
                    cursor: 'pointer',
                    textAlign: 'left',
                  }}
                >
                  <span
                    aria-hidden
                    style={{ fontSize: 'var(--fs-xs)', color: 'var(--sb-text-faint)', width: 10 }}
                  >
                    {open ? '▾' : '▸'}
                  </span>
                  <Badge mono>{run.run_id.slice(0, 8)}</Badge>
                  <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
                    {formatWhen(run.finished_at)}
                  </span>
                  {/* No badge when no outcome was recorded: an absent record is
                      a fact about this disk, not a status to invent. */}
                  {run.outcome && <StatusPill status={run.outcome} />}
                  <span
                    style={{
                      marginLeft: 'auto',
                      fontSize: 'var(--fs-sm)',
                      color: 'var(--sb-text-faint)',
                      whiteSpace: 'nowrap',
                    }}
                  >
                    {run.frame_count} {run.frame_count === 1 ? 'frame' : 'frames'}
                  </span>
                </button>

                {open && (
                  <div style={{ padding: '0 0 var(--sp-3) calc(10px + var(--sp-3))' }}>
                    {run.error_message && (
                      <p
                        style={{
                          margin: '0 0 var(--sp-2)',
                          fontSize: 'var(--fs-sm)',
                          lineHeight: 1.5,
                          color: 'var(--sb-danger-bright)',
                          overflowWrap: 'anywhere',
                        }}
                      >
                        {run.error_message}
                      </p>
                    )}
                    {runFrames === 'loading' && (
                      <div
                        style={{
                          display: 'flex',
                          alignItems: 'center',
                          gap: 'var(--sp-2)',
                          color: 'var(--sb-text-muted)',
                          fontSize: 'var(--fs-sm)',
                        }}
                      >
                        <Spinner size={12} /> Reading frames…
                      </div>
                    )}
                    {Array.isArray(runFrames) && runFrames.length === 0 && (
                      <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
                        No frames on disk for this run.
                      </span>
                    )}
                    {Array.isArray(runFrames) && runFrames.length > 0 && (
                      <div
                        style={{
                          display: 'grid',
                          gridTemplateColumns: 'repeat(auto-fill, minmax(96px, 1fr))',
                          gap: 'var(--sp-2)',
                        }}
                      >
                        {runFrames.map((path) => {
                          const src = frameSrc(path)
                          if (!src) return null
                          return (
                            <img
                              key={path}
                              src={src}
                              alt="saved run frame"
                              loading="lazy"
                              style={{
                                display: 'block',
                                width: '100%',
                                height: 'auto',
                                border: '1px solid var(--sb-border)',
                                borderRadius: 'var(--r-sm)',
                                background: 'var(--sb-surface-2)',
                              }}
                            />
                          )
                        })}
                      </div>
                    )}
                  </div>
                )}
              </div>
            )
          })}
        </div>
      )}
    </Card>
  )
}

// Elapsed as something readable at a glance over a laggy remote session: whole
// seconds under a minute, minutes and seconds under an hour, hours after that.
function formatElapsed(ms: number): string {
  const sec = Math.max(0, Math.floor(ms / 1000))
  if (sec < 60) return `${sec}s`
  const min = Math.floor(sec / 60)
  if (min < 60) return `${min}m ${String(sec % 60).padStart(2, '0')}s`
  const hr = Math.floor(min / 60)
  return `${hr}h ${String(min % 60).padStart(2, '0')}m`
}

// ───────────────────────────────────────────────────────── model endpoint

// MODEL ENDPOINT — which model host this machine will drive, and the one action
// that proves it works. `check_model_endpoint` sends a real /v1/messages round
// trip rather than pinging a health route, because a server can be listening and
// still not serve this API shape; only the round trip is evidence.
function ModelEndpointCard({ refreshKey }: { refreshKey: number }) {
  const [endpoint, setEndpoint] = useState<ModelEndpoint | null>(null)
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<{ ok: boolean; message: string } | null>(null)

  useEffect(() => {
    let active = true
    safeInvoke<ModelEndpoint>('model_endpoint').then((res) => {
      if (active && res.ok) setEndpoint(res.data)
    })
    return () => {
      active = false
    }
  }, [refreshKey])

  const verify = useCallback(async () => {
    setBusy(true)
    setResult(null)
    const res = await safeInvoke<string>('check_model_endpoint')
    setResult(
      res.ok ? { ok: true, message: 'Reachable — a run would get through.' } : { ok: false, message: res.error },
    )
    setBusy(false)
  }, [])

  return (
    <Card
      title={<SectionTitle>Model endpoint</SectionTitle>}
      actions={
        <Button variant="secondary" size="sm" onClick={verify} disabled={busy}>
          {busy ? 'Verifying…' : 'Verify'}
        </Button>
      }
    >
      {!endpoint ? (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-2)',
            color: 'var(--sb-text-muted)',
            fontSize: 'var(--fs-md)',
          }}
        >
          <Spinner size={14} /> Reading…
        </div>
      ) : (
        <>
          <div
            style={{
              fontSize: 'var(--fs-md)',
              fontFamily: 'var(--font-mono)',
              color: 'var(--sb-text)',
              overflowWrap: 'anywhere',
            }}
          >
            {endpoint.base}
          </div>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              marginTop: 'var(--sp-3)',
              flexWrap: 'wrap',
            }}
          >
            <Badge mono tone="gold">
              {endpoint.model}
            </Badge>
            <Badge>{endpoint.is_anthropic ? 'Anthropic' : 'Self-hosted'}</Badge>
            <Badge tone={endpoint.source === 'fleet' ? 'success' : 'neutral'}>
              {SOURCE_LABEL[endpoint.source]}
            </Badge>
          </div>

          {/* WHERE IT CAME FROM, in words. The URL alone does not tell whoever is
              reading this over a remote desktop which knob to turn — a fleet
              value is fixed once in Settings for every machine, an env value is
              a shell on this box that somebody has to go and change. */}
          <p
            style={{
              margin: 'var(--sp-3) 0 0',
              fontSize: 'var(--fs-md)',
              lineHeight: 1.5,
              color: 'var(--sb-text-muted)',
            }}
          >
            {SOURCE_BLURB[endpoint.source]}
          </p>

          {/* The whole point of the guard, said before a run fails rather than
              after: this machine would bill Anthropic, so it will refuse. */}
          {endpoint.blocked && (
            <p
              style={{
                margin: 'var(--sp-2) 0 0',
                fontSize: 'var(--fs-md)',
                lineHeight: 1.5,
                color: 'var(--sb-danger-bright)',
              }}
            >
              Runs dispatched to this machine will be REFUSED. It is an enrolled worker
              and this endpoint is Anthropic's own API, so a run would bill Anthropic
              while looking self-hosted. Set the fleet model endpoint in Settings, or set
              CU_ALLOW_ANTHROPIC=1 on this machine to allow it deliberately.
            </p>
          )}
        </>
      )}

      {result && (
        <div
          style={{
            marginTop: 'var(--sp-3)',
            fontSize: 'var(--fs-md)',
            lineHeight: 1.5,
            color: result.ok ? 'var(--sb-success)' : 'var(--sb-danger-bright)',
            overflowWrap: 'anywhere',
          }}
        >
          {result.message}
        </div>
      )}
    </Card>
  )
}

const SOURCE_LABEL: Record<ModelEndpoint['source'], string> = {
  fleet: 'Fleet setting',
  env: 'Local env var',
  default: 'Default',
}

// A worker cannot read `/settings` — its device token is not a session
// credential — so the only fleet value it ever holds is the one that arrived on
// a dispatched run frame. Before the first such run it genuinely does not know,
// and saying so is more use than showing the env var as though it were settled.
const SOURCE_BLURB: Record<ModelEndpoint['source'], string> = {
  fleet: 'Set by the fleet operator in Settings and sent with the last run. Change it there, once, for every machine.',
  env: "From CU_ANTHROPIC_BASE in this machine's own environment. No fleet endpoint has arrived yet — a dispatched run carrying one would override this.",
  default: 'Nothing configured — neither the fleet nor this machine names an endpoint, so runs would go to Anthropic. Set the fleet model endpoint in Settings.',
}

// ───────────────────────────────────────────────────────── enrollment

// ENROLLMENT — that this machine holds a worker pass, and nothing more.
//
// Deliberately has no controls. Revoking is the operator's act, from the admin
// side, against the device row; a "leave the fleet" button here would be a
// machine deciding its own membership, and the one place that already exists
// (Sign out, which un-enrols) is behind a confirm for exactly that reason.
//
// `credential_class` is a local read of the Rust credential store, so this is a
// fact about the machine rather than a claim about the backend's opinion of it —
// a revoked worker still holds its dead token, and hears about the refusal
// through `device://rejected` instead.
function EnrollmentCard({ refreshKey }: { refreshKey: number }) {
  const [credential, setCredential] = useState<CredentialClass | null>(null)

  useEffect(() => {
    let active = true
    // `hasSession` is the half of the answer Rust cannot see: the session token
    // lives in localStorage, which is invisible from the command side.
    const hasSession = !!localStorage.getItem('screen_buddy_session_token')
    safeInvoke<CredentialClass>('credential_class', { hasSession }).then((res) => {
      if (active && res.ok) setCredential(res.data)
    })
    return () => {
      active = false
    }
  }, [refreshKey])

  return (
    <Card title={<SectionTitle>Enrollment</SectionTitle>}>
      <p style={{ margin: 0, fontSize: 'var(--fs-md)', lineHeight: 1.5, color: 'var(--sb-text-muted)' }}>
        {credential === 'device'
          ? 'Enrolled — this machine holds a worker pass and runs agents for the fleet. The pass is stored on this computer and never leaves it. Access is granted and revoked by the fleet operator.'
          : credential === 'session'
            ? 'Not enrolled — this machine signs in with an account rather than holding a worker pass. It runs agents for whoever is signed in.'
            : 'Not enrolled — this machine holds no credential, so nothing can dispatch work to it.'}
      </p>
    </Card>
  )
}


// ───────────────────────────────────────────────────────── uia probe

// UIA PROBE — a prototype's front door, deliberately a button rather than a
// devtools incantation.
//
// The worker aims clicks by pixel: a vision model estimates a point from a
// downscaled screenshot, and that estimate is our dominant failure mode.
// Windows already publishes the answer through UI Automation — every control's
// name, type and exact rectangle — so the question worth answering is what real
// applications actually report. This dumps that, read-only: nothing here
// clicks, and no part of the agent loop consults it yet.
//
// The delay is the whole ergonomics problem. UIA reads the FOREGROUND window,
// and while the operator is looking at this panel, the foreground window is
// ScreenBuddy — a dump taken now would faithfully describe the wrong thing. So
// the button counts down and the operator spends it clicking the app they
// actually want measured.
const UIA_COUNTDOWN_SECS = 5

function UiaProbeCard() {
  const [countdown, setCountdown] = useState<number | null>(null)
  const [busy, setBusy] = useState(false)
  const [dump, setDump] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [copied, setCopied] = useState(false)

  const run = useCallback(async (command: 'uia_dump' | 'uia_dump_all') => {
    setError(null)
    setDump(null)
    setCopied(false)
    // Count down in the UI so the operator knows exactly how long they have to
    // bring the target window forward.
    for (let n = UIA_COUNTDOWN_SECS; n > 0; n--) {
      setCountdown(n)
      await new Promise((r) => setTimeout(r, 1000))
    }
    setCountdown(null)
    setBusy(true)
    const res = await safeInvoke<unknown>(command)
    setBusy(false)
    if (!res.ok) {
      setError(res.error)
      return
    }
    setDump(JSON.stringify(res.data, null, 2))
  }, [])

  const copy = useCallback(() => {
    if (!dump) return
    navigator.clipboard?.writeText(dump)
    setCopied(true)
    setTimeout(() => setCopied(false), 1600)
  }, [dump])

  if (!isTauri()) return null

  return (
    <Card title={<SectionTitle>UI Automation probe</SectionTitle>}>
      <p style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)', lineHeight: 1.5 }}>
        Reads the controls Windows reports for whichever window is in front —
        names, types and exact rectangles. Press a button, then click the app you
        want measured; the countdown is there so the dump describes that app and
        not this panel. Read-only: nothing clicks, and no run consults it.
      </p>
      <div style={{ display: 'flex', gap: 'var(--sp-2)', marginTop: 'var(--sp-3)', flexWrap: 'wrap' }}>
        <Button
          variant="primary"
          size="sm"
          onClick={() => run('uia_dump')}
          disabled={busy || countdown !== null}
        >
          {countdown !== null
            ? `Switch windows… ${countdown}`
            : busy
              ? 'Reading…'
              : 'Dump clickable controls'}
        </Button>
        <Button
          variant="secondary"
          size="sm"
          onClick={() => run('uia_dump_all')}
          disabled={busy || countdown !== null}
        >
          Dump everything
        </Button>
        {dump && (
          <Button variant="ghost" size="sm" onClick={copy}>
            {copied ? 'Copied' : 'Copy JSON'}
          </Button>
        )}
      </div>
      {error && (
        <p className="error-message" style={{ marginTop: 'var(--sp-3)' }}>
          {error}
        </p>
      )}
      {dump && (
        <pre
          style={{
            marginTop: 'var(--sp-3)',
            maxHeight: 320,
            overflow: 'auto',
            fontSize: 'var(--fs-xs)',
            fontFamily: 'var(--font-mono)',
            color: 'var(--sb-text)',
            background: 'var(--sb-surface-1)',
            border: '1px solid var(--sb-border)',
            borderRadius: 'var(--r-sm)',
            padding: 'var(--sp-3)',
            whiteSpace: 'pre',
          }}
        >
          {dump}
        </pre>
      )}
    </Card>
  )
}

export default Machine

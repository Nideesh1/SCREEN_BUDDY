import { useCallback, useEffect, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import { Button, Card, Divider, EmptyState, SectionTitle, Spinner, StatusPill } from '../ui'

// DeviceRuns — one machine's runs, as a page of its own.
//
// This list used to live inside the Devices detail pane, stacked under NOW and
// SCREEN in a column that also carried access, remote desktop and the editable
// fields. That pane answers "what is this machine doing"; the list answers "what
// has this machine done", which is a different question with a different length
// — and a scrolling card inside a scrolling pane is where it stopped being
// readable. It is a destination now: #/devices/<id>/runs, with the run
// drilldown nested under it at #/devices/<id>/runs/<runId>.
//
// The page states WHICH machine it belongs to before it lists anything. Someone
// can land here from a bookmark or a pasted link with no pane on screen to give
// it context, and a bare list of tasks belonging to nobody is the failure this
// header exists to prevent — hence the device fetch, rather than carrying a name
// through navigation state that a direct load would not have.

// The subset of GET /devices/{id} this page and the run view need. The full
// record (Admin's `Device`) also carries the editable fields and the enrollment
// facts, none of which are read or written here.
export interface DeviceHead {
  device_id: string
  hostname: string
  name: string | null
  last_seen: string
  current_run_id: string | null
  online: boolean
  enrollment_state?: string
}

// One row of GET /runs/by-device/{device_id} — the backend's _run_summary, which
// is a run WITHOUT its result (a whole trajectory, wanted on exactly one run at
// a time). Not History's RunSummary: this route also carries the lifecycle
// timestamps, and those are what a duration is made of.
export interface DeviceRun {
  run_id: string
  task?: string
  model?: string
  status?: string
  num_steps?: number
  created_at?: string
  started_at?: string | null
  completed_at?: string | null
}

// How many runs this page lists. Generous because it is the machine's record
// rather than a card's worth of "recently" — but still bounded: History is the
// archive, and a worker that has been up for months is not one page.
const RUNS_PAGE = 50

// How often the page re-reads. The same 30s the fleet list uses, and for the
// same reason: there is no push channel this bundle can use in a plain browser,
// and 30s is well under the 90s the backend uses to decide `online`.
const POLL_MS = 30_000

// The display name an admin gave the machine, or what the machine calls itself.
// Deliberately not imported from Admin: Admin imports the run row from here, and
// a cycle between the two modules is not worth one string.
export function deviceLabel(device: DeviceHead): string {
  return device.name?.trim() || device.hostname
}

// Read one machine. Shared with FleetRun, which needs the same name to say whose
// run it is showing and where "back" goes.
//
// A failed read is a null device, never an error screen: the runs below it are
// fetched by id and load fine without it. Losing the header is a worse page, not
// a broken one.
export function useDevice(deviceId: string | undefined): DeviceHead | null {
  const [device, setDevice] = useState<DeviceHead | null>(null)
  useEffect(() => {
    if (!deviceId) {
      setDevice(null)
      return
    }
    let cancelled = false
    ;(async () => {
      try {
        const resp = await fetch(`${CU_BACKEND}/devices/${encodeURIComponent(deviceId)}`, {
          headers: authHeaders(),
        })
        if (!resp.ok) return
        const body = await resp.json()
        if (cancelled) return
        setDevice((body.device ?? body) as DeviceHead)
      } catch {
        // The header degrades to the bare id — see above.
      }
    })()
    return () => {
      cancelled = true
    }
  }, [deviceId])
  return device
}

// How long a run took, or has been going. Worth showing next to the step count
// because the two disagree in the cases worth noticing: a machine stuck
// re-clicking the same button burns minutes without adding steps, and a run that
// died on its first turn never adds any.
export function runDuration(run: DeviceRun, live: boolean): string | null {
  const start = Date.parse(String(run.started_at ?? run.created_at ?? ''))
  if (!Number.isFinite(start)) return null
  // Only a LIVE run may be measured against the clock. A run with no
  // `completed_at` that is not live never finished — it was minted and
  // abandoned, or its worker vanished — and clocking it to now reported the
  // hours since it was created as though it had spent them working: a run that
  // did nothing showed the longest duration on the page. It gets no duration
  // instead, and the row says what actually happened.
  if (!run.completed_at && !live) return null
  const end = run.completed_at ? Date.parse(run.completed_at) : Date.now()
  if (!Number.isFinite(end) || end < start) return null
  const secs = Math.round((end - start) / 1000)
  if (secs < 60) return `${secs}s`
  const mins = Math.round(secs / 60)
  if (mins < 60) return `${mins}m`
  return `${Math.floor(mins / 60)}h ${mins % 60}m`
}

// Green when the machine has checked in inside the backend's liveness window,
// grey when it has not, a hollow ring when it was switched off at this end —
// the same three states, and the same reasoning, as the fleet list's dot.
export function DeviceDot({ device }: { device: DeviceHead }) {
  const revoked = device.enrollment_state === 'revoked'
  return (
    <span
      aria-hidden
      style={{
        flexShrink: 0,
        boxSizing: 'border-box',
        width: 8,
        height: 8,
        borderRadius: '50%',
        background: revoked
          ? 'transparent'
          : device.online
            ? 'var(--sb-success)'
            : 'var(--sb-text-faint)',
        border: revoked ? '1.5px solid var(--sb-text-muted)' : undefined,
      }}
    />
  )
}

// What the machine is, in one line, under its name. Shared with FleetRun so the
// run page and the runs page identify a machine identically.
//
// The hostname is dropped when it IS the name: an unnamed machine is listed by
// its hostname, and "mac-mini-4 · mac-mini-4 · online" reads as two machines.
export function deviceLine(device: DeviceHead): string {
  const state =
    device.enrollment_state === 'revoked'
      ? 'access revoked'
      : device.online
        ? 'online'
        : 'offline'
  const parts = deviceLabel(device) === device.hostname ? [] : [device.hostname]
  parts.push(state, `last seen ${relativeTime(device.last_seen)}`)
  return parts.join(' · ')
}

// One run, as a row. Used at full length here and two at a time as the Devices
// pane's preview, so a run reads identically wherever it is listed.
export function RunRow({
  run,
  live,
  onOpen,
}: {
  run: DeviceRun
  live: boolean
  onOpen: () => void
}) {
  const duration = runDuration(run, live)
  // A run with no duration and no steps never got off the ground. Saying so
  // beats an empty gap where every other row carries a measurement.
  const stalled = !duration && !live && (run.num_steps ?? 0) === 0
  return (
    <button
      onClick={onOpen}
      title="Open this run — its narration, its actions and its frames"
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-3)',
        width: '100%',
        textAlign: 'left',
        padding: '10px 16px',
        background: 'transparent',
        border: 'none',
        cursor: 'pointer',
        color: 'var(--sb-text)',
      }}
      onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--sb-gold-dim)')}
      onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
    >
      {/* Pulses on `running`, which is the whole of "reads as live". */}
      <StatusPill status={live ? 'running' : run.status} />
      <span
        style={{
          flex: 1,
          minWidth: 0,
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
          fontSize: 'var(--fs-md)',
        }}
      >
        {run.task || '(untitled task)'}
      </span>
      <span
        style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)', whiteSpace: 'nowrap' }}
      >
        {run.num_steps ?? 0} steps
        {duration && (live ? ` · running for ${duration}` : ` · took ${duration}`)}
        {stalled && ' · never started'} ·{' '}
        {relativeTime(run.started_at ?? run.created_at)}
      </span>
    </button>
  )
}

// Read one machine's runs, newest first with the live one lifted to the top.
//
// The list comes from GET /runs/by-device — never from the device's FRAMES, as
// it once did. The frame join could not show a run that uploaded no frame: a run
// that died before its first turn, which is exactly the run an operator comes
// looking for.
export function useDeviceRuns(
  deviceId: string | undefined,
  limit: number,
  currentRunId: string | null | undefined,
): { runs: DeviceRun[] | null; isLive: (run: DeviceRun) => boolean; refresh: () => void } {
  const [runs, setRuns] = useState<DeviceRun[] | null>(null)
  const [nonce, setNonce] = useState(0)

  // The device record and the run record can disagree for a moment either side
  // of a transition, so a row counts as live if EITHER says so.
  const isLive = useCallback(
    (run: DeviceRun): boolean =>
      run.run_id === currentRunId || (run.status || '').toLowerCase() === 'running',
    [currentRunId],
  )

  useEffect(() => {
    if (!deviceId) return
    let cancelled = false
    ;(async () => {
      try {
        const resp = await fetch(
          `${CU_BACKEND}/runs/by-device/${encodeURIComponent(deviceId)}?limit=${limit}`,
          { headers: authHeaders() },
        )
        if (!resp.ok) {
          if (!cancelled) setRuns([])
          return
        }
        const body = await resp.json()
        if (cancelled) return
        const rows: DeviceRun[] = Array.isArray(body) ? body : (body.runs ?? [])
        // Newest-first from the backend, then the live run lifted to the top
        // whatever its created_at says: it is the only row still changing.
        setRuns([...rows].sort((a, b) => Number(isLive(b)) - Number(isLive(a))))
      } catch {
        // Failing to list runs is worth an empty state, not an error card: the
        // machine itself is unaffected and is still named above the list.
        if (!cancelled) setRuns([])
      }
    })()
    return () => {
      cancelled = true
    }
  }, [deviceId, limit, isLive, nonce])

  const refresh = useCallback(() => setNonce((n) => n + 1), [])
  return { runs, isLive, refresh }
}

function DeviceRuns() {
  const navigate = useNavigate()
  const { deviceId = '' } = useParams<{ deviceId: string }>()
  const device = useDevice(deviceId)
  const { runs, isLive, refresh } = useDeviceRuns(deviceId, RUNS_PAGE, device?.current_run_id)

  // A run starting or finishing is the only thing that alters this list, and
  // nothing pushes that to a browser tab — so the page asks again on the fleet
  // list's cadence. Refetching wholesale is safe here: unlike the device pane,
  // this page holds no draft a poll could overwrite.
  useEffect(() => {
    const id = setInterval(refresh, POLL_MS)
    return () => clearInterval(id)
  }, [refresh])

  const listed = runs !== null && runs.length > 0

  return (
    <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max)', margin: '0 auto' }}>
      {/* Back is a ROUTE, not history.back(): this page is linkable, and someone
          who arrived from a pasted URL has nothing behind them to go back to. */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--sp-3)',
          marginBottom: 'var(--sp-4)',
        }}
      >
        <Button
          variant="ghost"
          size="sm"
          onClick={() => navigate('/admin')}
          style={{ flexShrink: 0 }}
        >
          ← Devices
        </Button>
        <div style={{ minWidth: 0, flex: 1 }}>
          <h1
            style={{
              margin: 0,
              fontSize: 'var(--fs-2xl)',
              fontWeight: 700,
              color: 'var(--sb-text)',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}
          >
            {device ? deviceLabel(device) : 'Runs'}
          </h1>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              marginTop: 2,
              fontSize: 'var(--fs-sm)',
              color: 'var(--sb-text-muted)',
            }}
          >
            {device ? (
              <>
                <DeviceDot device={device} />
                <span>{deviceLine(device)}</span>
              </>
            ) : (
              // The machine could not be read. Its id is still the honest answer
              // to "whose runs are these" — better than a header that guesses.
              <span style={{ fontFamily: 'var(--font-mono)' }}>{deviceId}</span>
            )}
          </div>
        </div>
        <Button variant="secondary" size="sm" onClick={refresh} style={{ flexShrink: 0 }}>
          ↻ Refresh
        </Button>
      </div>

      <Card title={<SectionTitle>Runs</SectionTitle>} padded={!listed}>
        {runs === null && (
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-3)',
              color: 'var(--sb-text-muted)',
            }}
          >
            <Spinner size={14} /> Loading runs…
          </div>
        )}

        {runs !== null && runs.length === 0 && (
          <EmptyState
            icon="▷"
            title="Nothing has run here yet"
            hint="A machine that has never been given work is not a broken machine. Runs appear here newest first — the one in flight at the top — each with its own narration and its own frames."
          />
        )}

        {listed &&
          runs.map((run, i) => (
            <div key={run.run_id}>
              {i > 0 && <Divider style={{ margin: 0 }} />}
              <RunRow
                run={run}
                live={isLive(run)}
                onOpen={() =>
                  navigate(
                    `/devices/${encodeURIComponent(deviceId)}/runs/${encodeURIComponent(run.run_id)}`,
                  )
                }
              />
            </div>
          ))}
      </Card>
    </div>
  )
}

export default DeviceRuns

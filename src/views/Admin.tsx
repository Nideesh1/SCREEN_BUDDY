import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime, safeInvoke } from '../lib'
import { Badge, Button, Card, ConfirmModal, Divider, EmptyState, IconButton, SectionTitle, Spinner } from '../ui'
// The runs list lives on its own page now; the pane borrows its row and its
// reader so a run reads the same in the preview as it does in the full list.
import { RunRow, useDeviceRuns } from './DeviceRuns'
// The task layer's shared shapes live with the Inbox (its front door), the same
// way the run row lives with DeviceRuns.
import { TaskRecord, TaskStatusPill, taskIsTerminal, utcRelative } from './Inbox'

// One machine in the fleet, field-for-field the backend contract for
// GET /devices. Everything except name / rustdesk_id / notes is REPORTED by the
// machine itself and read-only here; those three are the only writable fields
// (PATCH /devices/{id}) and are null until an admin fills them in.
//
// `online` is server-derived (last_seen within 90s) rather than computed here on
// purpose: the browser clock is not trustworthy, and two admins looking at the
// same fleet must see the same dots.
interface Device {
  device_id: string
  user_id: string
  hostname: string
  os: string
  os_version: string
  app_version: string
  name: string | null
  rustdesk_id: string | null
  notes: string | null
  last_seen: string
  current_run_id: string | null
  online: boolean
  created_at: string
  enrollment_state: EnrollmentState
  /** When this machine last redeemed a key. Null when it never has. */
  enrolled_at: string | null
}

// Whether a machine holds a working worker pass. Derived server-side (from the
// device row's token state, which is never exposed) so two admins reading the
// same fleet cannot disagree about it — the same reasoning as `online`.
//
// The two states that are not "enrolled" mean opposite things and must never
// render alike:
//
//   enrolled     — redeemed a key, holds a live pass, can run agents.
//   revoked      — held one and the operator deliberately turned it off. The row
//                  stays in the fleet with its name, RustDesk id and notes so it
//                  can be handed a fresh key. Not an error, and not offline.
//   not_enrolled — never had one, and does not want one. This is the operator's
//                  OWN machine, which signs in with Google instead; it is the
//                  normal state for that row and nothing to flag.
type EnrollmentState = 'enrolled' | 'revoked' | 'not_enrolled'

// The current run behind a device's current_run_id. GET /runs/{id} answers with
// { run, events } (see RunDetail); we want only the handful of run fields the
// NOW block shows, so the events array is ignored.
interface CurrentRun {
  run_id: string
  task?: string
  model?: string
  num_steps?: number
  started_at?: string | number
  created_at?: string | number
}

type Load =
  | { state: 'loading' }
  | { state: 'error'; message: string }
  | { state: 'ready'; devices: Device[] }

// How often the list re-fetches. There is no push channel we can use: the
// `remote://` Tauri events only exist inside the desktop app, and this same
// bundle is served in a plain browser. 30s is well under the 90s the backend
// uses to decide `online`, so a machine that drops off is never stale by more
// than one poll.
const POLL_MS = 30_000

// Devices — the admin shell's fleet supervisor. Two panes with no navigation
// between them: the list on the left is what someone glances at from across the
// room, and selecting a row swaps the detail on the right in place. Desktop
// only, deliberately — the whole premise is that the fleet runs unattended, so
// this is a screen you walk up to, not one you hold.
function Devices() {
  const navigate = useNavigate()
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const [addingMachine, setAddingMachine] = useState(false)
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const thisDeviceId = useThisDeviceId()

  // `quiet` keeps the poll from tearing the pane down every 30 seconds: only the
  // first load (and an explicit refresh) may drop back to the spinner, and a
  // failed poll leaves the last good list on screen rather than replacing a live
  // fleet with an error card.
  const fetchDevices = useCallback(async (quiet = false) => {
    if (!quiet) setLoad({ state: 'loading' })
    try {
      const resp = await fetch(`${CU_BACKEND}/devices`, { headers: authHeaders() })
      if (!resp.ok) {
        if (!quiet) setLoad({ state: 'error', message: `Failed to load devices (${resp.status})` })
        return
      }
      const data = await resp.json()
      // Accept either a bare array or { devices: [...] }, as the runs list does.
      const devices: Device[] = Array.isArray(data) ? data : (data.devices ?? [])
      setLoad({ state: 'ready', devices })
    } catch (err) {
      if (!quiet) {
        setLoad({ state: 'error', message: err instanceof Error ? err.message : 'Network error' })
      }
    }
  }, [])

  useEffect(() => {
    fetchDevices()
    const id = setInterval(() => fetchDevices(true), POLL_MS)
    return () => clearInterval(id)
  }, [fetchDevices])

  const devices = load.state === 'ready' ? load.devices : []
  const onlineCount = devices.filter((d) => d.online).length
  // Surfaced beside the online count because a revoked machine looks entirely
  // normal in the list otherwise — same name, same row — and "3 · 1 online"
  // would leave the operator to work out why the other two never check in.
  const revokedCount = devices.filter((d) => d.enrollment_state === 'revoked').length

  // Keep a selection alive across polls: fall back to the first device when
  // nothing is selected or the selected machine has been forgotten.
  const selected = devices.find((d) => d.device_id === selectedId) ?? devices[0] ?? null
  useEffect(() => {
    if (selected && selected.device_id !== selectedId) setSelectedId(selected.device_id)
  }, [selected, selectedId])

  return (
    <div
      style={{
        height: '100%',
        display: 'flex',
        flexDirection: 'column',
        padding: 'var(--sp-5)',
        gap: 'var(--sp-4)',
        maxWidth: 'var(--page-max)',
        margin: '0 auto',
      }}
    >
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--sp-3)' }}>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
          Devices
        </h1>
        {load.state === 'ready' && devices.length > 0 && (
          <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
            {devices.length} · {onlineCount} online
            {revokedCount > 0 && ` · ${revokedCount} revoked`}
          </span>
        )}
        <div style={{ marginLeft: 'auto', display: 'flex', gap: 'var(--sp-2)' }}>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => fetchDevices()}
            disabled={load.state === 'loading'}
          >
            ↻ Refresh
          </Button>
          <Button variant="primary" size="sm" onClick={() => setAddingMachine(true)}>
            + Add machine
          </Button>
        </div>
      </div>

      {/* Remounted on every open so a dismissed key is gone for good rather than
          sitting in state waiting to be reopened — the modal's whole premise is
          that the key is shown once. */}
      {addingMachine && <AddMachineModal onClose={() => setAddingMachine(false)} />}

      {load.state === 'loading' && (
        <Card>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              gap: 'var(--sp-3)',
              color: 'var(--sb-text-muted)',
              padding: 'var(--sp-6)',
            }}
          >
            <Spinner /> Loading devices…
          </div>
        </Card>
      )}

      {load.state === 'error' && (
        <Card>
          <div className="error-message">{load.message}</div>
        </Card>
      )}

      {load.state === 'ready' && devices.length === 0 && (
        <Card>
          <EmptyState
            icon="▱"
            title="No devices yet"
            hint="Add machine mints an enrollment key: install ScreenBuddy on the laptop or VM, choose “Enrol this machine”, and paste the key. It appears here as soon as it checks in."
          />
        </Card>
      )}

      {load.state === 'ready' && devices.length > 0 && (
        <div style={{ flex: 1, minHeight: 0, display: 'flex', gap: 'var(--sp-4)' }}>
          <Card padded={false} style={{ width: 300, flexShrink: 0, overflowY: 'auto' }}>
            {devices.map((device, i) => (
              <div key={device.device_id}>
                {i > 0 && <Divider style={{ margin: 0 }} />}
                <DeviceRow
                  device={device}
                  selected={device.device_id === selected?.device_id}
                  isThisMachine={device.device_id === thisDeviceId}
                  onClick={() => setSelectedId(device.device_id)}
                />
              </div>
            ))}
          </Card>

          {selected && (
            <div style={{ flex: 1, minWidth: 0, overflowY: 'auto' }}>
              <DeviceDetail
                // Remount on selection so every editable field re-seeds from the
                // newly selected device instead of carrying the previous one's
                // half-typed draft across.
                key={selected.device_id}
                device={selected}
                isThisMachine={selected.device_id === thisDeviceId}
                // The FLEET run view, not runs/:runId. A run executed on another
                // machine has no local agent:// stream and no screenshots on
                // this disk, so the local detail view renders it as a live panel
                // that never streams and a gallery of paths that do not resolve.
                //
                // Nested under the machine, not at a flat /fleet/runs/<id>: the
                // run belongs to a machine, and the URL is what tells the run
                // view where "back" goes when someone lands on it cold.
                onOpenRun={(runId) =>
                  navigate(
                    `/devices/${encodeURIComponent(selected.device_id)}/runs/${encodeURIComponent(runId)}`,
                  )
                }
                onOpenRuns={() =>
                  navigate(`/devices/${encodeURIComponent(selected.device_id)}/runs`)
                }
                onOpenTask={(taskId) =>
                  navigate(
                    `/devices/${encodeURIComponent(selected.device_id)}/tasks/${encodeURIComponent(taskId)}`,
                  )
                }
                onChanged={() => fetchDevices(true)}
                onAddMachine={() => setAddingMachine(true)}
              />
            </div>
          )}
        </div>
      )}
    </div>
  )
}

// ───────────────────────────────────────────────────────── add machine

// What POST /enroll/keys answers with. The key is plaintext here and nowhere
// else — the backend stores only a hash, so this response is the single moment
// it exists in readable form.
interface EnrollKey {
  key: string
  expires_at: string
}

type Mint =
  | { state: 'minting' }
  | { state: 'error'; message: string }
  | { state: 'ready'; key: EnrollKey }

// "expires in 58 minutes" — the TTL as a duration, because "expires at 14:32" is
// a number the reader then has to subtract from their own clock while standing
// at a different machine.
function expiresInWords(expiresAt: string): string {
  const ms = Date.parse(expiresAt) - Date.now()
  if (!Number.isFinite(ms) || ms <= 0) return 'expired'
  const minutes = Math.round(ms / 60_000)
  if (minutes < 1) return 'expires in under a minute'
  if (minutes === 1) return 'expires in 1 minute'
  if (minutes < 60) return `expires in ${minutes} minutes`
  const hours = Math.round(minutes / 60)
  return `expires in ${hours === 1 ? '1 hour' : `${hours} hours`}`
}

// Add machine: mint a one-time enrollment key and put it in front of the
// operator once. Minting starts on open rather than behind a second button —
// opening this is already the decision, and a key that goes unused just expires.
//
// Copy is the primary action, not Done: the operator is carrying this string to
// another machine, and the key is long enough that reading it off the screen and
// typing it is a mistake waiting to happen.
function AddMachineModal({ onClose }: { onClose: () => void }) {
  const [mint, setMint] = useState<Mint>({ state: 'minting' })
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    let alive = true
    ;(async () => {
      try {
        const resp = await fetch(`${CU_BACKEND}/enroll/keys`, {
          method: 'POST',
          headers: { ...authHeaders(), 'content-type': 'application/json' },
        })
        if (!alive) return
        if (!resp.ok) {
          setMint({ state: 'error', message: `Could not mint a key (${resp.status})` })
          return
        }
        const data: EnrollKey = await resp.json()
        setMint({ state: 'ready', key: data })
      } catch (err) {
        if (!alive) return
        setMint({
          state: 'error',
          message: err instanceof Error ? err.message : 'Network error',
        })
      }
    })()
    return () => {
      alive = false
    }
  }, [])

  const copy = useCallback(() => {
    if (mint.state !== 'ready') return
    navigator.clipboard?.writeText(mint.key.key)
    setCopied(true)
    setTimeout(() => setCopied(false), 1600)
  }, [mint])

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Add machine"
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 2000,
        background: 'rgba(0, 0, 0, 0.72)',
        backdropFilter: 'blur(2px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        padding: 'var(--sp-6)',
      }}
    >
      <div
        style={{
          width: '100%',
          maxWidth: 480,
          background: 'var(--sb-surface-1)',
          border: '1px solid var(--sb-border-gold)',
          borderRadius: 'var(--r-lg)',
          boxShadow: 'var(--shadow-2)',
          overflow: 'hidden',
        }}
      >
        <div
          style={{
            padding: '14px 20px',
            borderBottom: '1px solid var(--sb-border)',
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-2)',
          }}
        >
          <span aria-hidden style={{ fontSize: 16 }}>
            ▱
          </span>
          <SectionTitle>Add machine</SectionTitle>
        </div>

        <div
          style={{ padding: 20, display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}
        >
          {mint.state === 'minting' && (
            <div
              style={{
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
                gap: 'var(--sp-3)',
                color: 'var(--sb-text-muted)',
                padding: 'var(--sp-5)',
              }}
            >
              <Spinner /> Minting a key…
            </div>
          )}

          {mint.state === 'error' && <div className="error-message">{mint.message}</div>}

          {mint.state === 'ready' && (
            <>
              <div
                style={{
                  fontFamily: 'var(--font-mono)',
                  fontSize: 'var(--fs-lg)',
                  lineHeight: 1.5,
                  wordBreak: 'break-all',
                  color: 'var(--sb-gold-bright)',
                  background: 'var(--sb-surface-3)',
                  border: '1px solid var(--sb-border)',
                  borderRadius: 'var(--r-md)',
                  padding: 'var(--sp-3)',
                  // Selectable so copy has a manual fallback in a browser that
                  // denies clipboard access.
                  userSelect: 'all',
                }}
              >
                {mint.key.key}
              </div>

              <Button variant="primary" onClick={copy} style={{ justifyContent: 'center' }}>
                {copied ? '✓ Copied' : 'Copy key'}
              </Button>

              <div
                style={{
                  fontSize: 'var(--fs-md)',
                  fontWeight: 600,
                  color: 'var(--sb-danger-bright)',
                }}
              >
                This key will not be shown again — {expiresInWords(mint.key.expires_at)}.
              </div>

              <ol
                style={{
                  margin: 0,
                  paddingLeft: 18,
                  display: 'flex',
                  flexDirection: 'column',
                  gap: 4,
                  fontSize: 'var(--fs-md)',
                  lineHeight: 1.5,
                  color: 'var(--sb-text-muted)',
                }}
              >
                <li>Install ScreenBuddy on the machine.</li>
                <li>On its sign-in screen, choose “Enrol this machine”.</li>
                <li>Paste this key. It works once.</li>
              </ol>
            </>
          )}

          <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
            <Button variant="secondary" onClick={onClose}>
              {mint.state === 'ready' ? 'Done' : 'Close'}
            </Button>
          </div>
        </div>
      </div>
    </div>
  )
}

// ───────────────────────────────────────────────────────── list

// The display name an admin gave the machine, or what the machine calls itself.
function displayName(device: Device): string {
  return device.name?.trim() || device.hostname
}

// This machine's own device id, or null when we can't know it — which is every
// browser tab, since the id lives in a file only the desktop app can read.
//
// Worth the extra call: the machine you are sitting at registers itself like any
// other, so without this its row is indistinguishable from the fleet it is
// supervising. Marking it beats filtering it out — it really can run agents, and
// hiding it would make "3 devices" disagree with what you can actually see.
function useThisDeviceId(): string | null {
  const [id, setId] = useState<string | null>(null)
  useEffect(() => {
    let alive = true
    safeInvoke<{ device_id: string }>('device_info').then((res) => {
      if (alive && res.ok) setId(res.data.device_id)
    })
    return () => {
      alive = false
    }
  }, [])
  return id
}

// The "this machine" tag. Muted rather than gold: it is an orientation aid, not
// a status, and it must not compete with the online dot or the mid-run warning.
function ThisMachineTag() {
  return (
    <span
      style={{
        flexShrink: 0,
        fontSize: 'var(--fs-xs)',
        fontWeight: 600,
        letterSpacing: '0.04em',
        textTransform: 'uppercase',
        color: 'var(--sb-text-muted)',
        border: '1px solid var(--sb-border)',
        borderRadius: 'var(--r-pill)',
        padding: '1px 7px',
        whiteSpace: 'nowrap',
      }}
    >
      this machine
    </span>
  )
}

// The "access revoked" tag. Shaped like ThisMachineTag rather than a danger
// badge on purpose: a revoked machine is not broken and not an alert, it is a
// machine somebody switched off deliberately, and dressing that as a warning
// would put a red mark on the operator's own decision. The slash carries the
// "locked out" reading that colour is not allowed to.
//
// Only 'revoked' gets a tag. 'not_enrolled' is the operator's own machine
// behaving exactly as intended, and tagging it would make the normal case look
// like the exceptional one.
function RevokedTag() {
  return (
    <span
      style={{
        flexShrink: 0,
        fontSize: 'var(--fs-xs)',
        fontWeight: 600,
        letterSpacing: '0.04em',
        textTransform: 'uppercase',
        color: 'var(--sb-text-muted)',
        border: '1px solid var(--sb-text-faint)',
        borderRadius: 'var(--r-pill)',
        padding: '1px 7px',
        whiteSpace: 'nowrap',
      }}
    >
      ⃠ revoked
    </span>
  )
}

// A machine that is offline while still holding a current_run_id did not finish
// and did not stop — it vanished mid-task, and the run behind it will sit at
// "running" until something reconciles it. This is the one condition on this
// screen worth interrupting someone over.
//
// Not raised for a revoked machine: it went quiet because its pass was pulled,
// which is a known cause with a known cure. Calling that a mystery would send
// someone looking for a dead laptop that is sitting there fine. The stranded run
// is still said out loud — in the access card, where the reason is.
function diedMidRun(device: Device): boolean {
  return !device.online && !!device.current_run_id && device.enrollment_state !== 'revoked'
}

// One row in the left list. Everything here is sized to be legible at a glance
// from a few feet away: the dot carries the state, the name carries the weight,
// and the third line is the only place detail is allowed.
function DeviceRow({
  device,
  selected,
  isThisMachine,
  onClick,
}: {
  device: Device
  selected: boolean
  isThisMachine: boolean
  onClick: () => void
}) {
  const alert = diedMidRun(device)
  return (
    <button
      onClick={onClick}
      style={{
        display: 'flex',
        gap: 'var(--sp-3)',
        width: '100%',
        textAlign: 'left',
        padding: '12px 16px',
        background: selected ? 'var(--sb-gold-dim)' : 'transparent',
        border: 'none',
        // The selected row is the one the right pane is showing, so it gets a
        // gold spine as well as a wash — the wash alone is easy to lose against
        // the hover state of a neighbouring row.
        borderLeft: `2px solid ${selected ? 'var(--sb-gold)' : 'transparent'}`,
        cursor: 'pointer',
        color: 'var(--sb-text)',
        font: 'inherit',
        transition: 'background 0.15s ease',
      }}
      onMouseEnter={(e) => {
        if (!selected) e.currentTarget.style.background = 'var(--sb-surface-3)'
      }}
      onMouseLeave={(e) => {
        if (!selected) e.currentTarget.style.background = 'transparent'
      }}
    >
      <StatusDot
        online={device.online}
        revoked={device.enrollment_state === 'revoked'}
        style={{ marginTop: 5 }}
      />
      <span style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 2 }}>
        <span style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', minWidth: 0 }}>
          <span
            style={{
              fontSize: 'var(--fs-base)',
              fontWeight: 600,
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}
          >
            {displayName(device)}
          </span>
          {isThisMachine && <ThisMachineTag />}
          {device.enrollment_state === 'revoked' && <RevokedTag />}
        </span>
        <span
          style={{
            fontSize: 'var(--fs-sm)',
            color: 'var(--sb-text-muted)',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
        >
          {statusLine(device)}
        </span>
        {alert && (
          <span
            style={{
              fontSize: 'var(--fs-sm)',
              fontWeight: 600,
              color: 'var(--sb-danger-bright)',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}
          >
            ⚠ died mid-run
          </span>
        )}
      </span>
    </button>
  )
}

// The single status line under a device's name: what it is doing, or how long
// it has been gone.
function statusLine(device: Device): string {
  // A revoked machine reads as offline in every field the backend derives —
  // being locked out is exactly what stops it checking in — so saying "offline"
  // here would send someone to look at a machine that is powered on and fine.
  if (device.enrollment_state === 'revoked') {
    return `locked out · last seen ${relativeTime(device.last_seen)}`
  }
  if (!device.online) return `offline · ${relativeTime(device.last_seen)}`
  if (device.current_run_id) return 'running'
  return `idle · ${relativeTime(device.last_seen)}`
}

// Green when the machine has checked in inside the backend's liveness window,
// grey when it hasn't. Deliberately not a StatusPill: a pill's label competes
// with the device name, and at list scale the dot alone is the whole signal.
//
// A revoked machine gets a hollow ring instead of either fill. Both filled
// states are claims about the machine — it is up, it is not — and neither is
// what is being said about a machine that has been switched off at this end.
function StatusDot({
  online,
  revoked,
  style,
}: {
  online: boolean
  revoked?: boolean
  style?: React.CSSProperties
}) {
  return (
    <span
      aria-hidden
      style={{
        flexShrink: 0,
        boxSizing: 'border-box',
        width: 8,
        height: 8,
        borderRadius: '50%',
        background: revoked ? 'transparent' : online ? 'var(--sb-success)' : 'var(--sb-text-faint)',
        border: revoked ? '1.5px solid var(--sb-text-muted)' : undefined,
        ...style,
      }}
    />
  )
}

// ───────────────────────────────────────────────────────── detail

// The right pane: machine facts, the run in flight, remote access, and the
// three editable fields. Owns its own draft state — the list keeps polling
// underneath, and a poll landing mid-edit must not rewrite what is being typed.
function DeviceDetail({
  device,
  isThisMachine,
  onOpenRun,
  onOpenRuns,
  onOpenTask,
  onChanged,
  onAddMachine,
}: {
  device: Device
  isThisMachine: boolean
  onOpenRun: (runId: string) => void
  /** Leave the pane for this machine's runs page. The pane previews two; the
   *  page is the whole record. */
  onOpenRuns: () => void
  /** Open one task's page — its diary thread and verdict controls. */
  onOpenTask: (taskId: string) => void
  onChanged: () => void
  /** Open the mint-a-key modal. Reachable from here as well as the header
   *  because a machine that was just revoked is the single most likely thing to
   *  need a new key, and sending someone back to the top of the page to find the
   *  button leaves the connection to make on their own. */
  onAddMachine: () => void
}) {
  const [name, setName] = useState(device.name ?? '')
  const [rustdeskId, setRustdeskId] = useState(device.rustdesk_id ?? '')
  const [notes, setNotes] = useState(device.notes ?? '')
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)
  const [forgetting, setForgetting] = useState(false)
  // Which destructive action is awaiting confirmation, if any. Revoke and Forget
  // are one click apart and only one is reversible, so each dialog also says
  // what the OTHER would have done — the choice between them is the part the
  // operator has usually not made yet.
  const [confirming, setConfirming] = useState<'revoke' | 'forget' | null>(null)
  const [revoking, setRevoking] = useState(false)

  const revoked = device.enrollment_state === 'revoked'
  // No worker pass means this row is the machine running the console, not a
  // worker (see isWorker). It changes what this pane is FOR, so it is read
  // once here rather than per card.
  const isConsole = !isWorker(device)

  // Compare against the server's nulls as empty strings, so clearing a field
  // that was already null doesn't read as an unsaved change.
  const dirty =
    name !== (device.name ?? '') ||
    rustdeskId !== (device.rustdesk_id ?? '') ||
    notes !== (device.notes ?? '')

  const save = useCallback(async () => {
    setSaving(true)
    setSaveError(null)
    try {
      const resp = await fetch(`${CU_BACKEND}/devices/${encodeURIComponent(device.device_id)}`, {
        method: 'PATCH',
        headers: { ...authHeaders(), 'content-type': 'application/json' },
        // Empty means "unset" — send null rather than "" so the backend stores
        // the same absence it started with and the fallbacks keep working.
        body: JSON.stringify({
          name: name.trim() || null,
          rustdesk_id: rustdeskId.trim() || null,
          notes: notes.trim() || null,
        }),
      })
      if (!resp.ok) {
        setSaveError(`Save failed (${resp.status})`)
        return
      }
      setSaved(true)
      setTimeout(() => setSaved(false), 1500)
      onChanged()
    } catch (err) {
      setSaveError(err instanceof Error ? err.message : 'Network error')
    } finally {
      setSaving(false)
    }
  }, [device.device_id, name, rustdeskId, notes, onChanged])

  // Revoke and Forget are one HTTP call apart and worlds apart in consequence,
  // so each confirm says what the OTHER one would have done. This is the pair
  // someone reaches for in a hurry, having decided only that a machine should
  // stop — the choice between them is the part they have not made yet.
  const revoke = useCallback(async () => {
    setConfirming(null)
    setRevoking(true)
    setSaveError(null)
    try {
      const resp = await fetch(
        `${CU_BACKEND}/devices/${encodeURIComponent(device.device_id)}/revoke`,
        { method: 'POST', headers: authHeaders() },
      )
      if (!resp.ok) {
        setSaveError(`Revoke failed (${resp.status})`)
        return
      }
      onChanged()
    } catch (err) {
      setSaveError(err instanceof Error ? err.message : 'Network error')
    } finally {
      setRevoking(false)
    }
  }, [device, onChanged])

  const forget = useCallback(async () => {
    setConfirming(null)
    setForgetting(true)
    setSaveError(null)
    try {
      const resp = await fetch(`${CU_BACKEND}/devices/${encodeURIComponent(device.device_id)}`, {
        method: 'DELETE',
        headers: authHeaders(),
      })
      if (!resp.ok) {
        setSaveError(`Forget failed (${resp.status})`)
        return
      }
      onChanged()
    } catch (err) {
      setSaveError(err instanceof Error ? err.message : 'Network error')
    } finally {
      setForgetting(false)
    }
  }, [device, onChanged])

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
      {confirming === 'revoke' && (
        <ConfirmModal
          title={`Revoke access for “${displayName(device)}”?`}
          body={[
            'Its pass stops working immediately: it can no longer run agents or check in.',
            'It stays in this list, keeping its name, RustDesk ID and notes, so you can hand ' +
              'it a new enrollment key whenever you want it back.',
            'To remove it from the fleet altogether, use Forget instead.',
          ]}
          confirmLabel="Revoke access"
          busy={revoking}
          onConfirm={revoke}
          onCancel={() => setConfirming(null)}
        />
      )}
      {confirming === 'forget' && (
        <ConfirmModal
          title={`Forget “${displayName(device)}”?`}
          body={[
            'It leaves the fleet entirely, taking the name, RustDesk ID and notes you set with ' +
              'it, and its pass stops working.',
            'Launching the app on that machine will NOT bring it back — only a new enrollment ' +
              'key will, as a blank device.',
            'To lock it out but keep this record, use Revoke access instead.',
          ]}
          confirmLabel="Forget device"
          danger
          busy={forgetting}
          onConfirm={forget}
          onCancel={() => setConfirming(null)}
        />
      )}
      <Card>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
          <h2 style={{ margin: 0, fontSize: 'var(--fs-xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
            {displayName(device)}
          </h2>
          {isThisMachine && <ThisMachineTag />}
          <div style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>
            <StatusDot online={device.online} revoked={revoked} />
            <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
              {revoked ? 'access revoked' : device.online ? 'online' : 'offline'}
            </span>
          </div>
        </div>
        {/* Machine facts: reported by the machine on check-in, never editable. */}
        <div style={{ marginTop: 'var(--sp-2)', fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
          {device.hostname} · {device.os} {device.os_version} · v{device.app_version} · last seen{' '}
          {relativeTime(device.last_seen)}
        </div>
        {diedMidRun(device) && (
          <div style={{ marginTop: 'var(--sp-3)' }} className="error-message">
            ⚠ This machine went offline while a run was still in flight. The run below never
            reported a result.
          </div>
        )}
      </Card>

      {/* Everything from here to ACCESS describes a machine DOING work, and the
          console's own machine never does any: it holds no worker pass, takes
          no tasks, uploads no frames. Rendered anyway, the pane was five cards
          of "not applicable" — a longer read that ended in less information
          than one sentence. So the work cards are simply absent, and the single
          card below says why once. */}
      {isConsole ? (
        <Card title={<SectionTitle>Console</SectionTitle>}>
          <div style={{ fontSize: 'var(--fs-md)', lineHeight: 1.6, color: 'var(--sb-text-muted)' }}>
            This machine runs the console. It signs in with your Google account rather than holding
            a worker pass, so it takes no tasks, answers no snapshots and records no runs — it is
            here because every install registers itself, and it is listed so the fleet count is
            honest.
          </div>
        </Card>
      ) : (
        <>
          <NowCard device={device} onOpenRun={onOpenRun} />

          {/* Under NOW because a task is the intent behind the run NOW shows:
              the run is what the machine is doing, the task is what it was
              told. Also where new work is handed out — the + New task modal
              posts for THIS machine. */}
          <TasksCard device={device} onOpenTask={onOpenTask} />

          {/* Directly under NOW: that card says a run exists, this one says
              what it is doing. Read the other way round they are the same
              machine's story in the right order. */}
          <ScreenCard device={device} />
        </>
      )}

      {/* Then the work itself. NOW and SCREEN are this second; RUNS is the last
          thing this machine did, and the door to everything it has ever done.
          Below it the pane stops describing the machine and starts configuring
          it — access, the way in, and the fields an admin types — which is the
          order someone reads in only after the answer above was not the one they
          wanted. */}
      {!isConsole && (
        <>
          <RunsCard device={device} onOpenRun={onOpenRun} onOpenRuns={onOpenRuns} />

          {/* ACCESS is about a worker pass, and the console has none — the card
              existed on this row only to say so, which the Console card above
              now says once. REMOTE DESKTOP goes for the same reason: its
              purpose is taking a screen back from an agent, and no agent ever
              drives this one. */}
          <AccessCard
            device={device}
            revoking={revoking}
            busy={saving || forgetting}
            onRevoke={() => setConfirming('revoke')}
            onAddMachine={onAddMachine}
          />

          <RemoteDesktopCard
            savedId={device.rustdesk_id}
            draft={rustdeskId}
            onDraftChange={setRustdeskId}
            dirty={rustdeskId !== (device.rustdesk_id ?? '')}
            saving={saving}
            onSave={save}
          />
        </>
      )}

      <Card title={<SectionTitle>Details</SectionTitle>}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
          <Field label="Name">
            <input
              className="agent-input"
              style={inputStyle}
              placeholder={device.hostname}
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </Field>
          <Field label="Notes">
            <input
              className="agent-input"
              style={inputStyle}
              placeholder="what this machine is for"
              value={notes}
              onChange={(e) => setNotes(e.target.value)}
            />
          </Field>
        </div>

        {saveError && (
          <div className="error-message" style={{ marginTop: 'var(--sp-3)' }}>
            {saveError}
          </div>
        )}

        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-3)',
            marginTop: 'var(--sp-4)',
          }}
        >
          <Button variant="primary" onClick={save} disabled={!dirty || saving || forgetting}>
            {saving ? 'Saving…' : 'Save'}
          </Button>
          {saved && (
            <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-success)' }}>Saved</span>
          )}
          <div style={{ marginLeft: 'auto' }}>
            <Button variant="danger" onClick={() => setConfirming('forget')} disabled={saving || forgetting}>
              {forgetting ? 'Forgetting…' : 'Forget'}
            </Button>
          </div>
        </div>
        <div style={{ marginTop: 'var(--sp-2)', fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
          {isConsole
            ? 'Forget removes this machine from the fleet along with everything you typed about it. It will register itself again the next time the app starts here.'
            : 'Forget removes this machine from the fleet along with everything you typed about it. To cut its access but keep the record, use Revoke access above.'}
        </div>
      </Card>
    </div>
  )
}

// ACCESS — whether this machine holds a working pass, and the one control that
// changes it.
//
// It is a card of its own, several inches from Forget, because those two buttons
// are the thing this screen is most likely to get wrong. Rendered side by side
// they read as the same act at two intensities; they are not. Revoke keeps the
// machine, its name and its notes and takes away the pass. Forget takes away the
// machine. So Revoke sits here with the state it acts on, in the neutral
// secondary style, and only Forget carries the danger colour.
//
// None of this is enforcement — the backend refuses a revoked machine's token
// whatever this pane renders. It is here so the operator can see which machines
// they have switched off, and switch one back on.
function AccessCard({
  device,
  revoking,
  busy,
  onRevoke,
  onAddMachine,
}: {
  device: Device
  revoking: boolean
  /** Another write on this device is in flight — save, forget — so the control
   *  here doesn't race it. */
  busy: boolean
  onRevoke: () => void
  onAddMachine: () => void
}) {
  const state = device.enrollment_state

  if (state === 'not_enrolled') {
    return (
      <Card title={<SectionTitle>Access</SectionTitle>}>
        <div style={{ fontSize: 'var(--fs-md)', lineHeight: 1.5, color: 'var(--sb-text-muted)' }}>
          This is the machine running the console, not a worker. It signs in with your Google
          account rather than holding a worker pass, so it takes no tasks and answers no snapshots —
          and there is nothing here to revoke.
        </div>
      </Card>
    )
  }

  if (state === 'revoked') {
    return (
      <Card title={<SectionTitle>Access</SectionTitle>}>
        <div style={{ fontSize: 'var(--fs-md)', lineHeight: 1.5, color: 'var(--sb-text)' }}>
          Access revoked. This machine is still in the fleet — everything you typed about it is
          kept — but its pass is dead, so it cannot run agents or check in.
        </div>
        {device.current_run_id && (
          <div
            style={{
              marginTop: 'var(--sp-2)',
              fontSize: 'var(--fs-md)',
              lineHeight: 1.5,
              color: 'var(--sb-text-muted)',
            }}
          >
            A run was still in flight when it lost access, so that run will never report a result.
          </div>
        )}
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-3)',
            marginTop: 'var(--sp-4)',
          }}
        >
          <Button variant="primary" onClick={onAddMachine}>
            + Add machine
          </Button>
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
            Mints a fresh key. Paste it on that machine to bring it straight back in.
          </span>
        </div>
      </Card>
    )
  }

  return (
    <Card title={<SectionTitle>Access</SectionTitle>}>
      <div style={{ fontSize: 'var(--fs-md)', lineHeight: 1.5, color: 'var(--sb-text-muted)' }}>
        Enrolled{device.enrolled_at ? ` ${relativeTime(device.enrolled_at)}` : ''} — this machine
        holds a worker pass and runs agents for the fleet.
      </div>
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--sp-3)',
          marginTop: 'var(--sp-4)',
        }}
      >
        <Button variant="secondary" onClick={onRevoke} disabled={revoking || busy}>
          {revoking ? 'Revoking…' : 'Revoke access'}
        </Button>
        <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
          Kills the pass and stops the machine dead, but keeps it here so you can re-enrol it.
        </span>
      </div>
    </Card>
  )
}

// NOW — what the machine is doing this second. The device record carries only
// the run id, so the task / step / model come from the same GET /runs/{id} the
// run detail route reads; the link hands the user off to that view rather than
// duplicating any of it here.
function NowCard({
  device,
  onOpenRun,
}: {
  device: Device
  onOpenRun: (runId: string) => void
}) {
  const runId = device.current_run_id
  const [run, setRun] = useState<CurrentRun | null>(null)

  useEffect(() => {
    if (!runId) {
      setRun(null)
      return
    }
    let cancelled = false
    ;(async () => {
      try {
        const resp = await fetch(`${CU_BACKEND}/runs/${encodeURIComponent(runId)}`, {
          headers: authHeaders(),
        })
        if (!resp.ok) return
        const data = await resp.json()
        if (cancelled) return
        setRun((data.run ?? data) as CurrentRun)
      } catch {
        // The id and the "open run" link are still useful without the details,
        // so a failed lookup degrades to those rather than to an error.
      }
    })()
    return () => {
      cancelled = true
    }
  }, [runId])

  if (!runId) {
    return (
      <Card title={<SectionTitle>Now</SectionTitle>}>
        <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
          {device.online ? 'Idle — no run in flight.' : 'Nothing was running when it went offline.'}
        </span>
      </Card>
    )
  }

  const steps = run?.num_steps
  const startedAt = run?.started_at ?? run?.created_at

  return (
    <Card title={<SectionTitle>Now</SectionTitle>}>
      <div style={{ display: 'flex', alignItems: 'flex-start', gap: 'var(--sp-4)' }}>
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ fontSize: 'var(--fs-base)', color: 'var(--sb-text)' }}>
            {run?.task || '(untitled task)'}
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
            {typeof steps === 'number' && <span>step {steps}</span>}
            {startedAt != null && <span>started {relativeTime(startedAt)}</span>}
            {run?.model && (
              <Badge mono tone="gold">
                {run.model}
              </Badge>
            )}
          </div>
        </div>
        <Button size="sm" onClick={() => onOpenRun(runId)}>
          Open run →
        </Button>
      </div>
    </Card>
  )
}

// ───────────────────────────────────────────────────────── tasks

// How many of a machine's most recent tasks the pane lists. Same reasoning as
// RUNS_PREVIEW below: the pane is the machine's present, not its archive, and
// an active task list is short by nature (one runs at a time).
const TASKS_PREVIEW = 5

// TASKS — the work this machine has been told to do, and the door to handing
// it more. Each row links to the task's own page (#/devices/<id>/tasks/<id>),
// where the diary thread and the verdict controls live; nothing is judged from
// here. Terminal tasks are filtered out — the pane shows standing intent, and
// the Inbox is where finished-but-unjudged work already announces itself.
function TasksCard({
  device,
  onOpenTask,
}: {
  device: Device
  onOpenTask: (taskId: string) => void
}) {
  const [tasks, setTasks] = useState<TaskRecord[] | null>(null)
  const [creating, setCreating] = useState(false)
  const [nonce, setNonce] = useState(0)

  useEffect(() => {
    let cancelled = false
    ;(async () => {
      try {
        const resp = await fetch(
          `${CU_BACKEND}/tasks?device_id=${encodeURIComponent(device.device_id)}&limit=50`,
          { headers: authHeaders() },
        )
        if (!resp.ok) {
          if (!cancelled) setTasks([])
          return
        }
        const body = await resp.json()
        if (cancelled) return
        const rows: TaskRecord[] = Array.isArray(body) ? body : (body.tasks ?? [])
        setTasks(rows.filter((t) => !taskIsTerminal(t.status)).slice(0, TASKS_PREVIEW))
      } catch {
        // An unlistable queue is an empty state, not an error card — the
        // machine above is unaffected.
        if (!cancelled) setTasks([])
      }
    })()
    return () => {
      cancelled = true
    }
  }, [device.device_id, nonce])

  const listed = tasks !== null && tasks.length > 0
  const worker = isWorker(device)

  return (
    <Card
      title={<SectionTitle>Tasks</SectionTitle>}
      actions={
        <Button
          variant="primary"
          size="sm"
          onClick={() => setCreating(true)}
          // The console's own machine holds no worker pass, so it never polls
          // for tasks. Work queued at it would sit `queued` forever with no
          // readback and no error — the quietest possible failure.
          disabled={!worker}
          title={
            worker
              ? 'Hand this machine a task'
              : 'This is the machine running the console, not a worker — it never picks up tasks.'
          }
        >
          + New task
        </Button>
      }
      padded={!listed}
    >
      {/* Remounted per open, AddMachineModal's idiom: a dismissed draft must
          not sit in state waiting to be reopened half-typed. */}
      {creating && (
        <NewTaskModal
          device={device}
          onClose={() => setCreating(false)}
          onCreated={() => {
            setCreating(false)
            setNonce((n) => n + 1)
          }}
        />
      )}

      {tasks === null && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-3)',
            color: 'var(--sb-text-muted)',
          }}
        >
          <Spinner size={14} /> Loading tasks…
        </div>
      )}

      {tasks !== null && tasks.length === 0 && (
        <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
          {worker
            ? 'Nothing queued or in flight. New task hands this machine work: it reads the spec back to you before it starts, and only you can mark the result done.'
            : 'This is the machine running the console, not a worker — it takes no tasks. Enrol another machine to hand out work.'}
        </span>
      )}

      {listed &&
        tasks.map((task, i) => (
          <div key={task.task_id}>
            {i > 0 && <Divider style={{ margin: 0 }} />}
            <button
              onClick={() => onOpenTask(task.task_id)}
              title="Open this task — its diary thread and its controls"
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
                font: 'inherit',
              }}
              onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--sb-gold-dim)')}
              onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
            >
              <TaskStatusPill status={task.status} />
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
                {task.title}
              </span>
              <span
                style={{
                  fontSize: 'var(--fs-sm)',
                  color: 'var(--sb-text-muted)',
                  whiteSpace: 'nowrap',
                }}
              >
                created {utcRelative(task.created_at)}
              </span>
            </button>
          </div>
        ))}
    </Card>
  )
}

// New task: title + spec, and the optional workspace for repo-touching work.
// Posts POST /tasks for THIS machine; the task is born `queued` and the worker
// answers with its readback, which lands in the Inbox for a verdict — so the
// modal's job ends at "queued", and it says so instead of pretending the work
// started.
function NewTaskModal({
  device,
  onClose,
  onCreated,
}: {
  device: Device
  onClose: () => void
  onCreated: (task: TaskRecord) => void
}) {
  const [title, setTitle] = useState('')
  const [spec, setSpec] = useState('')
  const [repo, setRepo] = useState('')
  const [branch, setBranch] = useState('')
  const [mode, setMode] = useState<'scratch' | 'existing'>('scratch')
  // The definition-of-done builder: criteria typed one at a time, Enter
  // appends. Local strings until submit — items only get ids server-side.
  const [checklist, setChecklist] = useState<string[]>([])
  const [checkDraft, setCheckDraft] = useState('')
  const [posting, setPosting] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const addCriterion = useCallback(() => {
    const text = checkDraft.trim()
    if (!text) return
    setChecklist((prev) => [...prev, text])
    setCheckDraft('')
  }, [checkDraft])

  const submit = useCallback(async () => {
    setPosting(true)
    setError(null)
    try {
      const body: Record<string, unknown> = {
        device_id: device.device_id,
        title: title.trim(),
        spec: spec.trim(),
      }
      // The workspace block only exists when the task touches a repo — an
      // empty one would fail the backend's min-length on `repo` anyway.
      if (repo.trim()) {
        body.workspace = {
          repo: repo.trim(),
          mode,
          ...(branch.trim() ? { branch: branch.trim() } : {}),
        }
      }
      // A criterion typed but not yet Entered still counts — losing it to a
      // missed keystroke is the kind of thing nobody notices until the worker
      // reads back a definition of done with a hole in it. Only sent when the
      // builder was used at all: the checklist edge lands with parallel
      // backend work, and TaskCreate's extra="forbid" must not break a plain
      // title+spec task against a backend that predates it.
      const criteria = [...checklist, ...(checkDraft.trim() ? [checkDraft.trim()] : [])]
      if (criteria.length > 0) body.checklist = criteria
      const resp = await fetch(`${CU_BACKEND}/tasks`, {
        method: 'POST',
        headers: { ...authHeaders(), 'content-type': 'application/json' },
        body: JSON.stringify(body),
      })
      if (!resp.ok) {
        setError(`The task was refused (${resp.status}).`)
        return
      }
      onCreated((await resp.json()) as TaskRecord)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Network error')
    } finally {
      setPosting(false)
    }
  }, [device.device_id, title, spec, repo, branch, mode, checklist, checkDraft, onCreated])

  const ready = title.trim().length > 0 && spec.trim().length > 0

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="New task"
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 2000,
        background: 'rgba(0, 0, 0, 0.72)',
        backdropFilter: 'blur(2px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        padding: 'var(--sp-6)',
      }}
    >
      <div
        style={{
          width: '100%',
          maxWidth: 560,
          background: 'var(--sb-surface-1)',
          border: '1px solid var(--sb-border-gold)',
          borderRadius: 'var(--r-lg)',
          boxShadow: 'var(--shadow-2)',
          overflow: 'hidden',
        }}
      >
        <div
          style={{
            padding: '14px 20px',
            borderBottom: '1px solid var(--sb-border)',
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-2)',
          }}
        >
          <SectionTitle>New task for {displayName(device)}</SectionTitle>
        </div>

        <div style={{ padding: 20, display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
          <Field label="Title">
            <input
              className="agent-input"
              style={inputStyle}
              placeholder="what this task is, in one line"
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              autoFocus
            />
          </Field>
          <Field label="Spec">
            <textarea
              className="agent-input"
              style={{ ...inputStyle, minHeight: 120, resize: 'vertical', lineHeight: 1.5 }}
              placeholder="the whole ask — the worker reads this back to you before it starts"
              value={spec}
              onChange={(e) => setSpec(e.target.value)}
            />
          </Field>
          {/* The checklist builder mirrors the task page's card: append and
              remove only, no editing a typed criterion in place — reword by
              removing and re-adding, so what gets queued is always exactly
              what was typed last. */}
          <Field label="Checklist">
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
              {checklist.map((item, i) => (
                <div
                  // Index keys are fine here: the list only appends and
                  // removes, and the rows hold no state of their own.
                  key={i}
                  style={{ display: 'flex', alignItems: 'flex-start', gap: 'var(--sp-2)' }}
                >
                  <span style={{ color: 'var(--sb-text-faint)', flexShrink: 0, lineHeight: 1.5 }}>
                    ○
                  </span>
                  <span
                    style={{
                      flex: 1,
                      minWidth: 0,
                      fontSize: 'var(--fs-md)',
                      lineHeight: 1.5,
                      color: 'var(--sb-text)',
                      whiteSpace: 'pre-wrap',
                    }}
                  >
                    {item}
                  </span>
                  <IconButton
                    size={22}
                    title="Remove this criterion"
                    onClick={() => setChecklist((prev) => prev.filter((_, j) => j !== i))}
                  >
                    ✕
                  </IconButton>
                </div>
              ))}
              <input
                className="agent-input"
                style={inputStyle}
                placeholder="optional — a criterion of done, Enter adds it"
                value={checkDraft}
                onChange={(e) => setCheckDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault()
                    addCriterion()
                  }
                }}
              />
            </div>
          </Field>
          <Field label="Repo">
            <input
              className="agent-input"
              style={inputStyle}
              placeholder="optional — only for work that touches a repo"
              value={repo}
              onChange={(e) => setRepo(e.target.value)}
            />
          </Field>
          {repo.trim() && (
            <>
              <Field label="Branch">
                <input
                  className="agent-input"
                  style={inputStyle}
                  placeholder="optional — the repo's default when blank"
                  value={branch}
                  onChange={(e) => setBranch(e.target.value)}
                />
              </Field>
              <Field label="Checkout">
                <select
                  className="agent-input"
                  style={inputStyle}
                  value={mode}
                  onChange={(e) => setMode(e.target.value as 'scratch' | 'existing')}
                >
                  <option value="scratch">scratch — clone fresh</option>
                  <option value="existing">existing — use the checkout it has</option>
                </select>
              </Field>
            </>
          )}

          {error && <div className="error-message">{error}</div>}

          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'flex-end',
              gap: 'var(--sp-3)',
              marginTop: 'var(--sp-2)',
            }}
          >
            <span
              style={{
                marginRight: 'auto',
                fontSize: 'var(--fs-sm)',
                color: 'var(--sb-text-faint)',
              }}
            >
              Queues the task — the worker's readback lands in the Inbox for your go-ahead.
            </span>
            <Button variant="secondary" onClick={onClose} disabled={posting}>
              Cancel
            </Button>
            <Button variant="primary" onClick={submit} disabled={!ready || posting}>
              {posting ? 'Queuing…' : 'Queue task'}
            </Button>
          </div>
        </div>
      </div>
    </div>
  )
}

// ───────────────────────────────────────────────────────── runs

// How many runs the pane previews. The full list is a page of its own now
// (#/devices/<id>/runs); what earns space HERE is recency, not the archive.
// This pane is read top-down as one machine's present state, and "the last thing
// it did" is part of that state — an idle machine that finished a minute ago and
// one that has been sitting there since Tuesday look identical otherwise. Two
// rows say that. Twelve pushed everything below them — access, remote desktop,
// the fields an admin types — out of reach behind a list.
const RUNS_PREVIEW = 2

// RUNS — the machine's two most recent, and the way to the rest.
//
// The header button is there whether or not any row is: on a machine with no
// runs it is the only thing that says where they would appear, and on a busy one
// it is the way out of a preview that is deliberately too short.
function RunsCard({
  device,
  onOpenRun,
  onOpenRuns,
}: {
  device: Device
  onOpenRun: (runId: string) => void
  onOpenRuns: () => void
}) {
  const { runs, isLive } = useDeviceRuns(device.device_id, RUNS_PREVIEW, device.current_run_id)
  const listed = runs !== null && runs.length > 0

  return (
    <Card
      title={<SectionTitle>Runs</SectionTitle>}
      actions={
        <Button variant="secondary" size="sm" onClick={onOpenRuns}>
          All runs →
        </Button>
      }
      padded={!listed}
    >
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
        <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
          This machine has not run anything yet.
        </span>
      )}

      {listed &&
        runs.map((run, i) => (
          <div key={run.run_id}>
            {i > 0 && <Divider style={{ margin: 0 }} />}
            <RunRow run={run} live={isLive(run)} onOpen={() => onOpenRun(run.run_id)} />
          </div>
        ))}
    </Card>
  )
}

// ───────────────────────────────────────────────────────── screen

// SCREEN — what the machine is looking at right now, and nothing else.
//
// NOW, above, can only say that a run exists and which step it is on. A worker
// grinding through a task and a worker stuck in a loop clicking the same button
// produce the identical card: a run id and a step count that keeps ticking. The
// frame is the only thing that separates them, and nobody is sitting at these
// machines to look — which is why it is rendered at a size you can read rather
// than as a thumbnail.
//
// It used to carry a strip of the last two dozen frames as well. That strip
// belonged to no run: it interleaved every frame this machine had ever uploaded,
// so the moment it had done more than one piece of work — the normal case — the
// order stopped meaning anything. Frames are the run's record and are rendered
// in the run's own timeline (FleetRun), where each one sits between the
// narration that decided on it and the action that followed.
//
// Workers upload each turn's frame to object storage and the backend hands back
// PRESIGNED, short-lived URLs. Nothing here holds one past a failed load: see
// `reportBadUrl`, which treats a broken image as an expired signature and asks
// the backend again, because a silently blank panel reads as a broken feature
// rather than as a URL that timed out.

// One uploaded frame, from ScreenshotOut (routers/screenshots.py): url,
// device_id, run_id, seq, content_type, created_at, expires_at.
interface Frame {
  /** Only has to be STABLE and comparable: it is what "is there a newer frame
   *  than the one on screen?" is answered with. */
  id: string
  url: string
  captured_at: string
  /** When the presigned URL stops working, when the backend bothers to say. */
  expires_at: string | null
}

type Snap =
  | { state: 'idle' }
  | { state: 'pending' }
  | { state: 'timeout' }
  | { state: 'error'; message: string }

// How often a RUNNING machine is checked for a newer frame. Fast enough to track
// a run turn by turn (a computer-use step is seconds, not milliseconds) without
// re-signing URLs faster than the worker produces frames.
const SCREEN_POLL_MS = 5_000

// The snapshot wait. POST /snapshot returns once the request is QUEUED — the
// machine still has to pick the command up, wake, capture and upload — so the
// frame lands seconds later, or never if nothing is listening. 45s is generous
// for a machine that is answering and short enough that one that is not gets
// SAID rather than left spinning.
const SNAPSHOT_POLL_MS = 2_000
const SNAPSHOT_TIMEOUT_MS = 45_000

// How long a presigned URL is assumed to stay good when the backend does not
// send an expiry. Only ever makes this pane re-fetch sooner than it needed to,
// which is the harmless direction to be wrong in.
const URL_ASSUMED_LIFE_MS = 4 * 60_000

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms))

// The wall-clock time a frame was taken, shown ALONGSIDE the relative time:
// "3m ago" says how stale the run is, while the clock time is what this frame
// gets compared against the next one on when the question is whether the machine
// has been repeating itself.
function clockTime(iso: string): string {
  const ms = Date.parse(iso)
  if (!Number.isFinite(ms)) return '—'
  return new Date(ms).toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  })
}

function firstString(row: Record<string, unknown>, keys: string[]): string | null {
  for (const key of keys) {
    const value = row[key]
    if (typeof value === 'string' && value) return value
  }
  return null
}

// ScreenshotOut deliberately does NOT return an object_key — a raw storage path
// is useless to a browser holding no credentials and would publish the fleet's
// key layout — so there is no server-side id to key React on.
//
// `created_at` stands in for one. It comes from the write path, is unique per
// capture in practice, and is stable across the re-signs that change `url` every
// few minutes — which is what a key has to be, since a lightbox left open across
// a poll must not swap the image out from under whoever is reading it.
function normalizeFrame(raw: unknown): Frame | null {
  if (!raw || typeof raw !== 'object') return null
  const row = raw as Record<string, unknown>
  const url = firstString(row, ['url'])
  // A row with no URL is nothing this pane can render, so it is dropped rather
  // than shown as a broken tile.
  if (!url) return null
  const capturedAt = firstString(row, ['created_at']) ?? ''
  return {
    id: capturedAt || url,
    url,
    captured_at: capturedAt,
    expires_at: firstString(row, ['expires_at']),
  }
}

// True once the backend's own stated expiry for a URL has passed. Used only to
// re-sign BEFORE showing a frame full-size; an expiry we were never told about
// falls back to URL_ASSUMED_LIFE_MS.
function expired(frame: Frame | null): boolean {
  if (!frame?.expires_at) return false
  const ms = Date.parse(frame.expires_at)
  return Number.isFinite(ms) && ms <= Date.now()
}

// A device row with no enrollment is not a worker. It is the machine running
// this console: every install self-registers, so the admin's own laptop appears
// in the fleet like any other, but it authenticates with the operator's Google
// session and never holds a worker pass. Everything the fleet asks OF a machine
// — a snapshot, a dispatch — is answered by a pass-holding worker, so on this
// row those controls cannot work, and offering them produced a bare "(409)".
export function isWorker(device: Pick<Device, 'enrollment_state'>): boolean {
  return device.enrollment_state !== 'not_enrolled'
}

function ScreenCard({ device }: { device: Device }) {
  const deviceId = device.device_id
  const revoked = device.enrollment_state === 'revoked'
  const running = device.online && !!device.current_run_id

  const [frame, setFrame] = useState<Frame | null>(null)
  // Whether the backend has answered at all yet. Separate from `frame` because
  // "still loading" and "this machine has never sent a frame" are different
  // things to say, and the second one is a statement, not a blank panel.
  const [loaded, setLoaded] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // When the URL currently on screen was signed, for the pre-enlarge freshness
  // check.
  const [fetchedAt, setFetchedAt] = useState(0)
  // A URL that failed to load and failed AGAIN after being re-signed. That is not
  // an expiry, so it is shown rather than retried.
  const [broken, setBroken] = useState(false)
  const [snap, setSnap] = useState<Snap>({ state: 'idle' })
  const [zoomed, setZoomed] = useState(false)

  // Which URLs have already been re-signed once. Without this, an object that is
  // genuinely gone (rather than merely expired) puts the pane in a fetch loop.
  const resignedRef = useRef<Set<string>>(new Set())
  // The id on screen, read from a ref so the poll and the snapshot wait don't
  // have to be rebuilt every time it changes.
  const shownIdRef = useRef<string | null>(null)
  // Set on unmount so an in-flight snapshot wait stops touching state after the
  // operator has selected another machine.
  const goneRef = useRef(false)
  useEffect(
    () => () => {
      goneRef.current = true
    },
    [],
  )

  // Fetch the newest frame, and report which one that is so a caller can tell
  // whether it changed without waiting for a re-render.
  //
  // `resign` is the difference between "poll" and "I need this URL to work right
  // now". A poll that adopted every response would hand the <img> a fresh
  // signature for the identical JPEG every few seconds and make the browser
  // re-download it; a re-sign has to replace the URL even though the frame is
  // the same one.
  const loadLatest = useCallback(
    async (resign = false): Promise<string | null> => {
      try {
        const resp = await fetch(
          `${CU_BACKEND}/devices/${encodeURIComponent(deviceId)}/screenshots/latest`,
          { headers: authHeaders() },
        )
        // 404 is "this machine has never uploaded a frame" — an older worker
        // build, or one that has never run — not a failure. It gets the empty
        // state, which says so, rather than an error card.
        if (resp.status === 404) {
          setFrame(null)
          setLoaded(true)
          setError(null)
          return null
        }
        if (!resp.ok) {
          setError(`Could not load the frame (${resp.status})`)
          setLoaded(true)
          return shownIdRef.current
        }
        const next = normalizeFrame(await resp.json())
        setLoaded(true)
        setError(null)
        if (!next) {
          setFrame(null)
          return null
        }
        if (resign || next.id !== shownIdRef.current) {
          setFrame(next)
          setBroken(false)
          setFetchedAt(Date.now())
          // Fresh signature: whatever failed before is worth one more attempt.
          resignedRef.current = new Set()
        }
        return next.id
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Network error')
        setLoaded(true)
        return shownIdRef.current
      }
    },
    [deviceId],
  )

  useEffect(() => {
    loadLatest()
  }, [loadLatest])

  useEffect(() => {
    shownIdRef.current = frame?.id ?? null
  }, [frame])

  // Only a machine that is mid-run is polled. An idle worker's screen does not
  // change on its own — nothing is driving it — so a timer against one would
  // re-sign URLs forever to re-show the same JPEG, and an offline machine cannot
  // answer at all. Take snapshot is how you look at an idle machine.
  useEffect(() => {
    if (!running) return
    const timer = setInterval(() => {
      // A missed poll is corrected by the next one; loadLatest already swallows
      // its own failures into `error`.
      loadLatest()
    }, SCREEN_POLL_MS)
    return () => clearInterval(timer)
  }, [running, loadLatest])

  const takeSnapshot = useCallback(async () => {
    const baseline = shownIdRef.current
    setSnap({ state: 'pending' })
    try {
      const resp = await fetch(`${CU_BACKEND}/devices/${encodeURIComponent(deviceId)}/snapshot`, {
        method: 'POST',
        headers: authHeaders(),
      })
      if (!resp.ok) {
        setSnap({ state: 'error', message: `The request was refused (${resp.status}).` })
        return
      }
    } catch (err) {
      setSnap({ state: 'error', message: err instanceof Error ? err.message : 'Network error' })
      return
    }

    // The POST only means QUEUED. Everything after it is the machine's to do, so
    // the wait is bounded and its end is a STATEMENT — it did not answer — rather
    // than a spinner that outlives the operator's patience.
    const deadline = Date.now() + SNAPSHOT_TIMEOUT_MS
    while (Date.now() < deadline) {
      await sleep(SNAPSHOT_POLL_MS)
      if (goneRef.current) return
      const id = await loadLatest()
      if (id && id !== baseline) {
        setSnap({ state: 'idle' })
        return
      }
    }
    if (!goneRef.current) setSnap({ state: 'timeout' })
  }, [deviceId, loadLatest])

  // An image that fails to load is, nearly always, a signature that expired while
  // this pane sat open. Ask the backend once per URL — a second failure is
  // something else (the object is gone) and re-fetching would loop.
  const reportBadUrl = useCallback(
    (url: string) => {
      if (resignedRef.current.has(url)) {
        setBroken(true)
        return
      }
      resignedRef.current.add(url)
      loadLatest(true)
    },
    [loadLatest],
  )

  const enlarge = useCallback(async () => {
    setZoomed(true)
    // The frame on screen keeps rendering from the copy the browser decoded when
    // the panel loaded, so a dead URL stays invisible until the enlarged view
    // fetches it again and opens onto nothing. Re-sign first whenever this URL is
    // old enough to be a risk.
    if (expired(frame) || Date.now() - fetchedAt > URL_ASSUMED_LIFE_MS) await loadLatest(true)
  }, [frame, fetchedAt, loadLatest])

  // A snapshot is a request made OF a machine, so it is only offered to one that
  // could conceivably answer. A powered-off or locked-out machine would take the
  // click, queue a command nobody will read, and leave the operator watching a
  // 45-second timer for an answer that was never possible.
  const blocked = !isWorker(device)
    ? 'This is the machine running the console, not a worker — it holds no worker pass, so it cannot be asked for a snapshot.'
    : revoked
      ? 'Access revoked — this machine cannot be asked for anything until it is re-enrolled.'
      : !device.online
        ? 'Offline — this machine is not checked in, so there is nothing to ask. The last frame it uploaded is below.'
        : null

  return (
    <Card
      title={<SectionTitle>Screen</SectionTitle>}
      actions={
        <Button
          size="sm"
          variant="secondary"
          onClick={takeSnapshot}
          disabled={!!blocked || snap.state === 'pending'}
          title={blocked ?? 'Ask this machine for a fresh frame'}
        >
          {snap.state === 'pending' ? 'Asking…' : '⧉ Take snapshot'}
        </Button>
      }
    >
      {blocked && (
        <div
          style={{
            marginBottom: 'var(--sp-3)',
            fontSize: 'var(--fs-md)',
            color: 'var(--sb-text-muted)',
          }}
        >
          {blocked}
        </div>
      )}

      {snap.state === 'pending' && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-2)',
            marginBottom: 'var(--sp-3)',
            fontSize: 'var(--fs-md)',
            color: 'var(--sb-text-muted)',
          }}
        >
          <Spinner size={14} /> Asked {displayName(device)} for a frame. It has to wake, capture and
          upload, so this takes a few seconds.
        </div>
      )}

      {snap.state === 'timeout' && (
        <div
          style={{
            marginBottom: 'var(--sp-3)',
            fontSize: 'var(--fs-md)',
            lineHeight: 1.5,
            color: 'var(--sb-text)',
          }}
        >
          No answer from {displayName(device)} in {Math.round(SNAPSHOT_TIMEOUT_MS / 1000)}s — it may
          be asleep, busy, or no longer listening. The request is still queued, so a frame will
          appear here if it ever lands.
        </div>
      )}

      {snap.state === 'error' && (
        <div className="error-message" style={{ marginBottom: 'var(--sp-3)' }}>
          {snap.message}
        </div>
      )}

      {!loaded && !error && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            gap: 'var(--sp-3)',
            padding: 'var(--sp-5)',
            color: 'var(--sb-text-muted)',
          }}
        >
          <Spinner /> Loading the latest frame…
        </div>
      )}

      {error && !frame && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
          <span className="error-message">{error}</span>
          <Button size="sm" variant="ghost" onClick={() => loadLatest(true)}>
            Retry
          </Button>
        </div>
      )}

      {loaded && !frame && !error && (
        <EmptyState
          icon="🖥"
          title="This machine has never sent a frame"
          hint={
            device.online
              ? 'Workers upload a screenshot each turn, so a frame appears here once it runs something. A machine on an older build never uploads any — Take snapshot is the quickest way to tell which of the two this is.'
              : 'Workers upload a screenshot each turn. This one has either never run, or is on a build from before frames were uploaded.'
          }
        />
      )}

      {frame && (
        <>
          {/* The point of the feature: the frame at a size you can read, not a
              thumbnail. Same well and gold-bordered treatment as the live panel's
              latest screenshot (AgentRunPanel), so the two read as one thing seen
              from two places. */}
          <div
            style={{
              display: 'flex',
              justifyContent: 'center',
              background: 'var(--sb-surface-2)',
              border: '1px solid var(--sb-border)',
              borderRadius: 'var(--r-md)',
              padding: 'var(--sp-3)',
            }}
          >
            {broken ? (
              <div
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: 'var(--sp-3)',
                  padding: 'var(--sp-5)',
                  fontSize: 'var(--fs-md)',
                  color: 'var(--sb-text-muted)',
                }}
              >
                This frame would not load, even after asking for a fresh link.
                <Button size="sm" variant="ghost" onClick={() => loadLatest(true)}>
                  Reload
                </Button>
              </div>
            ) : (
              <img
                src={frame.url}
                alt={`screen of ${displayName(device)}`}
                onClick={enlarge}
                onError={() => reportBadUrl(frame.url)}
                style={{
                  display: 'block',
                  width: '100%',
                  maxWidth: 960,
                  height: 'auto',
                  borderRadius: 'var(--r-md)',
                  border: '1px solid var(--sb-border-gold)',
                  boxShadow: 'var(--shadow-2)',
                  cursor: 'zoom-in',
                }}
              />
            )}
          </div>

          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              marginTop: 'var(--sp-2)',
              fontSize: 'var(--fs-sm)',
              color: 'var(--sb-text-muted)',
            }}
          >
            <span style={{ fontFamily: 'var(--font-mono)' }}>{clockTime(frame.captured_at)}</span>
            <span>· {relativeTime(frame.captured_at)}</span>
            {/* Say which of the two this panel is doing, so a frame that stops
                changing reads as a stuck machine rather than a stopped pane. */}
            <span style={{ marginLeft: 'auto' }}>
              {running
                ? `following the run · refreshes every ${Math.round(SCREEN_POLL_MS / 1000)}s`
                : 'not refreshing — this machine is idle'}
            </span>
          </div>
        </>
      )}

      {zoomed && frame && (
        <FrameLightbox frame={frame} onClose={() => setZoomed(false)} onBadUrl={reportBadUrl} />
      )}
    </Card>
  )
}

// Full-screen view of the current frame, with the same chrome as the run
// replay's lightbox (RunDetail). Kept separate rather than shared because that
// one is typed on RunEvent and resolves LOCAL paths through the Tauri asset
// protocol; this is a presigned remote URL, and the thing that matters here — a
// URL can die between the panel and this view — does not exist over there.
//
// There is nothing to page through: this pane holds exactly one frame, the
// latest. Stepping between a run's frames belongs to the run's timeline, where
// each frame sits in the order it was taken.
function FrameLightbox({
  frame,
  onClose,
  onBadUrl,
}: {
  frame: Frame
  onClose: () => void
  onBadUrl: (url: string) => void
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  return (
    <div
      onClick={onClose}
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 1000,
        background: 'rgba(0, 0, 0, 0.86)',
        backdropFilter: 'blur(2px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        padding: 'var(--sp-6)',
      }}
    >
      <Button
        variant="secondary"
        size="sm"
        onClick={(e) => {
          e.stopPropagation()
          onClose()
        }}
        style={{ position: 'absolute', top: 'var(--sp-4)', right: 'var(--sp-4)' }}
      >
        ✕ Close
      </Button>

      <img
        src={frame.url}
        alt={`frame from ${clockTime(frame.captured_at)}`}
        onClick={(e) => e.stopPropagation()}
        // A presigned URL can die between the panel loading and this opening. The
        // re-sign happens in the parent; closing is what keeps the operator from
        // staring at a black rectangle waiting for it.
        onError={() => {
          onBadUrl(frame.url)
          onClose()
        }}
        style={{
          maxWidth: '90vw',
          maxHeight: '86vh',
          objectFit: 'contain',
          borderRadius: 'var(--r-md)',
          border: '1px solid var(--sb-border-gold)',
          boxShadow: 'var(--shadow-2)',
        }}
      />

      <div
        style={{
          position: 'absolute',
          bottom: 'var(--sp-4)',
          left: '50%',
          transform: 'translateX(-50%)',
          fontSize: 'var(--fs-sm)',
          fontFamily: 'var(--font-mono)',
          color: 'var(--sb-text-muted)',
        }}
      >
        {clockTime(frame.captured_at)} · {relativeTime(frame.captured_at)}
      </div>
    </div>
  )
}

// ───────────────────────────────────────────────────────── remote desktop

// REMOTE DESKTOP — where the machine's RustDesk id is kept, so it is at hand
// when a run goes wrong and someone has to take the screen back.
//
// Copy-to-clipboard is the only action, deliberately. There WAS a "Take over"
// button that opened `rustdesk://<id>`; it never launched anything, first
// because the webview swallowed the navigation and then, once handed to the OS
// properly, because nothing had registered the scheme. A launcher that silently
// does nothing is worse than no launcher: it makes the operator doubt the id
// rather than reach for RustDesk. Opening RustDesk and pasting is a few seconds
// and always works.
//
// The id is edited HERE rather than in the Details card below because filling it
// in is the whole reason this pane is writable: a machine with no id is a
// machine nobody can reach when its run goes wrong.
function RemoteDesktopCard({
  savedId,
  draft,
  onDraftChange,
  dirty,
  saving,
  onSave,
}: {
  /** What the backend currently has. Null until someone sets it. */
  savedId: string | null
  draft: string
  onDraftChange: (id: string) => void
  dirty: boolean
  saving: boolean
  onSave: () => void
}) {
  const [copied, setCopied] = useState(false)
  const [editing, setEditing] = useState(false)
  const id = savedId?.trim() ?? ''

  const copy = useCallback(() => {
    if (!id) return
    navigator.clipboard?.writeText(id)
    setCopied(true)
    setTimeout(() => setCopied(false), 1200)
  }, [id])

  // Grouped in threes the way RustDesk itself shows a 9-digit id — the number
  // gets read off one screen and typed into another, and the grouping is what
  // makes that survivable.
  const pretty = useMemo(() => id.replace(/(\d{3})(?=\d)/g, '$1 '), [id])

  // No id on file, or the admin asked to change the one there is.
  if (!id || editing) {
    return (
      <Card title={<SectionTitle>Remote desktop</SectionTitle>}>
        {!id && (
          <div
            style={{
              fontSize: 'var(--fs-md)',
              color: 'var(--sb-text-muted)',
              marginBottom: 'var(--sp-3)',
            }}
          >
            No RustDesk ID on file. Read it off RustDesk on that machine and paste it here —
            without it there is no way to take the screen back from an agent.
          </div>
        )}
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
          <input
            className="agent-input"
            style={{ ...inputStyle, fontFamily: 'var(--font-mono)', maxWidth: 220 }}
            placeholder="427494427"
            value={draft}
            onChange={(e) => onDraftChange(e.target.value)}
            autoFocus={editing}
          />
          <Button
            variant="primary"
            size="sm"
            disabled={!dirty || saving}
            onClick={() => {
              setEditing(false)
              onSave()
            }}
          >
            {saving ? 'Saving…' : 'Save ID'}
          </Button>
          {editing && (
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                onDraftChange(savedId ?? '')
                setEditing(false)
              }}
            >
              Cancel
            </Button>
          )}
        </div>
      </Card>
    )
  }

  return (
    <Card title={<SectionTitle>Remote desktop</SectionTitle>}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)', flexWrap: 'wrap' }}>
        <button
          onClick={copy}
          title="Copy RustDesk ID"
          style={{
            font: 'inherit',
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--fs-lg)',
            letterSpacing: '1px',
            color: 'var(--sb-text)',
            background: 'var(--sb-surface-2)',
            border: '1px solid var(--sb-border)',
            borderRadius: 'var(--r-sm)',
            padding: '8px 12px',
            cursor: 'pointer',
          }}
        >
          {pretty}
        </button>
        <span
          style={{
            fontSize: 'var(--fs-sm)',
            color: copied ? 'var(--sb-success)' : 'var(--sb-text-faint)',
          }}
        >
          {copied ? 'Copied' : 'click to copy'}
        </span>
        <Button variant="ghost" size="sm" onClick={() => setEditing(true)}>
          Change
        </Button>
      </div>
      <p
        style={{
          marginTop: 'var(--sp-3)',
          fontSize: 'var(--fs-sm)',
          color: 'var(--sb-text-muted)',
        }}
      >
        Copy this into RustDesk to take the screen back from an agent.
      </p>
    </Card>
  )
}

// ───────────────────────────────────────────────────────── bits

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
      <span
        style={{
          width: 96,
          flexShrink: 0,
          fontSize: 'var(--fs-sm)',
          color: 'var(--sb-text-muted)',
        }}
      >
        {label}
      </span>
      {children}
    </label>
  )
}

const inputStyle: React.CSSProperties = {
  flex: 1,
  minWidth: 0,
  boxSizing: 'border-box',
  padding: '9px 11px',
  fontFamily: 'inherit',
  fontSize: 'var(--fs-base)',
  background: 'var(--sb-surface-3)',
  color: 'var(--sb-text)',
  border: '1px solid var(--sb-border)',
  borderRadius: 'var(--r-sm)',
}

export default Devices

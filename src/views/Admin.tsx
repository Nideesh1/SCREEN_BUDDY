import { useCallback, useEffect, useMemo, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime, safeInvoke } from '../lib'
import { Badge, Button, Card, Divider, EmptyState, SectionTitle, Spinner } from '../ui'

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
}

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
          </span>
        )}
        <div style={{ marginLeft: 'auto' }}>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => fetchDevices()}
            disabled={load.state === 'loading'}
          >
            ↻ Refresh
          </Button>
        </div>
      </div>

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
            hint="A machine shows up here the first time ScreenBuddy runs on it and signs in to this account — laptops and VMs alike. Nothing to add by hand."
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
                onOpenRun={(runId) => navigate('/runs/' + runId)}
                onChanged={() => fetchDevices(true)}
              />
            </div>
          )}
        </div>
      )}
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

// A machine that is offline while still holding a current_run_id did not finish
// and did not stop — it vanished mid-task, and the run behind it will sit at
// "running" until something reconciles it. This is the one condition on this
// screen worth interrupting someone over.
function diedMidRun(device: Device): boolean {
  return !device.online && !!device.current_run_id
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
      <StatusDot online={device.online} style={{ marginTop: 5 }} />
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
  if (!device.online) return `offline · ${relativeTime(device.last_seen)}`
  if (device.current_run_id) return 'running'
  return `idle · ${relativeTime(device.last_seen)}`
}

// Green when the machine has checked in inside the backend's liveness window,
// grey when it hasn't. Deliberately not a StatusPill: a pill's label competes
// with the device name, and at list scale the dot alone is the whole signal.
function StatusDot({ online, style }: { online: boolean; style?: React.CSSProperties }) {
  return (
    <span
      aria-hidden
      style={{
        flexShrink: 0,
        width: 8,
        height: 8,
        borderRadius: '50%',
        background: online ? 'var(--sb-success)' : 'var(--sb-text-faint)',
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
  onChanged,
}: {
  device: Device
  isThisMachine: boolean
  onOpenRun: (runId: string) => void
  onChanged: () => void
}) {
  const [name, setName] = useState(device.name ?? '')
  const [rustdeskId, setRustdeskId] = useState(device.rustdesk_id ?? '')
  const [notes, setNotes] = useState(device.notes ?? '')
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)
  const [forgetting, setForgetting] = useState(false)

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

  const forget = useCallback(async () => {
    const ok = window.confirm(
      `Forget “${displayName(device)}”?\n\n` +
        'It disappears from this list along with the name and notes you set. ' +
        'If that machine signs in again it comes back as a new, unnamed device.',
    )
    if (!ok) return
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
      <Card>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
          <h2 style={{ margin: 0, fontSize: 'var(--fs-xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
            {displayName(device)}
          </h2>
          {isThisMachine && <ThisMachineTag />}
          <div style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>
            <StatusDot online={device.online} />
            <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
              {device.online ? 'online' : 'offline'}
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

      <NowCard device={device} onOpenRun={onOpenRun} />

      <RemoteDesktopCard
        savedId={device.rustdesk_id}
        draft={rustdeskId}
        onDraftChange={setRustdeskId}
        dirty={rustdeskId !== (device.rustdesk_id ?? '')}
        saving={saving}
        onSave={save}
      />

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
            <Button variant="danger" onClick={forget} disabled={saving || forgetting}>
              {forgetting ? 'Forgetting…' : 'Forget'}
            </Button>
          </div>
        </div>
        <div style={{ marginTop: 'var(--sp-2)', fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>
          Forgetting only clears this record. The device reappears if that machine signs in again.
        </div>
      </Card>
    </div>
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

// REMOTE DESKTOP — the machine's RustDesk id, and the two ways to act on it.
//
// Both affordances exist on purpose. `rustdesk://` only launches anything on a
// desktop that has registered the scheme; when it hasn't, the click does
// NOTHING and there is no event to detect that from, so copy-to-clipboard is
// not a convenience but the fallback that keeps the pane useful. Copy is always
// offered, never hidden behind the Take-over button having failed.
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
        <div style={{ marginLeft: 'auto' }}>
          <Button
            variant="primary"
            onClick={() => {
              // Hands off to the OS handler. If nothing has registered
              // `rustdesk://` this is silently inert — which is exactly why the
              // id above stays copyable.
              window.location.href = `rustdesk://${encodeURIComponent(id)}`
            }}
          >
            Take over
          </Button>
        </div>
      </div>
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

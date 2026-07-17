// Shared helpers for the ScreenBuddy shell + views.

// The Computer-Use backend (runs history/telemetry). Same env var the auth hook
// uses so a single override points everything at one backend.
export const CU_BACKEND =
  import.meta.env.VITE_CU_BACKEND_URL || 'http://localhost:8000'

// Deadline for the best-effort artifact metadata mirror. `fetch` has no default
// timeout, so without this a stalled connection never settles and any caller
// awaiting it hangs forever — which is indistinguishable, from the UI's side,
// from the request never having been sent at all.
const MIRROR_TIMEOUT_MS = 8000

// Bearer header built from the backend session token (the only credential the
// renderer trusts — set by useGoogleAuth after the /auth/google exchange).
export function authHeaders(): Record<string, string> {
  const token = localStorage.getItem('screen_buddy_session_token')
  return token ? { Authorization: `Bearer ${token}` } : {}
}

// Reconcile orphaned "running" runs on app startup. The local executor is a
// single AgentState (one run at a time), so any run still marked "running" when
// the app boots is a zombie: its process died on a restart/rebuild without ever
// PATCHing a terminal status, leaving Mongo stuck at "running" forever. We fetch
// the user's runs, and for each with status EXACTLY "running", PATCH it to
// "cancelled". We deliberately leave "pending" alone (a dispatched-but-unstarted
// run may still be validly queued in the command stream) and never touch
// terminal statuses. Best effort: every failure is swallowed/logged so this can
// never block startup or crash the UI. Reuses the same bearer auth as every
// other /runs call (authHeaders + the same PATCH shape the Rust finalizer uses).
export async function reconcileOrphanedRuns(): Promise<void> {
  try {
    const resp = await fetch(`${CU_BACKEND}/runs`, { headers: authHeaders() })
    if (!resp.ok) {
      console.warn(`[reconcile] GET /runs failed (${resp.status})`)
      return
    }
    const data = await resp.json()
    const runs: Array<{ run_id?: string; status?: string }> = Array.isArray(data)
      ? data
      : (data.runs ?? [])
    const orphans = runs.filter((r) => r.status === 'running' && r.run_id)
    await Promise.all(
      orphans.map(async (r) => {
        try {
          const url = `${CU_BACKEND}/runs/${encodeURIComponent(r.run_id as string)}`
          const patch = await fetch(url, {
            method: 'PATCH',
            headers: { ...authHeaders(), 'content-type': 'application/json' },
            body: JSON.stringify({
              status: 'cancelled',
              error_message: 'orphaned: app restarted',
            }),
          })
          if (!patch.ok) {
            console.warn(`[reconcile] PATCH ${r.run_id} failed (${patch.status})`)
          }
        } catch (err) {
          console.warn(`[reconcile] PATCH ${r.run_id} error`, err)
        }
      }),
    )
  } catch (err) {
    // Never let reconcile block or crash startup.
    console.warn('[reconcile] skipped', err)
  }
}

// ---- Backend "set registry" (Mongo set_refs) ------------------------------
// The desktop owns the LOCAL pinned sets; these helpers mirror create/delete
// into the backend registry so a dispatched run can pin sets by uuid. All calls
// reuse the SAME user bearer auth (authHeaders) + CU_BACKEND base as every other
// backend call (GET /runs, the startup reconcile). Every call is best-effort:
// failures are logged and swallowed so backend registration can never block or
// crash local set CRUD.

// One template option for the "Link to template" picker.
export interface SetTemplate {
  template_id: string
  name: string
}

// Fetch the user's run templates for the create-set "Link to template" picker.
// Payload may be a bare array or { templates: [] } (same shape NewRun consumes).
// Returns [] on any failure so the picker degrades to just a "None" option.
export async function fetchTemplates(): Promise<SetTemplate[]> {
  try {
    const resp = await fetch(`${CU_BACKEND}/templates`, { headers: authHeaders() })
    if (!resp.ok) {
      console.warn(`[sets] GET /templates failed (${resp.status})`)
      return []
    }
    const body = (await resp.json()) as
      | Array<{ template_id?: string; name?: string }>
      | { templates?: Array<{ template_id?: string; name?: string }> }
    const raw = Array.isArray(body) ? body : body.templates ?? []
    return raw
      .filter((t) => !!t.template_id)
      .map((t) => ({
        template_id: t.template_id as string,
        name: t.name || (t.template_id as string),
      }))
  } catch (err) {
    console.warn('[sets] GET /templates error', err)
    return []
  }
}

// Register (upsert) a local pinned set with the backend registry so a dispatched
// run can resolve it → pinned_set_ids. Best-effort: returns true on 2xx, else
// false (logged) — never throws, so the caller can surface a small warning
// without blocking set creation.
export async function registerSet(
  setUuid: string,
  name: string,
  templateId: string | null,
): Promise<boolean> {
  try {
    const resp = await fetch(`${CU_BACKEND}/sets`, {
      method: 'POST',
      headers: { ...authHeaders(), 'content-type': 'application/json' },
      body: JSON.stringify({ set_uuid: setUuid, name, template_id: templateId }),
    })
    if (!resp.ok) {
      console.warn(`[sets] POST /sets failed (${resp.status})`)
      return false
    }
    return true
  } catch (err) {
    console.warn('[sets] POST /sets error', err)
    return false
  }
}

// Deregister a set from the backend registry when it's deleted locally.
// Best-effort: swallow every error.
export async function unregisterSet(setUuid: string): Promise<void> {
  try {
    const resp = await fetch(`${CU_BACKEND}/sets/${encodeURIComponent(setUuid)}`, {
      method: 'DELETE',
      headers: authHeaders(),
    })
    if (!resp.ok) {
      console.warn(`[sets] DELETE /sets/${setUuid} failed (${resp.status})`)
    }
  } catch (err) {
    console.warn('[sets] DELETE /sets error', err)
  }
}

// ---- Backend "artifact registry" (Mongo artifacts) ------------------------
// The desktop owns the LOCAL artifact library (the physical files live in the
// Tauri app data dir and NEVER leave the machine); these helpers mirror the
// METADATA ONLY into the backend so other surfaces can resolve an artifact by
// id. Same shape as the set-registry helpers above: same bearer auth, same
// CU_BACKEND base, all best-effort — a failure is logged and swallowed so
// backend registration can never block or crash local artifact CRUD.

// One artifact's metadata. Mirrors the Rust `ArtifactMeta` (artifacts.rs) field
// for field — this is a shared contract with the backend, so the names here must
// stay in lockstep with meta.json.
export interface ArtifactMeta {
  artifact_id: string // lowercase-hex SHA-256 of the file contents (64 chars)
  name: string // editable display name; defaults to original_filename
  original_filename: string
  kind: string // "image" | "video" | "pdf" | "text" | "other"
  mime: string
  size_bytes: number
  width?: number | null // images only
  height?: number | null // images only
  duration_ms?: number | null // video only
  created_at: string // ISO8601 UTC
}

// Register (upsert) a locally-imported artifact's metadata with the backend.
// Returns true on 2xx, else false (logged) — never throws, so the caller can
// surface a small warning without discarding the (already-imported) local file.
export async function registerArtifact(meta: ArtifactMeta): Promise<boolean> {
  try {
    const resp = await fetch(`${CU_BACKEND}/artifacts`, {
      method: 'POST',
      headers: { ...authHeaders(), 'content-type': 'application/json' },
      body: JSON.stringify(meta),
      signal: AbortSignal.timeout(MIRROR_TIMEOUT_MS),
    })
    if (!resp.ok) {
      console.warn(`[artifacts] POST /artifacts failed (${resp.status})`)
      return false
    }
    return true
  } catch (err) {
    console.warn('[artifacts] POST /artifacts error', err)
    return false
  }
}

// Mirror a rename to the backend. Best-effort: returns true on 2xx, else false.
export async function renameRemoteArtifact(id: string, name: string): Promise<boolean> {
  try {
    const resp = await fetch(`${CU_BACKEND}/artifacts/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      headers: { ...authHeaders(), 'content-type': 'application/json' },
      body: JSON.stringify({ name }),
      signal: AbortSignal.timeout(MIRROR_TIMEOUT_MS),
    })
    if (!resp.ok) {
      console.warn(`[artifacts] PATCH /artifacts/${id} failed (${resp.status})`)
      return false
    }
    return true
  } catch (err) {
    console.warn('[artifacts] PATCH /artifacts error', err)
    return false
  }
}

// Deregister an artifact when it's deleted locally. Best-effort: swallow every
// error (the local delete is what matters; the mirror can drift and re-sync).
export async function unregisterArtifact(id: string): Promise<void> {
  try {
    const resp = await fetch(`${CU_BACKEND}/artifacts/${encodeURIComponent(id)}`, {
      method: 'DELETE',
      headers: authHeaders(),
      signal: AbortSignal.timeout(MIRROR_TIMEOUT_MS),
    })
    if (!resp.ok) {
      console.warn(`[artifacts] DELETE /artifacts/${id} failed (${resp.status})`)
    }
  } catch (err) {
    console.warn('[artifacts] DELETE /artifacts error', err)
  }
}

// Compact "3m ago" / "2h ago" / "5d ago" relative time from an ISO string or
// epoch ms. Falls back to the raw value if it can't be parsed.
export function relativeTime(value: string | number | null | undefined): string {
  if (value === null || value === undefined) return '—'
  const ms = typeof value === 'number' ? value : Date.parse(value)
  if (!Number.isFinite(ms)) return String(value)
  const diff = Date.now() - ms
  const sec = Math.round(diff / 1000)
  if (sec < 60) return 'just now'
  const min = Math.round(sec / 60)
  if (min < 60) return `${min}m ago`
  const hr = Math.round(min / 60)
  if (hr < 24) return `${hr}h ago`
  const day = Math.round(hr / 24)
  if (day < 7) return `${day}d ago`
  const wk = Math.round(day / 7)
  if (wk < 5) return `${wk}w ago`
  return new Date(ms).toLocaleDateString()
}

// Wrap a Tauri invoke so a not-yet-implemented command (the Rust agents merge in
// parallel) never crashes the UI. Returns { ok, data } | { ok:false, error }.
export type InvokeResult<T> =
  | { ok: true; data: T }
  | { ok: false; error: string }

export async function safeInvoke<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<InvokeResult<T>> {
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    const data = (await invoke(command, args)) as T
    return { ok: true, data }
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : String(err) }
  }
}

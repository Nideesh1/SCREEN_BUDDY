// Shared helpers for the ScreenBuddy shell + views.

// Host platform, for the handful of places the UI must differ (the Permissions
// card, which is macOS-only in substance). Read from the webview UA rather than
// @tauri-apps/plugin-os so this needs no new plugin, Rust registration, or
// capability entry: WebView2 always reports "Windows" and WKWebView never does.
//
// Prefer platform-NEUTRAL copy over branching on this. Most user-facing strings
// say "this computer" precisely so they read correctly on every build.
export const IS_WINDOWS = /Windows/.test(navigator.userAgent)

// The Computer-Use backend (runs history/telemetry). Same env var the auth hook
// uses so a single override points everything at one backend.
export const CU_BACKEND =
  import.meta.env.VITE_CU_BACKEND_URL || 'http://localhost:8000'

// The models the launcher offers. ONE list, imported by every picker (New Run,
// Templates, Schedule detail) — they were three copies before, with a comment
// asking the next editor to keep them "in lockstep" by hand.
//
// `value` is sent verbatim as the Messages request's `model`. It reaches
// whatever `CU_ANTHROPIC_BASE` points at, so a self-hosted endpoint that routes
// by its own configuration will ignore it — but the run history records this
// string, so an entry that lies about which model ran is a debugging trap. Add
// an option here for any endpoint you actually run against.
export const MODEL_OPTIONS: { value: string; label: string }[] = [
  { value: 'claude-sonnet-5', label: 'Claude Sonnet 5' },
  { value: 'claude-opus-4-8', label: 'Claude Opus 4.8' },
  { value: 'qwen38', label: 'Qwen3.8 27B (self-hosted)' },
]

// Fallback when nothing else (a template, a schedule) specifies one.
export const DEFAULT_MODEL = MODEL_OPTIONS[0].value

// Deadline for the best-effort artifact metadata mirror. `fetch` has no default
// timeout, so without this a stalled connection never settles and any caller
// awaiting it hangs forever — which is indistinguishable, from the UI's side,
// from the request never having been sent at all.
const MIRROR_TIMEOUT_MS = 8000

// Bearer header built from the backend session token (the only credential the
// renderer trusts — set by useGoogleAuth after the /auth/google exchange).
//
// An ENROLLED machine has nothing to put here: its device token lives in the
// Rust credential store and never crosses into the webview, so a fetch made from
// this file is unauthenticated on a worker. Anything a worker shell needs from
// the backend has to go through a Rust command, which carries whichever
// credential the machine actually holds.
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

// The canonical run-template shape, field-for-field the backend contract
// (snake_case). Templates are now per-user editable via the CRUD helpers below;
// `GET /templates` auto-seeds the built-ins for a user with none. NewRun reads a
// subset of these to prefill the launcher; the Templates manager edits them all.
export interface Template {
  template_id: string
  name: string
  task_scaffold: string
  model: string
  suggested_set_name: string
  set_names: string[]
  credential_target: string
  required_inputs: string[]
  builtin: boolean
  created_at: string
  updated_at: string
}

// One template option for the "Link to template" picker — a two-field subset of
// the canonical Template. Kept as a narrow projection so picker callers (e.g.
// PinnedLibrary) stay decoupled from the full shape.
export type SetTemplate = Pick<Template, 'template_id' | 'name'>

// Body accepted by POST /templates. Only name + task_scaffold are required; the
// rest fall back to backend defaults when omitted.
export interface CreateTemplateBody {
  name: string
  task_scaffold: string
  model?: string
  suggested_set_name?: string
  credential_target?: string
  set_names?: string[]
  required_inputs?: string[]
}

// Fields patchable via PATCH /templates/{id} — any subset of the writable fields.
export type TemplatePatch = Partial<CreateTemplateBody>

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

// ---- Template CRUD (owner-scoped /templates) ------------------------------
// Full management client for the per-user run templates the Templates manager
// edits. Same bearer auth + CU_BACKEND base as every other backend call. Unlike
// the best-effort mirror helpers above, these THROW on failure so the manager
// UI can surface an error and roll back its optimistic state. The picker-lite
// `fetchTemplates` above stays as-is for callers that only need id + name.

// Shared JSON headers (bearer auth + content-type) for template write calls.
function templateJsonHeaders(): Record<string, string> {
  return { ...authHeaders(), 'content-type': 'application/json' }
}

// Throw a readable, status-carrying error for any non-2xx template response.
async function ensureTemplateOk(resp: Response, what: string): Promise<void> {
  if (!resp.ok) {
    throw new Error(`${what} failed (HTTP ${resp.status})`)
  }
}

// GET /templates → Template[] (full shape; auto-seeds built-ins when the user
// has none). Payload may be a bare array or `{ templates: [] }`.
export async function listTemplates(): Promise<Template[]> {
  const resp = await fetch(`${CU_BACKEND}/templates`, { headers: authHeaders() })
  await ensureTemplateOk(resp, 'List templates')
  const data = await resp.json()
  return Array.isArray(data) ? (data as Template[]) : ((data.templates ?? []) as Template[])
}

// POST /templates → Template
export async function createTemplate(body: CreateTemplateBody): Promise<Template> {
  const resp = await fetch(`${CU_BACKEND}/templates`, {
    method: 'POST',
    headers: templateJsonHeaders(),
    body: JSON.stringify(body),
  })
  await ensureTemplateOk(resp, 'Create template')
  return (await resp.json()) as Template
}

// PATCH /templates/{id} → Template
export async function updateTemplate(id: string, patch: TemplatePatch): Promise<Template> {
  const resp = await fetch(`${CU_BACKEND}/templates/${encodeURIComponent(id)}`, {
    method: 'PATCH',
    headers: templateJsonHeaders(),
    body: JSON.stringify(patch),
  })
  await ensureTemplateOk(resp, 'Update template')
  return (await resp.json()) as Template
}

// DELETE /templates/{id} → 204
export async function deleteTemplate(id: string): Promise<void> {
  const resp = await fetch(`${CU_BACKEND}/templates/${encodeURIComponent(id)}`, {
    method: 'DELETE',
    headers: authHeaders(),
  })
  await ensureTemplateOk(resp, 'Delete template')
}

// POST /templates/seed → Template[] (idempotent — restores the built-in set).
export async function seedTemplates(): Promise<Template[]> {
  const resp = await fetch(`${CU_BACKEND}/templates/seed`, {
    method: 'POST',
    headers: templateJsonHeaders(),
  })
  await ensureTemplateOk(resp, 'Seed templates')
  const data = await resp.json()
  return Array.isArray(data) ? (data as Template[]) : ((data.templates ?? []) as Template[])
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

// ---- Host detection --------------------------------------------------------

// True when the renderer is running inside the Tauri webview rather than a plain
// browser tab. The admin panel is served on the web too (a finish-claim gets
// approved from a phone, away from the desk), so every Tauri-only path — invoke,
// notifications, the remote listener, capture — must be SKIPPED there rather
// than left to throw. Tauri v2 injects `__TAURI_INTERNALS__` onto window before
// any app code runs, which is why this is a reliable synchronous check that
// needs no plugin (same reasoning as IS_WINDOWS at the top of this file).
export function isTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

// Wrap a Tauri invoke so a not-yet-implemented command (the Rust agents merge in
// parallel) never crashes the UI. Returns { ok, data } | { ok:false, error }.
//
// `raw` is the rejection exactly as Rust threw it. Most commands reject with a
// plain string and `error` is the whole story, but a command that rejects with a
// struct (`enroll`, whose kind is the classification) would lose everything but
// its message on the way through — so the value is carried alongside for the one
// caller that knows its shape.
export type InvokeResult<T> =
  | { ok: true; data: T }
  | { ok: false; error: string; raw?: unknown }

export async function safeInvoke<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<InvokeResult<T>> {
  // Outside the Tauri webview there is no command bridge at all, so bail before
  // importing it: the dynamic import resolves fine in a browser build and the
  // failure would otherwise surface as an opaque "window.__TAURI_INTERNALS__ is
  // undefined" instead of something a caller can put in front of a user.
  if (!isTauri()) {
    return { ok: false, error: `${command} is unavailable outside the desktop app` }
  }
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    const data = (await invoke(command, args)) as T
    return { ok: true, data }
  } catch (err) {
    return { ok: false, error: invokeErrorMessage(err), raw: err }
  }
}

// A Rust command rejects with whatever its error type serializes to, so the
// value reaching here is an Error only sometimes. `String(err)` on a struct
// rejection is the literal "[object Object]" — useless in front of a user and
// worse than useless in a bug report — hence the message probe in between.
function invokeErrorMessage(err: unknown): string {
  if (err instanceof Error) return err.message
  if (err && typeof err === 'object') {
    const message = (err as { message?: unknown }).message
    if (typeof message === 'string') return message
  }
  return String(err)
}

// ───────────────────────────────────────────────────────── enrollment

// Which credential this machine holds, and therefore what it is: a Google
// session makes it the operator's own install, a device token makes it a worker
// that redeemed a one-time enrollment key and never touches the operator's
// account. A machine holds one or the other, never both.
export type CredentialClass = 'session' | 'device' | 'none'

// Everything the Rust half of enrollment exposes is named HERE and nowhere else,
// so if that side renames a command these three strings are the entire edit.
const CREDENTIAL_CLASS_COMMAND = 'credential_class'
const ENROLL_COMMAND = 'enroll'
const CLEAR_DEVICE_TOKEN_COMMAND = 'clear_device_token'

// Rust emits this when the backend refuses a call this machine made WHILE
// HOLDING A DEVICE TOKEN: the enrollment is dead — revoked, or the device row
// forgotten — and only a fresh key gets the machine back in. Rust deliberately
// does nothing about it beyond saying so (it does not drop the token, and it
// never falls back to Google sign-in, which would recreate the exposure
// enrollment exists to remove), so the recovery is entirely the UI's.
export const DEVICE_REJECTED_EVENT = 'device://rejected'

// The frontend's half of the credential question. `credential_class` can see the
// device token in the Rust store but never this one, which lives in localStorage
// and is therefore invisible from Rust.
function hasSessionToken(): boolean {
  return !!localStorage.getItem('screen_buddy_session_token')
}

// What this machine is. Never throws: the answer decides which shell renders at
// all, so every failure resolves to what the renderer can see by itself.
export async function credentialClass(): Promise<CredentialClass> {
  const hasSession = hasSessionToken()
  // A browser tab has no command bridge and cannot hold a device token —
  // enrollment writes to the Rust credential store, not localStorage — so a
  // Google session is the only credential it could possibly have.
  if (!isTauri()) return hasSession ? 'session' : 'none'
  const res = await safeInvoke<CredentialClass>(CREDENTIAL_CLASS_COMMAND, { hasSession })
  if (res.ok && (res.data === 'session' || res.data === 'device' || res.data === 'none')) {
    return res.data
  }
  // Command absent (a desktop build without the Rust half yet) or failed: fall
  // back to the session token, which keeps such a build signing in exactly as it
  // does today rather than stranding it on a splash it cannot get past.
  return hasSession ? 'session' : 'none'
}

// Why an enrollment failed, and therefore what to do about it. The three lead to
// three different next steps: get a fresh key, try again in a moment, or report
// a bug — so they are kept apart rather than collapsed into "it didn't work".
export type EnrollFailureKind = 'rejected' | 'unreachable' | 'internal'

export type EnrollResult =
  | { ok: true }
  | { ok: false; reason: EnrollFailureKind; message: string }

// Redeem a one-time enrollment key for a device token. Success means the token
// is persisted Rust-side and this machine is a worker from here on.
export async function enrollMachine(key: string): Promise<EnrollResult> {
  // Hand over the backend the rest of the renderer talks to. Left to itself the
  // command guesses from env/localhost, which is right in a dev build and wrong
  // in a release one pointed at a real host.
  const res = await safeInvoke<unknown>(ENROLL_COMMAND, {
    key: key.trim(),
    backend: CU_BACKEND,
  })
  if (res.ok) return { ok: true }
  return { ok: false, reason: enrollFailureKind(res.raw, res.error), message: res.error }
}

// `enroll` rejects with { kind, message }, and the kind is a judgement made where
// the HTTP status actually was — so it is believed whenever it is there. The
// regex covers only the paths that cannot produce one: safeInvoke refusing
// outside Tauri, or a desktop build whose Rust half predates the struct.
// Anything naming a transport failure is worth retrying; everything else is
// treated as a refused key, which is the safer default — a key is single-use,
// and telling someone to keep retrying one the backend has already rejected
// wastes the only attempt it had.
function enrollFailureKind(raw: unknown, message: string): EnrollFailureKind {
  const kind = (raw as { kind?: unknown } | null | undefined)?.kind
  if (kind === 'rejected' || kind === 'unreachable' || kind === 'internal') return kind
  return /network|unreachable|connect|refused|timed?\s?out|dns|offline|transport|send request/i.test(
    message,
  )
    ? 'unreachable'
    : 'rejected'
}

// Drop this machine's device token, un-enrolling it: the next credential read
// answers 'none' and the splash comes back with both doors on it again. Two
// callers — Sign out on a worker, and the recovery after the backend refuses the
// token — and both need the same thing afterwards, so neither of them decides
// what "signed out" means for a worker on its own.
//
// Returns false when the command failed, so a caller can say so rather than
// showing a splash for a machine that is still holding a token.
export async function unenrollMachine(): Promise<boolean> {
  return (await safeInvoke<null>(CLEAR_DEVICE_TOKEN_COMMAND)).ok
}

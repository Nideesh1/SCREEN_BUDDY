import { useCallback, useEffect, useRef, useState } from 'react'
import { open } from '@tauri-apps/plugin-dialog'
import {
  safeInvoke,
  registerArtifact,
  renameRemoteArtifact,
  unregisterArtifact,
  type ArtifactMeta,
} from '../lib'
import {
  Card,
  Button,
  IconButton,
  Chip,
  EmptyState,
  Spinner,
  TrashIcon,
  ImageIcon,
  DocIcon,
  FilmIcon,
} from '../ui'

// File-picker filters — the kinds the Rust artifact_import knows how to
// thumbnail + classify. Anything else still imports (kind "other"), it just
// shows a generic icon, so these filters are a convenience, not a gate.
const PICK_FILTERS = [
  { name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'heic'] },
  { name: 'Video', extensions: ['mp4', 'm4v', 'mov', 'webm', 'mkv', 'avi'] },
  { name: 'PDF', extensions: ['pdf'] },
  { name: 'Text', extensions: ['txt', 'md'] },
]

type Load =
  | { state: 'loading' }
  | { state: 'unavailable'; message: string }
  | { state: 'ready'; items: ArtifactMeta[] }

// A thumbnail lookup: id -> data URL, or null once we know there ISN'T one
// (text/other kinds, or a thumb that failed to generate). null is a real,
// cached answer — it stops us re-asking Rust on every render.
type Thumbs = Record<string, string | null>

// Human-readable byte size. Binary units (matches what Finder-adjacent tools
// report for media files) with one decimal past KB.
function humanSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—'
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let val = bytes / 1024
  let i = 0
  while (val >= 1024 && i < units.length - 1) {
    val /= 1024
    i += 1
  }
  return `${val.toFixed(1)} ${units[i]}`
}

// Fallback glyph when an artifact has no thumbnail.
function KindIcon({ kind, size = 26 }: { kind: string; size?: number }) {
  if (kind === 'image') return <ImageIcon size={size} />
  if (kind === 'video') return <FilmIcon size={size} />
  return <DocIcon size={size} />
}

// The persistent local artifact library. Files are imported ONCE (copied into
// the Tauri app data dir, content-addressed by SHA-256 so a re-import dedupes)
// and listed here as a thumbnail grid with rename + delete.
//
// Local-first: every mutation lands on disk via the Tauri command, and the
// backend mirror (registerArtifact / renameRemoteArtifact / unregisterArtifact)
// is strictly best-effort — a mirror failure warns but never rolls back or
// blocks the local truth. The Tauri commands are wrapped in safeInvoke so a
// not-yet-merged Rust side shows an "unavailable" state, not a crash.
function Artifacts() {
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const [thumbs, setThumbs] = useState<Thumbs>({})
  const [importing, setImporting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [warning, setWarning] = useState<string | null>(null)
  // Inline rename: the artifact being renamed + its draft name.
  const [editingId, setEditingId] = useState<string | null>(null)
  const [draftName, setDraftName] = useState('')
  // Two-step delete: the artifact awaiting confirmation.
  const [confirmId, setConfirmId] = useState<string | null>(null)
  // Thumbnail ids already asked for — see the fetch effect below.
  const requestedRef = useRef<Set<string>>(new Set())
  // Set by Escape so the resulting blur discards the draft instead of saving it.
  const cancelEditRef = useRef(false)

  const fetchArtifacts = useCallback(async () => {
    setLoad({ state: 'loading' })
    const res = await safeInvoke<ArtifactMeta[]>('artifact_list')
    if (res.ok) setLoad({ state: 'ready', items: res.data ?? [] })
    else setLoad({ state: 'unavailable', message: res.error })
  }, [])

  useEffect(() => {
    fetchArtifacts()
  }, [fetchArtifacts])

  // Pull each thumbnail as a data URL, once per artifact. artifact_thumb Errs
  // when there's no thumb.jpg — that's expected (text/other), so we cache null
  // and render a kind icon instead of surfacing an error.
  //
  // `requestedRef` (not the `thumbs` state) is what gates re-fetching: keying off
  // state would make this effect depend on the very thing it writes, restarting
  // the loop after every single fetch. The ref makes each id fetched exactly once
  // for the life of the view, and surviving a list refresh is the point — a
  // re-fetch after a delete must not re-decode every other artifact's thumbnail.
  useEffect(() => {
    if (load.state !== 'ready') return
    const missing = load.items.filter((a) => !requestedRef.current.has(a.artifact_id))
    if (missing.length === 0) return
    let active = true
    ;(async () => {
      for (const art of missing) {
        if (!active) return
        // Claim ids one at a time, immediately before fetching — claiming the
        // whole batch up front would strand the un-fetched tail as permanently
        // "requested" (spinning forever) if this run is cancelled mid-loop.
        if (requestedRef.current.has(art.artifact_id)) continue
        requestedRef.current.add(art.artifact_id)
        const res = await safeInvoke<string>('artifact_thumb', { id: art.artifact_id })
        if (!active) {
          // Cancelled: drop the claim so the next run re-fetches this one.
          requestedRef.current.delete(art.artifact_id)
          return
        }
        setThumbs((prev) => ({
          ...prev,
          [art.artifact_id]: res.ok ? (res.data ?? null) : null,
        }))
      }
    })()
    return () => {
      active = false
    }
  }, [load])

  // Import: pick files, hand the paths to Rust (hash + copy + thumbnail), then
  // mirror each resulting meta to the backend. Slow by nature (hashing + ffmpeg
  // + PDFium), hence the spinner.
  const importFiles = useCallback(async () => {
    if (importing) return
    setError(null)
    setWarning(null)
    let selected: string | string[] | null = null
    try {
      selected = await open({ multiple: true, filters: PICK_FILTERS })
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      return
    }
    const paths = selected == null ? [] : Array.isArray(selected) ? selected : [selected]
    if (paths.length === 0) return

    setImporting(true)
    try {
      const res = await safeInvoke<ArtifactMeta[]>('artifact_import', { paths })
      if (!res.ok) {
        setError(res.error)
        return
      }
      const imported = res.data ?? []
      // Rust skips files it can't read rather than failing the batch — say so
      // instead of silently importing fewer than the user picked.
      if (imported.length < paths.length) {
        const skipped = paths.length - imported.length
        setWarning(`${skipped} of ${paths.length} file(s) could not be imported and were skipped.`)
      }
      // The import is DONE the moment the bytes are on disk — show them now.
      // The backend mirror is best-effort metadata; awaiting it here made the
      // spinner outlive the actual work (a sub-second import) and, if a mirror
      // request stalled, stranded it on "Importing…" forever for work that had
      // already finished. Never gate local state on a remote round-trip.
      fetchArtifacts()
      setImporting(false)

      // Mirror in the background, in parallel, and only surface the outcome.
      void Promise.all(imported.map((meta) => registerArtifact(meta))).then(
        (results) => {
          if (results.some((ok) => !ok)) {
            setWarning((w) =>
              [w, 'Imported locally, but registering with the backend failed.']
                .filter(Boolean)
                .join(' '),
            )
          }
        },
      )
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setImporting(false)
    }
  }, [importing, fetchArtifacts])

  const startRename = useCallback((art: ArtifactMeta) => {
    setEditingId(art.artifact_id)
    setDraftName(art.name)
    setConfirmId(null)
  }, [])

  // Commit a rename: local meta.json first (the source of truth), then mirror.
  // The ONLY commit path is the input's blur (Enter blurs the field rather than
  // committing directly), so a keypress can never race the resulting blur into
  // renaming twice.
  const commitRename = useCallback(
    async (id: string) => {
      const name = draftName.trim()
      const current = load.state === 'ready' ? load.items.find((a) => a.artifact_id === id) : null
      setEditingId(null)
      // Escape asked us to discard this draft.
      if (cancelEditRef.current) {
        cancelEditRef.current = false
        return
      }
      // No-op on an empty or unchanged name — just close the editor.
      if (!name || name === current?.name) return
      setError(null)
      const res = await safeInvoke('artifact_rename', { id, name })
      if (!res.ok) {
        setError(res.error)
        return
      }
      // Reflect the new name immediately; fetchArtifacts re-reads disk anyway.
      setLoad((prev) =>
        prev.state === 'ready'
          ? {
              ...prev,
              items: prev.items.map((a) => (a.artifact_id === id ? { ...a, name } : a)),
            }
          : prev,
      )
      const mirrored = await renameRemoteArtifact(id, name)
      if (!mirrored) setWarning('Renamed locally, but updating the backend failed.')
    },
    [draftName, load],
  )

  // Delete: remove the local dir, then deregister. unregisterArtifact swallows
  // its own errors, so the local delete governs.
  const deleteArtifact = useCallback(
    async (id: string) => {
      setConfirmId(null)
      setError(null)
      const res = await safeInvoke('artifact_delete', { id })
      if (!res.ok) {
        setError(res.error)
        return
      }
      void unregisterArtifact(id)
      // Drop BOTH the cached thumbnail and the "already requested" claim. Ids are
      // content-addressed, so re-importing these exact bytes resurrects this same
      // id — leaving the claim behind would strand the reborn card on a spinner
      // that never resolves.
      setThumbs((prev) => {
        const next = { ...prev }
        delete next[id]
        return next
      })
      requestedRef.current.delete(id)
      fetchArtifacts()
    },
    [fetchArtifacts],
  )

  return (
    <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max)', margin: '0 auto' }}>
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--sp-3)',
          marginBottom: 'var(--sp-4)',
        }}
      >
        <h1
          style={{
            margin: 0,
            fontSize: 'var(--fs-2xl)',
            fontWeight: 700,
            color: 'var(--sb-gold-bright)',
          }}
        >
          Artifacts
        </h1>
        <div style={{ marginLeft: 'auto' }}>
          <Button
            variant="primary"
            disabled={importing}
            onClick={importFiles}
            title="Import photos, videos, PDFs or notes into your library"
          >
            {importing ? 'Importing…' : '＋ Upload'}
          </Button>
        </div>
      </div>

      {importing && (
        <Card style={{ marginBottom: 'var(--sp-4)' }}>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              color: 'var(--sb-text)',
              fontSize: 'var(--fs-base)',
              fontWeight: 600,
            }}
          >
            <Spinner size={15} /> Importing…
          </div>
          <span
            style={{
              display: 'block',
              marginTop: 'var(--sp-2)',
              fontSize: 'var(--fs-md)',
              color: 'var(--sb-text-muted)',
            }}
          >
            Hashing, copying and generating thumbnails — large videos take a moment. Runs entirely on
            your Mac.
          </span>
        </Card>
      )}

      {error && (
        <div className="error-message" style={{ marginBottom: 'var(--sp-3)' }}>
          {error}
        </div>
      )}
      {warning && (
        <div
          style={{
            marginBottom: 'var(--sp-3)',
            fontSize: 'var(--fs-md)',
            color: 'var(--sb-text-muted)',
          }}
        >
          ⚠ {warning}
        </div>
      )}

      {load.state === 'loading' && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-2)',
            color: 'var(--sb-text-muted)',
            fontSize: 'var(--fs-base)',
          }}
        >
          <Spinner size={16} /> Loading artifacts…
        </div>
      )}

      {load.state === 'unavailable' && (
        <Card>
          <EmptyState
            icon="⚠"
            title="Artifact library is unavailable right now"
            hint={load.message}
            action={
              <Button variant="secondary" onClick={fetchArtifacts}>
                Retry
              </Button>
            }
          />
        </Card>
      )}

      {load.state === 'ready' && load.items.length === 0 && (
        <Card>
          <EmptyState
            icon={<ImageIcon size={28} />}
            title="No artifacts yet"
            hint="Upload photos, videos, PDFs or notes once and reuse them across your runs. Files stay on your Mac."
            action={
              <Button variant="primary" onClick={importFiles} disabled={importing}>
                ＋ Upload
              </Button>
            }
          />
        </Card>
      )}

      {load.state === 'ready' && load.items.length > 0 && (
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fill, minmax(200px, 1fr))',
            gap: 'var(--sp-3)',
          }}
        >
          {load.items.map((art) => {
            const thumb = thumbs[art.artifact_id]
            const isEditing = editingId === art.artifact_id
            const isConfirming = confirmId === art.artifact_id
            return (
              <Card
                key={art.artifact_id}
                padded={false}
                style={isConfirming ? { borderColor: 'var(--sb-danger-bright)' } : undefined}
              >
                {/* Preview: real thumbnail, or a kind icon when there isn't one. */}
                <div
                  style={{
                    height: 132,
                    display: 'flex',
                    alignItems: 'center',
                    justifyContent: 'center',
                    background: 'var(--sb-surface-1)',
                    borderBottom: '1px solid var(--sb-border)',
                    overflow: 'hidden',
                  }}
                >
                  {thumb ? (
                    <img
                      src={thumb}
                      alt={art.name}
                      title={art.name}
                      style={{ width: '100%', height: '100%', objectFit: 'cover', display: 'block' }}
                    />
                  ) : thumb === null ? (
                    <span style={{ color: 'var(--sb-text-muted)' }}>
                      <KindIcon kind={art.kind} />
                    </span>
                  ) : (
                    // undefined => the thumb fetch hasn't answered yet.
                    <Spinner size={15} />
                  )}
                </div>

                <div
                  style={{
                    display: 'flex',
                    flexDirection: 'column',
                    gap: 'var(--sp-2)',
                    padding: 'var(--sp-3) var(--sp-3)',
                  }}
                >
                  <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>
                    {isEditing ? (
                      <input
                        value={draftName}
                        onChange={(e) => setDraftName(e.target.value)}
                        autoFocus
                        className="agent-input"
                        aria-label="Artifact name"
                        onBlur={() => commitRename(art.artifact_id)}
                        onKeyDown={(e) => {
                          // Both keys route through blur — the single commit path.
                          if (e.key === 'Enter') e.currentTarget.blur()
                          if (e.key === 'Escape') {
                            cancelEditRef.current = true
                            e.currentTarget.blur()
                          }
                        }}
                        style={{
                          flex: 1,
                          minWidth: 0,
                          boxSizing: 'border-box',
                          padding: '5px 7px',
                          fontSize: 'var(--fs-md)',
                          background: 'var(--sb-surface-3)',
                          color: 'var(--sb-text)',
                          border: '1px solid var(--sb-border-gold)',
                          borderRadius: 'var(--r-sm)',
                        }}
                      />
                    ) : (
                      <button
                        onClick={() => startRename(art)}
                        title={`${art.original_filename} — click to rename`}
                        style={{
                          flex: 1,
                          minWidth: 0,
                          textAlign: 'left',
                          background: 'none',
                          border: 'none',
                          padding: 0,
                          cursor: 'text',
                          fontSize: 'var(--fs-base)',
                          fontWeight: 600,
                          color: 'var(--sb-text)',
                          overflow: 'hidden',
                          textOverflow: 'ellipsis',
                          whiteSpace: 'nowrap',
                        }}
                      >
                        {art.name}
                      </button>
                    )}
                    {!isEditing && (
                      <IconButton
                        title="Delete artifact"
                        aria-label="Delete artifact"
                        onClick={() => setConfirmId(isConfirming ? null : art.artifact_id)}
                        style={{ color: 'var(--sb-danger-bright)' }}
                      >
                        <TrashIcon size={15} />
                      </IconButton>
                    )}
                  </div>

                  <div
                    style={{
                      display: 'flex',
                      alignItems: 'center',
                      gap: 'var(--sp-2)',
                      flexWrap: 'wrap',
                    }}
                  >
                    <Chip mono>{art.kind}</Chip>
                    <span
                      style={{
                        fontSize: 'var(--fs-sm)',
                        fontFamily: 'var(--font-mono)',
                        color: 'var(--sb-text-muted)',
                      }}
                    >
                      {humanSize(art.size_bytes)}
                    </span>
                  </div>

                  {isConfirming && (
                    <div
                      style={{
                        display: 'flex',
                        alignItems: 'center',
                        gap: 'var(--sp-2)',
                        paddingTop: 'var(--sp-2)',
                        borderTop: '1px solid var(--sb-border)',
                      }}
                    >
                      <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
                        Delete?
                      </span>
                      <div style={{ marginLeft: 'auto', display: 'flex', gap: 'var(--sp-2)' }}>
                        <Button variant="ghost" size="sm" onClick={() => setConfirmId(null)}>
                          Cancel
                        </Button>
                        <Button
                          variant="danger"
                          size="sm"
                          onClick={() => deleteArtifact(art.artifact_id)}
                        >
                          Delete
                        </Button>
                      </div>
                    </div>
                  )}
                </div>
              </Card>
            )
          })}
        </div>
      )}
    </div>
  )
}

export default Artifacts

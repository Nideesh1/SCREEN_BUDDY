import { useEffect, useState, useCallback, useRef } from 'react'
import { useNavigate } from 'react-router-dom'
import { open } from '@tauri-apps/plugin-dialog'
import { listen } from '@tauri-apps/api/event'
import { convertFileSrc } from '@tauri-apps/api/core'
import {
  safeInvoke,
  fetchTemplates,
  registerSet,
  unregisterSet,
  type SetTemplate,
  type ArtifactMeta,
} from '../lib'
import { Card, Button, IconButton, Chip, EmptyState, Spinner, PinIcon, TrashIcon, ImageIcon, DocIcon, FilmIcon } from '../ui'

// Video → set ingestion: a short clip is picked, the Rust pipeline proposes
// candidate frames, the user prunes them in the review grid, and the survivors
// are written as a NORMAL pinned set via the same pinned_create path. Cap = 12.
const VIDEO_FILTERS = [{ name: 'Video', extensions: ['mp4', 'mov'] }]
// Over-serve frames and let the user prune in the review grid — better to show
// ~30 and uncheck than show 3 and wonder where an item went.
const VIDEO_TARGET_K = 30

// One proposed frame from extract_frames_from_video. `thumb_b64` is a ready
// data URL for the review grid; `path` is a real JPEG on disk in the staging
// dir, fed straight into pinned_create on Save (identical to hand-picked files).
interface FrameCandidate {
  path: string
  ts_ms: number
  sharpness: number
  thumb_b64: string
}

// Serve a full-resolution staging frame (its on-disk JPEG path) through the
// Tauri asset protocol so the lightbox shows the real ≤1568px image, not the
// small base64 thumbnail. Returns null if convertFileSrc is unavailable/throws
// (non-Tauri context) so the caller can fall back to the thumbnail. The
// video_staging dir is whitelisted in tauri.conf.json's assetProtocol scope.
function fullFrameSrc(path: string): string | null {
  try {
    return convertFileSrc(path)
  } catch {
    return null
  }
}

interface PinnedSet {
  id: string
  name: string
  count: number
}

interface PinnedImage {
  name: string
  // "image" | "pdf" | "text". A pdf resolved to image-mode (scanned) carries a
  // preview dataUrl of its first rendered page.
  kind: string
  dataUrl: string
}

interface PinnedDetail {
  name: string
  images: PinnedImage[]
}

type Load =
  | { state: 'loading' }
  | { state: 'unavailable'; message: string }
  | { state: 'ready'; sets: PinnedSet[] }

// The artifact library as seen by the picker. Mirrors Artifacts.tsx's Load —
// a missing/erroring artifact_list degrades the picker, never the whole view.
type ArtLoad =
  | { state: 'loading' }
  | { state: 'unavailable'; message: string }
  | { state: 'ready'; items: ArtifactMeta[] }

// Thumbnail lookup: id -> data URL, or null once we know there ISN'T one
// (text/other kinds, or a thumb that failed to generate). null is a real,
// cached answer — it stops us re-asking Rust on every render.
type Thumbs = Record<string, string | null>

// Fallback glyph when an artifact has no thumbnail (same mapping as Artifacts).
function KindIcon({ kind, size = 26 }: { kind: string; size?: number }) {
  if (kind === 'image') return <ImageIcon size={size} />
  if (kind === 'video') return <FilmIcon size={size} />
  return <DocIcon size={size} />
}

// Pinned reference library. Lists saved image sets via pinned_list; a set can be
// opened (pinned_get) to preview thumbnails, and deleted (pinned_delete). The
// Tauri commands may not exist yet (Rust agents merge in parallel), so every
// invoke is wrapped — a missing command shows an "unavailable" state, not a crash.
function PinnedLibrary() {
  const navigate = useNavigate()
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const [openId, setOpenId] = useState<string | null>(null)
  const [detail, setDetail] = useState<PinnedDetail | null>(null)
  const [detailError, setDetailError] = useState<string | null>(null)
  const [creating, setCreating] = useState(false)
  const [newName, setNewName] = useState('')
  const [createError, setCreateError] = useState<string | null>(null)
  const [createWarning, setCreateWarning] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  // "Link to template" picker: templates from GET /templates, '' = None (null).
  const [templates, setTemplates] = useState<SetTemplate[]>([])
  const [templateId, setTemplateId] = useState<string>('')

  // ---- artifact picker (set composition) ----------------------------------
  // A set is composed by picking from the artifact library — media is imported
  // ONCE in Artifacts and reused here, so building a set never means re-uploading
  // from the device. The library loads lazily the first time the panel opens.
  const [artLoad, setArtLoad] = useState<ArtLoad>({ state: 'loading' })
  const [thumbs, setThumbs] = useState<Thumbs>({})
  // Artifact ids chosen for the set being created. Nothing starts checked.
  const [pickedArtifacts, setPickedArtifacts] = useState<Set<string>>(new Set())
  // Thumbnail ids already asked for — see the fetch effect below.
  const requestedRef = useRef<Set<string>>(new Set())

  // ---- video → set ingestion flow -----------------------------------------
  // 'idle' = not active; 'extracting' = pipeline running (progress bar);
  // 'review' = candidate grid up for pruning + naming + save.
  const [videoMode, setVideoMode] = useState<'idle' | 'extracting' | 'review'>('idle')
  const [videoPct, setVideoPct] = useState(0)
  const [videoError, setVideoError] = useState<string | null>(null)
  const [candidates, setCandidates] = useState<FrameCandidate[]>([])
  // Selected candidate paths — every frame starts UNCHECKED; the user opts in.
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const [videoName, setVideoName] = useState('')
  const [videoTemplateId, setVideoTemplateId] = useState('')
  const [videoBusy, setVideoBusy] = useState(false)
  // Index of the frame open in the enlarge/preview lightbox (null = closed).
  const [lightboxIdx, setLightboxIdx] = useState<number | null>(null)

  const fetchSets = useCallback(async () => {
    setLoad({ state: 'loading' })
    const res = await safeInvoke<PinnedSet[]>('pinned_list')
    if (res.ok) {
      setLoad({ state: 'ready', sets: res.data ?? [] })
    } else {
      setLoad({ state: 'unavailable', message: res.error })
    }
  }, [])

  useEffect(() => {
    fetchSets()
  }, [fetchSets])

  const fetchArtifacts = useCallback(async () => {
    setArtLoad({ state: 'loading' })
    const res = await safeInvoke<ArtifactMeta[]>('artifact_list')
    if (res.ok) setArtLoad({ state: 'ready', items: res.data ?? [] })
    else setArtLoad({ state: 'unavailable', message: res.error })
  }, [])

  // Load the library whenever the create panel opens, so an artifact imported in
  // the Artifacts view mid-session shows up here without a reload. The selection
  // resets with it: ids held over from a previous open may have been deleted in
  // Artifacts since, and a stale id would fail the create (or silently inflate
  // the "N selected" count against a list that no longer contains it).
  useEffect(() => {
    if (!creating) return
    setPickedArtifacts(new Set())
    fetchArtifacts()
  }, [creating, fetchArtifacts])

  // Pull each thumbnail as a data URL, once per artifact. artifact_thumb Errs
  // when there's no thumb.jpg — expected for text/other, so we cache null and
  // render a kind icon rather than surfacing an error.
  //
  // `requestedRef` (not the `thumbs` state) gates re-fetching: keying off state
  // would make this effect depend on the very thing it writes, restarting the
  // loop after every fetch. The ref makes each id fetched exactly once for the
  // life of the view, surviving the re-fetch on each panel open.
  useEffect(() => {
    if (artLoad.state !== 'ready') return
    const missing = artLoad.items.filter((a) => !requestedRef.current.has(a.artifact_id))
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
  }, [artLoad])

  // Kinds a set can actually send to the model. `load_blocks` (pinned.rs) only
  // emits content blocks for these — an artifact of any other kind resolves
  // fine but contributes NOTHING to the prompt. The artifact library is
  // deliberately broader than a set (it stores videos too), so the picker has
  // to enforce this or a pinned video would be a silent no-op: set created,
  // model sees nothing, no error anywhere. Videos become pinnable via the
  // video → frames flow, which turns them into image frames.
  const SENDABLE_KINDS = new Set(['image', 'pdf', 'text'])
  const isSendable = (kind: string) => SENDABLE_KINDS.has(kind)

  const toggleArtifact = useCallback((id: string) => {
    setPickedArtifacts((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }, [])

  // Load run templates once for the "Link to template" picker (best-effort;
  // an empty result just leaves only the "None" option).
  useEffect(() => {
    let active = true
    ;(async () => {
      const t = await fetchTemplates()
      if (active) setTemplates(t)
    })()
    return () => {
      active = false
    }
  }, [])

  const openSet = useCallback(async (id: string) => {
    setOpenId(id)
    setDetail(null)
    setDetailError(null)
    const res = await safeInvoke<PinnedDetail>('pinned_get', { id })
    if (res.ok) setDetail(res.data)
    else setDetailError(res.error)
  }, [])

  // Create a set from the artifact library: the chosen artifact ids go to Rust,
  // which resolves each to its already-imported bytes. No OS dialog, no
  // re-upload — the library is the single source of media.
  const createSet = useCallback(async () => {
    const name = newName.trim()
    const artifactIds = [...pickedArtifacts]
    if (!name || artifactIds.length === 0 || busy) return
    setBusy(true)
    setCreateError(null)
    setCreateWarning(null)
    try {
      // Returns the new set's local id ({ id }) — same shape as pinned_create.
      const res = await safeInvoke<{ id: string }>('pinned_create_from_artifacts', {
        name,
        artifactIds,
      })
      if (res.ok) {
        // Mirror the new set into the backend registry so a dispatched run can
        // pin it by uuid. The local set id IS the set_uuid. Best-effort: on
        // failure we warn but keep the (already-created) local set.
        const setUuid = res.data?.id
        if (setUuid) {
          const registered = await registerSet(setUuid, name, templateId || null)
          if (!registered) {
            setCreateWarning('Set created locally, but registering it with the backend failed.')
          }
        }
        setCreating(false)
        setNewName('')
        setTemplateId('')
        setPickedArtifacts(new Set())
        fetchSets()
      } else {
        setCreateError(res.error)
      }
    } catch (err) {
      setCreateError(err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }, [newName, busy, templateId, pickedArtifacts, fetchSets])

  const resetVideo = useCallback(() => {
    setVideoMode('idle')
    setVideoPct(0)
    setVideoError(null)
    setCandidates([])
    setPicked(new Set())
    setVideoName('')
    setVideoTemplateId('')
    setVideoBusy(false)
    setLightboxIdx(null)
  }, [])

  // Pick a video, run the local extraction pipeline, land in the review grid.
  // 100% local — the video bytes never leave the machine (no upload).
  const startVideoFlow = useCallback(async () => {
    setCreating(false)
    setVideoError(null)
    let selected: string | string[] | null = null
    try {
      selected = await open({ multiple: false, filters: VIDEO_FILTERS })
    } catch (err) {
      setVideoError(err instanceof Error ? err.message : String(err))
      return
    }
    const path = Array.isArray(selected) ? selected[0] : selected
    if (!path) return

    setVideoMode('extracting')
    setVideoPct(0)
    setCandidates([])
    // Subscribe to extraction progress for the duration of this run.
    const unlisten = await listen<{ pct: number }>('agent://video_extract_progress', (e) => {
      setVideoPct(Math.max(0, Math.min(100, e.payload.pct)))
    })
    try {
      const res = await safeInvoke<FrameCandidate[]>('extract_frames_from_video', {
        path,
        targetK: VIDEO_TARGET_K,
      })
      if (res.ok) {
        const frames = res.data ?? []
        if (frames.length === 0) {
          setVideoError('No usable frames were found in this video.')
          setVideoMode('idle')
        } else {
          setCandidates(frames)
          setPicked(new Set()) // default: NONE checked — user opts in the frames they want
          setVideoMode('review')
        }
      } else {
        setVideoError(res.error)
        setVideoMode('idle')
      }
    } catch (err) {
      setVideoError(err instanceof Error ? err.message : String(err))
      setVideoMode('idle')
    } finally {
      unlisten()
    }
  }, [])

  const toggleFrame = useCallback((path: string) => {
    setPicked((prev) => {
      const next = new Set(prev)
      if (next.has(path)) next.delete(path)
      else next.add(path)
      return next
    })
  }, [])

  // Save the pruned frames as a normal pinned set: pinned_create copies the
  // chosen staging JPEGs in, then registerSet mirrors it to the backend — the
  // EXACT same write + registration path hand-picked images use.
  const saveVideoSet = useCallback(async () => {
    const name = videoName.trim()
    const paths = candidates.filter((c) => picked.has(c.path)).map((c) => c.path)
    if (!name || paths.length === 0 || videoBusy) return
    setVideoBusy(true)
    setVideoError(null)
    try {
      const res = await safeInvoke<{ id: string }>('pinned_create', { name, paths })
      if (res.ok) {
        const setUuid = res.data?.id
        if (setUuid) {
          const registered = await registerSet(setUuid, name, videoTemplateId || null)
          if (!registered) {
            setCreateWarning('Set created locally, but registering it with the backend failed.')
          }
        }
        resetVideo()
        fetchSets()
      } else {
        setVideoError(res.error)
      }
    } catch (err) {
      setVideoError(err instanceof Error ? err.message : String(err))
    } finally {
      setVideoBusy(false)
    }
  }, [videoName, candidates, picked, videoBusy, videoTemplateId, resetVideo, fetchSets])

  const deleteSet = useCallback(
    async (id: string) => {
      const res = await safeInvoke('pinned_delete', { id })
      if (!res.ok) {
        // Surface but keep the list — re-fetch reflects truth either way.
        setDetailError(res.error)
      }
      // Deregister from the backend registry too (the local id IS the set_uuid).
      // Best-effort: unregisterSet swallows its own errors.
      void unregisterSet(id)
      if (openId === id) {
        setOpenId(null)
        setDetail(null)
      }
      fetchSets()
    },
    [fetchSets, openId],
  )

  // Return to the sets list from the drill-down detail screen.
  const closeSet = useCallback(() => {
    setOpenId(null)
    setDetail(null)
    setDetailError(null)
  }, [])

  // ---- drill-down detail screen -------------------------------------------
  // When a set is open we render a DEDICATED full-screen view for it instead of
  // the list (the list is hidden). `openId` drives list-vs-detail: null = list,
  // a set id = this screen. The set's name/count come from `detail` once
  // pinned_get answers, and fall back to the list row meta while it's in flight
  // so the header is populated during loading.
  if (openId !== null) {
    const meta = load.state === 'ready' ? load.sets.find((s) => s.id === openId) : undefined
    const setName = detail?.name ?? meta?.name ?? 'Set'
    const itemCount = detail ? detail.images.length : meta?.count
    return (
      <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max)', margin: '0 auto' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)', marginBottom: 'var(--sp-4)' }}>
          <button
            onClick={closeSet}
            className="agent-input"
            aria-label="Back to all sets"
            style={{
              display: 'inline-flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              padding: '7px 12px',
              fontSize: 'var(--fs-base)',
              fontWeight: 600,
              cursor: 'pointer',
              color: 'var(--sb-text)',
              background: 'var(--sb-surface-3)',
              border: '1px solid var(--sb-border)',
              borderRadius: 'var(--r-sm)',
            }}
          >
            ← All sets
          </button>
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', minWidth: 0 }}>
            <PinIcon size={18} />
            <h1
              style={{
                margin: 0,
                fontSize: 'var(--fs-xl)',
                fontWeight: 700,
                color: 'var(--sb-gold-bright)',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                whiteSpace: 'nowrap',
              }}
            >
              {setName}
            </h1>
            {itemCount !== undefined && (
              <Chip mono>
                {itemCount} {itemCount === 1 ? 'item' : 'items'}
              </Chip>
            )}
          </div>
          <div style={{ marginLeft: 'auto' }}>
            <IconButton
              title="Delete set"
              aria-label="Delete set"
              onClick={() => deleteSet(openId)}
              style={{ color: 'var(--sb-danger-bright)' }}
            >
              <TrashIcon size={16} />
            </IconButton>
          </div>
        </div>

        {detailError && (
          <div className="error-message" style={{ marginBottom: 'var(--sp-3)' }}>
            {detailError}
          </div>
        )}

        {!detail && !detailError && (
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              color: 'var(--sb-text-muted)',
              fontSize: 'var(--fs-base)',
            }}
          >
            <Spinner size={16} /> Loading set…
          </div>
        )}

        {detail && detail.images.length === 0 && (
          <Card>
            <EmptyState
              icon={<ImageIcon size={28} />}
              title="This set is empty"
              hint="It has no items to preview."
            />
          </Card>
        )}

        {detail && detail.images.length > 0 && (
          <div
            style={{
              display: 'grid',
              gridTemplateColumns: 'repeat(auto-fill, minmax(200px, 1fr))',
              gap: 'var(--sp-3)',
            }}
          >
            {detail.images.map((img, i) => (
              <Card key={i} padded={false}>
                <div
                  style={{
                    height: 180,
                    display: 'flex',
                    alignItems: 'center',
                    justifyContent: 'center',
                    background: 'var(--sb-surface-1)',
                    borderBottom: '1px solid var(--sb-border)',
                    overflow: 'hidden',
                  }}
                >
                  {img.dataUrl ? (
                    <img
                      src={img.dataUrl}
                      alt={img.name}
                      style={{
                        width: '100%',
                        height: '100%',
                        objectFit: 'cover',
                        display: 'block',
                      }}
                    />
                  ) : (
                    <span
                      style={{
                        display: 'inline-flex',
                        flexDirection: 'column',
                        alignItems: 'center',
                        gap: 'var(--sp-2)',
                        color: 'var(--sb-text-muted)',
                      }}
                    >
                      <KindIcon kind={img.kind} size={34} />
                      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-xs)' }}>
                        {img.kind}
                      </span>
                    </span>
                  )}
                </div>
                <div style={{ padding: 'var(--sp-2) var(--sp-3)' }}>
                  <span
                    style={{
                      display: 'block',
                      fontSize: 'var(--fs-md)',
                      fontWeight: 600,
                      color: 'var(--sb-text)',
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                    }}
                    title={`${img.name}${img.kind === 'pdf' ? ' (visual)' : ''}`}
                  >
                    {img.name}
                  </span>
                </div>
              </Card>
            ))}
          </div>
        )}
      </div>
    )
  }

  return (
    <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max)', margin: '0 auto' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)', marginBottom: 'var(--sp-4)' }}>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-gold-bright)' }}>
          Pinned
        </h1>
        <div style={{ marginLeft: 'auto', display: 'flex', gap: 'var(--sp-2)' }}>
          <Button
            variant="secondary"
            disabled={videoMode !== 'idle'}
            onClick={startVideoFlow}
            title="Extract frames from a short video to build a set"
          >
            <FilmIcon size={15} /> New set from video
          </Button>
          <Button
            variant="primary"
            onClick={() => {
              setCreating((c) => !c)
              setCreateError(null)
            }}
          >
            ＋ New set
          </Button>
        </div>
      </div>

      {videoError && (
        <div className="error-message" style={{ marginBottom: 'var(--sp-3)' }}>
          {videoError}
        </div>
      )}

      {videoMode === 'extracting' && (
        <Card style={{ marginBottom: 'var(--sp-4)' }}>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
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
              <Spinner size={15} /> Extracting frames… {videoPct}%
            </div>
            <div
              style={{
                height: 8,
                borderRadius: 'var(--r-sm)',
                background: 'var(--sb-surface-3)',
                overflow: 'hidden',
              }}
            >
              <div
                style={{
                  height: '100%',
                  width: `${videoPct}%`,
                  background: 'var(--sb-gold-bright)',
                  transition: 'width 160ms ease',
                }}
              />
            </div>
            <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
              Runs entirely on your Mac — the video is never uploaded.
            </span>
          </div>
        </Card>
      )}

      {videoMode === 'review' && (
        <Card style={{ marginBottom: 'var(--sp-4)' }}>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
              <span style={{ fontWeight: 600, color: 'var(--sb-text)', fontSize: 'var(--fs-base)' }}>
                Review frames
              </span>
              <Chip mono>
                {picked.size} of {candidates.length} selected
              </Chip>
              <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
                Click a frame to preview · click the check to keep or drop it
              </span>
              <div style={{ marginLeft: 'auto', display: 'flex', gap: 'var(--sp-2)' }}>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => setPicked(new Set(candidates.map((c) => c.path)))}
                  disabled={videoBusy || picked.size === candidates.length}
                >
                  Select all
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => setPicked(new Set())}
                  disabled={videoBusy || picked.size === 0}
                >
                  Clear
                </Button>
                <Button variant="ghost" size="sm" onClick={resetVideo} disabled={videoBusy}>
                  Cancel
                </Button>
              </div>
            </div>

            <div
              style={{
                display: 'grid',
                gridTemplateColumns: 'repeat(auto-fill, minmax(220px, 1fr))',
                gap: 'var(--sp-3)',
              }}
            >
              {candidates.map((c, idx) => {
                const on = picked.has(c.path)
                return (
                  <div
                    key={c.path}
                    style={{
                      position: 'relative',
                      borderRadius: 'var(--r-md)',
                      overflow: 'hidden',
                      border: on
                        ? '2px solid var(--sb-gold-bright)'
                        : '2px solid var(--sb-border)',
                      background: 'var(--sb-surface-1)',
                      boxShadow: on ? 'var(--shadow-1)' : 'none',
                    }}
                  >
                    {/* Body: click to open the enlarge/preview lightbox. */}
                    <button
                      onClick={() => setLightboxIdx(idx)}
                      title={`Preview frame at ${(c.ts_ms / 1000).toFixed(1)}s`}
                      style={{
                        display: 'block',
                        width: '100%',
                        padding: 0,
                        border: 'none',
                        cursor: 'zoom-in',
                        background: 'transparent',
                      }}
                    >
                      <img
                        src={c.thumb_b64}
                        alt={`frame at ${c.ts_ms}ms`}
                        style={{
                          width: '100%',
                          height: 165,
                          objectFit: 'cover',
                          display: 'block',
                          opacity: on ? 1 : 0.45,
                          filter: on ? 'none' : 'grayscale(0.35)',
                          transition: 'opacity 120ms ease, filter 120ms ease',
                        }}
                      />
                    </button>
                    {/* Selection toggle — separate from the preview click. */}
                    <button
                      onClick={() => toggleFrame(c.path)}
                      aria-pressed={on}
                      title={on ? 'Selected — click to drop' : 'Not selected — click to keep'}
                      style={{
                        position: 'absolute',
                        top: 6,
                        right: 6,
                        width: 26,
                        height: 26,
                        borderRadius: '50%',
                        display: 'flex',
                        alignItems: 'center',
                        justifyContent: 'center',
                        fontSize: 14,
                        fontWeight: 700,
                        cursor: 'pointer',
                        color: on ? '#0A0A0A' : 'var(--sb-text-muted)',
                        background: on ? 'var(--sb-gold-bright)' : 'rgba(10,10,12,0.72)',
                        border: on
                          ? '1px solid var(--sb-gold-bright)'
                          : '1px solid var(--sb-border)',
                        boxShadow: 'var(--shadow-1)',
                      }}
                    >
                      {on ? '✓' : '＋'}
                    </button>
                    <span
                      style={{
                        position: 'absolute',
                        bottom: 0,
                        left: 0,
                        right: 0,
                        padding: '3px 6px',
                        fontSize: 'var(--fs-xs)',
                        fontFamily: 'var(--font-mono)',
                        color: '#fff',
                        background: 'linear-gradient(transparent, rgba(0,0,0,0.65))',
                      }}
                    >
                      {(c.ts_ms / 1000).toFixed(1)}s
                    </span>
                  </div>
                )
              })}
            </div>

            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
              <input
                value={videoName}
                onChange={(e) => setVideoName(e.target.value)}
                placeholder="Set name"
                autoFocus
                className="agent-input"
                onKeyDown={(e) => {
                  if (e.key === 'Enter') saveVideoSet()
                }}
                style={{
                  flex: 1,
                  boxSizing: 'border-box',
                  padding: '9px 11px',
                  fontSize: 'var(--fs-base)',
                  background: 'var(--sb-surface-3)',
                  color: 'var(--sb-text)',
                  border: '1px solid var(--sb-border)',
                  borderRadius: 'var(--r-sm)',
                }}
              />
              <select
                value={videoTemplateId}
                onChange={(e) => setVideoTemplateId(e.target.value)}
                aria-label="Link to template"
                title="Link this set to a run template"
                style={{
                  boxSizing: 'border-box',
                  padding: '9px 11px',
                  fontSize: 'var(--fs-base)',
                  background: 'var(--sb-surface-3)',
                  color: 'var(--sb-text)',
                  border: '1px solid var(--sb-border)',
                  borderRadius: 'var(--r-sm)',
                  maxWidth: 220,
                }}
              >
                <option value="">Link to template… (None)</option>
                {templates.map((t) => (
                  <option key={t.template_id} value={t.template_id}>
                    {t.name}
                  </option>
                ))}
              </select>
              <Button
                variant="primary"
                disabled={!videoName.trim() || picked.size === 0 || videoBusy}
                onClick={saveVideoSet}
              >
                {videoBusy ? 'Saving…' : `Save set (${picked.size})`}
              </Button>
            </div>
          </div>
        </Card>
      )}

      {videoMode === 'review' && lightboxIdx !== null && candidates[lightboxIdx] && (
        <div
          onClick={() => setLightboxIdx(null)}
          style={{
            position: 'fixed',
            inset: 0,
            zIndex: 100,
            background: 'rgba(0,0,0,0.82)',
            display: 'flex',
            flexDirection: 'column',
            alignItems: 'center',
            justifyContent: 'center',
            gap: 'var(--sp-3)',
            padding: 'var(--sp-5)',
          }}
        >
          {(() => {
            const c = candidates[lightboxIdx]
            const on = picked.has(c.path)
            const go = (delta: number) => (e: React.MouseEvent) => {
              e.stopPropagation()
              setLightboxIdx((i) =>
                i === null ? i : (i + delta + candidates.length) % candidates.length,
              )
            }
            return (
              <>
                <div
                  onClick={(e) => e.stopPropagation()}
                  style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}
                >
                  <IconButton title="Previous frame" aria-label="Previous frame" onClick={go(-1)}>
                    ‹
                  </IconButton>
                  <img
                    // Enlarge the FULL-resolution staging frame (≤1568px) via the
                    // Tauri asset protocol, not the tiny grid thumbnail. Fall back
                    // to thumb_b64 if convertFileSrc is unavailable/throws.
                    src={fullFrameSrc(c.path) ?? c.thumb_b64}
                    alt={`frame at ${c.ts_ms}ms`}
                    style={{
                      maxWidth: '85vw',
                      maxHeight: '82vh',
                      width: 'auto',
                      height: 'auto',
                      objectFit: 'contain',
                      borderRadius: 'var(--r-md)',
                      border: on
                        ? '2px solid var(--sb-gold-bright)'
                        : '2px solid var(--sb-border)',
                      background: 'var(--sb-surface-1)',
                      boxShadow: 'var(--shadow-2)',
                    }}
                  />
                  <IconButton title="Next frame" aria-label="Next frame" onClick={go(1)}>
                    ›
                  </IconButton>
                </div>
                <div
                  onClick={(e) => e.stopPropagation()}
                  style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}
                >
                  <Chip mono>
                    {lightboxIdx + 1} / {candidates.length} · {(c.ts_ms / 1000).toFixed(1)}s
                  </Chip>
                  <Button
                    variant={on ? 'secondary' : 'primary'}
                    size="sm"
                    onClick={() => toggleFrame(c.path)}
                  >
                    {on ? '✓ Selected — drop it' : '＋ Keep this frame'}
                  </Button>
                  <Button variant="ghost" size="sm" onClick={() => setLightboxIdx(null)}>
                    Close
                  </Button>
                </div>
              </>
            )
          })()}
        </div>
      )}

      {creating && (
        <Card style={{ marginBottom: 'var(--sp-4)' }}>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
              <span style={{ fontWeight: 600, color: 'var(--sb-text)', fontSize: 'var(--fs-base)' }}>
                Choose artifacts
              </span>
              {artLoad.state === 'ready' && artLoad.items.length > 0 && (
                <>
                  <Chip mono>
                    {pickedArtifacts.size} of {artLoad.items.length} selected
                  </Chip>
                  <span style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
                    Click an artifact to add it to this set
                  </span>
                  <div style={{ marginLeft: 'auto', display: 'flex', gap: 'var(--sp-2)' }}>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() =>
                        setPickedArtifacts(new Set(artLoad.items.map((a) => a.artifact_id)))
                      }
                      disabled={busy || pickedArtifacts.size === artLoad.items.length}
                    >
                      Select all
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => setPickedArtifacts(new Set())}
                      disabled={busy || pickedArtifacts.size === 0}
                    >
                      Clear
                    </Button>
                  </div>
                </>
              )}
            </div>

            {artLoad.state === 'loading' && (
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

            {/* The library is the only media source here, so if it can't be read
                there is nothing to pick — say why and offer a retry. */}
            {artLoad.state === 'unavailable' && (
              <EmptyState
                icon="⚠"
                title="Artifact library is unavailable right now"
                hint={artLoad.message}
                action={
                  <Button variant="secondary" onClick={fetchArtifacts}>
                    Retry
                  </Button>
                }
              />
            )}

            {artLoad.state === 'ready' && artLoad.items.length === 0 && (
              <EmptyState
                icon={<ImageIcon size={28} />}
                title="No artifacts yet"
                hint="Upload media in Artifacts first — import a file once and reuse it in any number of pinned sets."
                action={
                  <Button variant="primary" onClick={() => navigate('/artifacts')}>
                    Go to Artifacts
                  </Button>
                }
              />
            )}

            {artLoad.state === 'ready' && artLoad.items.length > 0 && (
              <div
                style={{
                  display: 'grid',
                  gridTemplateColumns: 'repeat(auto-fill, minmax(150px, 1fr))',
                  gap: 'var(--sp-3)',
                  maxHeight: 420,
                  overflowY: 'auto',
                }}
              >
                {artLoad.items.map((art) => {
                  const thumb = thumbs[art.artifact_id]
                  const on = pickedArtifacts.has(art.artifact_id)
                  const sendable = isSendable(art.kind)
                  return (
                    <button
                      key={art.artifact_id}
                      onClick={() => sendable && toggleArtifact(art.artifact_id)}
                      aria-pressed={on}
                      disabled={!sendable}
                      title={
                        sendable
                          ? `${art.name} — click to ${on ? 'remove from' : 'add to'} this set`
                          : `${art.name} — ${art.kind} can't be pinned to a set. Use "Create from video" to turn it into frames.`
                      }
                      style={{
                        position: 'relative',
                        padding: 0,
                        textAlign: 'left',
                        cursor: sendable ? 'pointer' : 'not-allowed',
                        borderRadius: 'var(--r-md)',
                        overflow: 'hidden',
                        border: on
                          ? '2px solid var(--sb-gold-bright)'
                          : '2px solid var(--sb-border)',
                        background: 'var(--sb-surface-2)',
                        boxShadow: on ? 'var(--shadow-1)' : 'none',
                        opacity: sendable ? 1 : 0.45,
                      }}
                    >
                      <div
                        style={{
                          height: 110,
                          display: 'flex',
                          alignItems: 'center',
                          justifyContent: 'center',
                          background: 'var(--sb-surface-1)',
                          borderBottom: '1px solid var(--sb-border)',
                          overflow: 'hidden',
                          opacity: on ? 1 : 0.6,
                          transition: 'opacity 120ms ease',
                        }}
                      >
                        {thumb ? (
                          <img
                            src={thumb}
                            alt={art.name}
                            style={{
                              width: '100%',
                              height: '100%',
                              objectFit: 'cover',
                              display: 'block',
                            }}
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
                          padding: 'var(--sp-2) var(--sp-3)',
                        }}
                      >
                        <span
                          style={{
                            fontSize: 'var(--fs-md)',
                            fontWeight: 600,
                            color: 'var(--sb-text)',
                            overflow: 'hidden',
                            textOverflow: 'ellipsis',
                            whiteSpace: 'nowrap',
                          }}
                        >
                          {art.name}
                        </span>
                        <Chip mono>{art.kind}</Chip>
                      </div>
                      {/* Selection state — the whole card is the toggle, this is
                          the affordance that shows which way it went. */}
                      <span
                        style={{
                          position: 'absolute',
                          top: 6,
                          right: 6,
                          width: 24,
                          height: 24,
                          borderRadius: '50%',
                          display: 'flex',
                          alignItems: 'center',
                          justifyContent: 'center',
                          fontSize: 13,
                          fontWeight: 700,
                          color: on ? '#0A0A0A' : 'var(--sb-text-muted)',
                          background: on ? 'var(--sb-gold-bright)' : 'rgba(10,10,12,0.72)',
                          border: on
                            ? '1px solid var(--sb-gold-bright)'
                            : '1px solid var(--sb-border)',
                          boxShadow: 'var(--shadow-1)',
                        }}
                      >
                        {on ? '✓' : '＋'}
                      </span>
                    </button>
                  )
                })}
              </div>
            )}

            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
            <input
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              placeholder="Set name"
              autoFocus
              className="agent-input"
              onKeyDown={(e) => {
                if (e.key === 'Enter') createSet()
              }}
              style={{
                flex: 1,
                boxSizing: 'border-box',
                padding: '9px 11px',
                fontSize: 'var(--fs-base)',
                background: 'var(--sb-surface-3)',
                color: 'var(--sb-text)',
                border: '1px solid var(--sb-border)',
                borderRadius: 'var(--r-sm)',
              }}
            />
            <select
              value={templateId}
              onChange={(e) => setTemplateId(e.target.value)}
              aria-label="Link to template"
              title="Link this set to a run template"
              style={{
                boxSizing: 'border-box',
                padding: '9px 11px',
                fontSize: 'var(--fs-base)',
                background: 'var(--sb-surface-3)',
                color: 'var(--sb-text)',
                border: '1px solid var(--sb-border)',
                borderRadius: 'var(--r-sm)',
                maxWidth: 220,
              }}
            >
              <option value="">Link to template… (None)</option>
              {templates.map((t) => (
                <option key={t.template_id} value={t.template_id}>
                  {t.name}
                </option>
              ))}
            </select>
            <Button
              variant="primary"
              disabled={!newName.trim() || pickedArtifacts.size === 0 || busy}
              onClick={createSet}
            >
              {busy ? 'Creating…' : `Create set (${pickedArtifacts.size})`}
            </Button>
            </div>
          </div>
        </Card>
      )}

      {createError && <div className="error-message" style={{ marginBottom: 'var(--sp-3)' }}>{createError}</div>}
      {createWarning && (
        <div
          style={{
            marginBottom: 'var(--sp-3)',
            fontSize: 'var(--fs-md)',
            color: 'var(--sb-text-muted)',
          }}
        >
          ⚠ {createWarning}
        </div>
      )}

      {load.state === 'loading' && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', color: 'var(--sb-text-muted)', fontSize: 'var(--fs-base)' }}>
          <Spinner size={16} /> Loading sets…
        </div>
      )}

      {load.state === 'unavailable' && (
        <Unavailable message={load.message} onRetry={fetchSets} />
      )}

      {load.state === 'ready' && load.sets.length === 0 && (
        <Card>
          <EmptyState
            icon={<PinIcon size={28} />}
            title="No pinned sets yet"
            hint="Create a set of reference images, PDFs or notes to give your runs visual context."
            action={
              <Button
                variant="primary"
                onClick={() => {
                  setCreating(true)
                  setCreateError(null)
                }}
              >
                ＋ New set
              </Button>
            }
          />
        </Card>
      )}

      {load.state === 'ready' && load.sets.length > 0 && (
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fill, minmax(240px, 1fr))',
            gap: 'var(--sp-3)',
          }}
        >
          {load.sets.map((set) => {
            return (
              <Card key={set.id} padded={false}>
                <div style={{ display: 'flex', alignItems: 'flex-start', gap: 'var(--sp-2)', padding: 'var(--sp-3) var(--sp-4)' }}>
                  <button
                    onClick={() => openSet(set.id)}
                    title={`Open ${set.name}`}
                    style={{
                      flex: 1,
                      minWidth: 0,
                      textAlign: 'left',
                      background: 'none',
                      border: 'none',
                      padding: 0,
                      cursor: 'pointer',
                      display: 'flex',
                      flexDirection: 'column',
                      gap: 'var(--sp-2)',
                    }}
                  >
                    <span
                      style={{
                        display: 'inline-flex',
                        alignItems: 'center',
                        gap: 'var(--sp-2)',
                        fontSize: 'var(--fs-base)',
                        fontWeight: 600,
                        color: 'var(--sb-text)',
                        overflow: 'hidden',
                        minWidth: 0,
                      }}
                    >
                      <PinIcon size={15} />
                      <span
                        style={{
                          overflow: 'hidden',
                          textOverflow: 'ellipsis',
                          whiteSpace: 'nowrap',
                        }}
                      >
                        {set.name}
                      </span>
                    </span>
                    <Chip mono>
                      {set.count} {set.count === 1 ? 'file' : 'files'}
                    </Chip>
                  </button>
                  <IconButton
                    title="Delete set"
                    aria-label="Delete set"
                    onClick={() => deleteSet(set.id)}
                    style={{ color: 'var(--sb-danger-bright)' }}
                  >
                    <TrashIcon size={16} />
                  </IconButton>
                </div>
              </Card>
            )
          })}
        </div>
      )}
    </div>
  )
}

function Unavailable({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <Card>
      <EmptyState
        icon="⚠"
        title="Pinned library is unavailable right now"
        hint={message}
        action={
          <Button variant="secondary" onClick={onRetry}>
            Retry
          </Button>
        }
      />
    </Card>
  )
}

export default PinnedLibrary

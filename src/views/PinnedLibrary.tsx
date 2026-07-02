import { useEffect, useState, useCallback } from 'react'
import { open } from '@tauri-apps/plugin-dialog'
import { listen } from '@tauri-apps/api/event'
import { convertFileSrc } from '@tauri-apps/api/core'
import { safeInvoke, fetchTemplates, registerSet, unregisterSet, type SetTemplate } from '../lib'
import { Card, Button, IconButton, Chip, EmptyState, Spinner, PinIcon, TrashIcon, ImageIcon, DocIcon, FilmIcon } from '../ui'

// File-picker filters: images, PDFs, and text/markdown. Matches the kinds the
// Rust pinned_create accepts and ingests.
const PICK_FILTERS = [
  { name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp'] },
  { name: 'PDF', extensions: ['pdf'] },
  { name: 'Text', extensions: ['txt', 'md'] },
]

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

// Pinned reference library. Lists saved image sets via pinned_list; a set can be
// opened (pinned_get) to preview thumbnails, and deleted (pinned_delete). The
// Tauri commands may not exist yet (Rust agents merge in parallel), so every
// invoke is wrapped — a missing command shows an "unavailable" state, not a crash.
function PinnedLibrary() {
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

  // Create a one-off set: pick files via the OS dialog, then pinned_create.
  const createSet = useCallback(async () => {
    const name = newName.trim()
    if (!name || busy) return
    setBusy(true)
    setCreateError(null)
    setCreateWarning(null)
    try {
      const selected = await open({ multiple: true, filters: PICK_FILTERS })
      const paths = selected == null ? [] : Array.isArray(selected) ? selected : [selected]
      if (paths.length === 0) return
      // pinned_create returns the new set's local id ({ id }).
      const res = await safeInvoke<{ id: string }>('pinned_create', { name, paths })
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
        fetchSets()
      } else {
        setCreateError(res.error)
      }
    } catch (err) {
      setCreateError(err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }, [newName, busy, templateId, fetchSets])

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
            <Button variant="primary" disabled={!newName.trim() || busy} onClick={createSet}>
              {busy ? 'Adding…' : 'Choose files…'}
            </Button>
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
            const isOpen = openId === set.id
            return (
              <Card key={set.id} padded={false} style={isOpen ? { borderColor: 'var(--sb-border-gold)' } : undefined}>
                <div style={{ display: 'flex', alignItems: 'flex-start', gap: 'var(--sp-2)', padding: 'var(--sp-3) var(--sp-4)' }}>
                  <button
                    onClick={() => (isOpen ? setOpenId(null) : openSet(set.id))}
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

                {isOpen && (
                  <div
                    style={{
                      padding: 'var(--sp-3) var(--sp-4)',
                      borderTop: '1px solid var(--sb-border)',
                      background: 'var(--sb-surface-2)',
                    }}
                  >
                    {detailError && <div className="error-message">{detailError}</div>}
                    {!detail && !detailError && (
                      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 'var(--sp-2)', color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)' }}>
                        <Spinner size={13} /> Loading preview…
                      </span>
                    )}
                    {detail && (
                      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--sp-2)' }}>
                        {detail.images.length === 0 && (
                          <span style={{ color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)' }}>
                            (empty set)
                          </span>
                        )}
                        {detail.images.map((img, i) =>
                          img.dataUrl ? (
                            <img
                              key={i}
                              src={img.dataUrl}
                              alt={img.name}
                              title={`${img.name}${img.kind === 'pdf' ? ' (visual)' : ''}`}
                              style={{
                                width: 84,
                                height: 84,
                                objectFit: 'cover',
                                borderRadius: 'var(--r-sm)',
                                border: '1px solid var(--sb-border)',
                              }}
                            />
                          ) : (
                            <div
                              key={i}
                              title={img.name}
                              style={{
                                width: 84,
                                height: 84,
                                display: 'flex',
                                flexDirection: 'column',
                                alignItems: 'center',
                                justifyContent: 'center',
                                gap: 4,
                                borderRadius: 'var(--r-sm)',
                                border: '1px solid var(--sb-border)',
                                background: 'var(--sb-surface-1)',
                                color: 'var(--sb-text-muted)',
                                fontSize: 'var(--fs-xs)',
                                padding: 6,
                                boxSizing: 'border-box',
                                textAlign: 'center',
                              }}
                            >
                              <span style={{ display: 'inline-flex' }}>
                                {img.kind === 'image' ? <ImageIcon size={22} /> : <DocIcon size={22} />}
                              </span>
                              <span style={{ wordBreak: 'break-word', fontFamily: 'var(--font-mono)' }}>{img.kind}</span>
                            </div>
                          ),
                        )}
                      </div>
                    )}
                  </div>
                )}
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

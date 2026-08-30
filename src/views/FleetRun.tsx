import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Navigate, useNavigate, useParams } from 'react-router-dom'
import { CU_BACKEND, authHeaders, relativeTime } from '../lib'
import { formatTokens } from './History'
import { Markdown, asText, summaryText } from './RunDetail'
import { DeviceDot, deviceLabel, deviceLine, useDevice, type DeviceHead } from './DeviceRuns'
import { ActionChip, Button, Card, Chip, EmptyState, SectionTitle, Spinner, StatChip, StatusPill } from '../ui'

// FleetRun — one REMOTE worker's run, read entirely from the backend.
//
// The Devices pane can already show what a machine is looking at (ScreenCard),
// which separates a worker that is working from one that is asleep. It cannot
// separate a worker that is making progress from one clicking the same button
// forty times: that is in the narration and the actions, and until now neither
// was visible anywhere outside the machine itself.
//
// This is deliberately NOT RunDetail. That view renders a run executed on THIS
// machine: it embeds AgentRunPanel's `agent://` stream and resolves screenshots
// through the Tauri asset protocol from local disk. A fleet run has neither — no
// local stream, no local files, and this same bundle is served in a plain
// browser — so everything here comes over HTTP. The timeline idiom (gold rail,
// text as prose, actions as ActionChips) is copied from RunDetail on purpose;
// only the transport differs.

// One persisted event. `data` is type-dependent and loose, exactly as in
// RunDetail. The url/expires_at pair is what the events route adds over the
// stored RunEvent: a screenshot's `artifact_object` is a storage key that a
// browser holding no credentials cannot fetch, so the backend signs it before
// it leaves the process (same reasoning as ScreenshotOut in Admin's ScreenCard).
interface RunEventRow {
  seq: number
  type: string
  data?: Record<string, unknown> | null
  artifact_object?: string | null
  artifact_kind?: string | null
  created_at?: string | null
  /** Presigned GET for a screenshot event; short-lived. */
  url?: string | null
  expires_at?: string | null
}

// The run record from GET /runs/{id}. Same fields RunDetail reads — this view
// shows a subset and does not duplicate its telemetry card.
interface RunRecord {
  run_id: string
  /** The machine that executed it, when one did. Null/absent on a run started
   *  locally — there is no device row behind those at all. Older backends do not
   *  send the field, which is why "does this run belong here?" only ever asks
   *  the question when a value is present. */
  device_id?: string | null
  task?: string
  model?: string
  status?: string
  num_steps?: number
  total_input_tokens?: number
  total_output_tokens?: number
  created_at?: string | number
  started_at?: string | number
  completed_at?: string | number
  result?: unknown
  error_message?: unknown
}

// How often the timeline asks for events NEWER than the highest seq it holds.
// A computer-use step is seconds, so this tracks a live run turn by turn.
const EVENT_POLL_MS = 3_000

// How often the run RECORD (status, steps, result) is re-read. Four times
// slower than the timeline because it is the expensive call: GET /runs/{id}
// still answers with the run's ENTIRE event array, which this view discards.
// The timeline never pays that — it pages incrementally off `since_seq` — and
// polling the record at the timeline's cadence would put the whole history back
// on the wire every three seconds, which on a 300-turn run is the difference
// between cheap and untenable.
const RUN_POLL_MS = 12_000

// Events per page. Big enough that a fresh load of a long run is a handful of
// requests, small enough that one response is not megabytes of tool payloads.
const EVENT_PAGE = 200

// Ceiling on pages drained in a single tick. `has_more` is the backend's
// signal and normally ends the loop; this only stops a malformed answer (one
// that always says "more") from spinning forever.
const MAX_PAGES_PER_TICK = 25

// Distance from the bottom, in px, within which the timeline counts as "at the
// tail" and keeps following. Not zero: a live run appends while the operator is
// reading, and sub-pixel scroll heights mean an exact comparison drops out of
// follow mode on its own.
const TAIL_SLACK_PX = 48

// How long a presigned URL is assumed good when the backend does not state an
// expiry. Only ever re-signs sooner than needed — the harmless direction.
const URL_ASSUMED_LIFE_MS = 4 * 60_000

// Lines of narration or raw payload shown before a row collapses behind "Show
// more". A single model turn can be several hundred words and a tool payload
// can be a page of JSON; either one uncollapsed pushes every other event off
// the screen, which is the opposite of what a timeline is for.
const CLAMP_LINES = 12

const TERMINAL = new Set(['completed', 'failed', 'cancelled', 'error', 'done', 'stopped'])

function isTerminal(status: string | undefined): boolean {
  return TERMINAL.has((status || '').toLowerCase())
}

function str(v: unknown): string {
  if (v === undefined || v === null) return ''
  return String(v)
}

function safeJson(input: unknown): string {
  if (input === undefined || input === null) return ''
  try {
    return JSON.stringify(input, null, 2)
  } catch {
    return String(input)
  }
}

// Read one event off the wire. The events route is landing in parallel with
// this view, so the two fields it adds over the stored RunEvent are read from
// either the row or its `data` — the same tolerance ScreenCard's normalizeFrame
// applies, and for the same reason: a strict read that misses `url` renders a
// timeline with no screenshots in it, which looks like a worker that never
// captured any rather than like a field named differently than expected.
function normalizeEvent(raw: unknown): RunEventRow | null {
  if (!raw || typeof raw !== 'object') return null
  const row = raw as Record<string, unknown>
  const seq = typeof row.seq === 'number' ? row.seq : Number(row.seq)
  if (!Number.isFinite(seq)) return null
  const data = (row.data ?? null) as Record<string, unknown> | null
  const pick = (key: string): string | null => {
    const top = row[key]
    if (typeof top === 'string' && top) return top
    const inner = data?.[key]
    if (typeof inner === 'string' && inner) return inner
    return null
  }
  return {
    seq,
    type: str(row.type),
    data,
    artifact_object: pick('artifact_object'),
    artifact_kind: pick('artifact_kind'),
    created_at: pick('created_at'),
    // `artifact_url` is what the events route returns; `url` is only the shape
    // the device screenshot routes use. Reading the wrong one left every frame
    // in the timeline without an image while the event around it rendered fine.
    url: pick('artifact_url') ?? pick('url'),
    expires_at: pick('expires_at'),
  }
}

// Display order: OLDEST FIRST.
//
// A run is a narrative — a tool_use only means anything after the text that
// decided on it — so it is read top-down, and it matches RunDetail's timeline
// so the local and remote views are not two different idioms for one thing.
// Watching a live run wants the opposite end, and that is served by FOLLOWING
// the tail rather than by inverting the list: new events are appended and the
// pane stays pinned to the bottom, until the operator scrolls up, at which
// point following stops and a "jump to latest" control appears. Reversing the
// list instead would make a live run readable and a finished one backwards.
//
// The comparison is on time first, seq second, and that is not belt-and-braces.
// The desktop loop's `seq` counts its own events, but a screenshot uploaded to
// object storage is committed under a SEPARATE frame counter (agent.rs bumps
// `shot_seq`, not `seq`), so its event's seq is small and unrelated. Ordering on
// seq alone would rake every uploaded frame to the top of the run. `created_at`
// is stamped by the single worker that persists the bus, so it is one clock and
// it is the run's real order; seq only breaks ties within the same instant.
// A frame on the timeline. The worker labels its own uploads `screenshot`, while
// a frame that reached storage by another path arrives as a generic event with
// an image artifact, so both count.
function isShot(ev: RunEventRow): boolean {
  return ev.type === 'screenshot' || ev.artifact_kind === 'image'
}

function compareEvents(a: RunEventRow, b: RunEventRow): number {
  const at = a.created_at ? Date.parse(a.created_at) : NaN
  const bt = b.created_at ? Date.parse(b.created_at) : NaN
  if (Number.isFinite(at) && Number.isFinite(bt) && at !== bt) return at - bt
  return a.seq - b.seq
}

function FleetRun() {
  const navigate = useNavigate()
  // `deviceId` is absent only on the legacy /fleet/runs/:runId link when the run
  // turned out to belong to no machine — see FleetRunRedirect.
  const { runId = '', deviceId } = useParams<{ runId: string; deviceId?: string }>()
  const device = useDevice(deviceId)

  // Back to the machine's runs, which is the page this one is nested under —
  // never history.back(). A run is linkable and bookmarkable, so the operator
  // who arrived from a pasted URL has nothing behind them, and the URL is the
  // only thing that knows where up is.
  const onBack = useCallback(() => {
    if (deviceId) navigate(`/devices/${encodeURIComponent(deviceId)}/runs`)
    else navigate('/admin')
  }, [navigate, deviceId])

  const [run, setRun] = useState<RunRecord | null>(null)
  const [events, setEvents] = useState<RunEventRow[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  // When the current batch of presigned URLs was minted, for the pre-enlarge
  // freshness check.
  const [signedAt, setSignedAt] = useState(0)

  // The pagination cursor. A ref rather than state because the poll reads it on
  // every tick and must never fire against a stale closure — re-requesting from
  // an old seq is exactly the whole-history refetch this view exists to avoid.
  const sinceRef = useRef(-1)
  // URLs already re-signed once. A second failure is not an expiry (the object
  // is gone) and re-fetching would loop — same guard as ScreenCard.
  const resignedRef = useRef<Set<string>>(new Set())
  // Set when the incremental route answers 404: this build of the backend does
  // not have it yet. The full GET /runs/{id} then carries the timeline instead,
  // at the run-record cadence. Degraded on purpose and in the right direction —
  // the operator sees the run rather than an error — but noted in the UI,
  // because in this mode screenshots have no signed URL and cannot render.
  const [wholeHistoryOnly, setWholeHistoryOnly] = useState(false)
  const wholeHistoryRef = useRef(false)

  const merge = useCallback((incoming: RunEventRow[]) => {
    if (incoming.length === 0) return
    setEvents((prev) => {
      // Keyed by seq AND type: the two counters described in compareEvents mean
      // a frame event and a loop event can legitimately share a seq, and keying
      // on seq alone would silently drop one of them.
      const byKey = new Map(prev.map((e) => [`${e.seq}:${e.type}`, e]))
      for (const ev of incoming) byKey.set(`${ev.seq}:${ev.type}`, ev)
      return [...byKey.values()].sort(compareEvents)
    })
  }, [])

  // Drain everything after the cursor. One tick may span several pages: a run
  // that was started before this view was opened has a backlog, and a live one
  // can produce more than a page between polls.
  const pullEvents = useCallback(async (): Promise<void> => {
    if (wholeHistoryRef.current) return
    for (let page = 0; page < MAX_PAGES_PER_TICK; page++) {
      // No cursor yet is an ABSENT since_seq, not a negative one. The backend
      // validates since_seq >= 0, so sending the -1 sentinel made the very
      // first request of every run a 422 — the timeline never loaded once, and
      // an empty timeline reads as "this run recorded nothing" rather than as a
      // failed request.
      const cursor = sinceRef.current >= 0 ? `since_seq=${sinceRef.current}&` : ''
      const resp = await fetch(
        `${CU_BACKEND}/runs/${encodeURIComponent(runId)}/events?${cursor}limit=${EVENT_PAGE}`,
        { headers: authHeaders() },
      )
      if (resp.status === 404) {
        // Ambiguous between "no such run" and "no such route", so it is only
        // treated as the latter once the run record itself has loaded — a run
        // we can read cannot be a missing run.
        wholeHistoryRef.current = true
        setWholeHistoryOnly(true)
        return
      }
      if (!resp.ok) throw new Error(`Could not load the timeline (${resp.status})`)
      const body = await resp.json()
      const rows: unknown[] = Array.isArray(body) ? body : (body.events ?? [])
      const batch = rows.map(normalizeEvent).filter((e): e is RunEventRow => e !== null)
      merge(batch)
      setSignedAt(Date.now())
      // Fresh signatures — whatever failed to load before deserves one more try.
      resignedRef.current = new Set()
      for (const ev of batch) {
        if (ev.seq > sinceRef.current) sinceRef.current = ev.seq
      }
      const more = Boolean(
        (body as { has_more?: unknown; more?: unknown }).has_more ??
          (body as { more?: unknown }).more ??
          false,
      )
      // A page that advanced nothing would loop forever whatever `has_more`
      // claims, so an empty batch ends the drain regardless.
      if (!more || batch.length === 0) return
    }
  }, [runId, merge])

  const pullRun = useCallback(async (): Promise<void> => {
    const resp = await fetch(`${CU_BACKEND}/runs/${encodeURIComponent(runId)}`, {
      headers: authHeaders(),
    })
    if (!resp.ok) throw new Error(`Could not load the run (${resp.status})`)
    const body = await resp.json()
    const record = (body.run ?? body) as RunRecord
    setRun(record)
    // Only in fallback mode is the event array worth anything: normally the
    // timeline is authoritative and this payload is dead weight we cannot
    // decline (see RUN_POLL_MS).
    if (wholeHistoryRef.current && Array.isArray(body.events)) {
      const rows = (body.events as unknown[])
        .map(normalizeEvent)
        .filter((e): e is RunEventRow => e !== null)
      setEvents(rows.sort(compareEvents))
    }
  }, [runId])

  // First load: the record decides whether this run is even readable, so a
  // failure here is the error card. The timeline follows.
  useEffect(() => {
    let cancelled = false
    sinceRef.current = -1
    wholeHistoryRef.current = false
    setWholeHistoryOnly(false)
    setEvents([])
    setRun(null)
    setLoading(true)
    setError(null)
    ;(async () => {
      try {
        await pullRun()
        if (cancelled) return
        await pullEvents()
        if (cancelled) return
        // Fallback discovered during the drain: the record's own events array
        // is the timeline, and it was fetched before we knew that.
        if (wholeHistoryRef.current) await pullRun()
      } catch (err) {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Network error')
      } finally {
        if (!cancelled) setLoading(false)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [runId, pullRun, pullEvents])

  const live = run != null && !isTerminal(run.status)

  // Polling stops the moment the run is terminal. A finished run is static:
  // every further request would re-sign URLs for a history that cannot change,
  // forever, on a tab somebody left open. Nothing re-arms it either — a run
  // does not come back from `completed`.
  useEffect(() => {
    if (!live) return
    const timer = setInterval(() => {
      pullEvents().catch(() => {
        // One dropped poll is corrected by the next. A live run is not worth
        // replacing the timeline on screen with an error card.
      })
    }, EVENT_POLL_MS)
    return () => clearInterval(timer)
  }, [live, pullEvents])

  useEffect(() => {
    if (!live) return
    const timer = setInterval(() => {
      pullRun().catch(() => {})
    }, RUN_POLL_MS)
    return () => clearInterval(timer)
  }, [live, pullRun])

  // A screenshot whose URL died. Ask for that ONE event again rather than
  // reloading the timeline: `since_seq = seq - 1, limit = 1` is the cheapest
  // re-sign available and leaves the cursor alone, so the poll does not
  // re-deliver everything after it.
  const resign = useCallback(
    async (ev: RunEventRow) => {
      if (!ev.url || wholeHistoryRef.current) return
      if (resignedRef.current.has(ev.url)) return
      resignedRef.current.add(ev.url)
      try {
        const resp = await fetch(
          `${CU_BACKEND}/runs/${encodeURIComponent(runId)}/events` +
            `?since_seq=${ev.seq - 1}&limit=1`,
          { headers: authHeaders() },
        )
        if (!resp.ok) return
        const body = await resp.json()
        const rows: unknown[] = Array.isArray(body) ? body : (body.events ?? [])
        const fresh = rows.map(normalizeEvent).filter((e): e is RunEventRow => e !== null)
        merge(fresh)
      } catch {
        // The row renders its "would not load" marker either way.
      }
    },
    [runId, merge],
  )

  // Enlarging happens in place, so the same URL the thumbnail used is about to
  // be fetched at full size — and a thumbnail the browser already decoded keeps
  // rendering long after its signature died, which makes an expired link
  // invisible until exactly this moment. Re-sign first when the batch is old
  // enough to be a risk (ScreenCard's openShot, same reasoning).
  const refreshIfStale = useCallback(
    async (ev: RunEventRow) => {
      const expiresMs = ev.expires_at ? Date.parse(ev.expires_at) : NaN
      const expired = Number.isFinite(expiresMs) && expiresMs <= Date.now()
      if (!expired && Date.now() - signedAt <= URL_ASSUMED_LIFE_MS) return
      resignedRef.current.delete(ev.url ?? '')
      await resign(ev)
    },
    [signedAt, resign],
  )

  // Whose run this actually is, versus whose page it was opened under. The two
  // disagree in two real cases: a run started locally has no device at all
  // (`device_id` null — nothing in the fleet executed it), and a hand-edited or
  // stale link can put a real run under the wrong machine. Neither is rendered
  // silently: the run is still readable — refusing to show a run someone asked
  // for by id helps nobody — but the page says out loud that this machine is not
  // where it happened, and offers the machine where it did.
  //
  // Only asked when the backend actually sent a device_id: a build that omits
  // the field must not make every run look misfiled.
  const foreign = deviceId != null && run != null && run.device_id !== undefined &&
    run.device_id !== deviceId

  return (
    <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max-wide)', margin: '0 auto' }}>
      <Header
        run={run}
        runId={runId}
        onBack={onBack}
        backLabel={device ? `← ${deviceLabel(device)}` : deviceId ? '← Runs' : '← Devices'}
        subtitle={!foreign && device ? { device } : null}
      />

      {foreign && (
        <div
          className="error-message"
          style={{
            marginBottom: 'var(--sp-4)',
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-3)',
          }}
        >
          <span style={{ flex: 1, minWidth: 0 }}>
            {run?.device_id
              ? 'This run did not happen on the machine whose page you opened it from — it belongs to another machine in the fleet.'
              : 'This run was started locally, not by a fleet machine, so it belongs to no machine on this page.'}
          </span>
          {run?.device_id && (
            <Button
              size="sm"
              onClick={() =>
                navigate(
                  `/devices/${encodeURIComponent(run.device_id as string)}/runs/${encodeURIComponent(runId)}`,
                )
              }
              style={{ flexShrink: 0 }}
            >
              Open on its machine →
            </Button>
          )}
        </div>
      )}

      {/* A failed run's reason goes above everything, in the error style, before
          any card the eye has to scan past. The whole point of opening a failed
          run is this sentence. */}
      {run && isFailed(run.status) && (
        <div className="error-message" style={{ marginBottom: 'var(--sp-4)', whiteSpace: 'pre-wrap' }}>
          {asText(run.error_message) || 'This run failed, and reported no reason.'}
        </div>
      )}

      {error && !run && (
        <div className="error-message" style={{ marginBottom: 'var(--sp-4)' }}>
          {error}
        </div>
      )}

      {loading && !run && (
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--sp-3)',
            padding: 'var(--sp-5)',
            color: 'var(--sb-text-muted)',
          }}
        >
          <Spinner /> Loading run…
        </div>
      )}

      {run && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
          <Facts run={run} live={live} />
          <Result run={run} />
          <Timeline
            events={events}
            live={live}
            run={run}
            degraded={wholeHistoryOnly}
            onBadUrl={resign}
            onEnlarge={refreshIfStale}
          />
        </div>
      )}
    </div>
  )
}

function isFailed(status: string | undefined): boolean {
  const s = (status || '').toLowerCase()
  return s === 'failed' || s === 'error'
}

// ───────────────────────────────────────────────────────── header

function Header({
  run,
  runId,
  onBack,
  backLabel,
  subtitle,
}: {
  run: RunRecord | null
  runId: string
  onBack: () => void
  /** Names the destination rather than the gesture: back from a run goes to one
   *  named machine's runs, and saying which one is the difference between a
   *  navigation and a guess. */
  backLabel: string
  /** The machine this run ran on, when the page is sure of it. Null while the
   *  device is still loading, when it cannot be read, and — deliberately —
   *  whenever the run does not belong to it, since the banner below is then
   *  saying the opposite of what this line would. */
  subtitle: { device: DeviceHead } | null
}) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-3)',
        marginBottom: 'var(--sp-5)',
      }}
    >
      <Button
        variant="ghost"
        size="sm"
        onClick={onBack}
        title="Back to this machine's runs"
        style={{ flexShrink: 0, maxWidth: 220, overflow: 'hidden', textOverflow: 'ellipsis' }}
      >
        {backLabel}
      </Button>
      <div style={{ minWidth: 0, flex: 1 }}>
        <div
          style={{
            fontSize: 'var(--fs-xl)',
            fontWeight: 600,
            color: 'var(--sb-gold-bright)',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
          title={run?.task}
        >
          {run?.task || '(untitled task)'}
        </div>
        {/* Whose run this is, said before the id: a run opened from a link is
            otherwise a task with no machine attached to it. */}
        {subtitle && (
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              fontSize: 'var(--fs-sm)',
              color: 'var(--sb-text-muted)',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}
          >
            <DeviceDot device={subtitle.device} />
            <span>
              {deviceLabel(subtitle.device)} · {deviceLine(subtitle.device)}
            </span>
          </div>
        )}
        <div
          style={{
            fontSize: 'var(--fs-xs)',
            fontFamily: 'var(--font-mono)',
            color: 'var(--sb-text-faint)',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
        >
          {run?.run_id ?? runId}
        </div>
      </div>
      {run?.model && (
        <Chip mono tone="neutral" title={run.model} style={{ flexShrink: 0 }}>
          {run.model}
        </Chip>
      )}
      <StatusPill status={run?.status} />
    </div>
  )
}

// ───────────────────────────────────────────────────────── facts

function Facts({ run, live }: { run: RunRecord; live: boolean }) {
  return (
    <Card title={<SectionTitle>Run</SectionTitle>}>
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(96px, 1fr))',
          gap: 'var(--sp-4)',
        }}
      >
        <StatChip label="Steps" value={String(run.num_steps ?? 0)} />
        <StatChip label="Input tok" value={formatTokens(run.total_input_tokens)} />
        <StatChip label="Output tok" value={formatTokens(run.total_output_tokens)} />
        <StatChip label="Started" value={relativeTime(run.started_at ?? run.created_at)} />
        <StatChip
          label={live ? 'State' : 'Finished'}
          value={live ? 'in flight' : relativeTime(run.completed_at)}
        />
      </div>
    </Card>
  )
}

// ───────────────────────────────────────────────────────── result

// The run's own summary, rendered with RunDetail's markdown reader rather than a
// second one — a summary must not read differently depending on which machine
// executed it.
function Result({ run }: { run: RunRecord }) {
  const text = summaryText(run.result)
  if (!text) return null
  return (
    <Card title={<SectionTitle>Result</SectionTitle>}>
      <Markdown text={text} />
    </Card>
  )
}

// ───────────────────────────────────────────────────────── timeline

function Timeline({
  events,
  live,
  run,
  degraded,
  onBadUrl,
  onEnlarge,
}: {
  events: RunEventRow[]
  live: boolean
  run: RunRecord
  degraded: boolean
  onBadUrl: (ev: RunEventRow) => void
  onEnlarge: (ev: RunEventRow) => Promise<void>
}) {
  const scrollRef = useRef<HTMLDivElement | null>(null)
  // Whether the pane is still riding the tail. Starts true so a live run opens
  // on its newest events, which is what someone opening a running machine came
  // to see; goes false the instant the operator scrolls away from the bottom.
  const [following, setFollowing] = useState(true)
  const count = events.length
  // Frames are counted separately because this is the only place they are
  // listed: the device pane shows a machine's latest frame and nothing more, so
  // "this run captured 14 frames" is a fact only the run's own record carries.
  const shots = events.reduce((n, ev) => n + (isShot(ev) ? 1 : 0), 0)

  const onScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    const atTail = el.scrollHeight - el.scrollTop - el.clientHeight <= TAIL_SLACK_PX
    setFollowing(atTail)
  }, [])

  // Follow the tail — but only while following. Scrolling the pane back to the
  // bottom under someone who deliberately scrolled up to read a step from four
  // minutes ago is the one thing a live view must never do; a run appending
  // every few seconds would make the earlier history unreadable.
  useEffect(() => {
    if (!following) return
    const el = scrollRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [count, following])

  const jumpToLatest = useCallback(() => {
    const el = scrollRef.current
    if (el) el.scrollTop = el.scrollHeight
    setFollowing(true)
  }, [])

  return (
    <Card
      title={
        <SectionTitle>
          Timeline{count ? ` · ${count} events` : ''}
          {shots ? ` · ${shots} frames` : ''}
        </SectionTitle>
      }
      actions={
        live && !following ? (
          <Button size="sm" variant="secondary" onClick={jumpToLatest}>
            ↓ Jump to latest
          </Button>
        ) : live ? (
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
            following · every {Math.round(EVENT_POLL_MS / 1000)}s
          </span>
        ) : null
      }
    >
      {degraded && (
        <div
          style={{
            marginBottom: 'var(--sp-3)',
            fontSize: 'var(--fs-sm)',
            lineHeight: 1.5,
            color: 'var(--sb-text-muted)',
          }}
        >
          This backend does not serve the incremental timeline yet, so the whole run is re-read
          every {Math.round(RUN_POLL_MS / 1000)}s and its screenshots arrive as storage keys with
          no link a browser can open. The narration and the actions below are complete.
        </div>
      )}

      {count === 0 ? (
        <EmptyState
          icon="⏱"
          title={live ? 'Nothing reported yet' : 'This run recorded no timeline'}
          hint={
            live
              ? 'The machine has taken the run but has not reported a step. The first narration and screenshot land within a few seconds of its first turn.'
              : `The run ended ${(run.status || 'without events').toLowerCase()} without reporting a single step — usually a worker that lost its connection before its first turn.`
          }
        />
      ) : (
        <div
          ref={scrollRef}
          onScroll={onScroll}
          style={{
            maxHeight: '62vh',
            overflowY: 'auto',
            paddingRight: 'var(--sp-2)',
            display: 'flex',
            flexDirection: 'column',
            gap: 'var(--sp-3)',
            borderLeft: '2px solid var(--sb-gold-line)',
            paddingLeft: 'var(--sp-4)',
          }}
        >
          {events.map((ev) => (
            <EventRow
              key={`${ev.seq}:${ev.type}`}
              ev={ev}
              onBadUrl={onBadUrl}
              onEnlarge={onEnlarge}
            />
          ))}
        </div>
      )}
    </Card>
  )
}

// One event. The shapes are RunDetail's, so a run reads the same whichever
// machine ran it; only the screenshot case differs, because there is no local
// file to resolve here.
function EventRow({
  ev,
  onBadUrl,
  onEnlarge,
}: {
  ev: RunEventRow
  onBadUrl: (ev: RunEventRow) => void
  onEnlarge: (ev: RunEventRow) => Promise<void>
}) {
  const d = (ev.data ?? {}) as Record<string, unknown>

  if (isShot(ev)) {
    return <Shot ev={ev} onBadUrl={onBadUrl} onEnlarge={onEnlarge} />
  }

  switch (ev.type) {
    // `model_delta` is the streaming form of `text`; the desktop currently
    // posts only whole turns, but reading both costs one label and means a
    // desktop that starts streaming does not silently render as unknown JSON.
    case 'text':
    case 'model_delta': {
      const text = str(d.text ?? d.delta)
      if (!text.trim()) return null
      return <Clamped text={text} prose />
    }
    case 'action':
    case 'tool_use':
      return (
        <div>
          <ActionChip name={str(d.name)} input={d.input} />
        </div>
      )
    case 'tool_result': {
      const body = str(d.content ?? d.output ?? d.text) || safeJson(d)
      if (!body.trim()) return null
      return <Clamped text={body} />
    }
    case 'status': {
      // The desktop posts one of these per turn, carrying { turn, state }. It is
      // the only marker of where one turn ends and the next begins, so it is
      // rendered as a rule across the rail rather than as another line of text.
      const turn = d.turn
      if (turn != null) return <TurnRule turn={str(turn)} />
      return <Marker text={str(d.state ?? d.status ?? d.message)} tone="muted" />
    }
    case 'telemetry':
      return <Marker text={`telemetry · ${safeJson(ev.data)}`} tone="muted" />
    case 'done':
      return <Marker icon="✓" text={`done${d.reason ? ` (${str(d.reason)})` : ''}`} tone="gold" />
    case 'error':
      return <Marker icon="✕" text={`error: ${str(d.error ?? d.message)}`} tone="danger" />
    default:
      return <Clamped text={`${ev.type}: ${safeJson(ev.data)}`} />
  }
}

function TurnRule({ turn }: { turn: string }) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-2)',
        marginTop: 'var(--sp-2)',
      }}
    >
      <SectionTitle>Turn {turn}</SectionTitle>
      <div style={{ flex: 1, height: 1, background: 'var(--sb-gold-line)' }} />
    </div>
  )
}

// Long bodies collapse. `prose` is the model's own narration, set in the sans
// reading face; everything else is a payload and stays mono, because the thing
// being looked for in a tool result is usually a literal.
function Clamped({ text, prose }: { text: string; prose?: boolean }) {
  const [open, setOpen] = useState(false)
  const lines = useMemo(() => text.split('\n'), [text])
  const long = lines.length > CLAMP_LINES || text.length > 1200
  const shown = open || !long ? text : lines.slice(0, CLAMP_LINES).join('\n')

  return (
    <div style={{ minWidth: 0 }}>
      <div
        style={{
          margin: 0,
          fontFamily: prose ? 'var(--font-sans)' : 'var(--font-mono)',
          fontSize: prose ? 'var(--fs-md)' : 'var(--fs-sm)',
          lineHeight: prose ? 1.6 : 1.5,
          color: prose ? 'var(--sb-text)' : 'var(--sb-text-muted)',
          whiteSpace: 'pre-wrap',
          // Long unbroken tokens — a base64 blob, a URL — are what actually
          // widen a flex column past its container.
          overflowWrap: 'anywhere',
          wordBreak: 'break-word',
        }}
      >
        {shown}
        {long && !open && '…'}
      </div>
      {long && (
        <Button variant="ghost" size="sm" onClick={() => setOpen((v) => !v)} style={{ marginTop: 4 }}>
          {open ? 'Show less' : `Show all ${lines.length} lines`}
        </Button>
      )}
    </div>
  )
}

// A screenshot on the timeline. It expands IN PLACE rather than into a lightbox:
// the order is the whole point of this view — the frame after that click, the
// frame after the next one — and a modal takes the operator out of exactly the
// sequence they opened it to read.
function Shot({
  ev,
  onBadUrl,
  onEnlarge,
}: {
  ev: RunEventRow
  onBadUrl: (ev: RunEventRow) => void
  onEnlarge: (ev: RunEventRow) => Promise<void>
}) {
  const [big, setBig] = useState(false)
  const [broken, setBroken] = useState(false)
  // One re-sign per frame, ever. An expired signature is fixed by a fresh URL;
  // a frame whose object has been expired out of the bucket fails again on the
  // new one, and retrying that is a fetch loop against something that is gone.
  const triedRef = useRef(0)

  // No signed URL: either the run's frames stayed on the worker's own disk
  // (artifact_kind `screenshot_local`, a path meaningless on this machine) or
  // the backend answered without one. Say a frame exists rather than render a
  // broken tile — and never try the local path, which belongs to another
  // machine entirely.
  if (!ev.url) {
    return <Marker icon="📸" text="screenshot — kept on the worker, no link to it" tone="muted" />
  }

  if (broken) {
    return (
      <Marker
        icon="📸"
        text="screenshot — this frame would not load, even with a fresh link"
        tone="muted"
      />
    )
  }

  return (
    <img
      src={ev.url}
      alt={`screen at step ${ev.seq}`}
      loading="lazy"
      onClick={async () => {
        if (!big) await onEnlarge(ev)
        setBig((v) => !v)
      }}
      onError={() => {
        // Nearly always a signature that expired while this pane sat open, so
        // the first failure buys one fresh URL and the second is reported.
        if (triedRef.current >= 1) {
          setBroken(true)
          return
        }
        triedRef.current += 1
        onBadUrl(ev)
      }}
      style={{
        display: 'block',
        width: big ? '100%' : 220,
        maxWidth: big ? 960 : '100%',
        height: 'auto',
        borderRadius: 'var(--r-sm)',
        border: `1px solid ${big ? 'var(--sb-border-gold)' : 'var(--sb-border)'}`,
        background: 'var(--sb-surface-2)',
        boxShadow: big ? 'var(--shadow-2)' : undefined,
        cursor: big ? 'zoom-out' : 'zoom-in',
      }}
    />
  )
}

// A compact single-line marker on the rail — RunDetail's PillLine.
function Marker({
  icon,
  text,
  tone,
}: {
  icon?: string
  text: string
  tone: 'muted' | 'gold' | 'danger'
}) {
  if (!text.trim()) return null
  const color =
    tone === 'danger'
      ? 'var(--sb-danger-bright)'
      : tone === 'gold'
        ? 'var(--sb-gold)'
        : 'var(--sb-text-muted)'
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'baseline',
        gap: 6,
        fontSize: 'var(--fs-sm)',
        color,
        wordBreak: 'break-word',
      }}
    >
      {icon && <span aria-hidden>{icon}</span>}
      <span>{text}</span>
    </div>
  )
}

// ───────────────────────────────────────────────────────── legacy link

// #/fleet/runs/<runId> — where this view used to live, before a run was nested
// under the machine that ran it. Kept because the URL is out there: it was what
// the Devices pane linked to for the whole life of that pane, and anything
// bookmarked or pasted from it must not land on the fleet home with no
// explanation.
//
// The old URL names a run and no machine, so the machine is looked up: the run
// record carries `device_id`, and that is the missing half of the new address.
// A run with no device (started locally) has no nested address to send it to, so
// it is simply rendered here, unattached — the alternative is bouncing a valid
// link somewhere it did not ask for.
export function FleetRunRedirect() {
  const { runId = '' } = useParams<{ runId: string }>()
  const [deviceId, setDeviceId] = useState<string | null | undefined>(undefined)

  useEffect(() => {
    let cancelled = false
    ;(async () => {
      try {
        const resp = await fetch(`${CU_BACKEND}/runs/${encodeURIComponent(runId)}`, {
          headers: authHeaders(),
        })
        if (!resp.ok) {
          // Unreadable run: fall through to FleetRun, whose error card says so
          // properly. Redirecting to a machine we could not name would be worse.
          if (!cancelled) setDeviceId(null)
          return
        }
        const body = await resp.json()
        const record = (body.run ?? body) as RunRecord
        if (!cancelled) setDeviceId(record.device_id ?? null)
      } catch {
        if (!cancelled) setDeviceId(null)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [runId])

  if (deviceId === undefined) {
    return (
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--sp-3)',
          padding: 'var(--sp-5)',
          color: 'var(--sb-text-muted)',
        }}
      >
        <Spinner /> Loading run…
      </div>
    )
  }

  if (deviceId === null) return <FleetRun />

  // `replace` so Back skips the resolver rather than bouncing through it again.
  return (
    <Navigate
      to={`/devices/${encodeURIComponent(deviceId)}/runs/${encodeURIComponent(runId)}`}
      replace
    />
  )
}

export default FleetRun

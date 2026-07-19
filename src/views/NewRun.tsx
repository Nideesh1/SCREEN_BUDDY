import { useEffect, useState, useCallback } from 'react'
import { useNavigate } from 'react-router-dom'
import { invoke } from '@tauri-apps/api/core'
import { safeInvoke, CU_BACKEND, authHeaders } from '../lib'
import { Card, SectionTitle, Button, Chip, Spinner } from '../ui'
import { Link } from 'react-router-dom'
import { createSchedule, cronLabel } from '../schedules'
import RunsTabs from './RunsTabs'

// Default model used across the launcher when no template overrides it.
const DEFAULT_MODEL = 'claude-sonnet-5'

// The browser's IANA timezone — used verbatim for any schedule we create so the
// backend interprets the cron in the user's local zone.
const BROWSER_TZ = Intl.DateTimeFormat().resolvedOptions().timeZone

// Cron presets for the "Schedule instead of running now" affordance. `custom`
// reveals a raw cron field; every other option sets `cron` directly.
const CRON_PRESETS: { id: string; label: string; cron: string }[] = [
  { id: 'daily9', label: 'Daily 9am', cron: '0 9 * * *' },
  { id: 'weeklyMon', label: 'Weekly Mon 9am', cron: '0 9 * * 1' },
  { id: 'hourly', label: 'Hourly', cron: '0 * * * *' },
  { id: 'custom', label: 'Custom…', cron: '' },
]

// A run template, served by `GET ${CU_BACKEND}/templates` (snake_case). Each
// seeds the task textarea + model, and may suggest a Pinned library set (matched
// by name) and name a credential the run is expected to use.
interface RunTemplate {
  id: string
  name: string
  taskScaffold: string
  model: string
  suggestedSetName?: string
  credentialTarget?: string
  // Slot names the scaffold references as `{name}` placeholders (e.g.
  // ["items", "address", "card"]). Each renders as its own form field.
  requiredInputs: string[]
}

// Raw template shape as returned by the backend (snake_case).
interface RawTemplate {
  template_id: string
  name: string
  task_scaffold?: string
  model?: string
  suggested_set_name?: string
  credential_target?: string
  required_inputs?: string[]
  builtin?: boolean
}

const BLANK_TEMPLATE: RunTemplate = {
  id: 'blank',
  name: 'Blank run',
  taskScaffold: '',
  model: DEFAULT_MODEL,
  requiredInputs: [],
}

function normalizeTemplate(t: RawTemplate): RunTemplate {
  return {
    id: t.template_id,
    name: t.name,
    taskScaffold: t.task_scaffold ?? '',
    model: t.model || DEFAULT_MODEL,
    suggestedSetName: t.suggested_set_name || undefined,
    credentialTarget: t.credential_target || undefined,
    requiredInputs: Array.isArray(t.required_inputs) ? t.required_inputs : [],
  }
}

// Substitute `{name}` placeholders in the scaffold with the values the user
// typed. Only FILLED inputs are substituted; an empty value (or a placeholder
// with no matching input) is left literal so nothing crashes and the agent can
// still see which slots weren't provided.
function substituteInputs(scaffold: string, values: Record<string, string>): string {
  let out = scaffold
  for (const [key, raw] of Object.entries(values)) {
    const value = raw.trim()
    if (!value) continue
    out = out.split(`{${key}}`).join(value)
  }
  return out
}

// "items" → "Items", "delivery_address" → "Delivery Address".
function humanizeInput(name: string): string {
  return name
    .split(/[_\s]+/)
    .filter(Boolean)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(' ')
}

// Inputs that tend to hold long / multi-line values render as a textarea.
function isMultilineInput(name: string): boolean {
  return /item|address|list|note|detail|desc/i.test(name)
}

// Every `{token}` placeholder actually present in the scaffold, in scaffold
// order, de-duplicated. This is the source of truth for which fields to render.
function scaffoldPlaceholders(scaffold: string): string[] {
  const out: string[] = []
  const seen = new Set<string>()
  const re = /\{(\w+)\}/g
  let m: RegExpExecArray | null
  while ((m = re.exec(scaffold)) !== null) {
    const name = m[1]
    if (!seen.has(name)) {
      seen.add(name)
      out.push(name)
    }
  }
  return out
}

// The full set of variables to prompt for: placeholders present in the scaffold
// (in scaffold order) UNIONed with any declared required inputs that aren't in
// the scaffold, de-duplicated. Drives both the form fields and the "has
// variables?" condition.
function mergeTemplateVars(scaffold: string, requiredInputs: string[]): string[] {
  const out = scaffoldPlaceholders(scaffold)
  const seen = new Set(out)
  for (const name of requiredInputs) {
    if (!seen.has(name)) {
      seen.add(name)
      out.push(name)
    }
  }
  return out
}

interface PinnedSet {
  id: string
  name: string
  count: number
}

// Constrained "Runs" launcher: pick a template, edit the task, attach a
// library-only reference set, choose a model, then Start. On Start the FRONTEND
// mints the run via `POST /runs` and only then invokes the agent with the
// returned run_id — so the run row always exists before execution begins.
function NewRun() {
  const navigate = useNavigate()
  // BYOK gate: a validated Anthropic key is REQUIRED before the launcher unlocks.
  // null = still checking; false = show onboarding; true = show launcher.
  const [hasKey, setHasKey] = useState<boolean | null>(null)
  const [templates, setTemplates] = useState<RunTemplate[]>([BLANK_TEMPLATE])
  const [templateId, setTemplateId] = useState<string>(BLANK_TEMPLATE.id)
  const [task, setTask] = useState('')
  // Values for the selected template's `required_inputs`, keyed by slot name.
  // Substituted into the scaffold's `{name}` placeholders to build the run task.
  const [inputValues, setInputValues] = useState<Record<string, string>>({})
  const [model, setModel] = useState(DEFAULT_MODEL)
  const [sets, setSets] = useState<PinnedSet[]>([])
  const [pinnedSetIds, setPinnedSetIds] = useState<string[]>([])
  const [starting, setStarting] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // ── Scheduling affordance ──────────────────────────────────────────────────
  // When on, Start becomes "Create schedule" (POST /schedules) instead of an
  // immediate run. cron is driven by a preset; `custom` reveals the raw field.
  const [scheduleMode, setScheduleMode] = useState(false)
  const [scheduleName, setScheduleName] = useState('')
  const [cronPreset, setCronPreset] = useState<string>(CRON_PRESETS[0].id)
  const [customCron, setCustomCron] = useState('')
  const [scheduling, setScheduling] = useState(false)
  const [scheduled, setScheduled] = useState<string | null>(null)

  // The effective cron: the raw field when "custom" is selected, else the preset.
  const effectiveCron =
    cronPreset === 'custom' ? customCron.trim() : CRON_PRESETS.find((p) => p.id === cronPreset)?.cron ?? ''

  const loadSets = useCallback(async () => {
    const res = await safeInvoke<PinnedSet[]>('pinned_list')
    if (res.ok) setSets(res.data ?? [])
  }, [])

  // Toggle a set in/out of the multi-selection.
  const toggleSet = useCallback((id: string) => {
    setPinnedSetIds((prev) =>
      prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id],
    )
  }, [])

  useEffect(() => {
    loadSets()
  }, [loadSets])

  // On mount, ask Rust whether an encrypted key is already stored. Default to
  // false (show onboarding) on any error so we never silently allow a keyless run.
  useEffect(() => {
    let active = true
    ;(async () => {
      const res = await safeInvoke<boolean>('has_anthropic_key')
      if (active) setHasKey(res.ok ? res.data : false)
    })()
    return () => {
      active = false
    }
  }, [])

  // Templates now live on the backend. Fetch them on mount; the payload may be a
  // bare array or `{ templates: [] }`. On any failure / empty result, fall back
  // to a single Blank run so the launcher still works.
  useEffect(() => {
    let active = true
    ;(async () => {
      try {
        const resp = await fetch(`${CU_BACKEND}/templates`, { headers: authHeaders() })
        if (!resp.ok) return
        const body = (await resp.json()) as RawTemplate[] | { templates?: RawTemplate[] }
        const raw = Array.isArray(body) ? body : body.templates ?? []
        if (!active || raw.length === 0) return
        setTemplates(raw.map(normalizeTemplate))
      } catch {
        // keep the Blank-run fallback already seeded in state
      }
    })()
    return () => {
      active = false
    }
  }, [])

  const template = templates.find((t) => t.id === templateId)

  // The variables to prompt for: every `{token}` placeholder present in the
  // scaffold (`task` holds the scaffold source), UNIONed with the template's
  // declared required inputs, in scaffold order first. Drives the form fields
  // and the "this template has variables" branch below.
  const templateVars = mergeTemplateVars(task, template?.requiredInputs ?? [])

  // The task that actually launches the run: when the template has variables we
  // substitute their values into the scaffold's `{name}` placeholders;
  // otherwise the raw task text is used verbatim (unchanged behavior).
  const finalTask = templateVars.length > 0 ? substituteInputs(task, inputValues) : task

  // Update a single required-input field.
  const setInputValue = useCallback((name: string, value: string) => {
    setInputValues((prev) => ({ ...prev, [name]: value }))
  }, [])

  // Apply a template: prefill task + model, reset input values, and (if its
  // suggested set name maps to a Pinned library set) pre-select that set.
  const applyTemplate = useCallback(
    (t: RunTemplate | undefined) => {
      setTemplateId(t?.id ?? BLANK_TEMPLATE.id)
      if (!t) return
      setTask(t.taskScaffold)
      setModel(t.model)
      setInputValues({})
      if (t.suggestedSetName) {
        const match = sets.find((s) => s.name === t.suggestedSetName)
        setPinnedSetIds(match ? [match.id] : [])
      } else {
        setPinnedSetIds([])
      }
    },
    [sets],
  )

  const start = useCallback(async () => {
    if (!finalTask.trim() || starting) return
    setStarting(true)
    setError(null)
    try {
      // a) Mint the run row on the backend FIRST.
      const resp = await fetch(`${CU_BACKEND}/runs`, {
        method: 'POST',
        headers: { ...authHeaders(), 'content-type': 'application/json' },
        body: JSON.stringify({ task: finalTask, model, pinned_image_refs: [] }),
      })
      if (!resp.ok) {
        setError(`Could not create run (HTTP ${resp.status}). Agent not started.`)
        return
      }
      const data = (await resp.json()) as { run_id?: string }
      const runId = data.run_id
      if (!runId) {
        setError('Backend did not return a run id. Agent not started.')
        return
      }

      // b) Only now kick off the agent, handing it the pre-minted run id.
      await invoke('start_agent_task', {
        prompt: finalTask,
        auth: localStorage.getItem('screen_buddy_session_token') ?? undefined,
        pinnedSetIds,
        runId,
        model,
        backend: CU_BACKEND,
      })

      // c) Drill into the run-detail route, which mounts the live panel.
      navigate('/runs/' + runId)
    } catch (err) {
      setError(`Failed to start run: ${err instanceof Error ? err.message : String(err)}`)
    } finally {
      setStarting(false)
    }
  }, [finalTask, model, pinnedSetIds, starting, navigate])

  // Create a schedule instead of running now. Pins the single first selected set
  // (the backend schedule references one pinned_set_id). Navigates to the
  // Scheduled list on success.
  const createScheduleNow = useCallback(async () => {
    if (!finalTask.trim() || !scheduleName.trim() || !effectiveCron || scheduling) return
    setScheduling(true)
    setError(null)
    setScheduled(null)
    try {
      await createSchedule({
        name: scheduleName.trim(),
        cron: effectiveCron,
        timezone: BROWSER_TZ,
        task: finalTask,
        model,
        pinned_set_id: pinnedSetIds[0] ?? null,
      })
      setScheduled(scheduleName.trim())
      navigate('/scheduled')
    } catch (err) {
      setError(`Failed to create schedule: ${err instanceof Error ? err.message : String(err)}`)
    } finally {
      setScheduling(false)
    }
  }, [finalTask, scheduleName, effectiveCron, model, pinnedSetIds, scheduling, navigate])

  const canStart = finalTask.trim().length > 0 && !starting
  const canSchedule =
    finalTask.trim().length > 0 && scheduleName.trim().length > 0 && effectiveCron.length > 0 && !scheduling

  // ── BYOK gate ──────────────────────────────────────────────────────────────
  // Still resolving whether a key exists: hold the layout with a quiet spinner.
  if (hasKey === null) {
    return (
      <div
        style={{
          maxWidth: 'var(--page-max-narrow)',
          margin: '0 auto',
          padding: 'var(--sp-6) var(--sp-5)',
          display: 'flex',
          justifyContent: 'center',
          color: 'var(--sb-text-muted)',
        }}
      >
        <Spinner size={18} />
      </div>
    )
  }

  // No validated key yet → show the onboarding setup instead of the launcher.
  if (hasKey === false) {
    return <KeyOnboarding onConnected={() => setHasKey(true)} />
  }

  return (
    <div
      style={{
        maxWidth: 'var(--page-max-narrow)',
        margin: '0 auto',
        padding: 'var(--sp-6) var(--sp-5)',
        display: 'flex',
        flexDirection: 'column',
        gap: 'var(--sp-5)',
        boxSizing: 'border-box',
      }}
    >
      <RunsTabs />

      {/* Heading */}
      <div style={{ textAlign: 'center' }}>
        <h1
          style={{
            margin: 0,
            fontSize: 'var(--fs-2xl)',
            fontWeight: 700,
            letterSpacing: '0.2px',
            color: 'var(--sb-gold-bright)',
          }}
        >
          Compose a run
        </h1>
        <p
          style={{
            margin: 'var(--sp-2) 0 0',
            fontSize: 'var(--fs-md)',
            color: 'var(--sb-text-muted)',
            maxWidth: 460,
            marginLeft: 'auto',
            marginRight: 'auto',
          }}
        >
          Pick a template, describe the task, and ScreenBuddy will carry it out on this Mac.
        </p>
      </div>

      <Card style={{ boxShadow: 'var(--shadow-2)' }}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-5)' }}>
          {/* Template — selectable chips */}
          <Field label="Template">
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--sp-2)' }}>
              {templates.map((t) => {
                const selected = t.id === templateId
                return (
                  <button
                    key={t.id}
                    type="button"
                    onClick={() => applyTemplate(t)}
                    style={templateChipStyle(selected)}
                  >
                    {t.name}
                  </button>
                )
              })}
            </div>
            {template?.credentialTarget && (
              <div style={{ marginTop: 'var(--sp-3)' }}>
                <Chip tone="gold" title={`This run uses the credential “${template.credentialTarget}”`}>
                  🔑 uses credential:{' '}
                  <span style={{ fontFamily: 'var(--font-mono)' }}>{template.credentialTarget}</span>
                </Chip>
              </div>
            )}
          </Field>

          {/* Template variables — one labeled field per `{token}` in the
              scaffold (unioned with declared required_inputs). Only shown when
              the selected template actually has variables. */}
          {templateVars.length > 0 && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
              {templateVars.map((name) => {
                const value = inputValues[name] ?? ''
                const multiline = isMultilineInput(name)
                return (
                  <Field key={name} label={humanizeInput(name)}>
                    {multiline ? (
                      <textarea
                        className="agent-input"
                        value={value}
                        onChange={(e) => setInputValue(name, e.target.value)}
                        placeholder={`Enter ${humanizeInput(name).toLowerCase()}…`}
                        rows={3}
                        style={{ ...textareaStyle, minHeight: 72 }}
                      />
                    ) : (
                      <input
                        className="agent-input"
                        value={value}
                        onChange={(e) => setInputValue(name, e.target.value)}
                        placeholder={`Enter ${humanizeInput(name).toLowerCase()}…`}
                        style={{ ...selectStyle, cursor: 'text' }}
                      />
                    )}
                  </Field>
                )
              })}
            </div>
          )}

          {/* Task — for templates WITH variables we show a single read-only
              preview of the substituted final task (what actually launches); the
              per-variable fields above are the only editable inputs. Templates
              without variables keep the original single editable textarea. */}
          {templateVars.length > 0 ? (
            <Field label="Final task (preview)">
              <textarea
                className="agent-input"
                value={finalTask}
                readOnly
                rows={6}
                style={{ ...textareaStyle, opacity: 0.85, cursor: 'default' }}
              />
              <span style={hintStyle}>
                Filled inputs are substituted into the scaffold. This is what launches the run.
              </span>
            </Field>
          ) : (
            <Field label="Task">
              <textarea
                className="agent-input"
                value={task}
                onChange={(e) => setTask(e.target.value)}
                placeholder="e.g. Open Safari, search for the weather in Tokyo, and read it back to me…"
                rows={6}
                style={textareaStyle}
              />
            </Field>
          )}

          {/* Pinned references + Model side by side */}
          <div
            style={{
              display: 'grid',
              gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))',
              gap: 'var(--sp-4)',
            }}
          >
            <Field label="Pinned references">
              <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--sp-2)' }}>
                {sets.length === 0 && (
                  <span style={hintStyle}>No reference sets in your Pinned library yet.</span>
                )}
                {sets.map((s) => {
                  const selected = pinnedSetIds.includes(s.id)
                  return (
                    <button
                      key={s.id}
                      type="button"
                      onClick={() => toggleSet(s.id)}
                      style={templateChipStyle(selected)}
                    >
                      📌 {s.name} · {s.count} {s.count === 1 ? 'file' : 'files'}
                    </button>
                  )
                })}
              </div>
              <span style={hintStyle}>
                Reference sets come from your Pinned library. Select any number — their
                images are combined for this run.
              </span>
            </Field>

            <Field label="Model">
              <select value={model} onChange={(e) => setModel(e.target.value)} style={selectStyle}>
                <option value="claude-sonnet-5">Claude Sonnet 5</option>
                <option value="claude-opus-4-8">Claude Opus 4.8</option>
              </select>
              <span style={hintStyle}>Opus is more capable; Sonnet is faster.</span>
            </Field>
          </div>

          {/* Schedule-instead-of-run toggle */}
          <label
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--sp-2)',
              cursor: 'pointer',
              fontSize: 'var(--fs-md)',
              color: 'var(--sb-text-muted)',
            }}
          >
            <input
              type="checkbox"
              checked={scheduleMode}
              onChange={(e) => {
                setScheduleMode(e.target.checked)
                setError(null)
                setScheduled(null)
              }}
            />
            ⏰ Schedule instead of running now
          </label>

          {/* Cron inputs — revealed when scheduling */}
          {scheduleMode && (
            <div
              style={{
                display: 'flex',
                flexDirection: 'column',
                gap: 'var(--sp-4)',
                padding: 'var(--sp-4)',
                background: 'var(--sb-surface-2)',
                border: '1px solid var(--sb-border)',
                borderRadius: 'var(--r-md)',
              }}
            >
              <Field label="Schedule name">
                <input
                  className="agent-input"
                  value={scheduleName}
                  onChange={(e) => setScheduleName(e.target.value)}
                  placeholder="e.g. Morning inbox sweep"
                  style={selectStyle}
                />
              </Field>

              <Field label="Frequency">
                <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--sp-2)' }}>
                  {CRON_PRESETS.map((p) => {
                    const selected = p.id === cronPreset
                    return (
                      <button
                        key={p.id}
                        type="button"
                        onClick={() => setCronPreset(p.id)}
                        style={templateChipStyle(selected)}
                      >
                        {p.label}
                      </button>
                    )
                  })}
                </div>
              </Field>

              {cronPreset === 'custom' && (
                <Field label="Cron expression">
                  <input
                    className="agent-input"
                    value={customCron}
                    onChange={(e) => setCustomCron(e.target.value)}
                    placeholder="e.g. 0 9 * * 1-5"
                    spellCheck={false}
                    style={{ ...selectStyle, fontFamily: 'var(--font-mono)', cursor: 'text' }}
                  />
                </Field>
              )}

              {/* Live human-readable preview + timezone */}
              <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
                {effectiveCron ? (
                  <>
                    <span style={{ color: 'var(--sb-gold)' }}>{cronLabel(effectiveCron)}</span>
                    <span style={{ color: 'var(--sb-text-faint)' }}> · {BROWSER_TZ}</span>
                  </>
                ) : (
                  <span style={{ color: 'var(--sb-text-faint)' }}>Enter a cron expression to preview.</span>
                )}
              </div>
            </div>
          )}

          {error && (
            <div
              style={{
                padding: '10px var(--sp-3)',
                borderRadius: 'var(--r-sm)',
                fontSize: 'var(--fs-md)',
                color: 'var(--sb-danger-bright)',
                background: 'rgba(192, 57, 43, 0.12)',
                border: '1px solid rgba(192, 57, 43, 0.40)',
              }}
            >
              {error}
            </div>
          )}

          {scheduled && (
            <div
              style={{
                padding: '10px var(--sp-3)',
                borderRadius: 'var(--r-sm)',
                fontSize: 'var(--fs-md)',
                color: 'var(--sb-success)',
                background: 'rgba(111, 184, 122, 0.12)',
                border: '1px solid rgba(111, 184, 122, 0.30)',
              }}
            >
              Scheduled “{scheduled}”.
            </div>
          )}

          {/* Start / Create schedule */}
          <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
            {scheduleMode ? (
              <Button
                variant="primary"
                size="md"
                disabled={!canSchedule}
                onClick={createScheduleNow}
                style={{
                  minWidth: 180,
                  padding: '13px 26px',
                  fontSize: 'var(--fs-lg)',
                  opacity: canSchedule ? 1 : 0.45,
                  cursor: canSchedule ? 'pointer' : 'not-allowed',
                }}
              >
                {scheduling ? (
                  <>
                    <Spinner size={16} style={{ borderTopColor: '#0A0A0A', borderColor: 'rgba(0,0,0,0.25)' }} />
                    Scheduling…
                  </>
                ) : (
                  <>⏰ Create schedule</>
                )}
              </Button>
            ) : (
              <Button
                variant="primary"
                size="md"
                disabled={!canStart}
                onClick={start}
                style={{
                  minWidth: 180,
                  padding: '13px 26px',
                  fontSize: 'var(--fs-lg)',
                  opacity: canStart ? 1 : 0.45,
                  cursor: canStart ? 'pointer' : 'not-allowed',
                }}
              >
                {starting ? (
                  <>
                    <Spinner size={16} style={{ borderTopColor: '#0A0A0A', borderColor: 'rgba(0,0,0,0.25)' }} />
                    Starting…
                  </>
                ) : (
                  <>▶ Start run</>
                )}
              </Button>
            )}
          </div>
        </div>
      </Card>
    </div>
  )
}

// Onboarding gate shown when no Anthropic key is stored yet. Self-contained:
// validates the key directly via Rust (which calls Anthropic, never our backend),
// saves it encrypted, then flips the launcher open. The plaintext key is masked
// and never logged.
function KeyOnboarding({ onConnected }: { onConnected: () => void }) {
  const [apiKey, setApiKey] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const validateAndSave = useCallback(async () => {
    const trimmed = apiKey.trim()
    if (!trimmed || busy) return
    setBusy(true)
    setError(null)
    try {
      const { invoke } = await import('@tauri-apps/api/core')
      const res = await invoke<{ valid: boolean; error?: string }>('validate_anthropic_key', {
        key: trimmed,
      })
      if (!res.valid) {
        setError(res.error || 'That key was rejected by Anthropic.')
        return
      }
      await invoke('set_anthropic_key', { key: trimmed })
      setApiKey('')
      onConnected()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }, [apiKey, busy, onConnected])

  return (
    <div
      style={{
        maxWidth: 560,
        margin: '0 auto',
        padding: 'var(--sp-6) var(--sp-5)',
        display: 'flex',
        flexDirection: 'column',
        gap: 'var(--sp-5)',
        boxSizing: 'border-box',
      }}
    >
      <div style={{ textAlign: 'center' }}>
        <h1
          style={{
            margin: 0,
            fontSize: 'var(--fs-2xl)',
            fontWeight: 700,
            letterSpacing: '0.2px',
            color: 'var(--sb-gold-bright)',
          }}
        >
          Connect your Anthropic key
        </h1>
        <p
          style={{
            margin: 'var(--sp-2) auto 0',
            fontSize: 'var(--fs-md)',
            color: 'var(--sb-text-muted)',
            maxWidth: 440,
            lineHeight: 1.5,
          }}
        >
          ScreenBuddy runs on your own Anthropic API key — it's stored encrypted on
          this Mac and sent directly to Anthropic, never to our servers.
        </p>
      </div>

      <Card style={{ boxShadow: 'var(--shadow-2)' }}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
          <SectionTitle>Set up to start</SectionTitle>

          <Field label="Anthropic API key">
            <input
              type="password"
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') validateAndSave()
              }}
              placeholder="sk-ant-…"
              autoComplete="off"
              spellCheck={false}
              className="agent-input"
              style={{ ...selectStyle, cursor: 'text', fontFamily: 'var(--font-mono)' }}
            />
          </Field>

          {error && (
            <div
              style={{
                padding: '10px var(--sp-3)',
                borderRadius: 'var(--r-sm)',
                fontSize: 'var(--fs-md)',
                color: 'var(--sb-danger-bright)',
                background: 'rgba(192, 57, 43, 0.12)',
                border: '1px solid rgba(192, 57, 43, 0.40)',
              }}
            >
              {error}
            </div>
          )}

          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
            <Button
              variant="primary"
              size="md"
              disabled={busy || !apiKey.trim()}
              onClick={validateAndSave}
              style={{
                minWidth: 160,
                opacity: busy || !apiKey.trim() ? 0.55 : 1,
                cursor: busy || !apiKey.trim() ? 'not-allowed' : 'pointer',
              }}
            >
              {busy ? (
                <>
                  <Spinner size={14} style={{ borderTopColor: '#0A0A0A', borderColor: 'rgba(0,0,0,0.25)' }} />
                  Validating…
                </>
              ) : (
                'Validate & Save'
              )}
            </Button>
            <Link
              to="/settings"
              style={{
                fontSize: 'var(--fs-sm)',
                color: 'var(--sb-gold)',
                textDecoration: 'underline',
              }}
            >
              Manage in Settings
            </Link>
          </div>
        </div>
      </Card>
    </div>
  )
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
      <SectionTitle>{label}</SectionTitle>
      {children}
    </label>
  )
}

// A selectable template chip. Selected = gold-dim pill + gold text + gold line;
// idle = neutral surface, muted text.
function templateChipStyle(selected: boolean): React.CSSProperties {
  return {
    fontFamily: 'var(--font-sans)',
    fontSize: 'var(--fs-md)',
    fontWeight: 600,
    padding: '7px 14px',
    borderRadius: 'var(--r-pill)',
    cursor: 'pointer',
    whiteSpace: 'nowrap',
    transition: 'all 0.15s ease',
    color: selected ? 'var(--sb-gold-bright)' : 'var(--sb-text-muted)',
    background: selected ? 'var(--sb-gold-dim)' : 'var(--sb-surface-3)',
    border: `1px solid ${selected ? 'var(--sb-border-gold)' : 'var(--sb-border)'}`,
  }
}

const fieldBase: React.CSSProperties = {
  width: '100%',
  boxSizing: 'border-box',
  fontFamily: 'var(--font-sans)',
  fontSize: 'var(--fs-base)',
  color: 'var(--sb-text)',
  background: 'var(--sb-surface-3)',
  border: '1px solid var(--sb-border)',
  borderRadius: 'var(--r-md)',
}

const textareaStyle: React.CSSProperties = {
  ...fieldBase,
  resize: 'vertical',
  padding: '12px 14px',
  lineHeight: 1.55,
  minHeight: 132,
}

const selectStyle: React.CSSProperties = {
  ...fieldBase,
  padding: '10px 12px',
  cursor: 'pointer',
}

const hintStyle: React.CSSProperties = {
  fontSize: 'var(--fs-sm)',
  color: 'var(--sb-text-faint)',
}

export default NewRun

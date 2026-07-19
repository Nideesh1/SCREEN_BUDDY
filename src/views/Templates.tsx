import { useCallback, useEffect, useState } from 'react'
import {
  listTemplates,
  createTemplate,
  updateTemplate,
  deleteTemplate,
  seedTemplates,
  type Template,
  type CreateTemplateBody,
} from '../lib'
import {
  Card,
  SectionTitle,
  Button,
  EmptyState,
  Spinner,
  Divider,
  Badge,
  IconButton,
  TrashIcon,
} from '../ui'
import RunsTabs from './RunsTabs'

// The model options offered in the editor. Kept in lockstep with NewRun's model
// picker so a template's model always resolves to a launchable model there.
const MODEL_OPTIONS: { value: string; label: string }[] = [
  { value: 'claude-sonnet-5', label: 'Claude Sonnet 5' },
  { value: 'claude-opus-4-8', label: 'Claude Opus 4.8' },
]
const DEFAULT_MODEL = MODEL_OPTIONS[0].value

type Load =
  | { state: 'loading' }
  | { state: 'error'; message: string }
  | { state: 'ready'; templates: Template[] }

// The editable form state. Lists (set_names / required_inputs) are edited as
// raw comma/newline-separated text and split into arrays on save, so the user
// never wrestles with per-item add/remove controls.
interface FormState {
  name: string
  task_scaffold: string
  model: string
  suggested_set_name: string
  credential_target: string
  set_names: string
  required_inputs: string
}

const BLANK_FORM: FormState = {
  name: '',
  task_scaffold: '',
  model: DEFAULT_MODEL,
  suggested_set_name: '',
  credential_target: '',
  set_names: '',
  required_inputs: '',
}

// Split a comma/newline-separated string into a trimmed, non-empty list.
function splitList(raw: string): string[] {
  return raw
    .split(/[\n,]/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0)
}

function formFromTemplate(t: Template): FormState {
  return {
    name: t.name,
    task_scaffold: t.task_scaffold,
    model: t.model || DEFAULT_MODEL,
    suggested_set_name: t.suggested_set_name ?? '',
    credential_target: t.credential_target ?? '',
    set_names: (t.set_names ?? []).join(', '),
    required_inputs: (t.required_inputs ?? []).join(', '),
  }
}

// Build the create/patch body from the form. Same shape for both (the backend
// PATCH accepts any subset; we send the full editable set).
function bodyFromForm(f: FormState): CreateTemplateBody {
  return {
    name: f.name.trim(),
    task_scaffold: f.task_scaffold,
    model: f.model,
    suggested_set_name: f.suggested_set_name.trim(),
    credential_target: f.credential_target.trim(),
    set_names: splitList(f.set_names),
    required_inputs: splitList(f.required_inputs),
  }
}

// Templates manager: list every template with New / Edit / Delete, and an
// inline create/edit form. Mirrors the Scheduled view's load-state + optimistic
// refresh pattern. Built-ins are fully editable/deletable (the backend allows
// it); we only badge them.
function Templates() {
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const [busyId, setBusyId] = useState<string | null>(null)
  const [seeding, setSeeding] = useState(false)

  // Editor state: null = closed; 'new' = creating; otherwise the id being edited.
  const [editing, setEditing] = useState<string | 'new' | null>(null)
  const [form, setForm] = useState<FormState>(BLANK_FORM)
  const [saving, setSaving] = useState(false)
  const [formError, setFormError] = useState<string | null>(null)

  const fetchAll = useCallback(async () => {
    setLoad({ state: 'loading' })
    try {
      const templates = await listTemplates()
      setLoad({ state: 'ready', templates })
    } catch (err) {
      setLoad({ state: 'error', message: err instanceof Error ? err.message : 'Network error' })
    }
  }, [])

  useEffect(() => {
    fetchAll()
  }, [fetchAll])

  const openNew = useCallback(() => {
    setForm(BLANK_FORM)
    setFormError(null)
    setEditing('new')
  }, [])

  const openEdit = useCallback((t: Template) => {
    setForm(formFromTemplate(t))
    setFormError(null)
    setEditing(t.template_id)
  }, [])

  const closeEditor = useCallback(() => {
    setEditing(null)
    setFormError(null)
  }, [])

  const save = useCallback(async () => {
    if (!form.name.trim()) {
      setFormError('Name is required.')
      return
    }
    setSaving(true)
    setFormError(null)
    try {
      const body = bodyFromForm(form)
      if (editing === 'new') {
        const created = await createTemplate(body)
        setLoad((prev) =>
          prev.state === 'ready'
            ? { state: 'ready', templates: [created, ...prev.templates] }
            : prev,
        )
      } else if (editing) {
        const updated = await updateTemplate(editing, body)
        setLoad((prev) =>
          prev.state === 'ready'
            ? {
                state: 'ready',
                templates: prev.templates.map((x) =>
                  x.template_id === updated.template_id ? updated : x,
                ),
              }
            : prev,
        )
      }
      setEditing(null)
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Save failed')
    } finally {
      setSaving(false)
    }
  }, [form, editing])

  const remove = useCallback(
    async (t: Template) => {
      if (!window.confirm(`Delete template “${t.name}”?`)) return
      setBusyId(t.template_id)
      try {
        await deleteTemplate(t.template_id)
        setLoad((prev) =>
          prev.state === 'ready'
            ? {
                state: 'ready',
                templates: prev.templates.filter((x) => x.template_id !== t.template_id),
              }
            : prev,
        )
        if (editing === t.template_id) setEditing(null)
      } catch (err) {
        console.error('[templates] delete failed', err)
      } finally {
        setBusyId(null)
      }
    },
    [editing],
  )

  const restoreBuiltins = useCallback(async () => {
    setSeeding(true)
    try {
      await seedTemplates()
      await fetchAll()
    } catch (err) {
      console.error('[templates] seed failed', err)
    } finally {
      setSeeding(false)
    }
  }, [fetchAll])

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

      <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--sp-3)' }}>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-text)' }}>
          Templates
        </h1>
        <div style={{ marginLeft: 'auto', display: 'flex', gap: 'var(--sp-2)' }}>
          <Button
            variant="secondary"
            size="sm"
            onClick={restoreBuiltins}
            disabled={seeding || load.state === 'loading'}
          >
            {seeding ? 'Restoring…' : '↺ Restore built-ins'}
          </Button>
          <Button variant="primary" size="sm" onClick={openNew} disabled={editing === 'new'}>
            + New template
          </Button>
        </div>
      </div>

      {/* Create/edit form — rendered above the list when the editor is open. */}
      {editing !== null && (
        <TemplateForm
          isNew={editing === 'new'}
          form={form}
          onChange={setForm}
          onSave={save}
          onCancel={closeEditor}
          saving={saving}
          error={formError}
        />
      )}

      <Card title={<SectionTitle>Your templates</SectionTitle>} padded={false}>
        {load.state === 'loading' && (
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
            <Spinner /> Loading templates…
          </div>
        )}

        {load.state === 'error' && (
          <div style={{ padding: 'var(--sp-4)' }}>
            <div className="error-message">{load.message}</div>
          </div>
        )}

        {load.state === 'ready' && load.templates.length === 0 && (
          <EmptyState
            icon="🗂"
            title="No templates yet"
            hint="Create one with “New template”, or restore the built-in set."
          />
        )}

        {load.state === 'ready' && load.templates.length > 0 && (
          <div>
            {load.templates.map((t, i) => (
              <div key={t.template_id}>
                {i > 0 && <Divider style={{ margin: 0 }} />}
                <TemplateRow
                  template={t}
                  busy={busyId === t.template_id}
                  active={editing === t.template_id}
                  onEdit={() => openEdit(t)}
                  onDelete={() => remove(t)}
                />
              </div>
            ))}
          </div>
        )}
      </Card>
    </div>
  )
}

function TemplateRow({
  template,
  busy,
  active,
  onEdit,
  onDelete,
}: {
  template: Template
  busy: boolean
  active: boolean
  onEdit: () => void
  onDelete: () => void
}) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-4)',
        padding: '12px 16px',
        background: active ? 'var(--sb-gold-dim)' : 'transparent',
        transition: 'background 0.15s ease',
      }}
    >
      <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 4 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)' }}>
          <span
            style={{
              fontSize: 'var(--fs-base)',
              fontWeight: 600,
              color: 'var(--sb-text)',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}
          >
            {template.name}
          </span>
          {template.builtin && <Badge tone="gold">built-in</Badge>}
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', flexWrap: 'wrap' }}>
          <Badge tone="neutral" mono>
            {template.model || '—'}
          </Badge>
          {template.credential_target && (
            <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)' }}>
              🔑 {template.credential_target}
            </span>
          )}
          {template.task_scaffold && (
            <span
              style={{
                fontSize: 'var(--fs-sm)',
                color: 'var(--sb-text-faint)',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                whiteSpace: 'nowrap',
                maxWidth: 320,
              }}
            >
              {template.task_scaffold}
            </span>
          )}
        </div>
      </div>

      <Button variant="secondary" size="sm" onClick={onEdit} disabled={busy}>
        Edit
      </Button>
      <IconButton
        onClick={onDelete}
        disabled={busy}
        aria-label="Delete template"
        title="Delete template"
        style={{ color: 'var(--sb-danger-bright)' }}
      >
        <TrashIcon />
      </IconButton>
    </div>
  )
}

function TemplateForm({
  isNew,
  form,
  onChange,
  onSave,
  onCancel,
  saving,
  error,
}: {
  isNew: boolean
  form: FormState
  onChange: (f: FormState) => void
  onSave: () => void
  onCancel: () => void
  saving: boolean
  error: string | null
}) {
  const set = <K extends keyof FormState>(key: K, value: FormState[K]) =>
    onChange({ ...form, [key]: value })

  return (
    <Card title={<SectionTitle>{isNew ? 'New template' : 'Edit template'}</SectionTitle>}>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-4)' }}>
        <Field label="Name">
          <input
            className="agent-input"
            value={form.name}
            onChange={(e) => set('name', e.target.value)}
            placeholder="e.g. Morning inbox sweep"
            style={inputStyle}
          />
        </Field>

        <Field label="Task scaffold">
          <textarea
            className="agent-input"
            value={form.task_scaffold}
            onChange={(e) => set('task_scaffold', e.target.value)}
            placeholder="The prompt this template prefills into the launcher…"
            rows={6}
            style={textareaStyle}
          />
        </Field>

        <div
          style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))',
            gap: 'var(--sp-4)',
          }}
        >
          <Field label="Model">
            <select value={form.model} onChange={(e) => set('model', e.target.value)} style={inputStyle}>
              {MODEL_OPTIONS.map((m) => (
                <option key={m.value} value={m.value}>
                  {m.label}
                </option>
              ))}
            </select>
          </Field>

          <Field label="Suggested set name">
            <input
              className="agent-input"
              value={form.suggested_set_name}
              onChange={(e) => set('suggested_set_name', e.target.value)}
              placeholder="Pinned set to pre-select"
              style={inputStyle}
            />
          </Field>
        </div>

        <Field label="Credential target">
          <input
            className="agent-input"
            value={form.credential_target}
            onChange={(e) => set('credential_target', e.target.value)}
            placeholder="Named credential this run expects (optional)"
            style={inputStyle}
          />
        </Field>

        <Field label="Set names" hint="Comma or newline separated.">
          <input
            className="agent-input"
            value={form.set_names}
            onChange={(e) => set('set_names', e.target.value)}
            placeholder="set-a, set-b"
            style={inputStyle}
          />
        </Field>

        <Field label="Required inputs" hint="Comma or newline separated.">
          <input
            className="agent-input"
            value={form.required_inputs}
            onChange={(e) => set('required_inputs', e.target.value)}
            placeholder="email, subject"
            style={inputStyle}
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

        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 'var(--sp-2)' }}>
          <Button variant="ghost" size="md" onClick={onCancel} disabled={saving}>
            Cancel
          </Button>
          <Button
            variant="primary"
            size="md"
            onClick={onSave}
            disabled={saving || !form.name.trim()}
            style={{ opacity: saving || !form.name.trim() ? 0.55 : 1 }}
          >
            {saving ? (
              <>
                <Spinner size={14} style={{ borderTopColor: '#0A0A0A', borderColor: 'rgba(0,0,0,0.25)' }} />
                Saving…
              </>
            ) : isNew ? (
              'Create template'
            ) : (
              'Save changes'
            )}
          </Button>
        </div>
      </div>
    </Card>
  )
}

function Field({
  label,
  hint,
  children,
}: {
  label: string
  hint?: string
  children: React.ReactNode
}) {
  return (
    <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
      <SectionTitle>{label}</SectionTitle>
      {children}
      {hint && <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-faint)' }}>{hint}</span>}
    </label>
  )
}

const inputBase: React.CSSProperties = {
  width: '100%',
  boxSizing: 'border-box',
  fontFamily: 'var(--font-sans)',
  fontSize: 'var(--fs-base)',
  color: 'var(--sb-text)',
  background: 'var(--sb-surface-3)',
  border: '1px solid var(--sb-border)',
  borderRadius: 'var(--r-md)',
}

const inputStyle: React.CSSProperties = {
  ...inputBase,
  padding: '10px 12px',
}

const textareaStyle: React.CSSProperties = {
  ...inputBase,
  resize: 'vertical',
  padding: '12px 14px',
  lineHeight: 1.55,
  minHeight: 120,
}

export default Templates

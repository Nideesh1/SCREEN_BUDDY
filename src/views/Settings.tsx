import { useEffect, useState, useCallback } from 'react'
import { useOutletContext } from 'react-router-dom'
import { open } from '@tauri-apps/plugin-shell'
import type { LayoutContext } from '../Layout'
import { safeInvoke } from '../lib'
import { Card, Button, Badge, Spinner } from '../ui'

const SETTINGS_DEEP_LINK = {
  screenRecording: 'x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture',
  accessibility: 'x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility',
} as const

interface Permissions {
  accessibility: boolean
  screenRecording: boolean
}

const TELEGRAM_BOT_TOKEN_KEY = 'screen_buddy_telegram_bot_token'
const TELEGRAM_CHAT_ID_KEY = 'screen_buddy_telegram_chat_id'

type PermLoad =
  | { state: 'loading' }
  | { state: 'unavailable'; message: string }
  | { state: 'ready'; perms: Permissions }

// Settings view — read-mostly account/model/storage info plus a live macOS
// permission check via check_permissions (wrapped; degrades if not yet wired).
function Settings() {
  // Account info is provided by the router Layout via <Outlet context>.
  const { userEmail, onSignOut } = useOutletContext<LayoutContext>()
  const [perm, setPerm] = useState<PermLoad>({ state: 'loading' })

  // The signed-in user's stable Google id (the JWT `sub`). This is the
  // `user_id` openfang / the remote API must target to drive THIS Mac.
  const userId = (() => {
    try {
      const t = localStorage.getItem('screen_buddy_session_token')
      if (!t) return ''
      return JSON.parse(atob(t.split('.')[1])).sub || ''
    } catch {
      return ''
    }
  })()
  const [copied, setCopied] = useState(false)
  const copyUserId = useCallback(() => {
    if (!userId) return
    navigator.clipboard?.writeText(userId)
    setCopied(true)
    setTimeout(() => setCopied(false), 1200)
  }, [userId])

  const fetchPerms = useCallback(async () => {
    setPerm({ state: 'loading' })
    const res = await safeInvoke<Permissions>('check_permissions')
    if (res.ok) setPerm({ state: 'ready', perms: res.data })
    else setPerm({ state: 'unavailable', message: res.error })
  }, [])

  useEffect(() => {
    fetchPerms()
  }, [fetchPerms])

  return (
    <div
      style={{
        padding: 'var(--sp-5)',
        maxWidth: 'var(--page-max-narrow)',
        margin: '0 auto',
        display: 'flex',
        flexDirection: 'column',
        gap: 'var(--sp-4)',
      }}
    >
      <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-gold-bright)' }}>
        Settings
      </h1>

      <Card title="Account">
        <Row label="Signed in as" value={userEmail || '—'} mono={!!userEmail} />
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: 'var(--sp-2)', fontSize: 'var(--fs-md)', padding: '3px 0' }}>
          <span style={{ color: 'var(--sb-text-muted)' }}>User ID</span>
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', minWidth: 0 }}>
            <span style={{ color: 'var(--sb-text)', fontFamily: 'var(--font-mono)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', maxWidth: 280 }} title={userId}>
              {userId || '—'}
            </span>
            {userId && (
              <Button variant="ghost" size="sm" onClick={copyUserId}>
                {copied ? 'Copied' : 'Copy'}
              </Button>
            )}
          </div>
        </div>
        <p style={{ margin: '2px 0 0', color: 'var(--sb-text-faint)', fontSize: 'var(--fs-sm)' }}>
          Your stable account id — the target for remote runs (openfang / API).
        </p>
        <div style={{ marginTop: 'var(--sp-3)' }}>
          <Button variant="secondary" size="sm" onClick={onSignOut}>
            Sign out
          </Button>
        </div>
      </Card>

      <Card title="Storage">
        <Row label="Screenshots" value="Stored on this Mac" />
        <div style={{ marginTop: 'var(--sp-3)' }}>
          <Button variant="ghost" size="sm" disabled title="Coming soon">
            Clear all
          </Button>
        </div>
      </Card>

      <Card
        title="Permissions"
        actions={
          <Button variant="secondary" size="sm" onClick={fetchPerms}>
            ↻ Re-check
          </Button>
        }
      >
        {perm.state === 'loading' && (
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)' }}>
            <Spinner size={14} /> Checking…
          </div>
        )}

        {perm.state === 'unavailable' && (
          <p style={{ color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)' }}>
            Permission check unavailable. <span style={{ opacity: 0.8 }}>{perm.message}</span>
          </p>
        )}

        {perm.state === 'ready' && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
            <PermRow
              label="Accessibility"
              granted={perm.perms.accessibility}
              requestCommand="request_accessibility"
              deepLink={SETTINGS_DEEP_LINK.accessibility}
              onAfter={fetchPerms}
            />
            <PermRow
              label="Screen Recording"
              granted={perm.perms.screenRecording}
              requestCommand="request_screen_recording"
              deepLink={SETTINGS_DEEP_LINK.screenRecording}
              onAfter={fetchPerms}
            />
            <p style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)', marginTop: 'var(--sp-1)' }}>
              After granting, you must quit and relaunch ScreenBuddy for the change to take effect.
            </p>
            <p style={{ fontSize: 'var(--fs-xs)', color: 'var(--sb-text-faint)', marginTop: -8 }}>
              In development builds the grant can reset on rebuild; a signed release build is stable.
            </p>
          </div>
        )}
      </Card>

      <AnthropicKeySection />

      <TelegramSection />
    </div>
  )
}

// BYOK — bring-your-own Anthropic API key. The key is validated locally via the
// `validate_anthropic_key` Rust command (which calls Anthropic DIRECTLY, never our
// backend), then handed to Rust (`set_anthropic_key`) which encrypts it at rest.
// The plaintext key never round-trips back to the UI: we only ever learn whether
// one is stored (`has_anthropic_key`).
function AnthropicKeySection() {
  const [hasKey, setHasKey] = useState<boolean | null>(null)
  const [apiKey, setApiKey] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)

  const refresh = useCallback(async () => {
    const res = await safeInvoke<boolean>('has_anthropic_key')
    setHasKey(res.ok ? res.data : false)
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  const validateAndSave = useCallback(async () => {
    const trimmed = apiKey.trim()
    if (!trimmed) return
    setBusy(true)
    setError(null)
    setSaved(false)
    try {
      const { invoke } = await import('@tauri-apps/api/core')
      // Validation now happens locally in Rust, which calls Anthropic directly —
      // the key never touches our servers.
      const res = await invoke<{ valid: boolean; error?: string }>('validate_anthropic_key', {
        key: trimmed,
      })
      if (!res.valid) {
        setError(res.error || 'That key was rejected by Anthropic.')
        return
      }
      await invoke('set_anthropic_key', { key: trimmed })
      setApiKey('')
      setSaved(true)
      setTimeout(() => setSaved(false), 2500)
      await refresh()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }, [apiKey, refresh])

  const remove = useCallback(async () => {
    setBusy(true)
    setError(null)
    try {
      const { invoke } = await import('@tauri-apps/api/core')
      await invoke('clear_anthropic_key')
      await refresh()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }, [refresh])

  return (
    <Card title="Anthropic API Key (BYOK)">
      <p style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)', margin: '0 0 var(--sp-4)', lineHeight: 1.5 }}>
        ScreenBuddy runs on YOUR Anthropic key (billed to you) — a validated key is
        <strong style={{ color: 'var(--sb-text)' }}> required</strong> to start runs.
        Your key is validated and used by sending it <strong style={{ color: 'var(--sb-text)' }}>directly to Anthropic</strong>;
        it is stored encrypted on this Mac and never sent to our servers.
      </p>

      {hasKey === null && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', color: 'var(--sb-text-muted)', fontSize: 'var(--fs-md)' }}>
          <Spinner size={14} /> Checking…
        </div>
      )}

      {hasKey === true && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)' }}>
          <Badge tone="gold">Using your own key ✓</Badge>
          <div style={{ marginLeft: 'auto' }}>
            <Button variant="danger" size="sm" onClick={remove} disabled={busy}>
              {busy ? 'Removing…' : 'Remove'}
            </Button>
          </div>
        </div>
      )}

      {hasKey === false && (
        <>
          <Field label="API key">
            <input
              type="password"
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              placeholder="sk-ant-…"
              autoComplete="off"
              spellCheck={false}
              className="agent-input"
              style={{ ...inputStyle, fontFamily: 'var(--font-mono)' }}
            />
          </Field>

          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)', marginTop: 'var(--sp-1)' }}>
            <Button
              variant="primary"
              onClick={validateAndSave}
              disabled={busy || !apiKey.trim()}
            >
              {busy ? 'Validating…' : saved ? '✓ Validated & saved' : 'Validate & Save'}
            </Button>
          </div>

          {error && (
            <div className="error-message" style={{ marginTop: 'var(--sp-3)', textAlign: 'left' }}>
              {error}
            </div>
          )}
        </>
      )}
    </Card>
  )
}

function TelegramSection() {
  const [botToken, setBotToken] = useState('')
  const [chatId, setChatId] = useState('')
  const [saved, setSaved] = useState(false)

  useEffect(() => {
    try {
      setBotToken(localStorage.getItem(TELEGRAM_BOT_TOKEN_KEY) || '')
      setChatId(localStorage.getItem(TELEGRAM_CHAT_ID_KEY) || '')
    } catch {
      /* localStorage unavailable; start with empty fields */
    }
  }, [])

  const save = useCallback(() => {
    try {
      localStorage.setItem(TELEGRAM_BOT_TOKEN_KEY, botToken)
      localStorage.setItem(TELEGRAM_CHAT_ID_KEY, chatId)
      setSaved(true)
      setTimeout(() => setSaved(false), 2000)
    } catch {
      /* persisting failed; nothing else we can do locally */
    }
  }, [botToken, chatId])

  return (
    <Card title="Telegram">
      <p style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)', margin: '0 0 var(--sp-4)', lineHeight: 1.5 }}>
        Lets you start runs from a Telegram chat later. Stored locally on this Mac only.
      </p>

      <Field label="Bot token">
        <input
          type="password"
          value={botToken}
          onChange={(e) => setBotToken(e.target.value)}
          placeholder="123456:ABC-DEF…"
          autoComplete="off"
          className="agent-input"
          style={{ ...inputStyle, fontFamily: 'var(--font-mono)' }}
        />
      </Field>

      <Field label="Chat ID">
        <input
          type="text"
          value={chatId}
          onChange={(e) => setChatId(e.target.value)}
          placeholder="e.g. 987654321"
          autoComplete="off"
          className="agent-input"
          style={{ ...inputStyle, fontFamily: 'var(--font-mono)' }}
        />
      </Field>

      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)', marginTop: 'var(--sp-4)' }}>
        <Button variant="primary" size="sm" onClick={save}>
          {saved ? 'Saved ✓' : 'Save'}
        </Button>
        <Button variant="ghost" size="sm" disabled title="Coming soon">
          Connect (coming soon)
        </Button>
      </div>

      <p style={{ fontSize: 'var(--fs-xs)', color: 'var(--sb-text-faint)', marginTop: 'var(--sp-3)' }}>
        Telegram triggering is coming soon — this saves your config locally for now.
      </p>
    </Card>
  )
}

const inputStyle: React.CSSProperties = {
  width: '100%',
  boxSizing: 'border-box',
  background: 'var(--sb-surface-3)',
  border: '1px solid var(--sb-border)',
  borderRadius: 'var(--r-sm)',
  padding: '9px 11px',
  fontSize: 'var(--fs-md)',
  color: 'var(--sb-text)',
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ display: 'block', marginBottom: 'var(--sp-3)' }}>
      <span
        style={{
          display: 'block',
          fontSize: 'var(--fs-xs)',
          textTransform: 'uppercase',
          letterSpacing: '0.8px',
          color: 'var(--sb-text-faint)',
          marginBottom: 'var(--sp-1)',
        }}
      >
        {label}
      </span>
      {children}
    </label>
  )
}

function Row({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', fontSize: 'var(--fs-md)', padding: '3px 0' }}>
      <span style={{ color: 'var(--sb-text-muted)' }}>{label}</span>
      <span style={{ color: 'var(--sb-text)', fontFamily: mono ? 'var(--font-mono)' : undefined }}>{value}</span>
    </div>
  )
}

function PermRow({
  label,
  granted,
  requestCommand,
  deepLink,
  onAfter,
}: {
  label: string
  granted: boolean
  requestCommand: string
  deepLink: string
  onAfter: () => void
}) {
  const [busy, setBusy] = useState(false)

  const grant = useCallback(async () => {
    setBusy(true)
    await safeInvoke<boolean>(requestCommand)
    setBusy(false)
    onAfter()
  }, [requestCommand, onAfter])

  const openSettings = useCallback(async () => {
    try {
      await open(deepLink)
    } catch {
      /* deep-link unavailable; user can open System Settings manually */
    }
  }, [deepLink])

  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--sp-3)',
        fontSize: 'var(--fs-md)',
        background: 'var(--sb-surface-2)',
        border: '1px solid var(--sb-border)',
        borderRadius: 'var(--r-sm)',
        padding: '10px 12px',
      }}
    >
      <span
        aria-hidden
        style={{
          display: 'inline-flex',
          alignItems: 'center',
          justifyContent: 'center',
          width: 22,
          height: 22,
          borderRadius: 'var(--r-pill)',
          fontSize: 12,
          fontWeight: 700,
          color: granted ? 'var(--sb-success)' : 'var(--sb-danger-bright)',
          background: granted ? 'rgba(111, 184, 122, 0.12)' : 'rgba(192, 57, 43, 0.12)',
          border: `1px solid ${granted ? 'rgba(111, 184, 122, 0.30)' : 'rgba(192, 57, 43, 0.40)'}`,
        }}
      >
        {granted ? '✓' : '✗'}
      </span>
      <span style={{ color: 'var(--sb-text)', fontWeight: 500 }}>{label}</span>
      {!granted && (
        <div style={{ marginLeft: 'auto' }}>
          <Button variant="secondary" size="sm" onClick={grant} disabled={busy}>
            {busy ? 'Requesting…' : 'Grant'}
          </Button>
        </div>
      )}
      <button
        onClick={openSettings}
        style={{
          marginLeft: granted ? 'auto' : 0,
          background: 'none',
          border: 'none',
          padding: 0,
          fontSize: 'var(--fs-sm)',
          color: 'var(--sb-gold)',
          textDecoration: 'underline',
          cursor: 'pointer',
        }}
      >
        Open Settings
      </button>
    </div>
  )
}

export default Settings

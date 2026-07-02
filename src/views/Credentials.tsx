import { useEffect, useState, useCallback } from 'react'
import { safeInvoke } from '../lib'
import { Card, Button, IconButton, EmptyState, Spinner } from '../ui'

interface Credential {
  target: string
  username: string
}

type Load =
  | { state: 'loading' }
  | { state: 'unavailable'; message: string }
  | { state: 'ready'; creds: Credential[] }

// Credentials manager — full CRUD over the Rust keychain commands. Passwords are
// never returned by cred_list (shown as ••••); they're write-only here. The model
// never sees these — ScreenBuddy types them in during a run. All invokes are
// wrapped so missing commands degrade to an "unavailable" panel.
function Credentials() {
  const [load, setLoad] = useState<Load>({ state: 'loading' })
  const [target, setTarget] = useState('')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [busy, setBusy] = useState(false)
  const [formError, setFormError] = useState<string | null>(null)

  const fetchCreds = useCallback(async () => {
    setLoad({ state: 'loading' })
    const res = await safeInvoke<Credential[]>('cred_list')
    if (res.ok) setLoad({ state: 'ready', creds: res.data ?? [] })
    else setLoad({ state: 'unavailable', message: res.error })
  }, [])

  useEffect(() => {
    fetchCreds()
  }, [fetchCreds])

  const add = useCallback(async () => {
    if (!target.trim() || !username.trim() || !password) return
    setBusy(true)
    setFormError(null)
    const res = await safeInvoke('cred_add', {
      target: target.trim(),
      username: username.trim(),
      password,
    })
    setBusy(false)
    if (!res.ok) {
      setFormError(res.error)
      return
    }
    setTarget('')
    setUsername('')
    setPassword('')
    fetchCreds()
  }, [target, username, password, fetchCreds])

  const remove = useCallback(
    async (target: string) => {
      const res = await safeInvoke('cred_delete', { target })
      if (!res.ok) setFormError(res.error)
      fetchCreds()
    },
    [fetchCreds],
  )

  return (
    <div style={{ padding: 'var(--sp-5)', maxWidth: 'var(--page-max-narrow)', margin: '0 auto' }}>
      <h1 style={{ margin: '0 0 6px', fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-gold-bright)' }}>
        Credentials
      </h1>
      <p style={{ margin: '0 0 var(--sp-4)', fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)' }}>
        🔒 The model never sees these — ScreenBuddy types them in for you.
      </p>

      {/* Add form */}
      <Card title="Add credential" style={{ marginBottom: 'var(--sp-5)' }}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-3)' }}>
          <div style={{ display: 'flex', gap: 'var(--sp-3)', flexWrap: 'wrap' }}>
            <input
              className="agent-input"
              placeholder="target / app (e.g. mail.google.com, Slack app)"
              value={target}
              onChange={(e) => setTarget(e.target.value)}
              style={{ ...inputStyle, flex: '1 1 180px' }}
            />
            <input
              className="agent-input"
              placeholder="username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              style={{ ...inputStyle, flex: '1 1 160px' }}
            />
            <input
              className="agent-input"
              type="password"
              placeholder="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              style={{ ...inputStyle, flex: '1 1 160px' }}
            />
          </div>
          <div>
            <Button
              variant="primary"
              disabled={busy || !target.trim() || !username.trim() || !password}
              onClick={add}
            >
              ＋ Add
            </Button>
          </div>
          {formError && <div className="error-message">{formError}</div>}
        </div>
      </Card>

      {/* List */}
      {load.state === 'loading' && (
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-2)', color: 'var(--sb-text-muted)', fontSize: 'var(--fs-base)' }}>
          <Spinner size={16} /> Loading credentials…
        </div>
      )}

      {load.state === 'unavailable' && (
        <Card>
          <EmptyState
            icon="⚠"
            title="Credential store unavailable"
            hint={load.message}
            action={
              <Button variant="secondary" onClick={fetchCreds}>
                Retry
              </Button>
            }
          />
        </Card>
      )}

      {load.state === 'ready' && load.creds.length === 0 && (
        <Card>
          <EmptyState
            icon="🔑"
            title="No saved credentials yet"
            hint="Add a target, username and password above; ScreenBuddy will type them in during runs."
          />
        </Card>
      )}

      {load.state === 'ready' && load.creds.length > 0 && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--sp-2)' }}>
          {load.creds.map((cred) => (
            <Card key={cred.target} padded={false}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--sp-3)', padding: 'var(--sp-3) var(--sp-4)' }}>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div
                    style={{
                      fontSize: 'var(--fs-base)',
                      fontWeight: 600,
                      color: 'var(--sb-text)',
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                    }}
                  >
                    {cred.target}
                  </div>
                  <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--sb-text-muted)', marginTop: 2 }}>
                    <span style={{ fontFamily: 'var(--font-mono)' }}>{cred.username}</span>
                    {' · '}
                    <span style={{ fontFamily: 'var(--font-mono)', letterSpacing: 2 }}>••••••</span>
                  </div>
                </div>
                <IconButton
                  title="Delete credential"
                  aria-label="Delete credential"
                  onClick={() => remove(cred.target)}
                  style={{ color: 'var(--sb-danger-bright)' }}
                >
                  🗑
                </IconButton>
              </div>
            </Card>
          ))}
        </div>
      )}
    </div>
  )
}

const inputStyle: React.CSSProperties = {
  boxSizing: 'border-box',
  padding: '9px 11px',
  fontFamily: 'inherit',
  fontSize: 'var(--fs-base)',
  background: 'var(--sb-surface-3)',
  color: 'var(--sb-text)',
  border: '1px solid var(--sb-border)',
  borderRadius: 'var(--r-sm)',
}

export default Credentials

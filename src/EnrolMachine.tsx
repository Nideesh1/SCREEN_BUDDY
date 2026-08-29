import { useCallback, useState } from 'react'
import { CU_BACKEND, enrollMachine, type EnrollResult } from './lib'

// The second door off the splash: this machine joins someone's fleet with a
// one-time key instead of signing in to an account. Deliberately NOT shaped like
// a login — there is no identity being claimed here, no password, no account to
// recover. One field, one button, and failure text that says which of the two
// things went wrong.
//
// A single centred column at any width. It is the one screen an operator reads
// while standing over an unfamiliar machine, so nothing here competes for room.
function EnrolMachine({ onEnrolled, onCancel }: { onEnrolled: () => void; onCancel: () => void }) {
  const [key, setKey] = useState('')
  const [busy, setBusy] = useState(false)
  const [failure, setFailure] = useState<Extract<EnrollResult, { ok: false }> | null>(null)

  const submit = useCallback(async () => {
    if (busy || !key.trim()) return
    setBusy(true)
    setFailure(null)
    const result = await enrollMachine(key)
    if (result.ok) {
      // Don't clear busy: the credential class is about to change under us and
      // this whole screen unmounts. Re-enabling the button first would flash a
      // live form for the moment in between.
      onEnrolled()
      return
    }
    setBusy(false)
    setFailure(result)
  }, [busy, key, onEnrolled])

  return (
    <div
      style={{
        height: '100vh',
        width: '100vw',
        boxSizing: 'border-box',
        overflowY: 'auto',
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 'var(--sp-5)',
        padding: 'var(--sp-6)',
        background:
          'radial-gradient(ellipse at top, rgba(212,175,55,0.08), transparent 60%), var(--sb-bg)',
        color: 'var(--sb-text)',
      }}
    >
      <div style={{ textAlign: 'center', maxWidth: 400 }}>
        <h1
          style={{
            margin: 0,
            fontSize: 'var(--fs-2xl)',
            fontWeight: 700,
            color: 'var(--sb-gold-bright)',
            letterSpacing: 0.5,
          }}
        >
          Enrol this machine
        </h1>
        <p
          style={{
            margin: '10px 0 0',
            fontSize: 'var(--fs-base)',
            lineHeight: 1.5,
            color: 'var(--sb-text-muted)',
          }}
        >
          Paste the enrollment key from the fleet's Devices page. This machine
          joins as a worker — it runs agents for the fleet and never signs in to
          the account itself.
        </p>
      </div>

      <form
        onSubmit={(e) => {
          e.preventDefault()
          submit()
        }}
        style={{
          width: '100%',
          maxWidth: 400,
          display: 'flex',
          flexDirection: 'column',
          gap: 'var(--sp-3)',
        }}
      >
        <input
          className="agent-input"
          value={key}
          onChange={(e) => setKey(e.target.value)}
          placeholder="Enrollment key"
          // The key is carried between two screens and pasted, never typed from
          // memory — every autocorrect the platform offers can only corrupt it.
          autoFocus
          autoComplete="off"
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          disabled={busy}
          style={{ fontFamily: 'var(--font-mono)', width: '100%' }}
        />

        <button
          type="submit"
          className="btn btn-primary"
          disabled={busy || !key.trim()}
          style={{ justifyContent: 'center' }}
        >
          {busy ? 'Enrolling…' : 'Enrol'}
        </button>
      </form>

      {failure && (
        <div
          role="alert"
          style={{
            maxWidth: 400,
            fontSize: 'var(--fs-md)',
            lineHeight: 1.5,
            textAlign: 'center',
            color: 'var(--sb-danger-bright)',
          }}
        >
          {failure.reason === 'unreachable' ? (
            <>
              Couldn't reach the ScreenBuddy backend at {CU_BACKEND}. The key is
              probably fine — check this machine's connection and try again.
            </>
          ) : failure.reason === 'internal' ? (
            // Neither the key nor the network: this machine couldn't complete
            // its own half. Nothing the operator can act on, so show what went
            // wrong verbatim rather than inventing advice.
            <>Enrollment failed on this machine: {failure.message}</>
          ) : (
            <>
              That key wasn't accepted. A key works once and expires 15 minutes
              after it is made, so ask for a fresh one.
            </>
          )}
        </div>
      )}

      <button type="button" className="btn btn-ghost" onClick={onCancel} disabled={busy}>
        ← Back
      </button>
    </div>
  )
}

export default EnrolMachine

import { useCallback, useState } from 'react'
import { unenrollMachine } from './lib'
import EnrolMachine from './EnrolMachine'

interface DeviceRevokedProps {
  /** This machine's credential changed under App — it either redeemed a fresh
   *  key here or dropped the dead one and stopped short of a new one. Both end
   *  the same way: re-read the credential and render whatever it now implies. */
  onCredentialChanged: () => void
  /** Leave the notice without touching the token, for someone who wants to look
   *  at the machine before doing anything about it. */
  onDismiss: () => void
}

// What a worker sees the moment the backend refuses its device token. The whole
// point of this screen is to name the cause: the machine is working, the network
// is working, and someone deliberately turned this machine's pass off. Left to
// the generic failure paths a revoked worker just watches every call fail and
// reads it as an outage.
//
// Recovery is one button. Dropping the dead token first is what makes the enrol
// screen the right screen — until then the machine still claims to be enrolled —
// and it is deliberately not automatic on the event itself, because a machine
// that erases its own credential on a 401 has no way to tell a revocation from a
// backend having a bad minute.
function DeviceRevoked({ onCredentialChanged, onDismiss }: DeviceRevokedProps) {
  const [enrolling, setEnrolling] = useState(false)
  const [clearing, setClearing] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const startOver = useCallback(async () => {
    setClearing(true)
    setError(null)
    if (await unenrollMachine()) {
      setEnrolling(true)
      return
    }
    setClearing(false)
    setError(
      "Couldn't clear this machine's old pass, so a new key wouldn't take. Restart ScreenBuddy and try again.",
    )
  }, [])

  // Past the point of no return: the old token is gone either way, so backing
  // out of the key field lands on the splash rather than back here.
  if (enrolling) {
    return <EnrolMachine onEnrolled={onCredentialChanged} onCancel={onCredentialChanged} />
  }

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
      <div style={{ textAlign: 'center', maxWidth: 440 }}>
        <div aria-hidden style={{ fontSize: 34, color: 'var(--sb-text-muted)' }}>
          ⃠
        </div>
        <h1
          style={{
            margin: '12px 0 0',
            fontSize: 'var(--fs-2xl)',
            fontWeight: 700,
            color: 'var(--sb-text)',
            letterSpacing: 0.5,
          }}
        >
          This machine's access was revoked
        </h1>
        <p
          style={{
            margin: '10px 0 0',
            fontSize: 'var(--fs-base)',
            lineHeight: 1.5,
            color: 'var(--sb-text-muted)',
          }}
        >
          The fleet's operator turned off this machine's pass, so it can no longer
          run agents or report anything back. This is not a connection problem and
          reinstalling will not change it.
        </p>
        <p
          style={{
            margin: '10px 0 0',
            fontSize: 'var(--fs-base)',
            lineHeight: 1.5,
            color: 'var(--sb-text-muted)',
          }}
        >
          To rejoin, ask the operator for a new enrollment key — “Add machine” on
          the fleet's Devices page — and paste it here.
        </p>
      </div>

      <button
        type="button"
        className="btn btn-primary"
        onClick={startOver}
        disabled={clearing}
        style={{ minWidth: 240, justifyContent: 'center' }}
      >
        {clearing ? 'Clearing…' : 'Enter a new key'}
      </button>

      {error && (
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
          {error}
        </div>
      )}

      <div style={{ textAlign: 'center' }}>
        <button type="button" className="btn btn-ghost" onClick={onDismiss} disabled={clearing}>
          Not now
        </button>
        <p
          style={{
            margin: '6px 0 0',
            fontSize: 'var(--fs-sm)',
            color: 'var(--sb-text-faint)',
            maxWidth: 320,
          }}
        >
          The app keeps running, but nothing it does will reach the fleet.
        </p>
      </div>
    </div>
  )
}

export default DeviceRevoked

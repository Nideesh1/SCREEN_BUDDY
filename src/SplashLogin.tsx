interface SplashLoginProps {
  login: () => void
  isLoading: boolean
  error: string | null
}

// Branded, centered gold/black login screen. The single gate before the
// authenticated "inside" of the app (see App.tsx).
function SplashLogin({ login, isLoading, error }: SplashLoginProps) {
  return (
    <div
      style={{
        height: '100vh',
        width: '100vw',
        boxSizing: 'border-box',
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 24,
        padding: 32,
        background:
          'radial-gradient(ellipse at top, rgba(212,175,55,0.08), transparent 60%), var(--sb-bg)',
        color: 'var(--sb-text)',
      }}
    >
      {/* Gold "SB" monogram */}
      <div
        style={{
          width: 88,
          height: 88,
          borderRadius: 20,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          fontSize: 34,
          fontWeight: 800,
          letterSpacing: 1,
          color: 'var(--sb-bg)',
          background:
            'linear-gradient(145deg, var(--sb-gold-bright) 0%, var(--sb-gold) 45%, var(--sb-gold-deep) 100%)',
          border: '1px solid rgba(242,210,122,0.6)',
          boxShadow: '0 8px 30px rgba(212,175,55,0.25)',
        }}
      >
        SB
      </div>

      <div style={{ textAlign: 'center' }}>
        <h1
          style={{
            margin: 0,
            fontSize: 30,
            fontWeight: 700,
            color: 'var(--sb-gold-bright)',
            letterSpacing: 0.5,
          }}
        >
          ScreenBuddy
        </h1>
        <p
          style={{
            margin: '8px 0 0',
            fontSize: 14,
            color: 'var(--sb-text-muted)',
            maxWidth: 320,
          }}
        >
          Your AI agent for the Mac. Sign in to get started.
        </p>
      </div>

      <button
        className="btn btn-primary"
        onClick={login}
        disabled={isLoading}
        style={{
          width: 'auto',
          minWidth: 240,
          maxWidth: 280,
          justifyContent: 'center',
        }}
      >
        {isLoading ? 'Signing in…' : 'Sign in with Google'}
      </button>

      {error && (
        <div
          style={{
            color: 'var(--sb-danger-bright)',
            fontSize: 13,
            maxWidth: 320,
            textAlign: 'center',
          }}
        >
          {error}
        </div>
      )}
    </div>
  )
}

export default SplashLogin

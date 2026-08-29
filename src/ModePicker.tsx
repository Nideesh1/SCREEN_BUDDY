import { MODES, useMode, type AppMode } from './mode'

// The one screen between sign-in and the shell: which of the three surfaces is
// this person here for. Asked once — the choice is remembered — and reversible
// from Settings, because one person is routinely all three roles on the same
// machine and being stuck in the wrong shell with no way out is the failure
// mode this screen is most likely to cause.
function ModePicker() {
  const { setMode } = useMode()
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
        padding: 'var(--sp-5)',
        background:
          'radial-gradient(ellipse at top, rgba(212,175,55,0.08), transparent 60%), var(--sb-bg)',
        color: 'var(--sb-text)',
      }}
    >
      <div style={{ textAlign: 'center', maxWidth: 420 }}>
        <h1 style={{ margin: 0, fontSize: 'var(--fs-2xl)', fontWeight: 700, color: 'var(--sb-gold-bright)' }}>
          How are you using ScreenBuddy?
        </h1>
        <p style={{ margin: '8px 0 0', fontSize: 'var(--fs-base)', color: 'var(--sb-text-muted)' }}>
          This only changes what you see. You can switch any time from Settings.
        </p>
      </div>

      <ModeCards onPick={setMode} />
    </div>
  )
}

// The three cards, shared between this screen and the Settings switcher so the
// wording of the choice never drifts between where it's made and where it's
// changed. Single column on a phone, side by side once there's room.
export function ModeCards({
  onPick,
  current,
}: {
  onPick: (mode: AppMode) => void
  current?: AppMode | null
}) {
  return (
    <div
      style={{
        display: 'flex',
        flexWrap: 'wrap',
        justifyContent: 'center',
        gap: 'var(--sp-4)',
        width: '100%',
        maxWidth: 900,
      }}
    >
      {MODES.map((m) => {
        const selected = current === m.id
        return (
          <button
            key={m.id}
            type="button"
            className="tap-card"
            onClick={() => onPick(m.id)}
            aria-current={selected ? 'true' : undefined}
            style={{
              // Wraps to one full-width card per row below ~700px, which is the
              // whole layout: three big tap targets, nothing else on screen.
              flex: '1 1 240px',
              minWidth: 0,
              minHeight: 132,
              borderColor: selected ? 'var(--sb-border-gold)' : undefined,
              background: selected ? 'var(--sb-gold-dim)' : undefined,
            }}
          >
            <div style={{ fontSize: 26, color: 'var(--sb-gold)', lineHeight: 1 }} aria-hidden>
              {m.icon}
            </div>
            <div
              style={{
                marginTop: 'var(--sp-3)',
                fontSize: 'var(--fs-lg)',
                fontWeight: 700,
                color: 'var(--sb-text)',
              }}
            >
              {m.label}
              {selected && (
                <span style={{ marginLeft: 8, fontSize: 'var(--fs-sm)', color: 'var(--sb-gold)' }}>
                  current
                </span>
              )}
            </div>
            <div
              style={{
                marginTop: 4,
                fontSize: 'var(--fs-md)',
                lineHeight: 1.5,
                color: 'var(--sb-text-muted)',
              }}
            >
              {m.blurb}
            </div>
          </button>
        )
      })}
    </div>
  )
}

export default ModePicker

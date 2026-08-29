import { Card } from '../ui'

// Placeholder home for admin mode.
//
// Admin is the shell for supervising agents running on other machines —
// approving or rejecting what they claim they finished, and steering runs in
// flight. That protocol is not designed yet, so this view deliberately says so
// rather than shipping a queue with nothing behind it: a screen that renders an
// empty inbox is indistinguishable from one whose backend is down, and the mode
// is more honest advertising what it will hold than pretending to hold it.
//
// The mode itself is real and worth having now — it is what keeps the admin,
// worker and personal layouts from compromising each other as each grows.
function Admin() {
  return (
    <div className="admin-page">
      <Card title="Approvals">
        <p style={{ fontSize: 'var(--fs-md)', color: 'var(--sb-text-muted)', lineHeight: 1.6 }}>
          This is where agents will wait on you — a queue of work an agent
          believes it finished, for you to approve, reject, or redirect.
        </p>
        <p
          style={{
            marginTop: 'var(--sp-3)',
            fontSize: 'var(--fs-md)',
            color: 'var(--sb-text-muted)',
            lineHeight: 1.6,
          }}
        >
          Nothing to show yet: the approval protocol is still being designed.
        </p>
      </Card>
    </div>
  )
}

export default Admin

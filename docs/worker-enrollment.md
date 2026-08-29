# Worker enrollment — spec

Status: **proposed, not built.** Written to be implemented by several agents
working in parallel; the contracts here are the coordination points and must not
drift.

---

## The problem

Every machine in the fleet currently signs in with Google and receives the same
session token. That token is the only credential the system has, so a worker
laptop and the operator's laptop are indistinguishable to the backend — both can
list the fleet, edit devices, dispatch runs, and (once it exists) approve an
agent's claim that it finished.

That is not a UI problem, and hiding the admin nav on workers would not fix it.
The real exposure: **a worker runs an untrusted computer-use agent with full
control of the desktop.** That agent can read anything the machine holds,
including the session token on disk. Today, stealing it yields the whole account.

The goal is that a compromised worker can steal only a credential that lets it do
the worker's own job: heartbeat, receive commands, report its own runs. Nothing
it can take should let it approve its own work or reach another machine.

## The shape of the fix

**Admin is how you authenticated, not which machine you are on.**

- Sign in with Google → a session token → admin. Works on any machine, including
  a browser.
- A worker never signs in with Google. At setup it redeems a one-time enrollment
  key for a device token, and that is the only credential it ever holds.

No designated admin machine, no "first device to enroll wins" race, no env var to
update when the operator changes laptops. A worker cannot escalate because it
never possesses a Google session — not because the client declines to try.

Being open source costs nothing here. Enforcement is a scope claim the server
checks; there is no secret in the client to find.

## Precedent in this codebase

`auth.py` already does exactly this once. `mint_streamer_token` issues a
long-lived token carrying `scope: STREAMER_SCOPE`, and `get_current_user`
**rejects** it:

```python
if payload.get("scope") == STREAMER_SCOPE:
    raise HTTPException(status_code=403, detail="Streamer token cannot access this endpoint")
```

The device scope is the same mechanism with a second claim (`device_id`) and a
wider set of routes it may reach. Follow that pattern rather than inventing a
parallel auth system.

---

## Tokens

Three kinds, all HS256 signed with `settings.session_secret`, all issued by
`auth.py`.

| | claims | lifetime | who holds it |
|---|---|---|---|
| **session** (today) | `sub`, `email` | short, refreshable | an operator who signed in with Google |
| **streamer** (today) | `sub`, `scope=streamer` | 365d | the fitness frame streamer |
| **device** (new) | `sub`, `scope=device`, `device_id` | long, revocable | one enrolled worker machine |

`sub` on a device token is the **owner's** user id, taken from the enrollment key.
That is how a worker lands in the right fleet without ever touching the owner's
Google account.

### Dependencies

- `get_current_user` — unchanged behaviour, but must now **also reject
  `scope=device`**. This is the single most important line in the change: every
  existing route depends on it, so rejecting there defaults the entire API to
  admin-only and each worker-reachable route becomes an explicit opt-in.
- `get_device` (new) — accepts **only** `scope=device`; returns
  `{user_id, device_id}`. Rejects session tokens, so a route meant for a worker
  cannot be silently driven by an admin token and appear to work in testing.
- `get_caller` (new) — accepts either, returns a tagged union
  (`kind: "user" | "device"`). Only for the few routes both legitimately use.

### Revocation

A device token must be revocable without rotating `session_secret` (which would
sign every operator out). Carry a `jti` and check it against the `Device` row:
`DELETE /devices/{id}` sets `revoked_at`, and `get_device` refuses a token whose
device is revoked or soft-deleted. One extra read per authenticated worker
request; acceptable, and it is what makes "forget this machine" mean anything.

Note this changes the existing soft-delete semantics: a forgotten device
currently returns on its next registration. Under enrollment it must **not** —
its token is dead and it has to be re-enrolled. Say so in the UI.

---

## Enrollment

Modelled on Tailscale auth keys. The operator mints a key, carries it to the new
machine once, and the machine trades it for a durable credential.

### `POST /enroll/keys` — admin only (`get_current_user`)

Mints a one-time key. Returns `{key, expires_at}`.

- The key is a high-entropy random string, shown **once**. Store only a hash
  (the codebase has `encrypt_decrypt.py` — check what it offers before adding a
  hashing approach).
- Short TTL. **15 minutes** — it is copied from one screen to another, not saved.
- Single use. Redemption must be atomic (`find_one_and_update` on
  `used_at: None`), so two machines racing the same key cannot both enrol.
- Rate-limit minting per user. A key is a bearer credential for joining a fleet.

### `POST /enroll` — no auth (the key IS the auth)

Body: `{key, device_id, hostname, os, os_version, app_version}`.

1. Look up the key by hash; reject if unknown, expired, or used.
2. Atomically mark it used, capturing the redeeming `device_id`.
3. Upsert the `Device` row for `(user_id_from_key, device_id)` — the same upsert
   `POST /devices` already performs, so **factor that logic out rather than
   writing it twice**. The human-owned fields (`name`, `rustdesk_id`, `notes`)
   keep their `$setOnInsert`-only treatment.
4. Mint and return a device token.

Failures must be indistinguishable to the caller: unknown, expired, and already
used all return the same 401. Do not tell a guesser which of the three it hit.

### `GET /devices/{id}` etc.

Unchanged, admin-only. The enrollment key never appears in a device row.

---

## Route audit

Every existing route currently accepts a session token and nothing else. After
`get_current_user` rejects device scope, the default is admin-only and the
worker-reachable set must be opened explicitly.

**Worker-reachable (use `get_device`), and scoped to the caller's own device:**

| route | why |
|---|---|
| `POST /devices` | re-registration on launch — but see below |
| `WS /agent/listen` | the command channel and heartbeat |
| `POST /runs` | a worker starting a dispatched run |
| `PATCH /runs/{id}` | reporting status/result of **its own** run |
| `POST /runs/{id}/events` | its own telemetry |
| `GET /templates`, `GET /sets`, `GET /artifacts` | reference data a run needs |

**Admin-only (unchanged, `get_current_user`):** everything else — `GET /devices`,
`PATCH /devices/{id}`, `DELETE /devices/{id}`, all of `/schedules`,
`/settings`, `/sessions`, template and artifact mutation, `/fitness/*`, and every
future checkpoint route.

**`POST /devices` deserves a decision.** Once enrollment exists, a worker's launch
registration could ride `/enroll` semantics instead — the device token already
identifies the machine, so re-registration is really "update my own row." Prefer
making it `get_device`-authed and self-scoped (a device may only update the row
matching its own `device_id` claim) over leaving it open. Reject any attempt to
register a `device_id` other than the caller's.

**Ownership is not enough.** A worker token's `sub` is the owner, so `user_id`
scoping alone would let worker A patch worker B's run. Every worker-reachable
route must additionally check the **`device_id`** claim. State this in each
route's docstring; it is the easiest thing here to get wrong.

**`/agent/dispatch`** stays on `verify_api_key` (service key). Unrelated trust
boundary, do not fold it in.

---

## Desktop (Rust)

`credentials.rs` stores small secrets already; follow it rather than inventing a
second store.

- Persist a device token alongside the session token. They are alternatives, not
  companions: a machine holds one or the other, never both.
- A new `enroll(key)` command: gathers the same facts `device::info()` reports,
  POSTs `/enroll`, persists the returned token on success.
- Every backend call picks whichever credential is present. There is one bearer
  helper today — route all calls through it rather than teaching each site about
  both.
- `remote.rs` opens `/agent/listen` with whichever token it holds.
- A command reporting **which credential class** this machine holds, so the UI can
  render the right shell without guessing.

**Do not let a worker fall back to Google sign-in on token rejection.** If the
device token is refused, the machine is un-enrolled and must say so. Falling back
would quietly recreate the very situation this removes.

---

## Desktop (React)

The auth gate currently asks "authenticated?". It must ask "authenticated *as
what*?".

```
        ┌─ Sign in with Google ──→ mode picker (Admin / Personal)
splash ─┤
        └─ Enrol this machine ──→ paste key ──→ Worker shell, always
```

- `SplashLogin` grows a second, visibly secondary action: **Enrol this machine**
  → a screen with one field and clear failure text.
- `mode.tsx` — mode stops being a free choice for workers. A device credential
  forces `worker` and the picker never appears; a session credential offers
  Admin / Personal as it does now. Keep the Settings switcher for sessions only.
- The Devices page grows **Add machine**: calls `POST /enroll/keys`, shows the
  key once with a copy button, states the expiry in words, and warns it will not
  be shown again. Copy-to-clipboard is the primary affordance — the operator is
  carrying this to another machine.
- Nothing about this is a security boundary. The server rejects a worker token on
  admin routes regardless of what the UI renders. Do not add client-side guards
  that read like enforcement.

---

## Build order

Sequential where the contract must exist first, parallel where it need not.

1. **`auth.py`** — device scope, `get_device`, `get_caller`, `jti`/revocation, and
   `get_current_user` rejecting device tokens. Nothing else can start until this
   lands, and it is small.
2. **Enrollment routes + key model** — `POST /enroll/keys`, `POST /enroll`, atomic
   single-use redemption.
3. **Route audit** — in parallel with 2, since it only depends on 1. Mechanical
   but wide; the `device_id` check is the part that needs care.
4. **Rust credential handling + `enroll` command** — parallel with 2 and 3 once
   the token shape is fixed.
5. **React splash / mode / Add machine** — parallel, against the frozen contract.
6. **End-to-end**: enrol the Windows laptop for real, confirm it appears, confirm
   `GET /devices` from that machine returns 403.

## What this does not solve

- **A worker can still exhaust its own run budget or report false telemetry about
  its own runs.** In scope for the checkpoint protocol, not for this.
- **Key interception.** Anyone holding the key for its 15-minute life can enrol a
  machine into the fleet. Acceptable: it is short-lived, single-use, and grants
  only worker scope.
- **Multiple humans supervising one fleet.** Still one account, one operator.
  Devices are scoped to a `user_id`; a team would need an `org_id` and membership.
- **Existing installs.** Machines already holding session tokens keep working as
  admins until re-enrolled. Decide whether that is a migration or simply the
  answer for the operator's own machine.

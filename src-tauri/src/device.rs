//! Stable device identity + self-registration with the backend.
//!
//! A ScreenBuddy install runs on a fleet of laptops and VMs, and the fleet view
//! needs to answer "is this the same machine I saw yesterday?". Hostnames get
//! renamed, VMs get re-imaged onto new IPs, OS versions move — none of them are
//! an identity. So on first launch we mint a UUID v4, write it next to the other
//! per-install state, and read that same value back forever after. Everything
//! else reported here (hostname, os, versions) is descriptive metadata that the
//! backend is free to overwrite on each upsert; only `device_id` is the key.
//!
//! Registration is `POST {backend}/devices`, bearer-authed with the session
//! token, and is BEST-EFFORT in exactly the sense `agent.rs`'s run persistence
//! is: it is fired from the remote listener, never awaited by startup, and any
//! failure is logged and dropped. A fleet-view row is not worth a machine that
//! won't boot.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

/// Where the minted id lives, inside the app data dir. Same directory and same
/// "one small file, one value" convention as `credentials.rs`'s `.cred_key` —
/// there is no second config location in this app, and inventing one would mean
/// two places to look when a device shows up twice in the fleet view.
const DEVICE_ID_FILE: &str = "device_id";

/// How long a registration attempt may hang before we give up. Registration is
/// decorative; it must never keep a socket task alive waiting on a dead server.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `POST /enroll` may hang. Shorter than the enrollment key's 15-minute
/// life by four orders of magnitude, and long enough for a cold backend: the
/// operator is standing at the machine waiting for an answer, so a stuck spinner
/// is worse than "couldn't reach the server, try again".
const ENROLL_TIMEOUT: Duration = Duration::from_secs(20);

/// Emitted when the backend refuses this machine's DEVICE token — the enrollment
/// is dead (revoked, expired, or the device row was forgotten) and the machine
/// must be re-enrolled with a fresh key.
///
/// This event exists so that refusal is LOUD. There is deliberately no automatic
/// recovery behind it: the machine does not clear its token, does not retry with
/// some other credential, and above all does not offer Google sign-in. See
/// `credentials::backend_credential` for why that fallback is the one thing this
/// design must not do.
pub const EV_DEVICE_REJECTED: &str = "device://rejected";

/// This machine's identity, as reported to the backend and to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub hostname: String,
    /// "macos" | "windows" | "linux" — `std::env::consts::OS` already emits
    /// exactly these strings, so there is nothing to map.
    pub os: String,
    pub os_version: String,
    pub app_version: String,
}

fn app_data(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("create app data dir: {e}"))?;
    Ok(dir)
}

/// Mint a random UUID v4 (RFC 4122: version nibble 4, variant bits 10).
///
/// Hand-rolled off `rand`, which is already a dependency (`pinned.rs` mints its
/// set ids the same way), rather than pulling in the `uuid` crate for sixteen
/// bytes and a format string.
fn mint_uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Cheap shape check for a value read back off disk: 36 chars of hex and dashes
/// in the canonical positions. We do not validate the version nibble — an id
/// written by an older build is still a perfectly good key.
fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Read this machine's device id, minting and persisting one on first launch.
///
/// A file that is missing, unreadable, empty, or garbage is treated the same as
/// "first launch": mint a fresh id and overwrite. Refusing to start because a
/// half-written file lost a fight with a power cut would be the worst possible
/// trade — the cost of a fresh id is one duplicate row in the fleet view, the
/// cost of an error path here is an app that will not open.
pub fn device_id(app: &AppHandle) -> Result<String, String> {
    let path = app_data(app)?.join(DEVICE_ID_FILE);
    if let Ok(raw) = fs::read_to_string(&path) {
        let id = raw.trim();
        if looks_like_uuid(id) {
            return Ok(id.to_string());
        }
        eprintln!("[device] unusable device id file at {path:?}; minting a new one");
    }
    let id = mint_uuid_v4();
    fs::write(&path, &id).map_err(|e| format!("write device id: {e}"))?;
    Ok(id)
}

/// Run a short command and return its trimmed stdout, or `None` on any failure.
/// Used only for OS facts std does not expose; every caller has a fallback.
fn probe(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    // Without CREATE_NO_WINDOW each of these flashes a console window over the
    // user's screen on Windows — which, on a computer-use agent, also lands in
    // the next screenshot the model is shown.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// This machine's network name.
///
/// std has no hostname API. On Windows the OS guarantees `COMPUTERNAME` in every
/// process environment, so no subprocess is needed there; elsewhere `hostname(1)`
/// is in the base system on both macOS and Linux. `HOSTNAME` is a last resort
/// rather than the first choice: many shells set it as a shell variable that a
/// GUI app never inherits, which makes it unreliable rather than wrong.
fn hostname() -> String {
    #[cfg(windows)]
    let primary = std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty());
    #[cfg(not(windows))]
    let primary = probe("hostname", &[]);

    primary
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// A human-meaningful OS version string, per platform.
///
/// macOS: `sw_vers -productVersion` → "15.3.1".
/// Windows: `cmd /c ver` prints "Microsoft Windows [Version 10.0.26100.2314]",
///   and we lift the bracketed number out. Deliberately not `GetVersionEx`
///   (which lies about anything past 8.1 unless the app carries a compatibility
///   manifest) and deliberately not the `windows`/`os_info` crates — a whole
///   dependency for one string. The console-window flash is handled in `probe`.
/// Linux: kernel release via `uname -r`. The distro's own version lives in
///   /etc/os-release, but the kernel is the one answer every distro has.
///
/// Any failure yields "unknown": this is metadata on a best-effort report, and
/// a device with an unknown OS version is still a device worth registering.
fn os_version() -> String {
    let raw = if cfg!(target_os = "macos") {
        probe("sw_vers", &["-productVersion"])
    } else if cfg!(target_os = "windows") {
        probe("cmd", &["/c", "ver"]).map(|s| parse_windows_ver(&s))
    } else {
        probe("uname", &["-r"])
    };
    raw.filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Pull "10.0.26100.2314" out of "Microsoft Windows [Version 10.0.26100.2314]".
/// Falls back to the whole line if that format ever changes — a slightly ugly
/// version string beats losing the fact entirely.
fn parse_windows_ver(line: &str) -> String {
    line.rsplit_once('[')
        .and_then(|(_, rest)| rest.trim().strip_suffix(']'))
        .map(|inner| inner.trim_start_matches("Version").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| line.trim().to_string())
}

/// Gather everything the backend and the UI need to describe this machine.
pub fn info(app: &AppHandle) -> Result<DeviceInfo, String> {
    Ok(DeviceInfo {
        device_id: device_id(app)?,
        hostname: hostname(),
        os: std::env::consts::OS.to_string(),
        os_version: os_version(),
        // The Tauri package version (tauri.conf.json / Cargo.toml), so this
        // tracks whatever version the installer actually shipped rather than
        // whatever a constant here was last updated to.
        app_version: app.package_info().version.to_string(),
    })
}

/// `POST {backend}/devices` — announce this machine. It is an upsert
/// server-side, so calling it on every launch and every reconnect is the
/// intended usage, not a duplicate. Best effort: logs and swallows every
/// failure, exactly like the `/runs` mirroring in `agent.rs`.
pub async fn register(app: AppHandle, backend: String, auth: String) {
    let info = match info(&app) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[device] registration skipped: {e}");
            return;
        }
    };
    let url = format!("{}/devices", backend.trim_end_matches('/'));
    let client = match reqwest::Client::builder().timeout(REGISTER_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[device] registration skipped: http client: {e}");
            return;
        }
    };
    let body = json!({
        "device_id": info.device_id,
        "hostname": info.hostname,
        "os": info.os,
        "os_version": info.os_version,
        "app_version": info.app_version,
    });
    // One credential choice for the whole app: a stored device token if this
    // machine is enrolled, otherwise the session token the frontend handed down.
    // Never both, never a fallback from one to the other.
    let mut req = client.post(&url).json(&body);
    if let Some(cred) = crate::credentials::backend_credential(&app, &auth) {
        req = req.header("authorization", format!("Bearer {cred}"));
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => {
            eprintln!("[device] registered {} ({})", info.device_id, info.hostname)
        }
        Ok(r) => {
            let status = r.status();
            eprintln!("[device] register: HTTP {status}");
            note_rejection(&app, status.as_u16(), "register");
        }
        Err(e) => eprintln!("[device] register: request failed: {e}"),
    }
}

/// Tell the UI when an authenticated call was refused *while holding a device
/// token*, so a worker can say "this machine is no longer enrolled" instead of
/// failing silently forever.
///
/// Gated on `is_enrolled` because 401/403 means something entirely different on
/// an admin machine — an expired Google session, which the existing sign-in flow
/// already handles. Only a worker is un-enrollable, and only a worker must never
/// be offered sign-in as the cure.
pub(crate) fn note_rejection(app: &AppHandle, status: u16, whence: &str) {
    if !matches!(status, 401 | 403) || !crate::credentials::is_enrolled(app) {
        return;
    }
    eprintln!("[device] enrollment refused by backend (HTTP {status} from {whence})");
    let _ = app.emit(
        EV_DEVICE_REJECTED,
        json!({ "status": status, "source": whence }),
    );
}

// ---- Tauri commands -------------------------------------------------------

/// This machine's identity, for the fleet/settings UI. Mirrors the shape of
/// `agent::model_endpoint`: a pure read with no visible side effect — except
/// that the very first call is what mints the id, if nothing has yet.
#[tauri::command]
pub fn device_info(app: AppHandle) -> Result<DeviceInfo, String> {
    info(&app)
}

// ---- enrollment -----------------------------------------------------------

/// What `POST /enroll` returns. `device_token` is deserialized and immediately
/// handed to the credential store; it is NOT part of `EnrollOk`, so it never
/// crosses back over the command boundary — same rule the vault applies to
/// passwords and the BYOK key.
#[derive(Debug, Deserialize)]
struct EnrollResponse {
    device_token: String,
    expires_at: Option<String>,
    scope: Option<String>,
    jti: Option<String>,
}

/// The successful half of enrollment, as the UI needs it: enough to confirm what
/// just happened, with no credential in it.
#[derive(Debug, Serialize)]
pub struct EnrollOk {
    pub device_id: String,
    pub hostname: String,
    pub expires_at: Option<String>,
    pub scope: Option<String>,
    pub jti: Option<String>,
}

/// The failing half. `kind` is the load-bearing field: the UI says two very
/// different things depending on it.
///
/// - `"rejected"` — the backend answered, and said no. The key is unknown,
///   expired, or already redeemed; the server returns one indistinguishable 401
///   for all three on purpose, so we cannot say which. Actionable by the human:
///   check the key, or ask the operator to mint a fresh one.
/// - `"unreachable"` — we never got an answer (DNS, TLS, timeout, connection
///   refused). The key is very likely still good and still ticking down its
///   15-minute life; the fix is to retry, not to retype.
/// - `"internal"` — this machine could not hold up its end (no device id, no
///   HTTP client, could not persist the token). Retyping the key will not help.
///
/// Collapsing these into one string would make the UI guess, and the wrong guess
/// costs the operator a walk back to the admin machine for a key that was fine.
#[derive(Debug, Serialize)]
pub struct EnrollError {
    pub kind: &'static str,
    pub message: String,
}

impl EnrollError {
    fn rejected(message: impl Into<String>) -> Self {
        Self { kind: "rejected", message: message.into() }
    }
    fn unreachable(message: impl Into<String>) -> Self {
        Self { kind: "unreachable", message: message.into() }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self { kind: "internal", message: message.into() }
    }
}

/// Classify a non-2xx answer from `/enroll`.
///
/// A 5xx is the SERVER failing, not the key failing. Reporting it as a rejection
/// would send the operator back for a replacement key while the one in their hand
/// is still perfectly good and still ticking — so it is reported as unreachable,
/// which is what it functionally is: no verdict was reached.
///
/// Everything else in the 4xx range is a verdict. The backend answers unknown,
/// expired and already-used with one identical 401 by design, so the message here
/// names all three possibilities rather than pretending to know which.
fn enroll_failure(status: u16) -> EnrollError {
    if (500..600).contains(&status) {
        EnrollError::unreachable(format!("backend error (HTTP {status})"))
    } else {
        EnrollError::rejected(
            "that key was not accepted — it may be mistyped, expired, or already used",
        )
    }
}

/// Redeem a one-time enrollment key for this machine's device token.
///
/// `POST {backend}/enroll` is the one backend call in the app that carries NO
/// bearer — the key itself is the auth — so it deliberately does not go through
/// `credentials::backend_credential`. On success the returned token is persisted
/// and this machine is a worker from that moment on: `credential_class` answers
/// `"device"`, and every subsequent backend call picks the device token up
/// automatically.
///
/// The facts sent are exactly what `info()` reports, so a machine that enrols and
/// a machine that re-registers describe themselves identically and land in the
/// same `Device` row.
///
/// The key is never logged, not even on failure — it is a bearer credential for
/// joining a fleet for as long as it lives.
#[tauri::command]
pub async fn enroll(
    app: AppHandle,
    key: String,
    backend: Option<String>,
) -> Result<EnrollOk, EnrollError> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(EnrollError::rejected("enter the enrollment key"));
    }

    let info = info(&app).map_err(EnrollError::internal)?;
    // Same precedence as `start_agent_task`: the frontend's configured base when
    // it has one (correct in release builds), env/localhost otherwise.
    let base = backend.unwrap_or_else(crate::agent::backend_url);
    let url = format!("{}/enroll", base.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(ENROLL_TIMEOUT)
        .build()
        .map_err(|e| EnrollError::internal(format!("http client: {e}")))?;

    let body = json!({
        "key": key,
        "device_id": info.device_id,
        "hostname": info.hostname,
        "os": info.os,
        "os_version": info.os_version,
        "app_version": info.app_version,
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        // No response at all: transport, not judgement. Report the endpoint so a
        // misconfigured backend URL is visible, never the key.
        .map_err(|e| EnrollError::unreachable(format!("could not reach {url}: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(enroll_failure(status.as_u16()));
    }

    let parsed: EnrollResponse = resp
        .json()
        .await
        .map_err(|e| EnrollError::internal(format!("unreadable enroll response: {e}")))?;
    if parsed.device_token.is_empty() {
        return Err(EnrollError::internal("enroll returned an empty device token"));
    }

    crate::credentials::set_device_token(&app, &parsed.device_token)
        .map_err(|e| EnrollError::internal(format!("could not store device token: {e}")))?;

    eprintln!(
        "[device] enrolled {} ({}) scope={:?}",
        info.device_id, info.hostname, parsed.scope
    );
    Ok(EnrollOk {
        device_id: info.device_id,
        hostname: info.hostname,
        expires_at: parsed.expires_at,
        scope: parsed.scope,
        jti: parsed.jti,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_canonical_v4_uuids() {
        let id = mint_uuid_v4();
        assert!(looks_like_uuid(&id), "{id}");
        assert_eq!(&id[14..15], "4", "version nibble: {id}");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "variant: {id}");
        assert_ne!(mint_uuid_v4(), mint_uuid_v4());
    }

    #[test]
    fn rejects_corrupt_ids() {
        assert!(!looks_like_uuid(""));
        assert!(!looks_like_uuid("not-a-uuid"));
        // Right length, dashes in the wrong places.
        assert!(!looks_like_uuid("0123456789abcdef0123456789abcdef-1-2"));
        // Right shape, non-hex payload.
        assert!(!looks_like_uuid("zzzzzzzz-0000-4000-8000-000000000000"));
    }

    #[test]
    fn parses_windows_ver_output() {
        assert_eq!(
            parse_windows_ver("Microsoft Windows [Version 10.0.26100.2314]"),
            "10.0.26100.2314"
        );
        // An unrecognised shape falls back to the whole line rather than "".
        assert_eq!(parse_windows_ver("something else"), "something else");
    }

    /// The whole reason `EnrollError` carries a `kind`: a rejected key sends the
    /// operator back for a new one, an unreachable backend tells them to retry
    /// with the key they already have. Getting 401 and 503 the same way round is
    /// the mistake this pins.
    #[test]
    fn separates_a_rejected_key_from_a_backend_that_did_not_answer() {
        assert_eq!(enroll_failure(401).kind, "rejected");
        assert_eq!(enroll_failure(404).kind, "rejected");
        assert_eq!(enroll_failure(429).kind, "rejected");
        assert_eq!(enroll_failure(500).kind, "unreachable");
        assert_eq!(enroll_failure(502).kind, "unreachable");
        assert_eq!(enroll_failure(503).kind, "unreachable");
    }

    /// The backend answers unknown / expired / already-used with one
    /// indistinguishable 401, so the copy must not claim to know which it was.
    #[test]
    fn rejection_message_names_every_cause_it_cannot_distinguish() {
        let m = enroll_failure(401).message;
        assert!(m.contains("mistyped"), "{m}");
        assert!(m.contains("expired"), "{m}");
        assert!(m.contains("already used"), "{m}");
    }

    #[test]
    fn os_constant_matches_the_wire_vocabulary() {
        assert!(matches!(
            std::env::consts::OS,
            "macos" | "windows" | "linux"
        ));
    }
}

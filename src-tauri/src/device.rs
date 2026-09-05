//! Stable device identity + self-registration with the backend.
//!
//! A ScreenBuddy install runs on a fleet of laptops and VMs, and the fleet view
//! needs to answer "is this the same machine I saw yesterday?". Hostnames get
//! renamed, VMs get re-imaged onto new IPs, OS versions move — none of them are
//! an identity. So on first launch we derive a UUID from the machine's own
//! hardware identifier, write it next to the other per-install state, and read
//! that same value back forever after. Everything else reported here (hostname,
//! os, versions) is descriptive metadata that the backend is free to overwrite
//! on each upsert; only `device_id` is the key.
//!
//! The id is derived rather than minted because a random UUID only survives what
//! the app data dir survives: reinstall the app, or wipe app data, and the
//! machine comes back as a stranger — a dead row in the fleet view and a walk
//! back to the admin machine for a fresh enrollment key. Hardware outlives both.
//! What the file still buys us is the read path: one probe, then a plain read
//! forever, and a machine whose hardware id is unreadable still gets an id.
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
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};

/// Where the minted id lives, inside the app data dir. Same directory and same
/// "one small file, one value" convention as `credentials.rs`'s `.cred_key` —
/// there is no second config location in this app, and inventing one would mean
/// two places to look when a device shows up twice in the fleet view.
const DEVICE_ID_FILE: &str = "device_id";

/// Fixed prefix mixed into the hardware id before hashing. Two jobs, both about
/// keeping the machine's real hardware identifier off the wire: it domain-
/// separates our ids from any other product that hashes the same
/// `IOPlatformUUID` (so a leaked id from elsewhere is not a lookup key here),
/// and it means the value we store and send is a one-way function of the
/// hardware rather than the hardware itself.
///
/// It is NOT a secret — it ships in the binary — and it must NEVER change:
/// bumping it re-identifies every machine that has not yet written its id file.
const DEVICE_ID_SALT: &str = "screenbuddy.device-id.v1|";

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
    format_uuid(&b)
}

/// Sixteen bytes in the canonical 8-4-4-4-12 hex layout. Shared by the random
/// and the derived id so both are the same shape to every reader of the file.
fn format_uuid(b: &[u8; 16]) -> String {
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

/// Hash a machine's hardware identifier into a UUID-shaped id.
///
/// The raw value never leaves the box. `IOPlatformUUID` and `MachineGuid` are
/// the identifiers other software on the same machine also fingerprints users
/// by, and shipping one to a server — where it lands in a database, a log line
/// and a backup — hands out a cross-product join key for nothing in return. A
/// SHA-256 over `salt || value` keeps every property we actually need (same
/// machine → same id, different machines → different ids) and throws away the
/// one we don't (the ability to work backwards to the hardware). `sha2` is
/// already a direct dependency — `artifacts.rs` content-addresses with it.
///
/// Truncating a 256-bit digest to 128 bits is not a weakness here: this is a
/// uniqueness key over a fleet of tens of machines, not a security boundary,
/// and 128 bits is exactly what a random UUID would have given us anyway.
///
/// The version nibble is 8 — "custom" in RFC 9562 — because that is honestly
/// what this is. Claiming 4 would assert the bytes are random when they are a
/// pure function of the hardware, and nothing in this app or the backend keys
/// off the version (see `looks_like_uuid`).
fn derive_device_id(hardware: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DEVICE_ID_SALT.as_bytes());
    // Case and surrounding whitespace are formatting of the transport (ioreg
    // prints upper, /etc/machine-id lower), not part of the identifier.
    hasher.update(hardware.trim().to_ascii_lowercase().as_bytes());
    let digest = hasher.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x80; // version 8 (custom)
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    format_uuid(&b)
}

/// This machine's hardware identifier, or `None` if the OS will not give one up.
///
/// Deliberately NOT the MAC address, which is the obvious first idea and the
/// wrong one: macOS rotates a private Wi-Fi address per network, cloning a VM
/// re-randomises it, a laptop's answer changes the moment it is docked, and
/// "the" MAC is ambiguous on any machine with more than one NIC. Every one of
/// those is a silent re-identification of an enrolled machine. The platform
/// UUIDs below have the one property a MAC lacks — they do not move.
///
/// - macOS: `IOPlatformUUID`, carried by the platform expert device.
/// - Windows: `MachineGuid`, written once by the OS installer.
/// - Linux: `/etc/machine-id` (systemd), with the older dbus path as a fallback.
///   Read directly rather than through a subprocess — they are plain files.
///
/// All three are stable across reinstalls of *our app*, which is the whole
/// point. None survives an OS reimage, and none is expected to: a reimaged
/// machine genuinely is a new machine.
fn hardware_uuid() -> Option<String> {
    if cfg!(target_os = "macos") {
        probe("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])
            .as_deref()
            .and_then(parse_ioreg_uuid)
    } else if cfg!(target_os = "windows") {
        // `/reg:64` pins the 64-bit view. Without it a 32-bit process is
        // WOW64-redirected to `SOFTWARE\Wow6432Node\Microsoft\Cryptography`,
        // where MachineGuid does not exist — so one machine would answer "no
        // hardware id" or a different id purely from the bitness of the build.
        // We ship x86_64, which already gets the native view, but the flag makes
        // that true independent of how the app is built. 32-bit-only Windows
        // rejects the flag outright, hence the retry without it.
        const KEY: &str = r"HKLM\SOFTWARE\Microsoft\Cryptography";
        probe("reg", &["query", KEY, "/v", "MachineGuid", "/reg:64"])
            .or_else(|| probe("reg", &["query", KEY, "/v", "MachineGuid"]))
            .as_deref()
            .and_then(parse_machine_guid)
    } else {
        ["/etc/machine-id", "/var/lib/dbus/machine-id"]
            .iter()
            .find_map(|p| fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// Lift the value out of ioreg's property dump, whose line reads:
/// `    "IOPlatformUUID" = "2C4B9A17-6E0D-5F82-9C31-7A5E1D4B8F60"`.
///
/// Matched on the quoted key so it cannot collide with another property that
/// merely mentions the name, and shape-checked before it is returned: a match
/// yielding something that is not a UUID would mean ioreg changed its format,
/// and a wrong-but-plausible id is worse than no id at all.
fn parse_ioreg_uuid(dump: &str) -> Option<String> {
    dump.lines()
        .filter_map(|l| l.split_once("\"IOPlatformUUID\""))
        .filter_map(|(_, rest)| rest.split_once('='))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .find(|v| looks_like_uuid(v))
}

/// Lift the value out of `reg query`'s tabular output:
/// `    MachineGuid    REG_SZ    5f7a1c3e-…`.
///
/// The columns are separated by whitespace of an unspecified width, so this
/// splits on whitespace rather than counting spaces and takes the last field —
/// the value, which is the only column that cannot contain anything else.
fn parse_machine_guid(out: &str) -> Option<String> {
    out.lines()
        .filter(|l| l.split_whitespace().next() == Some("MachineGuid"))
        .filter_map(|l| l.split_whitespace().last())
        .map(|v| v.to_string())
        .find(|v| looks_like_uuid(v))
}

/// Read this machine's device id, establishing and persisting one on first
/// launch.
///
/// **A valid file always wins.** Machines are enrolled against the id they are
/// already reporting; re-deriving on a machine that has one would silently
/// orphan its fleet row and kill its enrollment — and would do it on upgrade, to
/// every machine at once. So the hardware probe only ever answers "what id
/// should a machine that has none take?", never "was the existing answer
/// right?". Writing the derived value straight back to the same file is what
/// keeps that true: the read path above stays the only read path, and the probe
/// runs once per install rather than once per call.
///
/// A file that is missing, unreadable, empty, or garbage is treated as "first
/// launch". Refusing to start because a half-written file lost a fight with a
/// power cut would be the worst possible trade — the cost of a fresh id is one
/// duplicate row in the fleet view, the cost of an error path here is an app
/// that will not open. The same reasoning covers an unreadable hardware id
/// (a locked-down OS, a stripped container, an OS we did not anticipate): fall
/// back to a random UUID exactly as before, and log which path was taken so a
/// duplicated fleet row can still be explained after the fact.
pub fn device_id(app: &AppHandle) -> Result<String, String> {
    resolve_device_id(&app_data(app)?.join(DEVICE_ID_FILE), hardware_uuid)
}

/// The body of `device_id`, over a path and a hardware source rather than an
/// `AppHandle` — the rules above are the ones worth pinning in a test, and a
/// test cannot conjure a Tauri app.
///
/// `hardware` is a closure, not a value, precisely so the ordering is structural:
/// there is no way to write a caller that probes the hardware of a machine whose
/// file already answers the question.
fn resolve_device_id(
    path: &std::path::Path,
    hardware: impl FnOnce() -> Option<String>,
) -> Result<String, String> {
    if let Ok(raw) = fs::read_to_string(path) {
        let id = raw.trim();
        if looks_like_uuid(id) {
            return Ok(id.to_string());
        }
        eprintln!("[device] unusable device id file at {path:?}; establishing a new one");
    }
    let (id, whence) = match hardware() {
        Some(hw) => (
            derive_device_id(&hw),
            "derived from this machine's hardware id",
        ),
        None => (
            mint_uuid_v4(),
            "minted at random (no hardware id available — a reinstall will \
             re-identify this machine)",
        ),
    };
    fs::write(path, &id).map_err(|e| format!("write device id: {e}"))?;
    eprintln!("[device] device id {id} {whence}");
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

    /// A scratch directory per test, named like the ones in `artifacts.rs`.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sb_dev_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// THE requirement. Two machines are enrolled against ids that were minted
    /// at random, and the backend keys their fleet rows and their device tokens
    /// off exactly those strings. If the hardware id can ever override a file
    /// that is already there, shipping this orphans both of them at once — so
    /// the probe must not even be consulted, which is what the panicking closure
    /// asserts.
    #[test]
    fn an_existing_id_file_beats_the_hardware_and_is_not_even_asked() {
        let dir = scratch("keep");
        let path = dir.join(DEVICE_ID_FILE);
        let enrolled = "b1e2c3d4-0000-4000-8000-abcdefabcdef";
        fs::write(&path, enrolled).unwrap();

        let got = resolve_device_id(&path, || panic!("probed hardware despite a valid id file"));
        assert_eq!(got.unwrap(), enrolled);
        // And the file is untouched, so the next launch reads the same thing.
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), enrolled);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Trailing whitespace is what a file written by hand or by an editor looks
    /// like; it must not be mistaken for corruption and re-identify the machine.
    #[test]
    fn a_padded_id_file_still_wins() {
        let dir = scratch("pad");
        let path = dir.join(DEVICE_ID_FILE);
        fs::write(&path, "  b1e2c3d4-0000-4000-8000-abcdefabcdef\n").unwrap();
        let got = resolve_device_id(&path, || panic!("probed hardware despite a valid id file"));
        assert_eq!(got.unwrap(), "b1e2c3d4-0000-4000-8000-abcdefabcdef");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The property that makes derivation worth doing at all: wipe the app data
    /// dir, and the same machine comes back as the same device.
    #[test]
    fn the_derived_id_is_stable_across_a_wiped_app_data_dir() {
        let dir = scratch("derive");
        let path = dir.join(DEVICE_ID_FILE);
        let hw = || Some("2C4B9A17-6E0D-5F82-9C31-7A5E1D4B8F60".to_string());

        let first = resolve_device_id(&path, hw).unwrap();
        assert!(looks_like_uuid(&first), "{first}");
        assert_eq!(&first[14..15], "8", "version nibble should be custom: {first}");
        // Written back, so the second call is a plain read and never re-probes.
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        assert_eq!(
            resolve_device_id(&path, || panic!("re-probed after the first call")).unwrap(),
            first
        );

        // The reinstall: the file is gone, the hardware is not.
        fs::remove_file(&path).unwrap();
        assert_eq!(resolve_device_id(&path, hw).unwrap(), first);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The hash is a pure function of the hardware id, insensitive to how the OS
    /// happened to print it, and different machines land on different ids. The
    /// literal is pinned so a future edit to the salt or the byte layout — which
    /// would re-identify every machine that has not written its file yet — fails
    /// here instead of in the fleet view.
    #[test]
    fn derivation_is_stable_and_machine_specific() {
        let a = derive_device_id("2C4B9A17-6E0D-5F82-9C31-7A5E1D4B8F60");
        assert_eq!(a, derive_device_id("2C4B9A17-6E0D-5F82-9C31-7A5E1D4B8F60"));
        assert_eq!(a, derive_device_id(" 2c4b9a17-6e0d-5f82-9c31-7a5e1d4b8f60 \n"));
        assert_ne!(a, derive_device_id("00000000-0000-0000-0000-000000000000"));
        // The hardware id itself must not be recoverable from what we ship.
        assert!(!a.contains("2c4b9a17"), "{a}");
        assert_eq!(a, "ea674c2b-3a33-8b43-b934-cf02c8e861ec", "salt or layout changed");
    }

    /// A stripped container, a locked-down OS, an OS we did not anticipate: the
    /// app still starts and still gets an id. Losing hardware-stability is a
    /// duplicate fleet row; failing here is a machine that will not open.
    #[test]
    fn no_hardware_id_falls_back_to_a_random_uuid() {
        let dir = scratch("fallback");
        let path = dir.join(DEVICE_ID_FILE);

        let id = resolve_device_id(&path, || None).unwrap();
        assert!(looks_like_uuid(&id), "{id}");
        assert_eq!(&id[14..15], "4", "random fallback should be a v4: {id}");
        // Persisted like any other, so it is stable for as long as the file is.
        assert_eq!(
            resolve_device_id(&path, || None).unwrap(),
            id,
            "the fallback id must survive the next launch"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A hardware probe that answers with something other than a UUID is a
    /// probe whose output format changed; taking it at face value would give the
    /// machine a stable-but-wrong id, which is harder to notice than no id.
    #[test]
    fn a_garbled_hardware_answer_is_rejected_by_the_parsers() {
        assert_eq!(parse_ioreg_uuid("nothing to see here"), None);
        assert_eq!(
            parse_ioreg_uuid("    \"IOPlatformUUID\" = \"not-a-uuid\""),
            None
        );
        assert_eq!(parse_machine_guid("ERROR: The system was unable to find"), None);
        assert_eq!(parse_machine_guid("    MachineGuid    REG_SZ    ????"), None);
    }

    /// Real `ioreg -rd1 -c IOPlatformExpertDevice` output, trimmed. The key is
    /// matched quoted, so a neighbouring property that merely mentions the name
    /// cannot win the race.
    #[test]
    fn parses_the_platform_uuid_out_of_an_ioreg_dump() {
        let dump = "+-o J316sAP  <class IOPlatformExpertDevice, id 0x100000278>\n\
                    {\n\
                    \x20 \"IOPlatformSerialNumber\" = \"XXXXXXXXXX\"\n\
                    \x20 \"IOPlatformUUID\" = \"2C4B9A17-6E0D-5F82-9C31-7A5E1D4B8F60\"\n\
                    \x20 \"IOPolledInterface\" = \"AppleARMWatchdogTimerHibernateHandler is not serializable\"\n\
                    }\n";
        assert_eq!(
            parse_ioreg_uuid(dump).as_deref(),
            Some("2C4B9A17-6E0D-5F82-9C31-7A5E1D4B8F60")
        );
    }

    /// `reg query`'s three columns are separated by whitespace of no fixed
    /// width, and the blank line + echoed key path above the value are part of
    /// the output rather than noise we can assume away.
    #[test]
    fn parses_machine_guid_out_of_reg_query_output() {
        let out = "\r\n\
                   HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography\r\n\
                   \x20   MachineGuid    REG_SZ    5f7a1c3e-9b2d-4a11-8c60-0e3f5d7a91bc\r\n";
        assert_eq!(
            parse_machine_guid(out).as_deref(),
            Some("5f7a1c3e-9b2d-4a11-8c60-0e3f5d7a91bc")
        );
    }

    #[test]
    fn os_constant_matches_the_wire_vocabulary() {
        assert!(matches!(
            std::env::consts::OS,
            "macos" | "windows" | "linux"
        ));
    }
}

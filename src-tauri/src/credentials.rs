// credentials.rs — encrypted local credential vault (websites AND desktop apps).
//
// SECURITY MODEL:
// The vault is an AES-256-GCM encrypted file (`credentials.enc`). The 32-byte
// master key is stored in the OS credential store — the login Keychain on
// macOS, Credential Manager on Windows, both via the `keyring` crate — and its
// first use per app run is gated behind a biometric prompt (Touch ID / Windows
// Hello, via `robius-authentication`). The unlocked key is then cached in
// memory for the rest of the session so the user is prompted at most once per
// run — not on every credential operation.
//
// FALLBACK: if the credential store or the biometric gate is unavailable (older
// OS, no enrolled biometrics, an unsupported platform, or any runtime error) we
// fall back to the legacy on-disk key file at `app_data_dir/.cred_key`, so the
// vault keeps working and the build always compiles. We never hard-fail the
// vault just because the secure path is missing.
//
// The fallback is genuinely weaker: the master key sits in a plain file next to
// the ciphertext it protects, and `restrict_perms` can only tighten it on unix
// (chmod 0600) — on Windows it is a no-op and the file inherits the app-data
// ACL. That is why Windows now takes the Credential Manager path rather than
// dropping straight to the file key, which is what it did before.
//
// Passwords are returned ONLY through the non-command `lookup` helper (used by
// the agent loop's `use_credential` tool to type a secret locally). The
// `#[tauri::command]` surface never hands a password back to the frontend, and
// the secret is never placed into model context.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

const KEY_FILE: &str = ".cred_key";
const VAULT_FILE: &str = "credentials.enc";
/// BYOK: the user's own Anthropic API key, encrypted with the same AES-256-GCM
/// cipher + Keychain master key as the vault, in its own small file (kept out of
/// the vault list so it never surfaces in the credentials UI).
const ANTHROPIC_KEY_FILE: &str = "anthropic_key.enc";
/// The worker credential: a long-lived, worker-scoped device token minted by
/// `POST /enroll`. Same cipher and master key as the vault, its own small file
/// so it never appears in `cred_list`. Its mere EXISTENCE is what makes this
/// machine a worker — see `credential_class`.
const DEVICE_TOKEN_FILE: &str = "device_token.enc";
const NONCE_LEN: usize = 12;

/// Identifiers for the master-key entry in the OS credential store (macOS
/// Keychain service/account; Windows Credential Manager target/username).
/// Deliberately identical across platforms so the entry is recognisable, and
/// deliberately NOT changed from the original macOS values — an existing macOS
/// install must keep finding the key it already stored.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const KEYCHAIN_SERVICE: &str = "com.screenbuddy.vault";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const KEYCHAIN_ACCOUNT: &str = "vault-master-key";

/// Session cache of the unlocked 32-byte master key. Populated on first vault
/// access (after a one-time Touch ID prompt on macOS) and reused for the rest of
/// the app run so the user isn't re-prompted on every credential operation.
static CACHED_KEY: Mutex<Option<[u8; 32]>> = Mutex::new(None);

/// A stored credential. Passwords live here on disk (encrypted) but are stripped
/// before crossing the command boundary (see `CredentialMeta`). The key field is
/// `target`: a free-form label for a website OR a desktop app
/// (e.g. "mail.google.com", "Amazon — desktop app", "Slack app"). Vaults written
/// by the old "site"-keyed schema are still readable via the serde alias.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Credential {
    #[serde(alias = "site")]
    target: String,
    username: String,
    password: String,
}

/// Metadata-only view returned to the frontend — NEVER includes the password.
#[derive(Debug, Serialize)]
pub struct CredentialMeta {
    pub target: String,
    pub username: String,
}

fn app_data(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("create app data dir: {e}"))?;
    Ok(dir)
}

/// Obtain the 32-byte master key, using the session cache when warm. On a cold
/// cache: macOS tries the biometric-gated Keychain key first and silently falls
/// back to the on-disk file key on any failure; other platforms use the file key.
fn master_key(app: &AppHandle) -> Result<[u8; 32], String> {
    if let Some(key) = *CACHED_KEY.lock().map_err(|e| format!("key cache poisoned: {e}"))? {
        return Ok(key);
    }

    let key = acquire_key(app)?;

    *CACHED_KEY.lock().map_err(|e| format!("key cache poisoned: {e}"))? = Some(key);
    Ok(key)
}

/// macOS and Windows both have a real OS credential store, so both take the
/// secure path and only degrade to the file key on error.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn acquire_key(app: &AppHandle) -> Result<[u8; 32], String> {
    match keychain_key() {
        Ok(key) => Ok(key),
        Err(e) => {
            // Never hard-fail: degrade to the legacy file key so the vault works.
            log::warn!("OS credential store master key unavailable ({e}); using file key fallback");
            file_key(app)
        }
    }
}

/// Everything else (Linux/BSD) has no store wired up here, so the file key is
/// the only option.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn acquire_key(app: &AppHandle) -> Result<[u8; 32], String> {
    file_key(app)
}

/// Read (or create) the master key in the OS credential store — the login
/// Keychain on macOS, Credential Manager on Windows — gated behind a one-time
/// biometric prompt. Any failure here is surfaced to the caller, which falls
/// back to the file key, so this never panics the vault.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn keychain_key() -> Result<[u8; 32], String> {
    use base64::Engine;

    // Touch ID / biometric gate. Reading the master key is the sensitive moment;
    // we prompt once per run (subsequent reads hit the in-memory cache).
    biometric_gate()?;

    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("keychain entry: {e}"))?;

    match entry.get_password() {
        Ok(b64) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| format!("decode keychain key: {e}"))?;
            if bytes.len() != 32 {
                return Err("corrupt keychain key (wrong length)".into());
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            Ok(key)
        }
        Err(keyring::Error::NoEntry) => {
            // First run: mint a new key and persist it in the Keychain.
            let mut key = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut key);
            let b64 = base64::engine::general_purpose::STANDARD.encode(key);
            entry
                .set_password(&b64)
                .map_err(|e| format!("store keychain key: {e}"))?;
            Ok(key)
        }
        Err(e) => Err(format!("read keychain key: {e}")),
    }
}

/// Biometric gate — Touch ID on macOS, Windows Hello on Windows. Succeeds (Ok)
/// when the user authenticates; returns Err when biometrics are unavailable or
/// the user cancels, and the caller then decides whether to fall back. We allow
/// device-password fallback so machines without an enrolled fingerprint can
/// still unlock via the OS auth sheet.
///
/// The `Text` value below is already cross-platform (it carries an arm per
/// platform), so nothing in the body is macOS-specific. Note that the Windows
/// backend of `robius-authentication` IGNORES the `Policy` argument — Windows
/// Hello decides which factors it will accept — so `PolicyBuilder` only
/// actually governs the Apple path.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn biometric_gate() -> Result<(), String> {
    use robius_authentication::{
        AndroidText, BiometricStrength, Context, PolicyBuilder, Text, WindowsText,
    };

    let policy = PolicyBuilder::new()
        .biometrics(Some(BiometricStrength::Strong))
        .password(true)
        .build()
        .ok_or_else(|| "could not build authentication policy".to_string())?;

    let text = Text {
        apple: "unlock the ScreenBuddy credential vault",
        android: AndroidText {
            title: "Unlock the ScreenBuddy credential vault",
            subtitle: None,
            description: None,
        },
        windows: WindowsText::new(
            "ScreenBuddy",
            "Unlock the ScreenBuddy credential vault",
        )
        .ok_or_else(|| "could not build windows auth text".to_string())?,
    };

    Context::new(())
        .blocking_authenticate(text, &policy)
        .map_err(|e| format!("biometric authentication failed: {e:?}"))
}

/// Load the 32-byte on-disk master key, generating + persisting it on first use.
/// This is the cross-platform fallback, used when the OS credential store is
/// unavailable and as the only path on platforms with no store wired up.
fn file_key(app: &AppHandle) -> Result<[u8; 32], String> {
    let path = app_data(app)?.join(KEY_FILE);
    if path.exists() {
        let bytes = fs::read(&path).map_err(|e| format!("read key: {e}"))?;
        if bytes.len() != 32 {
            return Err("corrupt cred key (wrong length)".into());
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }

    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    fs::write(&path, key).map_err(|e| format!("write key: {e}"))?;
    restrict_perms(&path)?;
    Ok(key)
}

#[cfg(unix)]
fn restrict_perms(path: &PathBuf) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|e| format!("stat key: {e}"))?
        .permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms).map_err(|e| format!("chmod key: {e}"))
}

/// No-op off unix. On Windows there is no chmod equivalent, so the file simply
/// inherits the ACL of the app-data directory (user-owned, but readable by any
/// process running as that user, and by an administrator).
///
/// This is precisely why the Windows build must reach the Credential Manager
/// path in `keychain_key` — this fallback cannot protect the key on its own.
/// Tightening it further would mean a DACL/DPAPI pass, which is only worth doing
/// if the fallback ever becomes the primary path on Windows.
#[cfg(not(unix))]
fn restrict_perms(_path: &PathBuf) -> Result<(), String> {
    Ok(())
}

fn cipher(app: &AppHandle) -> Result<Aes256Gcm, String> {
    let key_bytes = master_key(app)?;
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    Ok(Aes256Gcm::new(key))
}

/// Decrypt the vault, returning all stored credentials. Empty if no vault yet.
fn read_vault(app: &AppHandle) -> Result<Vec<Credential>, String> {
    let path = app_data(app)?.join(VAULT_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let blob = fs::read(&path).map_err(|e| format!("read vault: {e}"))?;
    if blob.len() < NONCE_LEN {
        return Err("corrupt vault (too short)".into());
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher(app)?
        .decrypt(nonce, ciphertext)
        .map_err(|_| "vault decrypt failed (wrong key or tampered file)".to_string())?;
    serde_json::from_slice(&plaintext).map_err(|e| format!("parse vault: {e}"))
}

/// Encrypt + persist the full credential list (nonce is prepended to the file).
fn write_vault(app: &AppHandle, creds: &[Credential]) -> Result<(), String> {
    let plaintext = serde_json::to_vec(creds).map_err(|e| format!("serialize vault: {e}"))?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher(app)?
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|_| "vault encrypt failed".to_string())?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);

    let path = app_data(app)?.join(VAULT_FILE);
    fs::write(&path, blob).map_err(|e| format!("write vault: {e}"))?;
    restrict_perms(&path)?;
    Ok(())
}

/// Encrypt + persist one small secret in its own file, nonce prepended — the
/// same layout as the vault, minus the JSON list. Used for the two single-value
/// secrets (BYOK Anthropic key, device token) so there is exactly one place that
/// knows how a secret is written to disk.
fn write_secret(app: &AppHandle, file: &str, plaintext: &[u8], what: &str) -> Result<(), String> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher(app)?
        .encrypt(nonce, plaintext)
        .map_err(|_| format!("{what} encrypt failed"))?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);

    let path = app_data(app)?.join(file);
    fs::write(&path, blob).map_err(|e| format!("write {what}: {e}"))?;
    restrict_perms(&path)
}

/// Decrypt one small secret written by `write_secret`. Every failure — absent
/// file, short blob, wrong key, non-UTF-8 — collapses to `None`: a caller here
/// only ever wants "the secret, or nothing", and a corrupt file must not stop
/// the app any more than a corrupt device id does.
fn read_secret(app: &AppHandle, file: &str) -> Option<String> {
    let path = app_data(app).ok()?.join(file);
    if !path.exists() {
        return None;
    }
    let blob = fs::read(&path).ok()?;
    if blob.len() < NONCE_LEN {
        return None;
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher(app).ok()?.decrypt(nonce, ciphertext).ok()?;
    String::from_utf8(plaintext).ok()
}

/// Whether `file` exists in the app data dir. A pure existence check: no
/// decrypt, so it never trips the biometric gate. That matters — the credential
/// resolver runs on every backend call and must stay silent on machines that
/// hold no device token at all.
fn secret_exists(app: &AppHandle, file: &str) -> bool {
    app_data(app).map(|d| d.join(file).exists()).unwrap_or(false)
}

/// List stored credentials as metadata only — passwords are never returned here.
#[tauri::command]
pub fn cred_list(app: AppHandle) -> Result<Vec<CredentialMeta>, String> {
    Ok(read_vault(&app)?
        .into_iter()
        .map(|c| CredentialMeta {
            target: c.target,
            username: c.username,
        })
        .collect())
}

/// Upsert a credential by `target` (a website or desktop-app label).
#[tauri::command]
pub fn cred_add(
    app: AppHandle,
    target: String,
    username: String,
    password: String,
) -> Result<(), String> {
    let mut creds = read_vault(&app)?;
    match creds.iter_mut().find(|c| c.target == target) {
        Some(existing) => {
            existing.username = username;
            existing.password = password;
        }
        None => creds.push(Credential {
            target,
            username,
            password,
        }),
    }
    write_vault(&app, &creds)
}

/// Delete the credential for `target` (no-op if absent).
#[tauri::command]
pub fn cred_delete(app: AppHandle, target: String) -> Result<(), String> {
    let mut creds = read_vault(&app)?;
    creds.retain(|c| c.target != target);
    write_vault(&app, &creds)
}

// ---------------------------------------------------------------------------
// BYOK — bring-your-own Anthropic API key.
//
// Stored encrypted at rest with the SAME machinery as the vault (AES-256-GCM +
// Keychain/file master key), in its own `anthropic_key.enc` file so it never
// appears in `cred_list`. The plaintext key is NEVER returned across the
// `#[tauri::command]` boundary — the frontend can only set / probe / clear it.
// The agent loop reads it via the non-command `anthropic_key` helper.
// ---------------------------------------------------------------------------

/// Encrypt + persist the user's own Anthropic API key (BYOK). The key is never
/// logged.
#[tauri::command]
pub fn set_anthropic_key(app: AppHandle, key: String) -> Result<(), String> {
    write_secret(&app, ANTHROPIC_KEY_FILE, key.as_bytes(), "anthropic key")
}

/// Whether a BYOK Anthropic key is stored. NEVER returns the key itself; this is
/// a pure existence check (no decrypt, so it won't trigger the biometric gate).
#[tauri::command]
pub fn has_anthropic_key(app: AppHandle) -> bool {
    secret_exists(&app, ANTHROPIC_KEY_FILE)
}

/// Delete the stored BYOK Anthropic key (no-op if absent).
#[tauri::command]
pub fn clear_anthropic_key(app: AppHandle) -> Result<(), String> {
    let path = app_data(&app)?.join(ANTHROPIC_KEY_FILE);
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("remove anthropic key: {e}"))?;
    }
    Ok(())
}

/// Decrypt + return the stored BYOK Anthropic key for the agent loop's cu-stream
/// request. Non-command (not exposed to the frontend) so the plaintext key never
/// crosses the command boundary. Returns `None` if no key is stored or decrypt
/// fails. The key is never logged.
#[allow(dead_code)]
pub fn anthropic_key(app: &AppHandle) -> Option<String> {
    read_secret(app, ANTHROPIC_KEY_FILE)
}

/// Validate a BYOK Anthropic key by calling Anthropic DIRECTLY (never our
/// server) with a tiny request. Returns `{"valid": true}` on HTTP 200, otherwise
/// `{"valid": false, "error": <status or upstream message>}`. Transport errors
/// also resolve to `{"valid": false, ...}` (we only `Err` on a truly unexpected
/// failure). The key is NEVER logged.
#[tauri::command]
pub async fn validate_anthropic_key(key: String) -> Result<serde_json::Value, String> {
    use serde_json::json;

    let base = std::env::var("CU_ANTHROPIC_BASE")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let url = format!("{base}/v1/messages");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("http client build failed: {e}"))?;

    let body = json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}],
    });

    let resp = match client
        .post(&url)
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        // Transport-level failure (no network, DNS, TLS): treat as invalid rather
        // than erroring, so the UI shows a clean "couldn't validate" state.
        Err(e) => return Ok(json!({ "valid": false, "error": format!("request failed: {e}") })),
    };

    if resp.status().is_success() {
        Ok(json!({ "valid": true }))
    } else {
        // Surface the upstream message when present (e.g. a 401 explanation),
        // falling back to the bare status. Never include the key.
        let status = resp.status();
        let upstream = resp.text().await.ok().filter(|t| !t.is_empty());
        let error = upstream.unwrap_or_else(|| status.to_string());
        Ok(json!({ "valid": false, "error": error }))
    }
}

/// Non-command lookup used by the agent loop's `use_credential` tool. Returns the
/// `username` or `password` for a `target`. Exposed as a plain fn (not a
/// `#[tauri::command]`) so the loop can type the secret locally WITHOUT it ever
/// being returned to the frontend or placed into model context. `field` is
/// "username" | "password".
pub fn lookup(app: &AppHandle, target: &str, field: &str) -> Option<String> {
    let creds = read_vault(app).ok()?;
    let cred = creds.into_iter().find(|c| c.target == target)?;
    match field {
        "username" => Some(cred.username),
        "password" => Some(cred.password),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Device token — the worker credential.
//
// A worker machine never signs in with Google. It redeems a one-time enrollment
// key (see `device::enroll`) for a long-lived, worker-scoped device token, and
// that token is the ONLY backend credential it ever holds. The two credentials
// are alternatives, never companions: holding a device token is exactly what
// makes a machine a worker, so `backend_credential` prefers it unconditionally
// and any session token the frontend passes down is ignored while one is stored.
//
// Encryption at rest is not the security boundary here — a worker runs an
// untrusted computer-use agent with full control of the desktop, so anything on
// that disk is reachable by it. The boundary is the token's SCOPE, enforced
// server-side. Storing it through the vault's machinery is about having one
// credential store rather than two, and about the token not sitting in plain
// text next to the device id file.
//
// Cost note: reading it decrypts, which on a cold cache trips the one-per-run
// biometric gate. That is why `backend_credential` checks for the FILE before
// decrypting — an admin machine, which has no device token, never pays it. A
// worker does, but it already pays it for the BYOK key before any run can start.
// ---------------------------------------------------------------------------

/// Persist the device token returned by `POST /enroll`. Non-command: only the
/// enrollment path may write it, and the plaintext never crosses the command
/// boundary in either direction.
pub fn set_device_token(app: &AppHandle, token: &str) -> Result<(), String> {
    write_secret(app, DEVICE_TOKEN_FILE, token.as_bytes(), "device token")
}

/// The stored device token, or `None` on a machine that was never enrolled.
/// Non-command, like `anthropic_key` — the frontend can learn *that* this
/// machine is enrolled (`credential_class`) but never gets the token itself.
pub fn device_token(app: &AppHandle) -> Option<String> {
    read_secret(app, DEVICE_TOKEN_FILE)
}

/// Whether this machine is enrolled as a worker. Existence check only, so it is
/// cheap and silent — no decrypt, hence no biometric prompt.
pub fn is_enrolled(app: &AppHandle) -> bool {
    secret_exists(app, DEVICE_TOKEN_FILE)
}

/// Forget the enrollment, returning this machine to un-enrolled. Deliberately
/// NOT called automatically when the backend rejects the token: a 401 during a
/// server restart would otherwise silently un-enrol a working worker, and the
/// operator would have to carry a fresh key out to it. Rejection is surfaced to
/// the UI (`device::EV_DEVICE_REJECTED`); erasing is a decision, not a reflex.
#[tauri::command]
pub fn clear_device_token(app: AppHandle) -> Result<(), String> {
    let path = app_data(&app)?.join(DEVICE_TOKEN_FILE);
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("remove device token: {e}"))?;
    }
    Ok(())
}

/// THE credential choice. Every outgoing backend call resolves its bearer here
/// and nowhere else, so there is exactly one answer to "which token is this
/// machine?": `agent.rs`'s `with_bearer`, `device::register` and `remote.rs` all
/// route through it.
///
/// `session` is whatever session token the frontend passed down (empty when it
/// has none). A stored device token always wins.
///
/// NO FALLBACK, and this is the point of the whole change: an enrolled machine
/// whose device token is rejected does NOT fall back to `session`, does not
/// prompt for Google sign-in, and does not retry with anything else — it is
/// un-enrolled and must say so. Falling back would hand a worker, and the
/// untrusted agent running on it, an admin credential; that is exactly the
/// situation enrollment exists to remove. If you arrived here to "fix" a
/// worker's 401 by letting it borrow the session token, re-read this paragraph.
pub fn backend_credential(app: &AppHandle, session: &str) -> Option<String> {
    if is_enrolled(app) {
        // A device token that will not decrypt still means "this is a worker":
        // return None rather than sliding down to the session token.
        return device_token(app);
    }
    if session.is_empty() {
        None
    } else {
        Some(session.to_string())
    }
}

/// Which credential class this machine holds, for the UI shell: `"device"` (an
/// enrolled worker), `"session"` (signed in with Google), or `"none"`.
///
/// Only the device half is knowable from Rust — the Google session lives in the
/// frontend — so the caller describes its own half via `has_session`. Routing it
/// through here anyway keeps one implementation of the precedence rule, the same
/// one `backend_credential` applies, so the shell can never disagree with the
/// credential the requests actually carry. Called with no argument it answers
/// `"device"` or `"none"`, which is the correct read before sign-in.
#[tauri::command]
pub fn credential_class(app: AppHandle, has_session: Option<bool>) -> &'static str {
    if is_enrolled(&app) {
        "device"
    } else if has_session.unwrap_or(false) {
        "session"
    } else {
        "none"
    }
}

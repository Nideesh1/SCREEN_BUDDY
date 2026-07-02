// credentials.rs — encrypted local credential vault (websites AND desktop apps).
//
// SECURITY MODEL:
// The vault is an AES-256-GCM encrypted file (`credentials.enc`). The 32-byte
// master key is, on macOS, stored in the login Keychain (via the `keyring`
// crate) and its first use per app run is gated behind a Touch ID / biometric
// prompt (via `robius-authentication`). The unlocked key is then cached in
// memory for the rest of the session so the user is prompted at most once per
// run — not on every credential operation.
//
// FALLBACK: if the Keychain or the biometric gate is unavailable (older macOS,
// no enrolled biometrics, non-macOS, or any runtime error) we fall back to the
// legacy on-disk key file at `app_data_dir/.cred_key` (chmod 0600 on unix), so
// the vault keeps working and the build always compiles. We never hard-fail the
// vault just because the secure path is missing.
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
const NONCE_LEN: usize = 12;

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.screenbuddy.vault";
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn acquire_key(app: &AppHandle) -> Result<[u8; 32], String> {
    match keychain_key() {
        Ok(key) => Ok(key),
        Err(e) => {
            // Never hard-fail: degrade to the legacy file key so the vault works.
            log::warn!("keychain master key unavailable ({e}); using file key fallback");
            file_key(app)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn acquire_key(app: &AppHandle) -> Result<[u8; 32], String> {
    file_key(app)
}

/// macOS: read (or create) the master key in the login Keychain, gated behind a
/// one-time biometric prompt. Any failure here is surfaced to the caller, which
/// falls back to the file key — so this never panics the vault.
#[cfg(target_os = "macos")]
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

/// macOS biometric gate. Succeeds (Ok) when the user authenticates; returns Err
/// when biometrics are unavailable or the user cancels — the caller then decides
/// whether to fall back. We allow device-password fallback so machines without an
/// enrolled fingerprint can still unlock via the OS auth sheet.
#[cfg(target_os = "macos")]
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
/// This is the cross-platform fallback (and the only path on non-macOS).
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

/// Encrypt + persist the user's own Anthropic API key (BYOK). Nonce is prepended
/// to the file, matching the vault layout. The key is never logged.
#[tauri::command]
pub fn set_anthropic_key(app: AppHandle, key: String) -> Result<(), String> {
    let plaintext = key.into_bytes();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher(&app)?
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|_| "anthropic key encrypt failed".to_string())?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);

    let path = app_data(&app)?.join(ANTHROPIC_KEY_FILE);
    fs::write(&path, blob).map_err(|e| format!("write anthropic key: {e}"))?;
    restrict_perms(&path)
}

/// Whether a BYOK Anthropic key is stored. NEVER returns the key itself; this is
/// a pure existence check (no decrypt, so it won't trigger the biometric gate).
#[tauri::command]
pub fn has_anthropic_key(app: AppHandle) -> bool {
    app_data(&app)
        .map(|d| d.join(ANTHROPIC_KEY_FILE).exists())
        .unwrap_or(false)
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
    let path = app_data(app).ok()?.join(ANTHROPIC_KEY_FILE);
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

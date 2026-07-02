use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::{Manager, State};

mod agent;
mod capture;
mod computer;
mod credentials;
mod permissions;
mod pinned;
mod remote;
mod video;

use computer::Computer;

/// Lazily-initialized input driver guarded by a mutex. `Computer` holds OS
/// handles (Enigo) that are created only after Accessibility is granted, so we
/// build it on first use (starts as `None`).
///
/// Enigo is `!Send`/`!Sync` because it stores a raw `CGEventSource` pointer.
/// Tauri managed state must be `Send + Sync`, so we wrap it in this newtype and
/// assert thread-safety manually.
pub struct ComputerState(Mutex<Option<Computer>>);

// SAFETY: every access to the inner `Computer` (and thus the Enigo handle and
// its raw CGEventSource pointer) goes through `self.0.lock()`, so it is only
// ever touched by one thread at a time. The CGEventSource is valid for the life
// of the process. Serialized, single-threaded-at-a-time access makes sharing
// this across Tauri's command thread-pool sound.
unsafe impl Send for ComputerState {}
unsafe impl Sync for ComputerState {}

/// Get a locked, initialized Computer, creating it on first use. The closure
/// runs with `&mut Computer` so callers don't deal with the lazy-init dance.
fn with_computer<T>(
    state: &ComputerState,
    f: impl FnOnce(&mut Computer) -> Result<T, String>,
) -> Result<T, String> {
    let mut guard = state.0.lock().map_err(|e| format!("computer lock poisoned: {e}"))?;
    if guard.is_none() {
        *guard = Some(Computer::new().map_err(|e| e.to_string())?);
    }
    let comp = guard.as_mut().unwrap();
    f(comp)
}

// Google OAuth configuration - these will be read from env or config
const GOOGLE_CLIENT_ID: &str = "671399747481-3bvjd46ug0r12thppt9s05humcjfrpbc.apps.googleusercontent.com";

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
    pub id_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserInfo {
    pub email: String,
    pub name: Option<String>,
    pub picture: Option<String>,
}

#[tauri::command]
async fn exchange_code_for_token(code: String, code_verifier: String, redirect_uri: String) -> Result<TokenResponse, String> {
    let client = reqwest::Client::new();

    let params = [
        ("code", code.as_str()),
        ("client_id", GOOGLE_CLIENT_ID),
        ("code_verifier", code_verifier.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("grant_type", "authorization_code"),
    ];

    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Failed to exchange code: {}", e))?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(format!("Token exchange failed: {}", error_text));
    }

    response
        .json::<TokenResponse>()
        .await
        .map_err(|e| format!("Failed to parse token response: {}", e))
}

#[tauri::command]
async fn get_user_info(access_token: String) -> Result<UserInfo, String> {
    let client = reqwest::Client::new();

    let response = client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|e| format!("Failed to get user info: {}", e))?;

    if !response.status().is_success() {
        return Err("Failed to get user info".to_string());
    }

    response
        .json::<UserInfo>()
        .await
        .map_err(|e| format!("Failed to parse user info: {}", e))
}

#[tauri::command]
async fn refresh_access_token(refresh_token: String) -> Result<TokenResponse, String> {
    let client = reqwest::Client::new();

    let params = [
        ("client_id", GOOGLE_CLIENT_ID),
        ("refresh_token", refresh_token.as_str()),
        ("grant_type", "refresh_token"),
    ];

    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("Failed to refresh token: {}", e))?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(format!("Token refresh failed: {}", error_text));
    }

    response
        .json::<TokenResponse>()
        .await
        .map_err(|e| format!("Failed to parse token response: {}", e))
}

/// A captured screenshot, ready for the model. JSON-serialized form of
/// `capture::Capture`. `sent_w`/`sent_h` are the dimensions the model sees and
/// MUST be fed back to the input driver (via `set_screenshot_size`) before any
/// click so model-space coordinates scale to the live screen correctly.
#[derive(Debug, Serialize, Deserialize)]
pub struct CaptureResult {
    pub jpeg_base64: String,
    pub sent_w: u32,
    pub sent_h: u32,
    pub screen_w: u32,
    pub screen_h: u32,
}

/// Capture the primary screen at the vision budget and return the base64 JPEG
/// plus the coordinate contract (sent_* / screen_*). Replaces the old
/// `capture_screen` / `capture_screen_compressed` (screenshots crate).
///
/// Also wires the coordinate contract: the freshly captured `sent_w`/`sent_h`
/// are pushed into the (lazily-initialized) Computer so any subsequent click
/// scales coordinates against the same image the model was shown.
#[tauri::command]
fn capture_now(state: State<'_, ComputerState>) -> Result<CaptureResult, String> {
    let cap = capture::take_screenshot().map_err(|e| e.to_string())?;

    // Wire the (sent_w, sent_h) contract into the input driver if it already
    // exists. Don't force-init the Computer here (that needs Accessibility);
    // the click commands re-assert the size on first use.
    if let Ok(mut guard) = state.0.lock() {
        if let Some(comp) = guard.as_mut() {
            comp.set_screenshot_size(cap.sent_w as i32, cap.sent_h as i32);
        }
    }

    Ok(CaptureResult {
        jpeg_base64: cap.jpeg_base64,
        sent_w: cap.sent_w,
        sent_h: cap.sent_h,
        screen_w: cap.screen_w,
        screen_h: cap.screen_h,
    })
}

/// Move the mouse to a model-space coordinate. Thin wrapper over the driver;
/// full action dispatch arrives with the agent loop later.
#[tauri::command]
fn move_mouse(state: State<'_, ComputerState>, x: i32, y: i32) -> Result<(), String> {
    with_computer(&state, |c| c.mouse_move(x, y).map_err(|e| e.to_string()))
}

/// Left-click at a model-space coordinate (no modifiers).
#[tauri::command]
fn left_click(state: State<'_, ComputerState>, x: i32, y: i32) -> Result<(), String> {
    with_computer(&state, |c| c.left_click(x, y, &[]).map_err(|e| e.to_string()))
}

/// Type unicode text at the current focus (layout-independent).
#[tauri::command]
fn type_text(state: State<'_, ComputerState>, text: String) -> Result<(), String> {
    with_computer(&state, |c| c.type_text(&text).map_err(|e| e.to_string()))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_shell::init())
    .plugin(tauri_plugin_oauth::init())
    .plugin(tauri_plugin_dialog::init())
    .plugin(tauri_plugin_notification::init())
    .invoke_handler(tauri::generate_handler![
        exchange_code_for_token,
        get_user_info,
        refresh_access_token,
        capture_now,
        move_mouse,
        left_click,
        type_text,
        agent::start_agent_task,
        agent::stop_agent_task,
        remote::start_remote_listener,
        remote::stop_remote_listener,
        credentials::cred_list,
        credentials::cred_add,
        credentials::cred_delete,
        credentials::set_anthropic_key,
        credentials::has_anthropic_key,
        credentials::clear_anthropic_key,
        credentials::validate_anthropic_key,
        permissions::check_permissions,
        permissions::request_screen_recording,
        permissions::request_accessibility,
        pinned::pinned_list,
        pinned::pinned_create,
        pinned::pinned_delete,
        pinned::pinned_get,
        video::extract_frames_from_video
    ])
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }

      // Input driver: lazily created on first use (after Accessibility is
      // granted), so the managed state starts as None.
      app.manage(ComputerState(Mutex::new(None)));
      // Agent loop cancellation state (no task running at startup).
      app.manage(agent::AgentState::default());
      // Remote-listener cancellation state (no listener running at startup).
      app.manage(remote::RemoteState::default());

      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

// window.rs — raise + focus the main window from a background state.
//
// A scheduled run fires while ScreenBuddy is in the background; the frontend
// invokes `bring_to_front` so the Accept/Snooze/Skip modal is actually seen.
//
// On macOS `set_focus()` alone does NOT pull a backgrounded *app* forward — it
// only focuses within an already-active app. To truly steal focus from the
// frontmost app we also activate the NSApplication
// (`activateIgnoringOtherApps:`), which is the app-level equivalent of a Dock
// click. The window ops below still run everywhere for the show/unminimize.
//
// WINDOWS: no equivalent block is needed and none should be added. Windows
// restricts which process may take the foreground, but tao (the windowing layer
// under Tauri) already handles that inside `set_focus()`: it calls
// `SetForegroundWindow` and, if that is refused, falls back to a documented hack
// that synthesizes a left-ALT press/release through `SendInput` to acquire
// foreground permission. Hand-rolling `SetForegroundWindow` +
// `AttachThreadInput` here would only duplicate that.
//
// The ALT synthesis is worth knowing about for THIS app specifically: we are a
// computer-use agent that drives the keyboard through `SendInput` ourselves, and
// a stray ALT keystroke lands in whatever window currently has focus — where it
// can open a menu bar. So `bring_to_front` should be called between agent
// actions, not concurrently with one. Today's only caller (a scheduled run
// firing, via the frontend) satisfies that.

use tauri::{AppHandle, Manager};

/// Raise, unminimize, show, and focus the main window, activating the app on
/// macOS so it becomes the frontmost application even from the background. On
/// Windows and Linux `set_focus()` is sufficient on its own (see module docs).
#[tauri::command]
pub fn bring_to_front(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;

    // Best-effort: a minimized window can't take focus, and a hidden one won't
    // paint. Ignore per-op errors so one failure doesn't abort the sequence.
    let _ = window.unminimize();
    let _ = window.show();

    // App-level activation must come first on macOS: focusing a window inside an
    // inactive app is a no-op until the app itself is frontmost.
    #[cfg(target_os = "macos")]
    unsafe {
        use objc::{class, msg_send, sel, sel_impl};
        let ns_app: *mut objc::runtime::Object = msg_send![class!(NSApplication), sharedApplication];
        if !ns_app.is_null() {
            let _: () = msg_send![ns_app, activateIgnoringOtherApps: true];
        }
    }

    window.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

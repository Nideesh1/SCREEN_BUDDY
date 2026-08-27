// permissions.rs — OS permission probe.
//
// Ported from cu-input-tests/src/bin/check_permission.rs. Two SEPARATE,
// independent macOS gates:
//   * Accessibility   — AXIsProcessTrusted()           (ApplicationServices)
//     Required to synthesize input (click/type). Screen capture alone does NOT
//     grant this.
//   * Screen Recording — CGPreflightScreenCaptureAccess() (CoreGraphics)
//     Required to read pixels off the screen.
//
// macOS only re-reads these at process launch, so a freshly-granted permission
// won't reflect here until the app is quit & relaunched. We only *probe* (no
// prompt) so this is cheap and side-effect-free.
//
// ---------------------------------------------------------------------------
// WINDOWS: both probes report `true`, and that is accurate rather than a stub.
// Windows has no TCC: any process may call SetWindowsHookEx-free SendInput and
// may read the screen via the Desktop Duplication / GDI paths xcap uses. There
// is no consent gate to query, so there is nothing to prompt for and nothing
// that can be "denied" in the macOS sense.
//
// What DOES restrict the agent on Windows is UIPI (User Interface Privilege
// Isolation), and it is not a permission — it is an integrity-level rule:
//
//   * A process may only send input to windows at its OWN integrity level or
//     LOWER. ScreenBuddy runs at medium integrity, like Explorer and every
//     normal user app, so it can drive browsers, Office, Slack and so on — the
//     overwhelming majority of what a run targets.
//   * It CANNOT drive windows owned by an elevated (high-integrity) process:
//     Task Manager, an admin PowerShell, most installers. Clicks and keystrokes
//     are silently discarded — SendInput still reports success, so this surfaces
//     as the agent believing it acted while the screen never changes.
//   * It can NEVER drive a UAC consent dialog. Those render on a separate secure
//     desktop that no user-mode process can reach, elevated or not.
//
// We deliberately do NOT probe for elevation and surface it as a permission.
// Doing so would frame "run ScreenBuddy as administrator" as the fix, and
// handing an autonomous computer-use agent admin rights to dodge a silent-click
// problem is a bad trade. The honest guidance is the opposite: keep it
// unelevated and keep elevated windows out of a run's path. If we later want to
// help users diagnose the silent-discard case, the right shape is a post-action
// "screen did not change" heuristic in the agent loop, not a permission flag.
// ---------------------------------------------------------------------------

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct PermissionStatus {
    pub accessibility: bool,
    /// Serialized as `screen_recording`; Tauri camelCases it to `screenRecording`
    /// for the frontend.
    pub screen_recording: bool,
}

#[cfg(target_os = "macos")]
mod sys {
    use std::os::raw::c_void;

    pub type CFTypeRef = *const c_void;
    pub type CFDictionaryRef = *const c_void;
    pub type CFStringRef = *const c_void;
    pub type CFAllocatorRef = *const c_void;
    pub type CFIndex = isize;

    // AXIsProcessTrusted lives in ApplicationServices (the AX* API).
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        pub fn AXIsProcessTrusted() -> bool;

        // Prompting variant: pass a dict with kAXTrustedCheckOptionPrompt = true
        // to make macOS surface the "open System Settings" prompt and list us.
        pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;

        // CFStringRef constant key for the prompt option.
        pub static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    // CGPreflightScreenCaptureAccess lives in CoreGraphics. Preflight is the
    // non-prompting check ("do we already have it?"). CGRequestScreenCaptureAccess
    // is the prompting/registering variant that adds us to the Screen Recording
    // list the first time it runs.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGPreflightScreenCaptureAccess() -> bool;
        pub fn CGRequestScreenCaptureAccess() -> bool;
    }

    // CoreFoundation: build the single-entry options dictionary by hand.
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub static kCFBooleanTrue: CFTypeRef;
        pub static kCFAllocatorDefault: CFAllocatorRef;
        pub static kCFTypeDictionaryKeyCallBacks: c_void;
        pub static kCFTypeDictionaryValueCallBacks: c_void;

        pub fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: CFIndex,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;
        pub fn CFRelease(cf: CFTypeRef);
    }
}

/// Probe the two macOS permissions the agent needs. On non-macOS targets both
/// report `true` so the gate never blocks elsewhere.
#[tauri::command]
pub fn check_permissions() -> PermissionStatus {
    #[cfg(target_os = "macos")]
    {
        let accessibility = unsafe { sys::AXIsProcessTrusted() };
        let screen_recording = unsafe { sys::CGPreflightScreenCaptureAccess() };
        PermissionStatus {
            accessibility,
            screen_recording,
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        PermissionStatus {
            accessibility: true,
            screen_recording: true,
        }
    }
}

/// Actively request Screen Recording. The first call adds ScreenBuddy to the
/// Screen Recording list and pops the system prompt; the returned bool reflects
/// whether access is already granted (macOS still requires a relaunch before
/// `check_permissions` flips to true).
#[tauri::command]
pub fn request_screen_recording() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { sys::CGRequestScreenCaptureAccess() }
    }

    // Nothing to request: screen capture needs no consent off macOS. Returns
    // `true` to agree with `check_permissions`, which already reports it
    // granted — the old `false` claimed the opposite of the probe.
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Actively request Accessibility. Builds the `{ kAXTrustedCheckOptionPrompt:
/// true }` dictionary so macOS surfaces the "open System Settings" prompt and
/// lists the app, then returns whether we're already trusted.
#[tauri::command]
pub fn request_accessibility() -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::raw::c_void;
        unsafe {
            let keys: [*const c_void; 1] = [sys::kAXTrustedCheckOptionPrompt];
            let values: [*const c_void; 1] = [sys::kCFBooleanTrue];
            let options = sys::CFDictionaryCreate(
                sys::kCFAllocatorDefault,
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &sys::kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
                &sys::kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
            );
            let trusted = sys::AXIsProcessTrustedWithOptions(options);
            if !options.is_null() {
                sys::CFRelease(options);
            }
            trusted
        }
    }

    // Nothing to request: synthesizing input needs no consent off macOS (UIPI
    // is an integrity-level rule, not a grantable permission — see module docs).
    // Returns `true` to agree with `check_permissions`.
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

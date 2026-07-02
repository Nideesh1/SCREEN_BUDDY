// permissions.rs — macOS permission probe.
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

    #[cfg(not(target_os = "macos"))]
    {
        false
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

    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

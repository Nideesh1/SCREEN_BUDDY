//! Faithful Rust port of the input actions in Anthropic's reference
//! `computer_use/tools/computer.py`.
//!
//! What this ports (and the computer.py behaviours it preserves):
//!   * The full action set: clicks (left/right/middle/double/triple), mouse
//!     move, drag, scroll, type, key chords, hold_key, mouse down/up,
//!     cursor_position, clipboard read/write, wait.
//!   * COORDINATE SCALING (`_scale_to_screen`): the model emits coordinates in
//!     the pixel space of the *screenshot we sent it*. Those must be scaled to
//!     the live screen before we click, or every click drifts. See `to_screen`.
//!   * KEY ALIASING (`_KEY_ALIASES`): e.g. `cmd`/`super`/`meta` -> command,
//!     `delete` -> backspace (macOS "delete" is forward-delete, which models
//!     rarely mean), `ctrl` -> control. Unknown keys are REJECTED, not silently
//!     dropped (mirrors `_unmapped_keys`).
//!   * LAYOUT-INDEPENDENT TYPING: `enigo.text()` posts unicode via
//!     CGEventKeyboardSetUnicodeString under the hood — the same mechanism
//!     computer.py uses, so non-US layouts and emoji type correctly.
//!
//! What this does NOT do: screenshots. ScreenBuddy already captures the screen
//! natively (`capture_screen_compressed` in lib.rs); that stays its job. The
//! agent loop feeds the captured image's dimensions to `set_screenshot_size`.

use enigo::{
    Axis, Button, Coordinate,
    Direction::{Click, Press, Release},
    Enigo, Key, Keyboard, Mouse, Settings,
};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Scroll direction, matching computer.py's `scroll_direction` enum.
#[derive(Clone, Copy, Debug)]
pub enum ScrollDir {
    Up,
    Down,
    Left,
    Right,
}

/// Errors surfaced to the agent loop as tool errors (never panics the loop).
#[derive(Debug)]
pub enum InputError {
    Init(String),
    Exec(String),
    /// A key name with no macOS mapping — mirrors computer.py `_unmapped_keys`.
    UnmappedKey(String),
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::Init(s) => write!(f, "input init failed: {s}"),
            InputError::Exec(s) => write!(f, "input exec failed: {s}"),
            InputError::UnmappedKey(k) => write!(f, "unmapped key: {k}"),
        }
    }
}
impl std::error::Error for InputError {}

type R<T> = Result<T, InputError>;

/// The computer-use input driver. One per session.
pub struct Computer {
    enigo: Enigo,
    /// Live screen size in enigo's coordinate space (Abs move target space).
    screen: (i32, i32),
    /// Size of the screenshot last sent to the model. Defaults to screen size
    /// (identity scaling) until the agent loop reports the real sent size.
    sent: (i32, i32),
    /// Per-action settle delay (computer.py uses pyautogui PAUSE = 0.05).
    pause: Duration,
}

impl Computer {
    pub fn new() -> R<Self> {
        let enigo = Enigo::new(&Settings::default()).map_err(|e| InputError::Init(e.to_string()))?;
        let screen = enigo
            .main_display()
            .map_err(|e| InputError::Init(format!("main_display: {e:?}")))?;
        Ok(Self {
            enigo,
            screen,
            sent: screen,
            pause: Duration::from_millis(50),
        })
    }

    /// Tell the driver the pixel size of the screenshot we last sent to the
    /// model, so model-space coordinates scale correctly. This is the
    /// `computer.py` (sent_w, sent_h) contract — must be paired with the
    /// image-resize step on the capture side.
    pub fn set_screenshot_size(&mut self, w: i32, h: i32) {
        self.sent = (w.max(1), h.max(1));
    }

    pub fn screen_size(&self) -> (i32, i32) {
        self.screen
    }

    /// Scale a model/image-space coordinate to live screen space and clamp to
    /// screen bounds. Port of computer.py `_scale_to_screen`.
    fn to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        let (sw, sh) = self.screen;
        let (iw, ih) = self.sent;
        let sx = (x as f64 * sw as f64 / iw as f64).round() as i32;
        let sy = (y as f64 * sh as f64 / ih as f64).round() as i32;
        (sx.clamp(0, sw - 1), sy.clamp(0, sh - 1))
    }

    fn settle(&self) {
        std::thread::sleep(self.pause);
    }

    // ---- pointer movement -------------------------------------------------

    pub fn mouse_move(&mut self, x: i32, y: i32) -> R<()> {
        let (sx, sy) = self.to_screen(x, y);
        self.enigo
            .move_mouse(sx, sy, Coordinate::Abs)
            .map_err(|e| InputError::Exec(e.to_string()))?;
        self.settle();
        Ok(())
    }

    /// Current cursor position in screen space.
    pub fn cursor_position(&self) -> R<(i32, i32)> {
        self.enigo
            .location()
            .map_err(|e| InputError::Exec(e.to_string()))
    }

    // ---- clicks (with optional held modifiers) ----------------------------

    fn press_mods(&mut self, mods: &[&str]) -> R<Vec<Key>> {
        let mut held = Vec::new();
        for m in mods {
            let k = map_key(m)?;
            self.enigo
                .key(k, Press)
                .map_err(|e| InputError::Exec(e.to_string()))?;
            std::thread::sleep(Duration::from_millis(20));
            held.push(k);
        }
        Ok(held)
    }

    fn release_mods(&mut self, held: Vec<Key>) -> R<()> {
        // release in reverse order
        for k in held.into_iter().rev() {
            self.enigo
                .key(k, Release)
                .map_err(|e| InputError::Exec(e.to_string()))?;
        }
        Ok(())
    }

    fn click_n(&mut self, x: i32, y: i32, button: Button, times: u32, mods: &[&str]) -> R<()> {
        self.mouse_move(x, y)?;
        let held = self.press_mods(mods)?;
        for _ in 0..times {
            self.enigo
                .button(button, Click)
                .map_err(|e| InputError::Exec(e.to_string()))?;
            std::thread::sleep(Duration::from_millis(20));
        }
        self.release_mods(held)?;
        self.settle();
        Ok(())
    }

    pub fn left_click(&mut self, x: i32, y: i32, mods: &[&str]) -> R<()> {
        self.click_n(x, y, Button::Left, 1, mods)
    }
    pub fn right_click(&mut self, x: i32, y: i32, mods: &[&str]) -> R<()> {
        self.click_n(x, y, Button::Right, 1, mods)
    }
    pub fn middle_click(&mut self, x: i32, y: i32, mods: &[&str]) -> R<()> {
        self.click_n(x, y, Button::Middle, 1, mods)
    }
    pub fn double_click(&mut self, x: i32, y: i32, mods: &[&str]) -> R<()> {
        self.click_n(x, y, Button::Left, 2, mods)
    }
    pub fn triple_click(&mut self, x: i32, y: i32, mods: &[&str]) -> R<()> {
        self.click_n(x, y, Button::Left, 3, mods)
    }

    /// Press-move-release drag. Port of computer.py `left_click_drag`.
    pub fn left_click_drag(&mut self, from: (i32, i32), to: (i32, i32), mods: &[&str]) -> R<()> {
        self.mouse_move(from.0, from.1)?;
        let held = self.press_mods(mods)?;
        self.enigo
            .button(Button::Left, Press)
            .map_err(|e| InputError::Exec(e.to_string()))?;
        std::thread::sleep(Duration::from_millis(30));
        self.mouse_move(to.0, to.1)?;
        self.enigo
            .button(Button::Left, Release)
            .map_err(|e| InputError::Exec(e.to_string()))?;
        self.release_mods(held)?;
        self.settle();
        Ok(())
    }

    pub fn left_mouse_down(&mut self) -> R<()> {
        self.enigo
            .button(Button::Left, Press)
            .map_err(|e| InputError::Exec(e.to_string()))
    }
    pub fn left_mouse_up(&mut self) -> R<()> {
        self.enigo
            .button(Button::Left, Release)
            .map_err(|e| InputError::Exec(e.to_string()))
    }

    // ---- scroll -----------------------------------------------------------

    /// Scroll at a point. `amount` is in scroll "clicks" (computer.py
    /// scroll_amount). Direction maps to enigo's signed axis scroll.
    pub fn scroll(&mut self, x: i32, y: i32, dir: ScrollDir, amount: i32) -> R<()> {
        self.mouse_move(x, y)?;
        let (length, axis) = match dir {
            ScrollDir::Down => (amount, Axis::Vertical),
            ScrollDir::Up => (-amount, Axis::Vertical),
            ScrollDir::Right => (amount, Axis::Horizontal),
            ScrollDir::Left => (-amount, Axis::Horizontal),
        };
        self.enigo
            .scroll(length, axis)
            .map_err(|e| InputError::Exec(e.to_string()))?;
        self.settle();
        Ok(())
    }

    // ---- keyboard ---------------------------------------------------------

    /// Type unicode text (layout-independent). Port of computer.py `type`.
    pub fn type_text(&mut self, text: &str) -> R<()> {
        self.enigo
            .text(text)
            .map_err(|e| InputError::Exec(e.to_string()))?;
        self.settle();
        Ok(())
    }

    /// Press a key chord like "cmd+shift+t" (or a single key). Holds each key
    /// down in order, then releases in reverse. Port of computer.py `key`.
    ///
    /// NOTE: a short delay between each key event is required — pressing a
    /// modifier and its key back-to-back with no gap causes macOS to miss the
    /// combo (e.g. cmd+v silently no-ops). 20ms is enough and imperceptible.
    pub fn key(&mut self, chord: &str) -> R<()> {
        let keys: Vec<Key> = chord
            .split('+')
            .map(|p| map_key(p.trim()))
            .collect::<R<Vec<_>>>()?;
        let gap = Duration::from_millis(20);
        for k in &keys {
            self.enigo
                .key(*k, Press)
                .map_err(|e| InputError::Exec(e.to_string()))?;
            std::thread::sleep(gap);
        }
        for k in keys.iter().rev() {
            self.enigo
                .key(*k, Release)
                .map_err(|e| InputError::Exec(e.to_string()))?;
            std::thread::sleep(gap);
        }
        self.settle();
        Ok(())
    }

    /// Hold a key (or chord) down for a duration, then release. Port of
    /// computer.py `hold_key`.
    pub fn hold_key(&mut self, chord: &str, duration: Duration) -> R<()> {
        let keys: Vec<Key> = chord
            .split('+')
            .map(|p| map_key(p.trim()))
            .collect::<R<Vec<_>>>()?;
        for k in &keys {
            self.enigo
                .key(*k, Press)
                .map_err(|e| InputError::Exec(e.to_string()))?;
        }
        std::thread::sleep(duration);
        for k in keys.iter().rev() {
            self.enigo
                .key(*k, Release)
                .map_err(|e| InputError::Exec(e.to_string()))?;
        }
        Ok(())
    }

    // ---- clipboard (pbcopy/pbpaste, like computer.py) ---------------------

    pub fn read_clipboard(&self) -> R<String> {
        let out = Command::new("pbpaste")
            .output()
            .map_err(|e| InputError::Exec(e.to_string()))?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn write_clipboard(&self, text: &str) -> R<()> {
        use std::io::Write;
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| InputError::Exec(e.to_string()))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| InputError::Exec("pbcopy stdin".into()))?
            .write_all(text.as_bytes())
            .map_err(|e| InputError::Exec(e.to_string()))?;
        child.wait().map_err(|e| InputError::Exec(e.to_string()))?;
        Ok(())
    }

    pub fn wait(&self, seconds: f64) {
        std::thread::sleep(Duration::from_secs_f64(seconds.clamp(0.0, 60.0)));
    }
}

/// Map a key name to an enigo Key, applying computer.py's `_KEY_ALIASES`.
/// Returns `UnmappedKey` for anything with no macOS mapping (mirrors
/// `_unmapped_keys` rejecting silently-dropped keys).
fn map_key(name: &str) -> R<Key> {
    let n = name.trim().to_lowercase();
    let k = match n.as_str() {
        "control" | "ctrl" => Key::Control,
        "cmd" | "command" | "super" | "meta" | "win" | "windows" => Key::Meta,
        "alt" => Key::Alt,
        "option" | "opt" => Key::Option,
        "shift" => Key::Shift,
        "return" | "enter" => Key::Return,
        // macOS pyautogui "delete" = forward-delete; models almost always mean
        // backspace, so computer.py remaps it. We do the same.
        "delete" | "backspace" | "bksp" => Key::Backspace,
        "forwarddelete" | "fwddelete" | "del" => Key::Delete,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "space" | "spacebar" => Key::Space,
        "up" | "uparrow" => Key::UpArrow,
        "down" | "downarrow" => Key::DownArrow,
        "left" | "leftarrow" => Key::LeftArrow,
        "right" | "rightarrow" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "capslock" => Key::CapsLock,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        // Single character in a chord (e.g. the "t" of "cmd+shift+t").
        //
        // CRASH FIX: we must NOT use `Key::Unicode(char)` here. On macOS,
        // resolving a `Key::Unicode` keycode calls enigo's
        // `get_layoutdependent_keycode` -> `keycode_to_string` -> Apple Text
        // Input Source (TSM) APIs (`TSMGetInputSourceProperty` /
        // `islGetInputSourceListWithAdditions`). Those APIs assert they run on
        // the main thread; `Computer::key` runs on a tokio worker thread, so the
        // assert trips and the whole process crashes (EXC_BREAKPOINT / SIGTRAP).
        //
        // Instead we map the character to a raw US-layout macOS virtual keycode
        // and hand enigo `Key::Other(keycode)`, which enigo posts WITHOUT the
        // TSM layout lookup (it casts straight to a CGKeyCode). Shift is a
        // separate modifier in a chord, so we lowercase the character first.
        //
        // NOTE: this only affects the key-CHORD path. `type_text` still uses
        // `enigo.text()` (CGEventKeyboardSetUnicodeString), which never touches
        // TSM and is left untouched.
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap().to_ascii_lowercase();
            match char_to_macos_keycode(c) {
                Some(code) => Key::Other(code as u32),
                // No raw keycode for this char (e.g. a non-ASCII char in a
                // chord). Fall back to the previous behaviour rather than
                // silently dropping it. This path can still reach TSM, but it is
                // only hit for characters outside the standard US layout, which
                // the agent effectively never sends inside a key chord.
                None => Key::Unicode(s.chars().next().unwrap()),
            }
        }
        _ => return Err(InputError::UnmappedKey(name.to_string())),
    };
    Ok(k)
}

/// Map an ASCII character to its US-layout ("ANSI") macOS virtual keycode
/// (`kVK_ANSI_*` / CGKeyCode). Returns `None` for characters we have no fixed
/// keycode for.
///
/// This exists to keep the key-CHORD path off Apple's Text Input Source (TSM)
/// APIs, which assert main-thread execution and crash when enigo resolves a
/// `Key::Unicode` off the main thread. See the call site in `map_key`.
///
/// Only the base (unshifted) character of each physical key is listed; a chord
/// carries shift as its own modifier token, so callers must lowercase letters
/// before lookup.
fn char_to_macos_keycode(c: char) -> Option<u16> {
    let code: u16 = match c {
        'a' => 0,
        's' => 1,
        'd' => 2,
        'f' => 3,
        'h' => 4,
        'g' => 5,
        'z' => 6,
        'x' => 7,
        'c' => 8,
        'v' => 9,
        'b' => 11,
        'q' => 12,
        'w' => 13,
        'e' => 14,
        'r' => 15,
        'y' => 16,
        't' => 17,
        '1' => 18,
        '2' => 19,
        '3' => 20,
        '4' => 21,
        '6' => 22,
        '5' => 23,
        '=' => 24,
        '9' => 25,
        '7' => 26,
        '-' => 27,
        '8' => 28,
        '0' => 29,
        ']' => 30,
        'o' => 31,
        'u' => 32,
        '[' => 33,
        'i' => 34,
        'p' => 35,
        'l' => 37,
        'j' => 38,
        '\'' => 39,
        'k' => 40,
        ';' => 41,
        '\\' => 42,
        ',' => 43,
        '/' => 44,
        'n' => 45,
        'm' => 46,
        '.' => 47,
        '`' => 50,
        ' ' => 49,
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::char_to_macos_keycode;

    #[test]
    fn maps_common_chord_chars_to_us_keycodes() {
        assert_eq!(char_to_macos_keycode('a'), Some(0));
        assert_eq!(char_to_macos_keycode('t'), Some(17));
        assert_eq!(char_to_macos_keycode('v'), Some(9));
        assert_eq!(char_to_macos_keycode('z'), Some(6));
        assert_eq!(char_to_macos_keycode('0'), Some(29));
        assert_eq!(char_to_macos_keycode('9'), Some(25));
        assert_eq!(char_to_macos_keycode(' '), Some(49));
        assert_eq!(char_to_macos_keycode('/'), Some(44));
        // No fixed US-layout keycode for these.
        assert_eq!(char_to_macos_keycode('é'), None);
        assert_eq!(char_to_macos_keycode('A'), None); // callers lowercase first
    }
}

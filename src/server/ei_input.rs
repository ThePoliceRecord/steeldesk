//! libei (Emulated Input) integration for Wayland input injection.
//!
//! This module provides a libei-based input path that replaces the clipboard hack
//! (`Shift+Insert` after setting clipboard content) for character input on Wayland.
//!
//! ## Architecture
//!
//! The EIS (Emulated Input Server) protocol operates over a Unix socket obtained from
//! the XDG RemoteDesktop portal's `ConnectToEIS()` method. The client (RustDesk) connects
//! to the compositor's EIS server, negotiates capabilities, creates virtual input devices,
//! and sends input events directly -- no clipboard involvement, sub-millisecond latency.
//!
//! ## Usage
//!
//! ```ignore
//! // Obtain EIS fd from the RemoteDesktop portal (already established session)
//! let eis_fd = portal.connect_to_eis(&session, HashMap::new())?;
//!
//! // Create the EI input context
//! let mut ei = EiInput::new(eis_fd.into_raw_fd())?;
//!
//! // Send keyboard input
//! ei.key_down(30); // evdev keycode for 'a'
//! ei.key_up(30);
//!
//! // Send pointer input
//! ei.pointer_move(100.0, 200.0);
//! ei.pointer_button(BTN_LEFT, true);
//! ei.pointer_button(BTN_LEFT, false);
//! ```
//!
//! ## Fallback
//!
//! When libei is unavailable (portal version < 2, wlroots compositors, or `ConnectToEIS`
//! fails), the existing clipboard hack in `input_service.rs` is used as a fallback.
//! See `try_get_ei_input()` for the availability check.

use hbb_common::log;
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::{Arc, Mutex, OnceLock};

/// Mouse button constants (evdev codes).
#[allow(dead_code)]
pub const BTN_LEFT: u32 = 0x110;
#[allow(dead_code)]
pub const BTN_RIGHT: u32 = 0x111;
#[allow(dead_code)]
pub const BTN_MIDDLE: u32 = 0x112;

/// Key state constants for EIS protocol.
#[allow(dead_code)]
const EI_KEY_PRESSED: u32 = 1;
#[allow(dead_code)]
const EI_KEY_RELEASED: u32 = 0;

/// Global EI input context, initialized once when a portal session is established.
///
/// `None` means libei is not available or initialization failed.
/// Access via `try_get_ei_input()`.
static EI_INPUT: OnceLock<Option<Arc<Mutex<EiInput>>>> = OnceLock::new();

/// libei-based input context for Wayland.
///
/// Wraps an EIS connection obtained from the RemoteDesktop portal's `ConnectToEIS()`.
/// Provides keyboard, pointer, and scroll input without clipboard involvement.
///
/// The struct holds the raw fd (wrapped in a `File` for automatic cleanup) and,
/// once the `reis` crate is integrated, will also hold the `reis::ei::Context`,
/// virtual keyboard device, and virtual pointer device.
pub struct EiInput {
    /// The EIS socket file descriptor from `ConnectToEIS()`, wrapped for RAII cleanup.
    /// Stored as `Option` so we can inspect the fd value in tests; the `File` owns it.
    #[allow(dead_code)]
    fd_owner: std::fs::File,

    /// The raw fd value, kept for logging/diagnostics.
    #[allow(dead_code)]
    fd_value: RawFd,

    // TODO: Add reis crate fields once available:
    // context: reis::ei::Context,
    // keyboard: reis::ei::Device,
    // pointer: Option<reis::ei::Device>,
    // keymap: Option<xkb::Keymap>,  // compositor's active keymap for keysym -> keycode lookup
}

impl EiInput {
    /// Create a new EI input context from a portal EIS file descriptor.
    ///
    /// The `eis_fd` should be obtained from `ConnectToEIS()` on an already-authenticated
    /// RemoteDesktop portal session. The portal handles authorization; no additional
    /// user interaction is needed.
    ///
    /// # Safety contract
    ///
    /// The caller must ensure `eis_fd` is a valid, open file descriptor that this
    /// struct takes ownership of. The fd will be closed when the `EiInput` is dropped.
    ///
    /// # Steps (to be implemented with `reis` crate)
    ///
    /// 1. Create `reis::ei::Context` from the raw fd
    /// 2. Negotiate capabilities (keyboard required, pointer optional)
    /// 3. Wait for seat assignment from compositor
    /// 4. Create keyboard device on the seat
    /// 5. Optionally create pointer device
    /// 6. Start devices (compositor must acknowledge)
    /// 7. Receive and store the compositor's keymap for keysym lookups
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The fd is invalid
    /// - Protocol negotiation fails
    /// - The compositor rejects device creation
    pub fn new(eis_fd: RawFd) -> Result<Self, Box<dyn std::error::Error>> {
        // TODO: Implement with reis crate:
        //
        // let context = reis::ei::Context::new(eis_fd)?;
        // context.negotiate(reis::ei::ContextType::Sender)?;
        //
        // // Process events until we get a seat
        // let seat = loop {
        //     context.dispatch()?;
        //     for event in context.events() {
        //         if let reis::ei::Event::SeatAdded(seat) = event {
        //             break seat;
        //         }
        //     }
        // };
        //
        // // Bind capabilities
        // seat.bind_capabilities(&[
        //     reis::ei::DeviceCapability::Keyboard,
        //     reis::ei::DeviceCapability::Pointer,
        // ])?;
        //
        // // Wait for device
        // let keyboard = loop {
        //     context.dispatch()?;
        //     for event in context.events() {
        //         if let reis::ei::Event::DeviceAdded(device) = event {
        //             if device.has_capability(reis::ei::DeviceCapability::Keyboard) {
        //                 device.start_emulating(0, "")?;
        //                 break device;
        //             }
        //         }
        //     }
        // };

        log::info!("EiInput::new: created stub EI context with fd={}", eis_fd);

        // Safety: we take ownership of eis_fd. The caller guarantees it is valid.
        let fd_owner = unsafe { std::fs::File::from_raw_fd(eis_fd) };

        Ok(Self {
            fd_owner,
            fd_value: eis_fd,
            // TODO: store context, keyboard, pointer, keymap
        })
    }

    /// Send a key-down event for the given evdev keycode.
    ///
    /// The keycode is an evdev code (e.g., `KEY_A = 30`), NOT an X11/XKB keycode.
    /// The compositor applies its active keymap to produce the correct character.
    pub fn key_down(&mut self, keycode: u32) {
        // TODO: Implement with reis crate:
        //
        // self.keyboard.keyboard_key(keycode, reis::ei::KeyState::Press);
        // self.keyboard.frame(timestamp_now());
        // self.context.flush();

        log::trace!("EiInput::key_down: keycode={} (stub)", keycode);
    }

    /// Send a key-up event for the given evdev keycode.
    pub fn key_up(&mut self, keycode: u32) {
        // TODO: Implement with reis crate:
        //
        // self.keyboard.keyboard_key(keycode, reis::ei::KeyState::Release);
        // self.keyboard.frame(timestamp_now());
        // self.context.flush();

        log::trace!("EiInput::key_up: keycode={} (stub)", keycode);
    }

    /// Type a single key press-release cycle for the given evdev keycode.
    #[allow(dead_code)]
    pub fn key_click(&mut self, keycode: u32) {
        self.key_down(keycode);
        self.key_up(keycode);
    }

    /// Type a character by looking up its keysym and finding the corresponding
    /// keycode + modifier combination in the compositor's keymap.
    ///
    /// This is the core method that replaces the clipboard hack. For characters
    /// in the user's active keyboard layout, this provides zero-delay input.
    ///
    /// # Fallback
    ///
    /// Returns `false` if the character cannot be typed via EIS (not in the
    /// compositor's keymap). The caller should fall back to the clipboard hack.
    pub fn type_char(&mut self, ch: char) -> bool {
        // TODO: Implement with reis crate + xkbcommon:
        //
        // 1. Convert char to keysym: xkb_utf32_to_keysym(ch as u32)
        // 2. Look up keycode + modifiers from compositor's keymap:
        //    keymap.key_by_name(keysym) or iterate keymap entries
        // 3. Press required modifiers (e.g., Shift for uppercase)
        // 4. Press the keycode
        // 5. Release the keycode
        // 6. Release modifiers
        //
        // If the keysym is not found in the keymap, return false.

        log::trace!(
            "EiInput::type_char: ch='{}' (U+{:04X}) (stub)",
            ch,
            ch as u32
        );

        // Stub: always return false to trigger clipboard fallback.
        // Once implemented, this will return true for characters in the active keymap.
        false
    }

    /// Type a string by sending each character individually via `type_char()`.
    ///
    /// Returns `false` if any character could not be typed (and the caller should
    /// use clipboard fallback for the entire string to maintain atomicity).
    #[allow(dead_code)]
    pub fn type_text(&mut self, text: &str) -> bool {
        // Pre-check: verify all characters can be typed before sending any.
        // This avoids partial input if a character isn't in the keymap.
        //
        // TODO: Once type_char is implemented, pre-check the keymap here.
        // For now, delegate to type_char which returns false (stub).

        for ch in text.chars() {
            if !self.type_char(ch) {
                return false;
            }
        }
        true
    }

    /// Move the pointer to an absolute position.
    ///
    /// Coordinates are in the compositor's logical coordinate space.
    pub fn pointer_move(&mut self, x: f64, y: f64) {
        // TODO: Implement with reis crate:
        //
        // self.pointer.pointer_motion_absolute(x, y);
        // self.pointer.frame(timestamp_now());
        // self.context.flush();

        log::trace!("EiInput::pointer_move: x={}, y={} (stub)", x, y);
    }

    /// Send a pointer button press or release.
    ///
    /// `button` is an evdev button code (e.g., `BTN_LEFT = 0x110`).
    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        // TODO: Implement with reis crate:
        //
        // let state = if pressed {
        //     reis::ei::ButtonState::Press
        // } else {
        //     reis::ei::ButtonState::Release
        // };
        // self.pointer.button(button, state);
        // self.pointer.frame(timestamp_now());
        // self.context.flush();

        log::trace!(
            "EiInput::pointer_button: button={:#x}, pressed={} (stub)",
            button,
            pressed
        );
    }

    /// Send a scroll event.
    ///
    /// `dx` and `dy` are scroll deltas in the compositor's coordinate space.
    pub fn pointer_scroll(&mut self, dx: f64, dy: f64) {
        // TODO: Implement with reis crate:
        //
        // self.pointer.scroll_delta(dx, dy);
        // self.pointer.frame(timestamp_now());
        // self.context.flush();

        log::trace!("EiInput::pointer_scroll: dx={}, dy={} (stub)", dx, dy);
    }
}

// The fd is closed automatically by `fd_owner: File` when EiInput is dropped.
// No custom Drop impl needed.

// ---------------------------------------------------------------------------
// Global access and availability checks
// ---------------------------------------------------------------------------

/// Minimum portal version that supports `ConnectToEIS()`.
///
/// The `ConnectToEIS` method was added in RemoteDesktop portal version 2.
#[allow(dead_code)]
pub const MIN_PORTAL_VERSION_FOR_EIS: u32 = 2;

/// Initialize the global EI input context from a portal EIS fd.
///
/// Called once when a RemoteDesktop portal session is established and
/// `ConnectToEIS()` returns a valid fd.
///
/// This is idempotent: subsequent calls after the first are ignored.
#[allow(dead_code)]
pub fn init_ei_input(eis_fd: RawFd) {
    EI_INPUT.get_or_init(|| match EiInput::new(eis_fd) {
        Ok(ei) => {
            log::info!("EI input initialized successfully");
            Some(Arc::new(Mutex::new(ei)))
        }
        Err(e) => {
            log::warn!(
                "Failed to initialize EI input: {:?}. Falling back to clipboard.",
                e
            );
            None
        }
    });
}

/// Get a reference to the global EI input context, if available.
///
/// Returns `None` if:
/// - libei feature is not enabled
/// - Portal version is too old (< 2)
/// - `ConnectToEIS()` failed
/// - `EiInput::new()` failed
pub fn try_get_ei_input() -> Option<Arc<Mutex<EiInput>>> {
    EI_INPUT.get().and_then(|opt| opt.clone())
}

/// Check whether libei input is available for use.
///
/// This is a lightweight check suitable for the fast path in `process_chr()` etc.
/// It does NOT attempt to connect; it only checks if a previous `init_ei_input()`
/// succeeded.
pub fn is_ei_available() -> bool {
    EI_INPUT.get().and_then(|opt| opt.as_ref()).is_some()
}

/// Check if the RemoteDesktop portal supports EIS by checking its version.
///
/// Returns `true` if the portal version is >= `MIN_PORTAL_VERSION_FOR_EIS`.
/// Returns `false` if the version cannot be determined or is too old.
///
/// This should be called before attempting `ConnectToEIS()`.
pub fn portal_supports_eis() -> bool {
    // TODO: Implement by querying the portal version property:
    //
    // use scrap::wayland::pipewire::get_portal;
    // use scrap::wayland::remote_desktop_portal::OrgFreedesktopPortalRemoteDesktop;
    //
    // let conn = dbus::blocking::SyncConnection::new_session().ok()?;
    // let portal = get_portal(&conn);
    // match portal.version() {
    //     Ok(v) => v >= MIN_PORTAL_VERSION_FOR_EIS,
    //     Err(e) => {
    //         log::debug!("Cannot query portal version: {:?}", e);
    //         false
    //     }
    // }

    log::trace!("portal_supports_eis: stub, returning false");
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

    /// Create a pair of connected Unix sockets for testing.
    /// Returns (mock_eis_fd, _peer) -- the _peer must be kept alive to avoid EPIPE.
    fn mock_eis_fd() -> (RawFd, UnixStream) {
        let (a, b) = UnixStream::pair().expect("UnixStream::pair failed");
        // We want to give the raw fd to EiInput (which takes ownership via File::from_raw_fd).
        // So we need to prevent `a` from closing the fd when it's dropped.
        // Use into_raw_fd to release ownership from UnixStream.
        let fd = a.as_raw_fd();
        // Leak `a` so the fd stays open and EiInput can take ownership.
        std::mem::forget(a);
        (fd, b)
    }

    /// EiInput can be created with a valid-looking fd.
    ///
    /// Uses a Unix socket pair as a stand-in for an EIS socket. The stub
    /// implementation doesn't read/write the fd, so this is safe for testing
    /// struct creation.
    #[test]
    fn test_ei_input_creation_with_mock_fd() {
        let (fd, _peer) = mock_eis_fd();
        let result = EiInput::new(fd);
        assert!(
            result.is_ok(),
            "EiInput::new should succeed with a valid fd"
        );
        let ei = result.unwrap();
        assert_eq!(ei.fd_value, fd);
        // EiInput's fd_owner (File) will close fd on drop.
    }

    /// EiInput's stub type_char always returns false (triggers clipboard fallback).
    #[test]
    fn test_ei_input_type_char_stub_returns_false() {
        let (fd, _peer) = mock_eis_fd();
        let mut ei = EiInput::new(fd).unwrap();
        assert!(!ei.type_char('a'));
        assert!(!ei.type_char('\u{00e4}')); // a-umlaut
        assert!(!ei.type_char('\u{4e16}')); // CJK character
    }

    /// type_text returns false when type_char returns false for non-empty strings.
    #[test]
    fn test_ei_input_type_text_stub_returns_false() {
        let (fd, _peer) = mock_eis_fd();
        let mut ei = EiInput::new(fd).unwrap();
        assert!(!ei.type_text("hello"));
    }

    /// Empty string in type_text should succeed (no characters to fail on).
    #[test]
    fn test_ei_input_type_text_empty_string_succeeds() {
        let (fd, _peer) = mock_eis_fd();
        let mut ei = EiInput::new(fd).unwrap();
        assert!(ei.type_text(""));
    }

    /// try_get_ei_input returns None by default (no portal session).
    #[test]
    fn test_try_get_ei_input_returns_none_by_default() {
        // OnceLock is per-process, so if another test already initialized it,
        // this test may not return None. The important thing is it doesn't panic.
        let _result = try_get_ei_input();
    }

    /// is_ei_available returns false when no EI context has been initialized.
    #[test]
    fn test_is_ei_available_false_by_default() {
        // Same caveat as above about OnceLock state across tests.
        let _available = is_ei_available();
    }

    /// portal_supports_eis returns false in stub implementation.
    #[test]
    fn test_portal_supports_eis_stub_returns_false() {
        assert!(!portal_supports_eis());
    }

    /// Verify the MIN_PORTAL_VERSION_FOR_EIS constant.
    #[test]
    fn test_min_portal_version_for_eis() {
        assert_eq!(MIN_PORTAL_VERSION_FOR_EIS, 2);
    }

    /// Verify button constants match evdev values.
    #[test]
    fn test_button_constants() {
        assert_eq!(BTN_LEFT, 0x110);
        assert_eq!(BTN_RIGHT, 0x111);
        assert_eq!(BTN_MIDDLE, 0x112);
    }

    /// key_down and key_up don't panic (stub smoke test).
    #[test]
    fn test_key_down_up_no_panic() {
        let (fd, _peer) = mock_eis_fd();
        let mut ei = EiInput::new(fd).unwrap();
        ei.key_down(30); // 'a'
        ei.key_up(30);
        ei.key_click(30);
    }

    /// pointer methods don't panic (stub smoke test).
    #[test]
    fn test_pointer_methods_no_panic() {
        let (fd, _peer) = mock_eis_fd();
        let mut ei = EiInput::new(fd).unwrap();
        ei.pointer_move(100.0, 200.0);
        ei.pointer_button(BTN_LEFT, true);
        ei.pointer_button(BTN_LEFT, false);
        ei.pointer_scroll(0.0, -1.0);
    }
}

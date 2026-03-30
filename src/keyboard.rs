#[cfg(feature = "flutter")]
use crate::flutter;
#[cfg(target_os = "windows")]
use crate::platform::windows::{get_char_from_vk, get_unicode_from_vk};
#[cfg(not(any(feature = "flutter", feature = "cli")))]
use crate::ui::CUR_SESSION;
use crate::ui_session_interface::{InvokeUiSession, Session};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::{client::get_key_state, common::GrabState};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::log;
use hbb_common::message_proto::*;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use rdev::KeyCode;
use rdev::{Event, EventType, Key};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[cfg(windows)]
static mut IS_ALT_GR: bool = false;

#[allow(dead_code)]
const OS_LOWER_WINDOWS: &str = "windows";
#[allow(dead_code)]
const OS_LOWER_LINUX: &str = "linux";
#[allow(dead_code)]
const OS_LOWER_MACOS: &str = "macos";
#[allow(dead_code)]
const OS_LOWER_ANDROID: &str = "android";

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
static KEYBOARD_HOOKED: AtomicBool = AtomicBool::new(false);

// Track key down state for relative mouse mode exit shortcut.
// macOS: Cmd+G (track G key)
// Windows/Linux: Ctrl+Alt (track whichever modifier was pressed last)
// This prevents the exit from retriggering on OS key-repeat.
#[cfg(all(feature = "flutter", any(target_os = "windows", target_os = "macos", target_os = "linux")))]
static EXIT_SHORTCUT_KEY_DOWN: AtomicBool = AtomicBool::new(false);

// Track whether relative mouse mode is currently active.
// This is set by Flutter via set_relative_mouse_mode_state() and checked
// by the rdev grab loop to determine if exit shortcuts should be processed.
#[cfg(all(feature = "flutter", any(target_os = "windows", target_os = "macos", target_os = "linux")))]
static RELATIVE_MOUSE_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set the relative mouse mode state from Flutter.
/// This is called when entering or exiting relative mouse mode.
#[cfg(all(feature = "flutter", any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn set_relative_mouse_mode_state(active: bool) {
    RELATIVE_MOUSE_MODE_ACTIVE.store(active, Ordering::SeqCst);
    // Reset exit shortcut state when mode changes to avoid stale state
    if !active {
        EXIT_SHORTCUT_KEY_DOWN.store(false, Ordering::SeqCst);
    }
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
static IS_RDEV_ENABLED: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    static ref TO_RELEASE: Arc<Mutex<HashMap<Key, Event>>> = Arc::new(Mutex::new(HashMap::new()));
    static ref MODIFIERS_STATE: Mutex<HashMap<Key, bool>> = {
        let mut m = HashMap::new();
        m.insert(Key::ShiftLeft, false);
        m.insert(Key::ShiftRight, false);
        m.insert(Key::ControlLeft, false);
        m.insert(Key::ControlRight, false);
        m.insert(Key::Alt, false);
        m.insert(Key::AltGr, false);
        m.insert(Key::MetaLeft, false);
        m.insert(Key::MetaRight, false);
        Mutex::new(m)
    };
}

pub mod client {
    use super::*;

    lazy_static::lazy_static! {
        static ref IS_GRAB_STARTED: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    }

    pub fn start_grab_loop() {
        let mut lock = IS_GRAB_STARTED.lock().unwrap();
        if *lock {
            return;
        }
        super::start_grab_loop();
        *lock = true;
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn change_grab_status(state: GrabState, keyboard_mode: &str) {
        #[cfg(feature = "flutter")]
        if !IS_RDEV_ENABLED.load(Ordering::SeqCst) {
            return;
        }
        match state {
            GrabState::Ready => {}
            GrabState::Run => {
                #[cfg(windows)]
                update_grab_get_key_name(keyboard_mode);
                #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
                KEYBOARD_HOOKED.swap(true, Ordering::SeqCst);

                #[cfg(target_os = "linux")]
                rdev::enable_grab();
            }
            GrabState::Wait => {
                #[cfg(windows)]
                rdev::set_get_key_unicode(false);

                release_remote_keys(keyboard_mode);

                #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
                KEYBOARD_HOOKED.swap(false, Ordering::SeqCst);

                #[cfg(target_os = "linux")]
                rdev::disable_grab();
            }
            GrabState::Exit => {}
        }
    }

    pub fn process_event(keyboard_mode: &str, event: &Event, lock_modes: Option<i32>) {
        let keyboard_mode = get_keyboard_mode_enum(keyboard_mode);
        if is_long_press(&event) {
            return;
        }
        let peer = get_peer_platform().to_lowercase();
        for key_event in event_to_key_events(peer, &event, keyboard_mode, lock_modes) {
            send_key_event(&key_event);
        }
    }

    pub fn process_event_with_session<T: InvokeUiSession>(
        keyboard_mode: &str,
        event: &Event,
        lock_modes: Option<i32>,
        session: &Session<T>,
    ) {
        let keyboard_mode = get_keyboard_mode_enum(keyboard_mode);
        if is_long_press(&event) {
            return;
        }
        let peer = session.peer_platform().to_lowercase();
        for key_event in event_to_key_events(peer, &event, keyboard_mode, lock_modes) {
            session.send_key_event(&key_event);
        }
    }

    pub fn get_modifiers_state(
        alt: bool,
        ctrl: bool,
        shift: bool,
        command: bool,
    ) -> (bool, bool, bool, bool) {
        let modifiers_lock = MODIFIERS_STATE.lock().unwrap();
        let ctrl = *modifiers_lock.get(&Key::ControlLeft).unwrap()
            || *modifiers_lock.get(&Key::ControlRight).unwrap()
            || ctrl;
        let shift = *modifiers_lock.get(&Key::ShiftLeft).unwrap()
            || *modifiers_lock.get(&Key::ShiftRight).unwrap()
            || shift;
        let command = *modifiers_lock.get(&Key::MetaLeft).unwrap()
            || *modifiers_lock.get(&Key::MetaRight).unwrap()
            || command;
        let alt = *modifiers_lock.get(&Key::Alt).unwrap()
            || *modifiers_lock.get(&Key::AltGr).unwrap()
            || alt;

        (alt, ctrl, shift, command)
    }

    pub fn legacy_modifiers(
        key_event: &mut KeyEvent,
        alt: bool,
        ctrl: bool,
        shift: bool,
        command: bool,
    ) {
        if alt
            && !crate::is_control_key(&key_event, &ControlKey::Alt)
            && !crate::is_control_key(&key_event, &ControlKey::RAlt)
        {
            key_event.modifiers.push(ControlKey::Alt.into());
        }
        if shift
            && !crate::is_control_key(&key_event, &ControlKey::Shift)
            && !crate::is_control_key(&key_event, &ControlKey::RShift)
        {
            key_event.modifiers.push(ControlKey::Shift.into());
        }
        if ctrl
            && !crate::is_control_key(&key_event, &ControlKey::Control)
            && !crate::is_control_key(&key_event, &ControlKey::RControl)
        {
            key_event.modifiers.push(ControlKey::Control.into());
        }
        if command
            && !crate::is_control_key(&key_event, &ControlKey::Meta)
            && !crate::is_control_key(&key_event, &ControlKey::RWin)
        {
            key_event.modifiers.push(ControlKey::Meta.into());
        }
    }

    #[cfg(target_os = "android")]
    pub fn map_key_to_control_key(key: &rdev::Key) -> Option<ControlKey> {
        match key {
            Key::Alt => Some(ControlKey::Alt),
            Key::ShiftLeft => Some(ControlKey::Shift),
            Key::ControlLeft => Some(ControlKey::Control),
            Key::MetaLeft => Some(ControlKey::Meta),
            Key::AltGr => Some(ControlKey::RAlt),
            Key::ShiftRight => Some(ControlKey::RShift),
            Key::ControlRight => Some(ControlKey::RControl),
            Key::MetaRight => Some(ControlKey::RWin),
            _ => None,
        }
    }

    pub fn event_lock_screen() -> KeyEvent {
        let mut key_event = KeyEvent::new();
        key_event.set_control_key(ControlKey::LockScreen);
        key_event.down = true;
        key_event.mode = KeyboardMode::Legacy.into();
        key_event
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn lock_screen() {
        send_key_event(&event_lock_screen());
    }

    pub fn event_ctrl_alt_del() -> KeyEvent {
        let mut key_event = KeyEvent::new();
        if get_peer_platform() == "Windows" {
            key_event.set_control_key(ControlKey::CtrlAltDel);
            key_event.down = true;
        } else {
            key_event.set_control_key(ControlKey::Delete);
            legacy_modifiers(&mut key_event, true, true, false, false);
            key_event.press = true;
        }
        key_event.mode = KeyboardMode::Legacy.into();
        key_event
    }

    #[inline]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn ctrl_alt_del() {
        send_key_event(&event_ctrl_alt_del());
    }
}

#[cfg(windows)]
pub fn update_grab_get_key_name(keyboard_mode: &str) {
    match keyboard_mode {
        "map" => rdev::set_get_key_unicode(false),
        "translate" => rdev::set_get_key_unicode(true),
        "legacy" => rdev::set_get_key_unicode(true),
        _ => {}
    };
}

#[cfg(target_os = "windows")]
static mut IS_0X021D_DOWN: bool = false;

#[cfg(target_os = "macos")]
static mut IS_LEFT_OPTION_DOWN: bool = false;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn get_keyboard_mode() -> String {
    #[cfg(not(any(feature = "flutter", feature = "cli")))]
    if let Some(session) = CUR_SESSION.lock().unwrap().as_ref() {
        return session.get_keyboard_mode();
    }
    #[cfg(feature = "flutter")]
    if let Some(session) = flutter::get_cur_session() {
        return session.get_keyboard_mode();
    }
    "legacy".to_string()
}

/// Check if exit shortcut for relative mouse mode is active.
/// Exit shortcuts (only exits, not toggles):
/// - macOS: Cmd+G
/// - Windows/Linux: Ctrl+Alt (triggered when both are pressed)
/// Note: This shortcut is only available in Flutter client. Sciter client does not support relative mouse mode.
#[cfg(feature = "flutter")]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn is_exit_relative_mouse_shortcut(key: Key) -> bool {
    let modifiers = MODIFIERS_STATE.lock().unwrap();

    #[cfg(target_os = "macos")]
    {
        // macOS: Cmd+G to exit
        if key != Key::KeyG {
            return false;
        }
        let meta = *modifiers.get(&Key::MetaLeft).unwrap_or(&false)
            || *modifiers.get(&Key::MetaRight).unwrap_or(&false);
        return meta;
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Windows/Linux: Ctrl+Alt to exit
        // Triggered when Ctrl is pressed while Alt is down, or Alt is pressed while Ctrl is down
        let is_ctrl_key = key == Key::ControlLeft || key == Key::ControlRight;
        let is_alt_key = key == Key::Alt || key == Key::AltGr;

        if !is_ctrl_key && !is_alt_key {
            return false;
        }

        let ctrl = *modifiers.get(&Key::ControlLeft).unwrap_or(&false)
            || *modifiers.get(&Key::ControlRight).unwrap_or(&false);
        let alt = *modifiers.get(&Key::Alt).unwrap_or(&false)
            || *modifiers.get(&Key::AltGr).unwrap_or(&false);

        // When Ctrl is pressed and Alt is already down, or vice versa
        (is_ctrl_key && alt) || (is_alt_key && ctrl)
    }
}

/// Notify Flutter to exit relative mouse mode.
/// Note: This is Flutter-only. Sciter client does not support relative mouse mode.
#[cfg(feature = "flutter")]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn notify_exit_relative_mouse_mode() {
    let session_id = flutter::get_cur_session_id();
    flutter::push_session_event(&session_id, "exit_relative_mouse_mode", vec![]);
}


/// Handle relative mouse mode shortcuts in the rdev grab loop.
/// Returns true if the event should be blocked from being sent to the peer.
#[cfg(feature = "flutter")]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
#[inline]
fn can_exit_relative_mouse_mode_from_grab_loop() -> bool {
    // Only process exit shortcuts when relative mouse mode is actually active.
    // This prevents blocking Ctrl+Alt (or Cmd+G) when not in relative mouse mode.
    if !RELATIVE_MOUSE_MODE_ACTIVE.load(Ordering::SeqCst) {
        return false;
    }

    let Some(session) = flutter::get_cur_session() else {
        return false;
    };

    // Only for remote desktop sessions.
    if !session.is_default() {
        return false;
    }

    // Must have keyboard permission and not be in view-only mode.
    if !*session.server_keyboard_enabled.read().unwrap() {
        return false;
    }
    let lc = session.lc.read().unwrap();
    if lc.view_only.v {
        return false;
    }

    // Peer must support relative mouse mode.
    crate::common::is_support_relative_mouse_mode_num(lc.version)
}

#[cfg(feature = "flutter")]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
#[inline]
fn should_block_relative_mouse_shortcut(key: Key, is_press: bool) -> bool {
    if !KEYBOARD_HOOKED.load(Ordering::SeqCst) {
        return false;
    }

    // Determine which key to track for key-up blocking based on platform
    #[cfg(target_os = "macos")]
    let is_tracked_key = key == Key::KeyG;
    #[cfg(not(target_os = "macos"))]
    let is_tracked_key = key == Key::ControlLeft
        || key == Key::ControlRight
        || key == Key::Alt
        || key == Key::AltGr;

    // Block key up if key down was blocked (to avoid orphan key up event on remote).
    // This must be checked before clearing the flag below.
    if is_tracked_key && !is_press && EXIT_SHORTCUT_KEY_DOWN.swap(false, Ordering::SeqCst) {
        return true;
    }

    // Exit relative mouse mode shortcuts:
    // - macOS: Cmd+G
    // - Windows/Linux: Ctrl+Alt
    // Guard it to supported/eligible sessions to avoid blocking the chord unexpectedly.
    if is_exit_relative_mouse_shortcut(key) {
        if !can_exit_relative_mouse_mode_from_grab_loop() {
            return false;
        }
        if is_press {
            // Only trigger exit on transition from "not pressed" to "pressed".
            // This prevents retriggering on OS key-repeat.
            if !EXIT_SHORTCUT_KEY_DOWN.swap(true, Ordering::SeqCst) {
                notify_exit_relative_mouse_mode();
            }
        }
        return true;
    }

    false
}

fn start_grab_loop() {
    std::env::set_var("KEYBOARD_ONLY", "y");
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    std::thread::spawn(move || {
        let try_handle_keyboard = move |event: Event, key: Key, is_press: bool| -> Option<Event> {
            // fix #2211：CAPS LOCK don't work
            if key == Key::CapsLock || key == Key::NumLock {
                return Some(event);
            }

            let _scan_code = event.position_code;
            let _code = event.platform_code as KeyCode;

            #[cfg(feature = "flutter")]
            if should_block_relative_mouse_shortcut(key, is_press) {
                return None;
            }

            let res = if KEYBOARD_HOOKED.load(Ordering::SeqCst) {
                client::process_event(&get_keyboard_mode(), &event, None);
                if is_press {
                    None
                } else {
                    Some(event)
                }
            } else {
                Some(event)
            };

            #[cfg(target_os = "windows")]
            match _scan_code {
                0x1D | 0x021D => rdev::set_modifier(Key::ControlLeft, is_press),
                0xE01D => rdev::set_modifier(Key::ControlRight, is_press),
                0x2A => rdev::set_modifier(Key::ShiftLeft, is_press),
                0x36 => rdev::set_modifier(Key::ShiftRight, is_press),
                0x38 => rdev::set_modifier(Key::Alt, is_press),
                // Right Alt
                0xE038 => rdev::set_modifier(Key::AltGr, is_press),
                0xE05B => rdev::set_modifier(Key::MetaLeft, is_press),
                0xE05C => rdev::set_modifier(Key::MetaRight, is_press),
                _ => {}
            }

            #[cfg(target_os = "windows")]
            unsafe {
                // AltGr
                if _scan_code == 0x021D {
                    IS_0X021D_DOWN = is_press;
                }
            }

            #[cfg(target_os = "macos")]
            unsafe {
                if _code == rdev::kVK_Option {
                    IS_LEFT_OPTION_DOWN = is_press;
                }
            }

            return res;
        };
        let func = move |event: Event| match event.event_type {
            EventType::KeyPress(key) => try_handle_keyboard(event, key, true),
            EventType::KeyRelease(key) => try_handle_keyboard(event, key, false),
            _ => Some(event),
        };
        #[cfg(target_os = "macos")]
        rdev::set_is_main_thread(false);
        #[cfg(target_os = "windows")]
        rdev::set_event_popup(false);
        if let Err(error) = rdev::grab(func) {
            log::error!("rdev Error: {:?}", error)
        }
    });

    #[cfg(target_os = "linux")]
    if let Err(err) = rdev::start_grab_listen(move |event: Event| match event.event_type {
        EventType::KeyPress(key) | EventType::KeyRelease(key) => {
            let is_press = matches!(event.event_type, EventType::KeyPress(_));
            if let Key::Unknown(keycode) = key {
                log::error!("rdev get unknown key, keycode is {:?}", keycode);
            } else {
                #[cfg(feature = "flutter")]
                if should_block_relative_mouse_shortcut(key, is_press) {
                    return None;
                }
                client::process_event(&get_keyboard_mode(), &event, None);
            }
            None
        }
        _ => Some(event),
    }) {
        log::error!("Failed to init rdev grab thread: {:?}", err);
    };
}

// #[allow(dead_code)] is ok here. No need to stop grabbing loop.
#[allow(dead_code)]
fn stop_grab_loop() -> Result<(), rdev::GrabError> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    rdev::exit_grab()?;
    #[cfg(target_os = "linux")]
    rdev::exit_grab_listen();
    Ok(())
}

pub fn is_long_press(event: &Event) -> bool {
    let keys = MODIFIERS_STATE.lock().unwrap();
    match event.event_type {
        EventType::KeyPress(k) => {
            if let Some(&state) = keys.get(&k) {
                if state == true {
                    return true;
                }
            }
        }
        _ => {}
    };
    return false;
}

pub fn release_remote_keys(keyboard_mode: &str) {
    // todo!: client quit suddenly, how to release keys?
    let to_release = TO_RELEASE.lock().unwrap().clone();
    TO_RELEASE.lock().unwrap().clear();
    for (key, mut event) in to_release.into_iter() {
        event.event_type = EventType::KeyRelease(key);
        client::process_event(keyboard_mode, &event, None);
        // If Alt or AltGr is pressed, we need to send another key stoke to release it.
        // Because the controlled side may hold the alt state, if local window is switched by [Alt + Tab].
        if key == Key::Alt || key == Key::AltGr {
            event.event_type = EventType::KeyPress(key);
            client::process_event(keyboard_mode, &event, None);
            event.event_type = EventType::KeyRelease(key);
            client::process_event(keyboard_mode, &event, None);
        }
    }
}

pub fn get_keyboard_mode_enum(keyboard_mode: &str) -> KeyboardMode {
    match keyboard_mode {
        "map" => KeyboardMode::Map,
        "translate" => KeyboardMode::Translate,
        "legacy" => KeyboardMode::Legacy,
        _ => KeyboardMode::Map,
    }
}

#[inline]
pub fn is_modifier(key: &rdev::Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::MetaLeft
            | Key::MetaRight
            | Key::Alt
            | Key::AltGr
    )
}

#[inline]
#[allow(dead_code)]
pub fn is_modifier_code(evt: &KeyEvent) -> bool {
    match evt.union {
        Some(key_event::Union::Chr(code)) => {
            let key = rdev::linux_key_from_code(code);
            is_modifier(&key)
        }
        _ => false,
    }
}

#[inline]
pub fn is_numpad_rdev_key(key: &rdev::Key) -> bool {
    matches!(
        key,
        Key::Kp0
            | Key::Kp1
            | Key::Kp2
            | Key::Kp3
            | Key::Kp4
            | Key::Kp5
            | Key::Kp6
            | Key::Kp7
            | Key::Kp8
            | Key::Kp9
            | Key::KpMinus
            | Key::KpMultiply
            | Key::KpDivide
            | Key::KpPlus
            | Key::KpDecimal
    )
}

#[inline]
pub fn is_letter_rdev_key(key: &rdev::Key) -> bool {
    matches!(
        key,
        Key::KeyA
            | Key::KeyB
            | Key::KeyC
            | Key::KeyD
            | Key::KeyE
            | Key::KeyF
            | Key::KeyG
            | Key::KeyH
            | Key::KeyI
            | Key::KeyJ
            | Key::KeyK
            | Key::KeyL
            | Key::KeyM
            | Key::KeyN
            | Key::KeyO
            | Key::KeyP
            | Key::KeyQ
            | Key::KeyR
            | Key::KeyS
            | Key::KeyT
            | Key::KeyU
            | Key::KeyV
            | Key::KeyW
            | Key::KeyX
            | Key::KeyY
            | Key::KeyZ
    )
}

// https://github.com/rustdesk/rustdesk/issues/8599
// We just add these keys as letter keys.
#[inline]
pub fn is_letter_rdev_key_ex(key: &rdev::Key) -> bool {
    matches!(
        key,
        Key::LeftBracket | Key::RightBracket | Key::SemiColon | Key::Quote | Key::Comma | Key::Dot
    )
}

#[inline]
fn is_numpad_key(event: &Event) -> bool {
    matches!(event.event_type, EventType::KeyPress(key) | EventType::KeyRelease(key) if is_numpad_rdev_key(&key))
}

// Check is letter key for lock modes.
// Only letter keys need to check and send Lock key state.
#[inline]
fn is_letter_key_4_lock_modes(event: &Event) -> bool {
    matches!(event.event_type, EventType::KeyPress(key) | EventType::KeyRelease(key) if (is_letter_rdev_key(&key) || is_letter_rdev_key_ex(&key)))
}

fn parse_add_lock_modes_modifiers(
    key_event: &mut KeyEvent,
    lock_modes: i32,
    is_numpad_key: bool,
    is_letter_key: bool,
) {
    const CAPS_LOCK: i32 = 1;
    const NUM_LOCK: i32 = 2;
    // const SCROLL_LOCK: i32 = 3;
    if is_letter_key && (lock_modes & (1 << CAPS_LOCK) != 0) {
        key_event.modifiers.push(ControlKey::CapsLock.into());
    }
    if is_numpad_key && lock_modes & (1 << NUM_LOCK) != 0 {
        key_event.modifiers.push(ControlKey::NumLock.into());
    }
    // if lock_modes & (1 << SCROLL_LOCK) != 0 {
    //     key_event.modifiers.push(ControlKey::ScrollLock.into());
    // }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn add_lock_modes_modifiers(key_event: &mut KeyEvent, is_numpad_key: bool, is_letter_key: bool) {
    if is_letter_key && get_key_state(enigo::Key::CapsLock) {
        key_event.modifiers.push(ControlKey::CapsLock.into());
    }
    if is_numpad_key && get_key_state(enigo::Key::NumLock) {
        key_event.modifiers.push(ControlKey::NumLock.into());
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn convert_numpad_keys(key: Key) -> Key {
    if get_key_state(enigo::Key::NumLock) {
        return key;
    }
    match key {
        Key::Kp0 => Key::Insert,
        Key::KpDecimal => Key::Delete,
        Key::Kp1 => Key::End,
        Key::Kp2 => Key::DownArrow,
        Key::Kp3 => Key::PageDown,
        Key::Kp4 => Key::LeftArrow,
        Key::Kp5 => Key::Clear,
        Key::Kp6 => Key::RightArrow,
        Key::Kp7 => Key::Home,
        Key::Kp8 => Key::UpArrow,
        Key::Kp9 => Key::PageUp,
        _ => key,
    }
}

fn update_modifiers_state(event: &Event) {
    // for mouse
    let mut keys = MODIFIERS_STATE.lock().unwrap();
    match event.event_type {
        EventType::KeyPress(k) => {
            if keys.contains_key(&k) {
                keys.insert(k, true);
            }
        }
        EventType::KeyRelease(k) => {
            if keys.contains_key(&k) {
                keys.insert(k, false);
            }
        }
        _ => {}
    };
}

pub fn event_to_key_events(
    mut peer: String,
    event: &Event,
    keyboard_mode: KeyboardMode,
    _lock_modes: Option<i32>,
) -> Vec<KeyEvent> {
    peer.retain(|c| !c.is_whitespace());

    let mut key_event = KeyEvent::new();
    update_modifiers_state(event);

    match event.event_type {
        EventType::KeyPress(key) => {
            TO_RELEASE.lock().unwrap().insert(key, event.clone());
        }
        EventType::KeyRelease(key) => {
            TO_RELEASE.lock().unwrap().remove(&key);
        }
        _ => {}
    }

    key_event.mode = keyboard_mode.into();

    let mut key_events = match keyboard_mode {
        KeyboardMode::Map => map_keyboard_mode(peer.as_str(), event, key_event),
        KeyboardMode::Translate => translate_keyboard_mode(peer.as_str(), event, key_event),
        _ => {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                legacy_keyboard_mode(event, key_event)
            }
            #[cfg(any(target_os = "android", target_os = "ios"))]
            {
                Vec::new()
            }
        }
    };

    let is_numpad_key = is_numpad_key(&event);
    if keyboard_mode != KeyboardMode::Translate || is_numpad_key {
        let is_letter_key = is_letter_key_4_lock_modes(&event);
        for key_event in &mut key_events {
            if let Some(lock_modes) = _lock_modes {
                parse_add_lock_modes_modifiers(key_event, lock_modes, is_numpad_key, is_letter_key);
            } else {
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                add_lock_modes_modifiers(key_event, is_numpad_key, is_letter_key);
            }
        }
    }
    key_events
}

pub fn send_key_event(key_event: &KeyEvent) {
    #[cfg(not(any(feature = "flutter", feature = "cli")))]
    if let Some(session) = CUR_SESSION.lock().unwrap().as_ref() {
        session.send_key_event(key_event);
    }

    #[cfg(feature = "flutter")]
    if let Some(session) = flutter::get_cur_session() {
        session.send_key_event(key_event);
    }
}

pub fn get_peer_platform() -> String {
    #[cfg(not(any(feature = "flutter", feature = "cli")))]
    if let Some(session) = CUR_SESSION.lock().unwrap().as_ref() {
        return session.peer_platform();
    }
    #[cfg(feature = "flutter")]
    if let Some(session) = flutter::get_cur_session() {
        return session.peer_platform();
    }
    "Windows".to_string()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn legacy_keyboard_mode(event: &Event, mut key_event: KeyEvent) -> Vec<KeyEvent> {
    let mut events = Vec::new();
    // legacy mode(0): Generate characters locally, look for keycode on other side.
    let (mut key, down_or_up) = match event.event_type {
        EventType::KeyPress(key) => (key, true),
        EventType::KeyRelease(key) => (key, false),
        _ => {
            return events;
        }
    };

    let peer = get_peer_platform();
    let is_win = peer == "Windows";
    if is_win {
        key = convert_numpad_keys(key);
    }

    let alt = get_key_state(enigo::Key::Alt);
    #[cfg(windows)]
    let ctrl = {
        let mut tmp = get_key_state(enigo::Key::Control) || get_key_state(enigo::Key::RightControl);
        unsafe {
            if IS_ALT_GR {
                if alt || key == Key::AltGr {
                    if tmp {
                        tmp = false;
                    }
                } else {
                    IS_ALT_GR = false;
                }
            }
        }
        tmp
    };
    #[cfg(not(windows))]
    let ctrl = get_key_state(enigo::Key::Control) || get_key_state(enigo::Key::RightControl);
    let shift = get_key_state(enigo::Key::Shift) || get_key_state(enigo::Key::RightShift);
    #[cfg(windows)]
    let command = crate::platform::windows::get_win_key_state();
    #[cfg(not(windows))]
    let command = get_key_state(enigo::Key::Meta);
    let control_key = match key {
        Key::Alt => Some(ControlKey::Alt),
        Key::AltGr => Some(ControlKey::RAlt),
        Key::Backspace => Some(ControlKey::Backspace),
        Key::ControlLeft => {
            // when pressing AltGr, an extra VK_LCONTROL with a special
            // scancode with bit 9 set is sent, let's ignore this.
            #[cfg(windows)]
            if (event.position_code >> 8) == 0xE0 {
                unsafe {
                    IS_ALT_GR = true;
                }
                return events;
            }
            Some(ControlKey::Control)
        }
        Key::ControlRight => Some(ControlKey::RControl),
        Key::DownArrow => Some(ControlKey::DownArrow),
        Key::Escape => Some(ControlKey::Escape),
        Key::F1 => Some(ControlKey::F1),
        Key::F10 => Some(ControlKey::F10),
        Key::F11 => Some(ControlKey::F11),
        Key::F12 => Some(ControlKey::F12),
        Key::F2 => Some(ControlKey::F2),
        Key::F3 => Some(ControlKey::F3),
        Key::F4 => Some(ControlKey::F4),
        Key::F5 => Some(ControlKey::F5),
        Key::F6 => Some(ControlKey::F6),
        Key::F7 => Some(ControlKey::F7),
        Key::F8 => Some(ControlKey::F8),
        Key::F9 => Some(ControlKey::F9),
        Key::LeftArrow => Some(ControlKey::LeftArrow),
        Key::MetaLeft => Some(ControlKey::Meta),
        Key::MetaRight => Some(ControlKey::RWin),
        Key::Return => Some(ControlKey::Return),
        Key::RightArrow => Some(ControlKey::RightArrow),
        Key::ShiftLeft => Some(ControlKey::Shift),
        Key::ShiftRight => Some(ControlKey::RShift),
        Key::Space => Some(ControlKey::Space),
        Key::Tab => Some(ControlKey::Tab),
        Key::UpArrow => Some(ControlKey::UpArrow),
        Key::Delete => {
            if is_win && ctrl && alt {
                client::ctrl_alt_del();
                return events;
            }
            Some(ControlKey::Delete)
        }
        Key::Apps => Some(ControlKey::Apps),
        Key::Cancel => Some(ControlKey::Cancel),
        Key::Clear => Some(ControlKey::Clear),
        Key::Kana => Some(ControlKey::Kana),
        Key::Hangul => Some(ControlKey::Hangul),
        Key::Junja => Some(ControlKey::Junja),
        Key::Final => Some(ControlKey::Final),
        Key::Hanja => Some(ControlKey::Hanja),
        Key::Hanji => Some(ControlKey::Hanja),
        Key::Lang2 => Some(ControlKey::Convert),
        Key::Print => Some(ControlKey::Print),
        Key::Select => Some(ControlKey::Select),
        Key::Execute => Some(ControlKey::Execute),
        Key::PrintScreen => Some(ControlKey::Snapshot),
        Key::Help => Some(ControlKey::Help),
        Key::Sleep => Some(ControlKey::Sleep),
        Key::Separator => Some(ControlKey::Separator),
        Key::KpReturn => Some(ControlKey::NumpadEnter),
        Key::Kp0 => Some(ControlKey::Numpad0),
        Key::Kp1 => Some(ControlKey::Numpad1),
        Key::Kp2 => Some(ControlKey::Numpad2),
        Key::Kp3 => Some(ControlKey::Numpad3),
        Key::Kp4 => Some(ControlKey::Numpad4),
        Key::Kp5 => Some(ControlKey::Numpad5),
        Key::Kp6 => Some(ControlKey::Numpad6),
        Key::Kp7 => Some(ControlKey::Numpad7),
        Key::Kp8 => Some(ControlKey::Numpad8),
        Key::Kp9 => Some(ControlKey::Numpad9),
        Key::KpDivide => Some(ControlKey::Divide),
        Key::KpMultiply => Some(ControlKey::Multiply),
        Key::KpDecimal => Some(ControlKey::Decimal),
        Key::KpMinus => Some(ControlKey::Subtract),
        Key::KpPlus => Some(ControlKey::Add),
        Key::CapsLock | Key::NumLock | Key::ScrollLock => {
            return events;
        }
        Key::Home => Some(ControlKey::Home),
        Key::End => Some(ControlKey::End),
        Key::Insert => Some(ControlKey::Insert),
        Key::PageUp => Some(ControlKey::PageUp),
        Key::PageDown => Some(ControlKey::PageDown),
        Key::Pause => Some(ControlKey::Pause),
        _ => None,
    };
    if let Some(k) = control_key {
        key_event.set_control_key(k);
    } else {
        let name = event
            .unicode
            .as_ref()
            .and_then(|unicode| unicode.name.clone());
        let mut chr = match &name {
            Some(ref s) => {
                if s.len() <= 2 {
                    // exclude chinese characters
                    s.chars().next().unwrap_or('\0')
                } else {
                    '\0'
                }
            }
            _ => '\0',
        };
        if chr == '·' {
            // special for Chinese
            chr = '`';
        }
        if chr == '\0' {
            chr = match key {
                Key::Num1 => '1',
                Key::Num2 => '2',
                Key::Num3 => '3',
                Key::Num4 => '4',
                Key::Num5 => '5',
                Key::Num6 => '6',
                Key::Num7 => '7',
                Key::Num8 => '8',
                Key::Num9 => '9',
                Key::Num0 => '0',
                Key::KeyA => 'a',
                Key::KeyB => 'b',
                Key::KeyC => 'c',
                Key::KeyD => 'd',
                Key::KeyE => 'e',
                Key::KeyF => 'f',
                Key::KeyG => 'g',
                Key::KeyH => 'h',
                Key::KeyI => 'i',
                Key::KeyJ => 'j',
                Key::KeyK => 'k',
                Key::KeyL => 'l',
                Key::KeyM => 'm',
                Key::KeyN => 'n',
                Key::KeyO => 'o',
                Key::KeyP => 'p',
                Key::KeyQ => 'q',
                Key::KeyR => 'r',
                Key::KeyS => 's',
                Key::KeyT => 't',
                Key::KeyU => 'u',
                Key::KeyV => 'v',
                Key::KeyW => 'w',
                Key::KeyX => 'x',
                Key::KeyY => 'y',
                Key::KeyZ => 'z',
                Key::Comma => ',',
                Key::Dot => '.',
                Key::SemiColon => ';',
                Key::Quote => '\'',
                Key::LeftBracket => '[',
                Key::RightBracket => ']',
                Key::Slash => '/',
                Key::BackSlash => '\\',
                Key::Minus => '-',
                Key::Equal => '=',
                Key::BackQuote => '`',
                _ => '\0',
            }
        }
        if chr != '\0' {
            if chr == 'l' && is_win && command {
                client::lock_screen();
                return events;
            }
            key_event.set_chr(chr as _);
        } else {
            log::error!("Unknown key {:?}", &event);
            return events;
        }
    }
    let (alt, ctrl, shift, command) = client::get_modifiers_state(alt, ctrl, shift, command);
    client::legacy_modifiers(&mut key_event, alt, ctrl, shift, command);

    if down_or_up == true {
        key_event.down = true;
    }
    events.push(key_event);
    events
}

#[inline]
pub fn map_keyboard_mode(_peer: &str, event: &Event, key_event: KeyEvent) -> Vec<KeyEvent> {
    _map_keyboard_mode(_peer, event, key_event)
        .map(|e| vec![e])
        .unwrap_or_default()
}

fn _map_keyboard_mode(_peer: &str, event: &Event, mut key_event: KeyEvent) -> Option<KeyEvent> {
    match event.event_type {
        EventType::KeyPress(..) => {
            key_event.down = true;
        }
        EventType::KeyRelease(..) => {
            key_event.down = false;
        }
        _ => return None,
    };

    #[cfg(target_os = "windows")]
    let keycode = match _peer {
        OS_LOWER_WINDOWS => {
            // https://github.com/rustdesk/rustdesk/issues/1371
            // Filter scancodes that are greater than 255 and the height word is not 0xE0.
            if event.position_code > 255 && (event.position_code >> 8) != 0xE0 {
                return None;
            }
            event.position_code
        }
        OS_LOWER_MACOS => {
            if hbb_common::config::LocalConfig::get_kb_layout_type() == "ISO" {
                rdev::win_scancode_to_macos_iso_code(event.position_code)?
            } else {
                rdev::win_scancode_to_macos_code(event.position_code)?
            }
        }
        OS_LOWER_ANDROID => rdev::win_scancode_to_android_key_code(event.position_code)?,
        _ => rdev::win_scancode_to_linux_code(event.position_code)?,
    };
    #[cfg(target_os = "macos")]
    let keycode = match _peer {
        OS_LOWER_WINDOWS => rdev::macos_code_to_win_scancode(event.platform_code as _)?,
        OS_LOWER_MACOS => event.platform_code as _,
        OS_LOWER_ANDROID => rdev::macos_code_to_android_key_code(event.platform_code as _)?,
        _ => rdev::macos_code_to_linux_code(event.platform_code as _)?,
    };
    #[cfg(target_os = "linux")]
    let keycode = match _peer {
        OS_LOWER_WINDOWS => rdev::linux_code_to_win_scancode(event.position_code as _)?,
        OS_LOWER_MACOS => {
            if hbb_common::config::LocalConfig::get_kb_layout_type() == "ISO" {
                rdev::linux_code_to_macos_iso_code(event.position_code as _)?
            } else {
                rdev::linux_code_to_macos_code(event.position_code as _)?
            }
        }
        OS_LOWER_ANDROID => rdev::linux_code_to_android_key_code(event.position_code as _)?,
        _ => event.position_code as _,
    };
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let keycode = match _peer {
        OS_LOWER_WINDOWS => rdev::usb_hid_code_to_win_scancode(event.usb_hid as _)?,
        OS_LOWER_LINUX => rdev::usb_hid_code_to_linux_code(event.usb_hid as _)?,
        OS_LOWER_MACOS => {
            if hbb_common::config::LocalConfig::get_kb_layout_type() == "ISO" {
                rdev::usb_hid_code_to_macos_iso_code(event.usb_hid as _)?
            } else {
                rdev::usb_hid_code_to_macos_code(event.usb_hid as _)?
            }
        }
        OS_LOWER_ANDROID => rdev::usb_hid_code_to_android_key_code(event.usb_hid as _)?,
        _ => event.usb_hid as _,
    };
    key_event.set_chr(keycode as _);
    Some(key_event)
}

#[cfg(not(any(target_os = "ios")))]
fn try_fill_unicode(_peer: &str, event: &Event, key_event: &KeyEvent, events: &mut Vec<KeyEvent>) {
    match &event.unicode {
        Some(unicode_info) => {
            if let Some(name) = &unicode_info.name {
                if name.len() > 0 {
                    let mut evt = key_event.clone();
                    evt.set_seq(name.to_string());
                    evt.down = true;
                    events.push(evt);
                }
            }
        }
        None =>
        {
            #[cfg(target_os = "windows")]
            if _peer == OS_LOWER_LINUX {
                if is_hot_key_modifiers_down() && unsafe { !IS_0X021D_DOWN } {
                    if let Some(chr) = get_char_from_vk(event.platform_code as u32) {
                        let mut evt = key_event.clone();
                        evt.set_seq(chr.to_string());
                        evt.down = true;
                        events.push(evt);
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn try_fill_win2win_hotkey(
    peer: &str,
    event: &Event,
    key_event: &KeyEvent,
    events: &mut Vec<KeyEvent>,
) {
    if peer == OS_LOWER_WINDOWS && is_hot_key_modifiers_down() && unsafe { !IS_0X021D_DOWN } {
        let mut down = false;
        let win2win_hotkey = match event.event_type {
            EventType::KeyPress(..) => {
                down = true;
                if let Some(unicode) = get_unicode_from_vk(event.platform_code as u32) {
                    Some((unicode as u32 & 0x0000FFFF) | (event.platform_code << 16))
                } else {
                    None
                }
            }
            EventType::KeyRelease(..) => Some(event.platform_code << 16),
            _ => None,
        };
        if let Some(code) = win2win_hotkey {
            let mut evt = key_event.clone();
            evt.set_win2win_hotkey(code);
            evt.down = down;
            events.push(evt);
        }
    }
}

#[cfg(target_os = "windows")]
fn is_hot_key_modifiers_down() -> bool {
    if rdev::get_modifier(Key::ControlLeft) || rdev::get_modifier(Key::ControlRight) {
        return true;
    }
    if rdev::get_modifier(Key::Alt) || rdev::get_modifier(Key::AltGr) {
        return true;
    }
    if rdev::get_modifier(Key::MetaLeft) || rdev::get_modifier(Key::MetaRight) {
        return true;
    }
    return false;
}

#[inline]
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn is_altgr(event: &Event) -> bool {
    #[cfg(target_os = "linux")]
    if event.platform_code == 0xFE03 {
        true
    } else {
        false
    }

    #[cfg(target_os = "windows")]
    if unsafe { IS_0X021D_DOWN } && event.position_code == 0xE038 {
        true
    } else {
        false
    }
}

#[inline]
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn is_press(event: &Event) -> bool {
    matches!(event.event_type, EventType::KeyPress(_))
}

// https://github.com/rustdesk/rustdesk/wiki/FAQ#keyboard-translation-modes
pub fn translate_keyboard_mode(peer: &str, event: &Event, key_event: KeyEvent) -> Vec<KeyEvent> {
    let mut events: Vec<KeyEvent> = Vec::new();

    if let Some(unicode_info) = &event.unicode {
        if unicode_info.is_dead {
            #[cfg(target_os = "macos")]
            if peer != OS_LOWER_MACOS && unsafe { IS_LEFT_OPTION_DOWN } {
                // try clear dead key state
                // rdev::clear_dead_key_state();
            } else {
                return events;
            }
            #[cfg(not(target_os = "macos"))]
            return events;
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if is_numpad_key(&event) {
        events.append(&mut map_keyboard_mode(peer, event, key_event));
        return events;
    }

    #[cfg(target_os = "macos")]
    // ignore right option key
    if event.platform_code == rdev::kVK_RightOption as u32 {
        return events;
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if is_altgr(event) {
        return events;
    }

    #[cfg(target_os = "windows")]
    if event.position_code == 0x021D {
        return events;
    }

    #[cfg(target_os = "windows")]
    try_fill_win2win_hotkey(peer, event, &key_event, &mut events);

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if events.is_empty() && is_press(event) {
        try_fill_unicode(peer, event, &key_event, &mut events);
    }

    // If AltGr is down, no need to send events other than unicode.
    #[cfg(target_os = "windows")]
    unsafe {
        if IS_0X021D_DOWN {
            return events;
        }
    }

    #[cfg(target_os = "macos")]
    if !unsafe { IS_LEFT_OPTION_DOWN } {
        try_fill_unicode(peer, event, &key_event, &mut events);
    }

    if events.is_empty() {
        events.append(&mut map_keyboard_mode(peer, event, key_event));
    }
    events
}

#[cfg(not(any(target_os = "ios")))]
pub fn keycode_to_rdev_key(keycode: u32) -> Key {
    #[cfg(target_os = "windows")]
    return rdev::win_key_from_scancode(keycode);
    #[cfg(any(target_os = "linux"))]
    return rdev::linux_key_from_code(keycode);
    #[cfg(any(target_os = "android"))]
    return rdev::android_key_from_code(keycode);
    #[cfg(target_os = "macos")]
    return rdev::macos_key_from_code(keycode.try_into().unwrap_or_default());
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod input_source {
    #[cfg(target_os = "macos")]
    use hbb_common::log;
    use hbb_common::SessionID;

    use crate::ui_interface::{get_local_option, set_local_option};

    pub const CONFIG_OPTION_INPUT_SOURCE: &str = "input-source";
    // rdev grab mode
    pub const CONFIG_INPUT_SOURCE_1: &str = "Input source 1";
    pub const CONFIG_INPUT_SOURCE_1_TIP: &str = "input_source_1_tip";
    // flutter grab mode
    pub const CONFIG_INPUT_SOURCE_2: &str = "Input source 2";
    pub const CONFIG_INPUT_SOURCE_2_TIP: &str = "input_source_2_tip";

    pub const CONFIG_INPUT_SOURCE_DEFAULT: &str = CONFIG_INPUT_SOURCE_1;

    pub fn init_input_source() {
        #[cfg(target_os = "linux")]
        if !crate::platform::linux::is_x11() {
            // If switching from X11 to Wayland, the grab loop will not be started.
            // Do not change the config here.
            return;
        }
        #[cfg(target_os = "macos")]
        if !crate::platform::macos::is_can_input_monitoring(false) {
            log::error!("init_input_source, is_can_input_monitoring() false");
            set_local_option(
                CONFIG_OPTION_INPUT_SOURCE.to_string(),
                CONFIG_INPUT_SOURCE_2.to_string(),
            );
            return;
        }
        let cur_input_source = get_cur_session_input_source();
        if cur_input_source == CONFIG_INPUT_SOURCE_1 {
            super::IS_RDEV_ENABLED.store(true, super::Ordering::SeqCst);
        }
        super::client::start_grab_loop();
    }

    pub fn change_input_source(session_id: SessionID, input_source: String) {
        let cur_input_source = get_cur_session_input_source();
        if cur_input_source == input_source {
            return;
        }
        if input_source == CONFIG_INPUT_SOURCE_1 {
            #[cfg(target_os = "macos")]
            if !crate::platform::macos::is_can_input_monitoring(false) {
                log::error!("change_input_source, is_can_input_monitoring() false");
                return;
            }
            // It is ok to start grab loop multiple times.
            super::client::start_grab_loop();
            super::IS_RDEV_ENABLED.store(true, super::Ordering::SeqCst);
            crate::flutter_ffi::session_enter_or_leave(session_id, true);
        } else if input_source == CONFIG_INPUT_SOURCE_2 {
            // No need to stop grab loop.
            crate::flutter_ffi::session_enter_or_leave(session_id, false);
            super::IS_RDEV_ENABLED.store(false, super::Ordering::SeqCst);
        }
        set_local_option(CONFIG_OPTION_INPUT_SOURCE.to_string(), input_source);
    }

    #[inline]
    pub fn get_cur_session_input_source() -> String {
        #[cfg(target_os = "linux")]
        if !crate::platform::linux::is_x11() {
            return CONFIG_INPUT_SOURCE_2.to_string();
        }
        let input_source = get_local_option(CONFIG_OPTION_INPUT_SOURCE.to_string());
        if input_source.is_empty() {
            CONFIG_INPUT_SOURCE_DEFAULT.to_string()
        } else {
            input_source
        }
    }

    #[inline]
    pub fn get_supported_input_source() -> Vec<(String, String)> {
        #[cfg(target_os = "linux")]
        if !crate::platform::linux::is_x11() {
            return vec![(
                CONFIG_INPUT_SOURCE_2.to_string(),
                CONFIG_INPUT_SOURCE_2_TIP.to_string(),
            )];
        }
        vec![
            (
                CONFIG_INPUT_SOURCE_1.to_string(),
                CONFIG_INPUT_SOURCE_1_TIP.to_string(),
            ),
            (
                CONFIG_INPUT_SOURCE_2.to_string(),
                CONFIG_INPUT_SOURCE_2_TIP.to_string(),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbb_common::message_proto::*;
    use rdev::{Event, EventType, Key, UnicodeInfo};
    use std::sync::Mutex as StdMutex;
    use std::time::SystemTime;

    // -----------------------------------------------------------------------
    // Global lock: tests that read/write MODIFIERS_STATE or TO_RELEASE must
    // hold this lock to avoid races (cargo test runs in parallel by default).
    // -----------------------------------------------------------------------

    lazy_static::lazy_static! {
        static ref TEST_LOCK: StdMutex<()> = StdMutex::new(());
    }

    // -----------------------------------------------------------------------
    // Helper: construct a minimal rdev::Event for testing
    // -----------------------------------------------------------------------

    fn make_event(event_type: EventType) -> Event {
        Event {
            time: SystemTime::now(),
            unicode: None,
            event_type,
            platform_code: 0,
            position_code: 0,
            usb_hid: 0,
        }
    }

    fn make_key_press(key: Key) -> Event {
        make_event(EventType::KeyPress(key))
    }

    fn make_key_release(key: Key) -> Event {
        make_event(EventType::KeyRelease(key))
    }

    fn make_key_press_with_position(key: Key, position_code: u32) -> Event {
        let mut e = make_key_press(key);
        e.position_code = position_code;
        e
    }

    fn make_key_release_with_position(key: Key, position_code: u32) -> Event {
        let mut e = make_key_release(key);
        e.position_code = position_code;
        e
    }

    fn make_event_with_unicode(event_type: EventType, name: Option<String>) -> Event {
        let mut e = make_event(event_type);
        e.unicode = Some(UnicodeInfo {
            name,
            unicode: vec![],
            is_dead: false,
        });
        e
    }

    /// Reset MODIFIERS_STATE to all-false between tests that touch it.
    fn reset_modifiers_state() {
        let mut m = MODIFIERS_STATE.lock().unwrap();
        for v in m.values_mut() {
            *v = false;
        }
    }

    /// Reset TO_RELEASE between tests that touch it.
    fn reset_to_release() {
        TO_RELEASE.lock().unwrap().clear();
    }

    // ===================================================================
    // 1. get_keyboard_mode_enum  — string to protobuf enum
    // ===================================================================

    #[test]
    fn test_get_keyboard_mode_enum_map() {
        assert_eq!(get_keyboard_mode_enum("map"), KeyboardMode::Map);
    }

    #[test]
    fn test_get_keyboard_mode_enum_translate() {
        assert_eq!(get_keyboard_mode_enum("translate"), KeyboardMode::Translate);
    }

    #[test]
    fn test_get_keyboard_mode_enum_legacy() {
        assert_eq!(get_keyboard_mode_enum("legacy"), KeyboardMode::Legacy);
    }

    #[test]
    fn test_get_keyboard_mode_enum_unknown_defaults_to_map() {
        assert_eq!(get_keyboard_mode_enum("auto"), KeyboardMode::Map);
        assert_eq!(get_keyboard_mode_enum(""), KeyboardMode::Map);
        assert_eq!(get_keyboard_mode_enum("bogus"), KeyboardMode::Map);
    }

    // ===================================================================
    // 2. is_modifier  — rdev::Key classification
    // ===================================================================

    #[test]
    fn test_is_modifier_true_for_all_modifiers() {
        let modifiers = [
            Key::ShiftLeft,
            Key::ShiftRight,
            Key::ControlLeft,
            Key::ControlRight,
            Key::MetaLeft,
            Key::MetaRight,
            Key::Alt,
            Key::AltGr,
        ];
        for key in &modifiers {
            assert!(is_modifier(key), "{:?} should be a modifier", key);
        }
    }

    #[test]
    fn test_is_modifier_false_for_non_modifiers() {
        let non_modifiers = [
            Key::KeyA,
            Key::Num1,
            Key::Space,
            Key::Return,
            Key::Escape,
            Key::F1,
            Key::CapsLock,
            Key::NumLock,
            Key::Tab,
            Key::Kp0,
        ];
        for key in &non_modifiers {
            assert!(!is_modifier(key), "{:?} should NOT be a modifier", key);
        }
    }

    // ===================================================================
    // 3. is_numpad_rdev_key  — numpad key classification
    // ===================================================================

    #[test]
    fn test_is_numpad_rdev_key_true() {
        let numpad_keys = [
            Key::Kp0,
            Key::Kp1,
            Key::Kp2,
            Key::Kp3,
            Key::Kp4,
            Key::Kp5,
            Key::Kp6,
            Key::Kp7,
            Key::Kp8,
            Key::Kp9,
            Key::KpMinus,
            Key::KpMultiply,
            Key::KpDivide,
            Key::KpPlus,
            Key::KpDecimal,
        ];
        for key in &numpad_keys {
            assert!(
                is_numpad_rdev_key(key),
                "{:?} should be a numpad key",
                key
            );
        }
    }

    #[test]
    fn test_is_numpad_rdev_key_false() {
        let non_numpad = [
            Key::Num0,
            Key::Num1,
            Key::KeyA,
            Key::Return,
            Key::KpReturn,
            Key::Space,
        ];
        for key in &non_numpad {
            assert!(
                !is_numpad_rdev_key(key),
                "{:?} should NOT be a numpad key",
                key
            );
        }
    }

    // ===================================================================
    // 4. is_letter_rdev_key  — A-Z classification
    // ===================================================================

    #[test]
    fn test_is_letter_rdev_key_all_letters() {
        let letters = [
            Key::KeyA, Key::KeyB, Key::KeyC, Key::KeyD, Key::KeyE, Key::KeyF,
            Key::KeyG, Key::KeyH, Key::KeyI, Key::KeyJ, Key::KeyK, Key::KeyL,
            Key::KeyM, Key::KeyN, Key::KeyO, Key::KeyP, Key::KeyQ, Key::KeyR,
            Key::KeyS, Key::KeyT, Key::KeyU, Key::KeyV, Key::KeyW, Key::KeyX,
            Key::KeyY, Key::KeyZ,
        ];
        assert_eq!(letters.len(), 26);
        for key in &letters {
            assert!(is_letter_rdev_key(key), "{:?} should be a letter key", key);
        }
    }

    #[test]
    fn test_is_letter_rdev_key_false_for_digits_and_symbols() {
        assert!(!is_letter_rdev_key(&Key::Num0));
        assert!(!is_letter_rdev_key(&Key::Space));
        assert!(!is_letter_rdev_key(&Key::Comma));
        assert!(!is_letter_rdev_key(&Key::SemiColon));
    }

    // ===================================================================
    // 5. is_letter_rdev_key_ex  — extra "letter-like" keys (issue #8599)
    // ===================================================================

    #[test]
    fn test_is_letter_rdev_key_ex_true() {
        let extra = [
            Key::LeftBracket,
            Key::RightBracket,
            Key::SemiColon,
            Key::Quote,
            Key::Comma,
            Key::Dot,
        ];
        for key in &extra {
            assert!(
                is_letter_rdev_key_ex(key),
                "{:?} should be letter_ex",
                key
            );
        }
    }

    #[test]
    fn test_is_letter_rdev_key_ex_false_for_letters() {
        assert!(!is_letter_rdev_key_ex(&Key::KeyA));
        assert!(!is_letter_rdev_key_ex(&Key::KeyZ));
        assert!(!is_letter_rdev_key_ex(&Key::Num0));
    }

    // ===================================================================
    // 6. is_numpad_key / is_letter_key_4_lock_modes  — Event wrappers
    // ===================================================================

    #[test]
    fn test_is_numpad_key_event() {
        assert!(is_numpad_key(&make_key_press(Key::Kp5)));
        assert!(is_numpad_key(&make_key_release(Key::KpDecimal)));
        assert!(!is_numpad_key(&make_key_press(Key::Num5)));
        // Mouse events are not numpad keys
        assert!(!is_numpad_key(&make_event(EventType::MouseMove {
            x: 0.0,
            y: 0.0
        })));
    }

    #[test]
    fn test_is_letter_key_4_lock_modes_event() {
        // Real letter
        assert!(is_letter_key_4_lock_modes(&make_key_press(Key::KeyA)));
        // Extended letter (issue #8599)
        assert!(is_letter_key_4_lock_modes(&make_key_press(
            Key::SemiColon
        )));
        // Not a letter
        assert!(!is_letter_key_4_lock_modes(&make_key_press(Key::Kp0)));
        assert!(!is_letter_key_4_lock_modes(&make_key_press(Key::F1)));
    }

    // ===================================================================
    // 7. parse_add_lock_modes_modifiers  — bitfield to modifiers list
    // ===================================================================

    #[test]
    fn test_parse_add_lock_modes_caps_lock_on_letter() {
        let mut ke = KeyEvent::new();
        // CAPS_LOCK = bit 1 => bitmask (1 << 1) = 2
        let lock_modes = 1 << 1; // caps lock on
        parse_add_lock_modes_modifiers(&mut ke, lock_modes, false, true);
        assert_eq!(ke.modifiers.len(), 1);
        assert_eq!(
            ke.modifiers[0].enum_value(),
            Ok(ControlKey::CapsLock)
        );
    }

    #[test]
    fn test_parse_add_lock_modes_num_lock_on_numpad() {
        let mut ke = KeyEvent::new();
        // NUM_LOCK = bit 2 => bitmask (1 << 2) = 4
        let lock_modes = 1 << 2; // num lock on
        parse_add_lock_modes_modifiers(&mut ke, lock_modes, true, false);
        assert_eq!(ke.modifiers.len(), 1);
        assert_eq!(
            ke.modifiers[0].enum_value(),
            Ok(ControlKey::NumLock)
        );
    }

    #[test]
    fn test_parse_add_lock_modes_both_on_but_only_matching_key_type() {
        // Both caps and num lock on, but the key is only a letter (not numpad)
        let mut ke = KeyEvent::new();
        let lock_modes = (1 << 1) | (1 << 2);
        parse_add_lock_modes_modifiers(&mut ke, lock_modes, false, true);
        // Only CapsLock should be added because is_numpad_key=false
        assert_eq!(ke.modifiers.len(), 1);
        assert_eq!(
            ke.modifiers[0].enum_value(),
            Ok(ControlKey::CapsLock)
        );
    }

    #[test]
    fn test_parse_add_lock_modes_both_on_numpad_key() {
        // Both caps and num lock on, key is numpad (not letter)
        let mut ke = KeyEvent::new();
        let lock_modes = (1 << 1) | (1 << 2);
        parse_add_lock_modes_modifiers(&mut ke, lock_modes, true, false);
        // Only NumLock should be added because is_letter_key=false
        assert_eq!(ke.modifiers.len(), 1);
        assert_eq!(
            ke.modifiers[0].enum_value(),
            Ok(ControlKey::NumLock)
        );
    }

    #[test]
    fn test_parse_add_lock_modes_no_locks() {
        let mut ke = KeyEvent::new();
        parse_add_lock_modes_modifiers(&mut ke, 0, true, true);
        assert!(ke.modifiers.is_empty());
    }

    #[test]
    fn test_parse_add_lock_modes_irrelevant_bits_ignored() {
        // Set bits other than caps/num lock — should add nothing
        let mut ke = KeyEvent::new();
        // bit 0 and bit 3 are not CAPS_LOCK or NUM_LOCK
        parse_add_lock_modes_modifiers(&mut ke, 0b1001, true, true);
        assert!(ke.modifiers.is_empty());
    }

    // ===================================================================
    // 8. client::event_lock_screen  — pure KeyEvent constructor
    // ===================================================================

    #[test]
    fn test_event_lock_screen() {
        let ke = client::event_lock_screen();
        assert_eq!(
            ke.union,
            Some(key_event::Union::ControlKey(ControlKey::LockScreen.into()))
        );
        assert!(ke.down);
        assert_eq!(ke.mode.enum_value(), Ok(KeyboardMode::Legacy));
    }

    // ===================================================================
    // 9. client::legacy_modifiers  — modifier flag injection
    // ===================================================================

    #[test]
    fn test_legacy_modifiers_adds_alt() {
        let mut ke = KeyEvent::new();
        ke.set_chr('a' as u32);
        client::legacy_modifiers(&mut ke, true, false, false, false);
        assert_eq!(ke.modifiers.len(), 1);
        assert_eq!(ke.modifiers[0].enum_value(), Ok(ControlKey::Alt));
    }

    #[test]
    fn test_legacy_modifiers_adds_shift() {
        let mut ke = KeyEvent::new();
        ke.set_chr('a' as u32);
        client::legacy_modifiers(&mut ke, false, false, true, false);
        assert_eq!(ke.modifiers.len(), 1);
        assert_eq!(ke.modifiers[0].enum_value(), Ok(ControlKey::Shift));
    }

    #[test]
    fn test_legacy_modifiers_adds_ctrl() {
        let mut ke = KeyEvent::new();
        ke.set_chr('a' as u32);
        client::legacy_modifiers(&mut ke, false, true, false, false);
        assert_eq!(ke.modifiers.len(), 1);
        assert_eq!(ke.modifiers[0].enum_value(), Ok(ControlKey::Control));
    }

    #[test]
    fn test_legacy_modifiers_adds_meta() {
        let mut ke = KeyEvent::new();
        ke.set_chr('a' as u32);
        client::legacy_modifiers(&mut ke, false, false, false, true);
        assert_eq!(ke.modifiers.len(), 1);
        assert_eq!(ke.modifiers[0].enum_value(), Ok(ControlKey::Meta));
    }

    #[test]
    fn test_legacy_modifiers_adds_all_four() {
        let mut ke = KeyEvent::new();
        ke.set_chr('x' as u32);
        client::legacy_modifiers(&mut ke, true, true, true, true);
        assert_eq!(ke.modifiers.len(), 4);
    }

    #[test]
    fn test_legacy_modifiers_skips_if_key_is_alt() {
        // If the key event IS the Alt key, don't double-add Alt modifier
        let mut ke = KeyEvent::new();
        ke.set_control_key(ControlKey::Alt);
        client::legacy_modifiers(&mut ke, true, false, false, false);
        assert!(ke.modifiers.is_empty(), "should not add Alt when key is Alt");
    }

    #[test]
    fn test_legacy_modifiers_skips_if_key_is_ralt() {
        let mut ke = KeyEvent::new();
        ke.set_control_key(ControlKey::RAlt);
        client::legacy_modifiers(&mut ke, true, false, false, false);
        assert!(
            ke.modifiers.is_empty(),
            "should not add Alt when key is RAlt"
        );
    }

    #[test]
    fn test_legacy_modifiers_skips_if_key_is_shift() {
        let mut ke = KeyEvent::new();
        ke.set_control_key(ControlKey::Shift);
        client::legacy_modifiers(&mut ke, false, false, true, false);
        assert!(ke.modifiers.is_empty());
    }

    #[test]
    fn test_legacy_modifiers_skips_if_key_is_control() {
        let mut ke = KeyEvent::new();
        ke.set_control_key(ControlKey::Control);
        client::legacy_modifiers(&mut ke, false, true, false, false);
        assert!(ke.modifiers.is_empty());
    }

    #[test]
    fn test_legacy_modifiers_skips_if_key_is_meta() {
        let mut ke = KeyEvent::new();
        ke.set_control_key(ControlKey::Meta);
        client::legacy_modifiers(&mut ke, false, false, false, true);
        assert!(ke.modifiers.is_empty());
    }

    // ===================================================================
    // 10. is_long_press  — detects held modifier keys
    // ===================================================================

    #[test]
    fn test_is_long_press_false_for_first_press() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        let event = make_key_press(Key::ShiftLeft);
        assert!(!is_long_press(&event));
    }

    #[test]
    fn test_is_long_press_true_when_modifier_already_down() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        // Simulate ShiftLeft already being held
        {
            let mut m = MODIFIERS_STATE.lock().unwrap();
            m.insert(Key::ShiftLeft, true);
        }
        let event = make_key_press(Key::ShiftLeft);
        assert!(is_long_press(&event));

        // Clean up
        reset_modifiers_state();
    }

    #[test]
    fn test_is_long_press_false_for_release() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        {
            let mut m = MODIFIERS_STATE.lock().unwrap();
            m.insert(Key::ControlLeft, true);
        }
        // Release events should never be "long press"
        let event = make_key_release(Key::ControlLeft);
        assert!(!is_long_press(&event));

        reset_modifiers_state();
    }

    #[test]
    fn test_is_long_press_false_for_non_modifier_keys() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        // Regular keys are not tracked in MODIFIERS_STATE
        let event = make_key_press(Key::KeyA);
        assert!(!is_long_press(&event));
    }

    // ===================================================================
    // 11. update_modifiers_state  — tracks press/release of modifiers
    // ===================================================================

    #[test]
    fn test_update_modifiers_state_press_sets_true() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        let event = make_key_press(Key::Alt);
        update_modifiers_state(&event);

        let m = MODIFIERS_STATE.lock().unwrap();
        assert_eq!(*m.get(&Key::Alt).unwrap(), true);

        drop(m);
        reset_modifiers_state();
    }

    #[test]
    fn test_update_modifiers_state_release_sets_false() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        // First press
        update_modifiers_state(&make_key_press(Key::MetaLeft));
        {
            let m = MODIFIERS_STATE.lock().unwrap();
            assert_eq!(*m.get(&Key::MetaLeft).unwrap(), true);
        }
        // Then release
        update_modifiers_state(&make_key_release(Key::MetaLeft));
        {
            let m = MODIFIERS_STATE.lock().unwrap();
            assert_eq!(*m.get(&Key::MetaLeft).unwrap(), false);
        }
        reset_modifiers_state();
    }

    #[test]
    fn test_update_modifiers_state_ignores_non_modifiers() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        let event = make_key_press(Key::KeyA);
        update_modifiers_state(&event);
        // KeyA should not appear in the map (and the map should be unchanged)
        let m = MODIFIERS_STATE.lock().unwrap();
        assert!(m.get(&Key::KeyA).is_none());
        drop(m);
        reset_modifiers_state();
    }

    #[test]
    fn test_update_modifiers_state_ignores_mouse_events() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        let event = make_event(EventType::MouseMove { x: 10.0, y: 20.0 });
        update_modifiers_state(&event);
        // Nothing should change
        let m = MODIFIERS_STATE.lock().unwrap();
        for &v in m.values() {
            assert!(!v);
        }
        drop(m);
        reset_modifiers_state();
    }

    // ===================================================================
    // 12. keycode_to_rdev_key  — platform keycode to rdev::Key
    // ===================================================================

    #[test]
    fn test_keycode_to_rdev_key_known_linux_codes() {
        // On Linux, keycode_to_rdev_key delegates to rdev::linux_key_from_code.
        // rdev uses XKB keycodes (not raw evdev).
        assert_eq!(keycode_to_rdev_key(38), Key::KeyA);
        assert_eq!(keycode_to_rdev_key(56), Key::KeyB);
        assert_eq!(keycode_to_rdev_key(9), Key::Escape);
        assert_eq!(keycode_to_rdev_key(36), Key::Return);
        assert_eq!(keycode_to_rdev_key(65), Key::Space);
        assert_eq!(keycode_to_rdev_key(50), Key::ShiftLeft);
        assert_eq!(keycode_to_rdev_key(37), Key::ControlLeft);
        assert_eq!(keycode_to_rdev_key(64), Key::Alt);
    }

    #[test]
    fn test_keycode_to_rdev_key_function_keys() {
        // XKB keycodes: F1=67, F12=96
        assert_eq!(keycode_to_rdev_key(67), Key::F1);
        assert_eq!(keycode_to_rdev_key(96), Key::F12);
    }

    #[test]
    fn test_keycode_to_rdev_key_numpad() {
        // XKB keycodes: Kp0=90, Kp1=87, Kp7=79
        assert_eq!(keycode_to_rdev_key(90), Key::Kp0);
        assert_eq!(keycode_to_rdev_key(87), Key::Kp1);
        assert_eq!(keycode_to_rdev_key(79), Key::Kp7);
    }

    // ===================================================================
    // 13. is_modifier_code  — KeyEvent chr field → modifier check
    // ===================================================================

    #[test]
    fn test_is_modifier_code_true_for_modifier_linux_codes() {
        // XKB keycodes: ControlLeft=37, ShiftLeft=50, Alt=64
        let mut ke = KeyEvent::new();
        ke.set_chr(37u32);
        assert!(is_modifier_code(&ke));

        ke.set_chr(50u32);
        assert!(is_modifier_code(&ke));

        ke.set_chr(64u32);
        assert!(is_modifier_code(&ke));
    }

    #[test]
    fn test_is_modifier_code_false_for_regular_keys() {
        // XKB keycodes: KeyA=38, Space=65
        let mut ke = KeyEvent::new();
        ke.set_chr(38u32);
        assert!(!is_modifier_code(&ke));

        ke.set_chr(65u32);
        assert!(!is_modifier_code(&ke));
    }

    #[test]
    fn test_is_modifier_code_false_for_control_key_union() {
        // When the union is ControlKey (not Chr), should return false
        let mut ke = KeyEvent::new();
        ke.set_control_key(ControlKey::Alt);
        assert!(!is_modifier_code(&ke));
    }

    // ===================================================================
    // 14. map_keyboard_mode (Linux)  — key event to protobuf via position_code
    // ===================================================================

    #[test]
    fn test_map_keyboard_mode_linux_to_linux_press() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        // Linux→Linux: position_code passes through directly
        let event = make_key_press_with_position(Key::KeyA, 38);
        let results = map_keyboard_mode("linux", &event, ke);
        assert_eq!(results.len(), 1);
        assert!(results[0].down);
        // On Linux→Linux, keycode = position_code = 30
        match results[0].union {
            Some(key_event::Union::Chr(code)) => assert_eq!(code, 38),
            _ => panic!("expected Chr union, got {:?}", results[0].union),
        }
    }

    #[test]
    fn test_map_keyboard_mode_linux_to_linux_release() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        let event = make_key_release_with_position(Key::KeyA, 38);
        let results = map_keyboard_mode("linux", &event, ke);
        assert_eq!(results.len(), 1);
        assert!(!results[0].down);
    }

    #[test]
    fn test_map_keyboard_mode_returns_empty_for_mouse_events() {
        let ke = KeyEvent::new();
        let event = make_event(EventType::MouseMove { x: 0.0, y: 0.0 });
        let results = map_keyboard_mode("linux", &event, ke);
        assert!(results.is_empty());
    }

    #[test]
    fn test_map_keyboard_mode_linux_to_windows() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        // XKB 38 = KeyA -> should convert to Windows scancode (0x1E = 30)
        let event = make_key_press_with_position(Key::KeyA, 38);
        let results = map_keyboard_mode("windows", &event, ke);
        assert_eq!(results.len(), 1);
        assert!(results[0].down);
    }

    #[test]
    fn test_map_keyboard_mode_linux_to_macos() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        // XKB 38 = KeyA -> should convert to macOS kVK_ANSI_A = 0
        let event = make_key_press_with_position(Key::KeyA, 38);
        let results = map_keyboard_mode("macos", &event, ke);
        assert_eq!(results.len(), 1);
        match results[0].union {
            Some(key_event::Union::Chr(code)) => assert_eq!(code, 0),
            _ => panic!("expected Chr union for macOS KeyA"),
        }
    }

    // ===================================================================
    // 15. event_to_key_events  — integration-level: mode dispatch + lock modes
    // ===================================================================

    #[test]
    fn test_event_to_key_events_map_mode_basic() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let event = make_key_press_with_position(Key::KeyA, 38);
        let results =
            event_to_key_events("linux".to_string(), &event, KeyboardMode::Map, None);
        assert_eq!(results.len(), 1);
        assert!(results[0].down);
        assert_eq!(results[0].mode.enum_value(), Ok(KeyboardMode::Map));
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_event_to_key_events_map_mode_with_lock_modes_numpad() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        // NumLock on (bit 2), pressing Kp5 (evdev=76)
        let lock_modes = 1 << 2;
        let event = make_key_press_with_position(Key::Kp5, 84);
        let results = event_to_key_events(
            "linux".to_string(),
            &event,
            KeyboardMode::Map,
            Some(lock_modes),
        );
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .modifiers
                .iter()
                .any(|m| m.enum_value() == Ok(ControlKey::NumLock)),
            "NumLock modifier should be present for numpad key when NumLock is on"
        );
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_event_to_key_events_map_mode_with_caps_lock_letter() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        // CapsLock on (bit 1), pressing KeyA (evdev=30)
        let lock_modes = 1 << 1;
        let event = make_key_press_with_position(Key::KeyA, 38);
        let results = event_to_key_events(
            "linux".to_string(),
            &event,
            KeyboardMode::Map,
            Some(lock_modes),
        );
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .modifiers
                .iter()
                .any(|m| m.enum_value() == Ok(ControlKey::CapsLock)),
            "CapsLock modifier should be present for letter key when CapsLock is on"
        );
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_event_to_key_events_no_lock_mode_for_non_matching_key() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        // CapsLock on, but pressing a numpad key — should NOT add CapsLock
        let lock_modes = 1 << 1;
        let event = make_key_press_with_position(Key::Kp5, 84);
        let results = event_to_key_events(
            "linux".to_string(),
            &event,
            KeyboardMode::Map,
            Some(lock_modes),
        );
        assert_eq!(results.len(), 1);
        assert!(
            !results[0]
                .modifiers
                .iter()
                .any(|m| m.enum_value() == Ok(ControlKey::CapsLock)),
            "CapsLock should NOT be added for a numpad key"
        );
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_event_to_key_events_peer_string_whitespace_stripped() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        // Whitespace in peer string should be stripped
        let event = make_key_press_with_position(Key::KeyA, 38);
        let results = event_to_key_events(
            " li nux ".to_string(),
            &event,
            KeyboardMode::Map,
            None,
        );
        // Should still work as "linux" after whitespace removal
        assert_eq!(results.len(), 1);
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_event_to_key_events_tracks_to_release() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let event = make_key_press_with_position(Key::KeyA, 38);
        let _ = event_to_key_events("linux".to_string(), &event, KeyboardMode::Map, None);
        // Key should be tracked in TO_RELEASE
        {
            let tr = TO_RELEASE.lock().unwrap();
            assert!(tr.contains_key(&Key::KeyA));
        }
        // Release it
        let event = make_key_release_with_position(Key::KeyA, 38);
        let _ = event_to_key_events("linux".to_string(), &event, KeyboardMode::Map, None);
        {
            let tr = TO_RELEASE.lock().unwrap();
            assert!(!tr.contains_key(&Key::KeyA));
        }
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_event_to_key_events_release_event() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let event = make_key_release_with_position(Key::KeyA, 38);
        let results =
            event_to_key_events("linux".to_string(), &event, KeyboardMode::Map, None);
        assert_eq!(results.len(), 1);
        assert!(!results[0].down);
        reset_modifiers_state();
        reset_to_release();
    }

    // ===================================================================
    // 16. translate_keyboard_mode  — falls back to map for numpad/non-unicode
    // ===================================================================

    #[test]
    fn test_translate_mode_numpad_falls_back_to_map() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        let event = make_key_press_with_position(Key::Kp5, 84);
        let results = translate_keyboard_mode("linux", &event, ke);
        // Numpad keys use map mode in translate, so should get a result
        assert_eq!(results.len(), 1);
        assert!(results[0].down);
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_translate_mode_dead_key_returns_empty() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        let mut event = make_key_press(Key::KeyA);
        event.unicode = Some(UnicodeInfo {
            name: Some("a".to_string()),
            unicode: vec![],
            is_dead: true,
        });
        let results = translate_keyboard_mode("linux", &event, ke);
        assert!(results.is_empty(), "dead keys should be ignored");
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_translate_mode_with_unicode_sends_seq() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        let event = make_event_with_unicode(
            EventType::KeyPress(Key::KeyA),
            Some("a".to_string()),
        );
        let results = translate_keyboard_mode("linux", &event, ke);
        // Should contain a seq event for the unicode character
        let has_seq = results.iter().any(|e| e.has_seq());
        assert!(has_seq, "translate mode should produce seq event for unicode input");
        // The seq event should have down=true
        let seq_event = results.iter().find(|e| e.has_seq()).unwrap();
        assert!(seq_event.down);
        assert_eq!(seq_event.seq(), "a");
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_translate_mode_without_unicode_falls_back_to_map() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        // No unicode info, non-numpad key — should fallback to map mode
        let event = make_key_press_with_position(Key::Escape, 9);
        let results = translate_keyboard_mode("linux", &event, ke);
        assert_eq!(results.len(), 1);
        // Should have chr set (from map mode fallback)
        assert!(results[0].has_chr());
        reset_modifiers_state();
        reset_to_release();
    }

    // ===================================================================
    // 17. is_press helper (Linux only)
    // ===================================================================

    #[test]
    fn test_is_press_true_for_keypress() {
        let event = make_key_press(Key::KeyA);
        assert!(is_press(&event));
    }

    #[test]
    fn test_is_press_false_for_key_release() {
        let event = make_key_release(Key::KeyA);
        assert!(!is_press(&event));
    }

    #[test]
    fn test_is_press_false_for_mouse_event() {
        let event = make_event(EventType::MouseMove { x: 0.0, y: 0.0 });
        assert!(!is_press(&event));
    }

    // ===================================================================
    // 18. is_altgr (Linux-specific)
    // ===================================================================

    #[test]
    fn test_is_altgr_linux_true_for_0xfe03() {
        let mut event = make_key_press(Key::AltGr);
        event.platform_code = 0xFE03;
        assert!(is_altgr(&event));
    }

    #[test]
    fn test_is_altgr_linux_false_for_other_codes() {
        let mut event = make_key_press(Key::Alt);
        event.platform_code = 0xFFE9; // regular Alt keysym
        assert!(!is_altgr(&event));
    }

    // ===================================================================
    // 19. client::get_modifiers_state  — combines global + local flags
    // ===================================================================

    #[test]
    fn test_get_modifiers_state_passthrough_when_global_false() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        // When global state is all false, function returns local flags
        let (alt, ctrl, shift, cmd) = client::get_modifiers_state(true, true, true, true);
        assert!(alt);
        assert!(ctrl);
        assert!(shift);
        assert!(cmd);
        reset_modifiers_state();
    }

    #[test]
    fn test_get_modifiers_state_global_overrides() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        {
            let mut m = MODIFIERS_STATE.lock().unwrap();
            m.insert(Key::ControlLeft, true);
        }
        // Even though local ctrl=false, global ControlLeft=true makes it true
        let (alt, ctrl, shift, cmd) =
            client::get_modifiers_state(false, false, false, false);
        assert!(!alt);
        assert!(ctrl);
        assert!(!shift);
        assert!(!cmd);
        reset_modifiers_state();
    }

    #[test]
    fn test_get_modifiers_state_right_variants() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        {
            let mut m = MODIFIERS_STATE.lock().unwrap();
            m.insert(Key::ShiftRight, true);
            m.insert(Key::ControlRight, true);
            m.insert(Key::MetaRight, true);
            m.insert(Key::AltGr, true);
        }
        let (alt, ctrl, shift, cmd) =
            client::get_modifiers_state(false, false, false, false);
        assert!(alt, "AltGr should count as alt");
        assert!(ctrl, "ControlRight should count as ctrl");
        assert!(shift, "ShiftRight should count as shift");
        assert!(cmd, "MetaRight should count as command");
        reset_modifiers_state();
    }

    // ===================================================================
    // 20. Edge cases and regression-like tests
    // ===================================================================

    #[test]
    fn test_map_mode_empty_for_unknown_conversion() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        // Use position_code 0 which may not map to anything on some platforms
        // For Linux→macOS or Linux→Windows, 0 might not convert
        let event = make_key_press_with_position(Key::Unknown(0), 0);
        let results = map_keyboard_mode("macos", &event, ke);
        // 0 may or may not convert — we just check it doesn't panic
        // The result is either empty (conversion failed) or has 1 event
        assert!(results.len() <= 1);
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_event_to_key_events_mouse_event_returns_empty() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let event = make_event(EventType::MouseMove { x: 5.0, y: 10.0 });
        let results =
            event_to_key_events("linux".to_string(), &event, KeyboardMode::Map, None);
        assert!(results.is_empty());
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_translate_mode_lock_modes_added_for_numpad_keys() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        // In translate mode, lock modes ARE added for numpad keys
        let lock_modes = 1 << 2; // NumLock
        let event = make_key_press_with_position(Key::Kp5, 84);
        let results = event_to_key_events(
            "linux".to_string(),
            &event,
            KeyboardMode::Translate,
            Some(lock_modes),
        );
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .modifiers
                .iter()
                .any(|m| m.enum_value() == Ok(ControlKey::NumLock)),
            "Translate mode should add NumLock for numpad keys"
        );
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_translate_mode_lock_modes_not_added_for_letter_keys() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        // In translate mode, lock modes should NOT be added for non-numpad keys
        // because `keyboard_mode != KeyboardMode::Translate || is_numpad_key` gates it
        let lock_modes = 1 << 1; // CapsLock
        let event = make_event_with_unicode(
            EventType::KeyPress(Key::KeyA),
            Some("a".to_string()),
        );
        let results = event_to_key_events(
            "linux".to_string(),
            &event,
            KeyboardMode::Translate,
            Some(lock_modes),
        );
        // For translate mode with non-numpad key, lock modes should not be applied
        for r in &results {
            assert!(
                !r.modifiers
                    .iter()
                    .any(|m| m.enum_value() == Ok(ControlKey::CapsLock)),
                "Translate mode should NOT add CapsLock for letter keys"
            );
        }
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_map_mode_linux_to_android() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();
        let ke = KeyEvent::new();
        // Evdev 30 = KeyA → should convert to Android key code
        let event = make_key_press_with_position(Key::KeyA, 38);
        let results = map_keyboard_mode("android", &event, ke);
        // Should produce a result if the conversion exists
        assert_eq!(results.len(), 1);
        assert!(results[0].down);
        reset_modifiers_state();
        reset_to_release();
    }

    #[test]
    fn test_multiple_modifier_tracking_lifecycle() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_modifiers_state();
        reset_to_release();

        // Press Ctrl, then Shift
        update_modifiers_state(&make_key_press(Key::ControlLeft));
        update_modifiers_state(&make_key_press(Key::ShiftLeft));
        {
            let m = MODIFIERS_STATE.lock().unwrap();
            assert!(*m.get(&Key::ControlLeft).unwrap());
            assert!(*m.get(&Key::ShiftLeft).unwrap());
        }

        // Release Ctrl but Shift still held
        update_modifiers_state(&make_key_release(Key::ControlLeft));
        {
            let m = MODIFIERS_STATE.lock().unwrap();
            assert!(!m.get(&Key::ControlLeft).unwrap());
            assert!(*m.get(&Key::ShiftLeft).unwrap());
        }

        // Release Shift
        update_modifiers_state(&make_key_release(Key::ShiftLeft));
        {
            let m = MODIFIERS_STATE.lock().unwrap();
            assert!(!m.get(&Key::ShiftLeft).unwrap());
        }

        reset_modifiers_state();
    }

    #[test]
    fn test_parse_add_lock_modes_caps_and_num_both_active_for_both_key_types() {
        // If a key is somehow both numpad and letter (hypothetical), both modifiers added
        let mut ke = KeyEvent::new();
        let lock_modes = (1 << 1) | (1 << 2);
        parse_add_lock_modes_modifiers(&mut ke, lock_modes, true, true);
        assert_eq!(ke.modifiers.len(), 2);
        let has_caps = ke
            .modifiers
            .iter()
            .any(|m| m.enum_value() == Ok(ControlKey::CapsLock));
        let has_num = ke
            .modifiers
            .iter()
            .any(|m| m.enum_value() == Ok(ControlKey::NumLock));
        assert!(has_caps);
        assert!(has_num);
    }
}

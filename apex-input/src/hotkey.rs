use crate::Command;
use anyhow::{bail, Result};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use log::{info, warn};
use std::collections::HashSet;
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub struct HotkeyBindings {
    pub previous: String,
    pub next: String,
    pub lock_toggle: String,
}

impl Default for HotkeyBindings {
    fn default() -> Self {
        Self {
            previous: "Ctrl+Shift+Numpad *".to_string(),
            next: "Ctrl+Shift+Numpad /".to_string(),
            lock_toggle: "Ctrl+Shift+Numpad -".to_string(),
        }
    }
}

pub struct InputManager {
    _hkm: GlobalHotKeyManager,
}

impl InputManager {
    pub fn new(sender: broadcast::Sender<Command>, bindings: HotkeyBindings) -> Result<Self> {
        if kde_wayland_session() {
            if register_kde_hotkeys(sender.clone(), &bindings) {
                // Keep a GlobalHotKeyManager value only to preserve the public
                // shape of InputManager; KDE owns the actual registrations.
                let hkm = GlobalHotKeyManager::new()?;
                return Ok(Self { _hkm: hkm });
            }
            warn!("KDE global hotkey registration failed; falling back to X11 backend");
        }

        let hkm = GlobalHotKeyManager::new()?;

        let requested = [
            (
                "previous",
                bindings.previous.as_str(),
                Command::PreviousSource,
            ),
            ("next", bindings.next.as_str(), Command::NextSource),
            (
                "lock_toggle",
                bindings.lock_toggle.as_str(),
                Command::ToggleLockSource,
            ),
        ];

        let mut registrations: Vec<(HotKey, Command, &'static str, String)> = Vec::new();
        let mut ids = HashSet::new();
        for (label, spec, command) in requested {
            match parse_optional_hotkey(spec) {
                Ok(Some(hotkey)) => {
                    if !ids.insert(hotkey.id()) {
                        warn!("hotkey '{label}' ignored: duplicate combo '{spec}'");
                        continue;
                    }
                    registrations.push((hotkey, command, label, spec.to_string()));
                }
                Ok(None) => {
                    info!("hotkey '{label}' disabled");
                }
                Err(e) => {
                    warn!("hotkey '{label}' ignored: invalid combo '{spec}': {e}");
                }
            }
        }

        let mut active: Vec<(HotKey, Command)> = Vec::new();
        for (hotkey, command, label, spec) in registrations {
            match hkm.register(hotkey) {
                Ok(()) => {
                    info!("hotkey '{label}' registered via X11 backend: {spec}");
                    active.push((hotkey, command));
                }
                Err(e) => {
                    warn!("hotkey '{label}' failed to register '{spec}': {e}");
                }
            }
        }

        if active.is_empty() {
            warn!("no hotkeys registered; all shortcuts are disabled or unavailable");
        }

        let hotkey_handler = move |event: GlobalHotKeyEvent| {
            let Some((_, cmd)) = active.iter().find(|(hotkey, _)| event.id == hotkey.id()) else {
                return;
            };
            info!("hotkey fired: {:?}", cmd);
            sender.send(*cmd).expect("Failed to send command!");
        };

        GlobalHotKeyEvent::set_event_handler(Some(hotkey_handler));

        Ok(Self { _hkm: hkm })
    }
}

fn kde_wayland_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.to_ascii_lowercase().contains("kde"))
        .unwrap_or(false)
        && std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

fn register_kde_hotkeys(sender: broadcast::Sender<Command>, bindings: &HotkeyBindings) -> bool {
    let requested = [
        (
            "previous",
            "Previous provider",
            bindings.previous.as_str(),
            Command::PreviousSource,
        ),
        (
            "next",
            "Next provider",
            bindings.next.as_str(),
            Command::NextSource,
        ),
        (
            "lock_toggle",
            "Lock / unlock provider",
            bindings.lock_toggle.as_str(),
            Command::ToggleLockSource,
        ),
    ];

    let mut active: Vec<(String, Command)> = Vec::new();
    let conn = match dbus::blocking::Connection::new_session() {
        Ok(conn) => conn,
        Err(e) => {
            warn!("KDE hotkeys: DBus connect failed: {e}");
            return false;
        }
    };
    let proxy = conn.with_proxy(
        "org.kde.KWin",
        "/kglobalaccel",
        std::time::Duration::from_secs(3),
    );

    for (action, friendly, spec, command) in requested {
        let action_id = vec![
            "ss_oled".to_string(),
            action.to_string(),
            "ss-oled".to_string(),
            friendly.to_string(),
        ];
        // Clear any stale registration left behind by a previous run before
        // re-registering. kglobalaccel keeps a persistent registry: if a prior
        // process died without unregistering, its component entry survives and
        // doRegister lands a *second* entry. The duplicate makes
        // /component/ss_oled report isActive=false, so the key is never grabbed
        // and no globalShortcutPressed signal is ever delivered.
        let _ = proxy.method_call::<(), _, _, _>(
            "org.kde.KGlobalAccel",
            "unRegister",
            (action_id.clone(),),
        );

        if let Err(e) = proxy.method_call::<(), _, _, _>(
            "org.kde.KGlobalAccel",
            "doRegister",
            (action_id.clone(),),
        ) {
            warn!("KDE hotkeys: failed to register action '{action}': {e}");
            continue;
        }

        match parse_optional_qt_hotkey(spec) {
            Ok(Some(seq)) => {
                let keys: Vec<(Vec<i32>,)> = vec![(vec![seq],)];
                match proxy.method_call::<(Vec<(Vec<i32>,)>,), _, _, _>(
                    "org.kde.KGlobalAccel",
                    "setShortcutKeys",
                    // SetPresent (2) activates the shortcut; NoAutoloading (4)
                    // alone only saves it and leaves it inactive.
                    (action_id, keys, 2_u32 | 4_u32),
                ) {
                    Ok((actual,)) if !actual.is_empty() => {
                        info!("hotkey '{action}' registered via KDE: {spec}");
                        active.push((action.to_string(), command));
                    }
                    Ok(_) => {
                        warn!("KDE hotkeys: '{action}' rejected/unassigned for combo '{spec}'");
                    }
                    Err(e) => {
                        warn!("KDE hotkeys: failed to set '{action}' = '{spec}': {e}");
                    }
                }
            }
            Ok(None) => {
                let _ = proxy.method_call::<(Vec<(Vec<i32>,)>,), _, _, _>(
                    "org.kde.KGlobalAccel",
                    "setShortcutKeys",
                    (action_id, Vec::<(Vec<i32>,)>::new(), 4_u32),
                );
                info!("hotkey '{action}' disabled");
            }
            Err(e) => warn!("KDE hotkeys: invalid combo for '{action}' = '{spec}': {e}"),
        }
    }

    if active.is_empty() {
        warn!("KDE hotkeys: no shortcuts active");
        return true;
    }

    tokio::spawn(subscribe_kde_hotkeys(sender, active));
    true
}

async fn subscribe_kde_hotkeys(sender: broadcast::Sender<Command>, active: Vec<(String, Command)>) {
    use dbus::{
        arg::messageitem::MessageItem,
        message::{MatchRule, MessageType},
    };
    use dbus_tokio::connection;
    use futures::stream::StreamExt;

    let (resource, conn) = match connection::new_session_sync() {
        Ok(p) => p,
        Err(e) => {
            warn!("KDE hotkeys: listener DBus connect failed: {e}");
            return;
        }
    };
    tokio::spawn(async move {
        let err = resource.await;
        panic!("KDE hotkeys: lost DBus connection: {err}");
    });

    let mr = MatchRule::new()
        .with_type(MessageType::Signal)
        .with_path("/component/ss_oled")
        .with_interface("org.kde.kglobalaccel.Component")
        .with_member("globalShortcutPressed");
    let msg_match = match conn.add_match(mr).await {
        Ok(m) => m,
        Err(e) => {
            warn!("KDE hotkeys: add_match failed: {e}");
            return;
        }
    };
    info!("KDE hotkeys: listener registered");
    let (_msg_match, mut msg_stream) = msg_match.msg_stream();

    while let Some(msg) = msg_stream.next().await {
        let mut strs = Vec::new();
        for item in msg.get_items() {
            if let MessageItem::Str(s) = item {
                strs.push(s);
                if strs.len() == 2 {
                    break;
                }
            }
        }
        let component = strs.get(0).map(|s| s.as_str()).unwrap_or("");
        let action = strs.get(1).map(|s| s.as_str()).unwrap_or("");
        if component != "ss_oled" {
            continue;
        }
        let Some((_, command)) = active.iter().find(|(name, _)| name == action) else {
            continue;
        };
        info!("hotkey fired: {:?}", command);
        let _ = sender.send(*command);
    }
    warn!("KDE hotkeys: listener stream ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_plus_binding_is_accepted() {
        assert_eq!(parse_qt_hotkey("Ctrl+Shift++").unwrap(), 0x0600_002b);
        assert!(parse_hotkey("Ctrl+Shift++").is_ok());
    }

    #[test]
    fn keypad_plus_binding_is_accepted() {
        assert_eq!(parse_qt_hotkey("Ctrl+Numpad +").unwrap(), 0x2400_002b);
        assert!(parse_hotkey("Ctrl+Numpad +").is_ok());
    }

    #[test]
    fn keypad_digit_spellings() {
        // Both spellings must produce the same encoding: KeypadModifier|Ctrl|
        // Alt (0x2000_0000|0x0800_0000|0x0400_0000) + Qt::Key_1 (0x31).
        // The GUI's hint text uses the spaced form, so it must parse.
        assert_eq!(parse_qt_hotkey("Ctrl+Alt+Numpad1").unwrap(), 0x2c00_0031);
        assert_eq!(parse_qt_hotkey("Ctrl+Alt+Numpad 1").unwrap(), 0x2c00_0031);
        // Numpad 0 / 2 / 3 likewise, for the shipped bindings.
        assert_eq!(parse_qt_hotkey("Ctrl+Alt+Numpad 2").unwrap(), 0x2c00_0032);
        assert_eq!(parse_qt_hotkey("Ctrl+Alt+Numpad3").unwrap(), 0x2c00_0033);
        assert_eq!(parse_qt_hotkey("Ctrl+Alt+Numpad 0").unwrap(), 0x2c00_0030);
    }
}

fn parse_optional_qt_hotkey(input: &str) -> Result<Option<i32>> {
    let trimmed = input.trim();
    if trimmed.is_empty() || matches!(trimmed.to_ascii_lowercase().as_str(), "none" | "disabled") {
        return Ok(None);
    }
    parse_qt_hotkey(trimmed).map(Some)
}

fn parse_qt_hotkey(input: &str) -> Result<i32> {
    let mut value = 0_i32;
    let mut key: Option<i32> = None;
    let mut keypad = false;
    // Preserve a literal trailing '+' before splitting modifier separators.
    let input = input.trim();
    let normalized = input
        .strip_suffix('+')
        .map(|prefix| format!("{prefix}plus"));
    let input = normalized.as_deref().unwrap_or(input);
    for raw_part in input.split('+') {
        let part = raw_part.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => value |= 0x0400_0000,
            "shift" => value |= 0x0200_0000,
            "alt" | "option" => value |= 0x0800_0000,
            "super" | "meta" | "win" | "cmd" | "command" => value |= 0x1000_0000,
            other => {
                if key.is_some() {
                    bail!("multiple non-modifier keys in '{input}'");
                }
                let (code, is_keypad) = qt_key_code(other)?;
                key = Some(code);
                keypad = is_keypad;
            }
        }
    }
    let Some(key) = key else {
        bail!("missing key code");
    };
    if keypad {
        value |= 0x2000_0000; // Qt::KeypadModifier
    }
    Ok(value | key)
}

fn qt_key_code(part: &str) -> Result<(i32, bool)> {
    Ok(match part {
        "/" | "slash" => (0x2f, false),
        "numpad/" | "numpad /" | "numpaddivide" | "numpad divide" => (0x2f, true),
        "*" => (0x2a, false),
        "numpad*" | "numpad *" | "numpadmultiply" | "numpad multiply" => (0x2a, true),
        "-" | "minus" => (0x2d, false),
        "numpad-" | "numpad -" | "numpadsubtract" | "numpad subtract" => (0x2d, true),
        "+" | "plus" => (0x2b, false),
        "numpad+" | "numpad +" | "numpadplus" | "numpad plus" | "numpadadd" | "numpad add" => {
            (0x2b, true)
        }
        "enter" => (0x0100_0005, false),
        "numpadenter" | "numpad enter" => (0x0100_0005, true),
        "space" => (0x20, false),
        "tab" => (0x0100_0001, false),
        "escape" | "esc" => (0x0100_0000, false),
        "arrowup" | "up" => (0x0100_0013, false),
        "arrowdown" | "down" => (0x0100_0015, false),
        "arrowleft" | "left" => (0x0100_0012, false),
        "arrowright" | "right" => (0x0100_0014, false),
        "f1" => (0x0100_0030, false),
        "f2" => (0x0100_0031, false),
        "f3" => (0x0100_0032, false),
        "f4" => (0x0100_0033, false),
        "f5" => (0x0100_0034, false),
        "f6" => (0x0100_0035, false),
        "f7" => (0x0100_0036, false),
        "f8" => (0x0100_0037, false),
        "f9" => (0x0100_0038, false),
        "f10" => (0x0100_0039, false),
        "f11" => (0x0100_003a, false),
        "f12" => (0x0100_003b, false),
        // Accept both "Numpad1" and "Numpad 1". egui cannot distinguish a
        // numpad key from its top-row twin (Key::Num1 is documented as
        // "Either from the main row or from the numpad"), so the GUI's own
        // hint text uses the spaced spelling and it must parse.
        s if s.starts_with("numpad") && s.len() == "numpad".len() + 1 => {
            let ch = s.chars().last().unwrap();
            if ch.is_ascii_digit() {
                (ch as i32, true)
            } else {
                bail!("unsupported key '{part}'")
            }
        }
        s if s.starts_with("numpad ") && s.len() == "numpad ".len() + 1 => {
            let ch = s.chars().last().unwrap();
            if ch.is_ascii_digit() {
                (ch as i32, true)
            } else {
                bail!("unsupported key '{part}'")
            }
        }
        s if s.len() == 1 => {
            let ch = s.chars().next().unwrap();
            if ch.is_ascii_alphanumeric() {
                (ch.to_ascii_uppercase() as i32, false)
            } else {
                bail!("unsupported key '{part}'")
            }
        }
        _ => bail!("unsupported key '{part}'"),
    })
}

fn parse_optional_hotkey(input: &str) -> Result<Option<HotKey>> {
    let trimmed = input.trim();
    if trimmed.is_empty() || matches!(trimmed.to_ascii_lowercase().as_str(), "none" | "disabled") {
        return Ok(None);
    }
    parse_hotkey(trimmed).map(Some)
}

fn parse_hotkey(input: &str) -> Result<HotKey> {
    let mut modifiers = Modifiers::empty();
    let mut code: Option<Code> = None;

    // Preserve a literal trailing '+' before splitting modifier separators.
    let input = input.trim();
    let normalized = input
        .strip_suffix('+')
        .map(|prefix| format!("{prefix}plus"));
    let input = normalized.as_deref().unwrap_or(input);
    for raw_part in input.split('+') {
        let part = raw_part.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= Modifiers::CONTROL,
            "shift" => modifiers |= Modifiers::SHIFT,
            "alt" | "option" => modifiers |= Modifiers::ALT,
            "super" | "meta" | "win" | "cmd" | "command" => modifiers |= Modifiers::SUPER,
            other => {
                if code.is_some() {
                    bail!("multiple non-modifier keys in '{input}'");
                }
                code = Some(parse_code(other)?);
            }
        }
    }

    let Some(code) = code else {
        bail!("missing key code");
    };
    let modifiers = if modifiers.is_empty() {
        None
    } else {
        Some(modifiers)
    };
    Ok(HotKey::new(modifiers, code))
}

fn parse_code(part: &str) -> Result<Code> {
    Ok(match part {
        "/" | "slash" => Code::Slash,
        "numpad/" | "numpad /" | "numpaddivide" | "numpad divide" => Code::NumpadDivide,
        "*" | "numpad*" | "numpad *" | "numpadmultiply" | "numpad multiply" => Code::NumpadMultiply,
        "-" | "minus" => Code::Minus,
        "numpad-" | "numpad -" | "numpadsubtract" | "numpad subtract" => Code::NumpadSubtract,
        "+" | "plus" => Code::Equal,
        "numpad+" | "numpad +" | "numpadplus" | "numpad plus" | "numpadadd" | "numpad add" => {
            Code::NumpadAdd
        }
        "enter" => Code::Enter,
        "space" => Code::Space,
        "tab" => Code::Tab,
        "escape" | "esc" => Code::Escape,
        "arrowup" | "up" => Code::ArrowUp,
        "arrowdown" | "down" => Code::ArrowDown,
        "arrowleft" | "left" => Code::ArrowLeft,
        "arrowright" | "right" => Code::ArrowRight,
        "f1" => Code::F1,
        "f2" => Code::F2,
        "f3" => Code::F3,
        "f4" => Code::F4,
        "f5" => Code::F5,
        "f6" => Code::F6,
        "f7" => Code::F7,
        "f8" => Code::F8,
        "f9" => Code::F9,
        "f10" => Code::F10,
        "f11" => Code::F11,
        "f12" => Code::F12,
        "a" => Code::KeyA,
        "b" => Code::KeyB,
        "c" => Code::KeyC,
        "d" => Code::KeyD,
        "e" => Code::KeyE,
        "f" => Code::KeyF,
        "g" => Code::KeyG,
        "h" => Code::KeyH,
        "i" => Code::KeyI,
        "j" => Code::KeyJ,
        "k" => Code::KeyK,
        "l" => Code::KeyL,
        "m" => Code::KeyM,
        "n" => Code::KeyN,
        "o" => Code::KeyO,
        "p" => Code::KeyP,
        "q" => Code::KeyQ,
        "r" => Code::KeyR,
        "s" => Code::KeyS,
        "t" => Code::KeyT,
        "u" => Code::KeyU,
        "v" => Code::KeyV,
        "w" => Code::KeyW,
        "x" => Code::KeyX,
        "y" => Code::KeyY,
        "z" => Code::KeyZ,
        "0" => Code::Digit0,
        "1" => Code::Digit1,
        "2" => Code::Digit2,
        "3" => Code::Digit3,
        "4" => Code::Digit4,
        "5" => Code::Digit5,
        "6" => Code::Digit6,
        "7" => Code::Digit7,
        "8" => Code::Digit8,
        "9" => Code::Digit9,
        _ => bail!("unsupported key '{part}'"),
    })
}

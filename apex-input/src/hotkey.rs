use crate::Command;
use anyhow::Result;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use tokio::sync::broadcast;

pub struct InputManager {
    _hkm: GlobalHotKeyManager,
}

impl InputManager {
    pub fn new(sender: broadcast::Sender<Command>) -> Result<Self> {
        let hkm = GlobalHotKeyManager::new().unwrap();

        let modifiers = Some(Modifiers::ALT | Modifiers::CONTROL);

        // User-requested bindings (all with Ctrl+Alt):
        //   Numpad/  next provider
        //   Numpad*  previous provider
        //   Numpad-  lock provider (no auto-rotate; / and * still move)
        //   Numpad+  unlock provider (resume auto-rotate)
        let hotkey_previous = HotKey::new(modifiers, Code::NumpadMultiply);
        let hotkey_next = HotKey::new(modifiers, Code::NumpadDivide);
        let hotkey_lock = HotKey::new(modifiers, Code::NumpadSubtract);
        let hotkey_unlock = HotKey::new(modifiers, Code::NumpadAdd);

        hkm.register(hotkey_previous).unwrap();
        hkm.register(hotkey_next).unwrap();
        hkm.register(hotkey_lock).unwrap();
        hkm.register(hotkey_unlock).unwrap();

        let hotkey_handler = move |event: GlobalHotKeyEvent| {
            let cmd = if event.id == hotkey_previous.id() {
                Command::PreviousSource
            } else if event.id == hotkey_next.id() {
                Command::NextSource
            } else if event.id == hotkey_lock.id() {
                Command::LockSource
            } else if event.id == hotkey_unlock.id() {
                Command::UnlockSource
            } else {
                return;
            };
            sender.send(cmd).expect("Failed to send command!");
        };

        GlobalHotKeyEvent::set_event_handler(Some(hotkey_handler));

        Ok(Self { _hkm: hkm })
    }
}

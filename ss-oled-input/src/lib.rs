#[cfg(feature = "hotkeys")]
mod hotkey;
mod input;
#[cfg(feature = "hotkeys")]
pub use hotkey::{crate_hotkey_control::HotkeyControl, parse_optional_qt_hotkey, registry_handle, HotkeyBindings, InputManager};
pub use input::Command;

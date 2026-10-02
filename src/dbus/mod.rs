#[cfg(feature = "dbus-support")]
pub(crate) mod notifications;
// General desktop notifications: any app's org.freedesktop.Notifications.Notify,
// with no source filter. Gate on dbus-support only — it renders text, and does
// not decode images (see load_icon in that file for why app icons are absent).
#[cfg(feature = "dbus-support")]
pub(crate) mod notifications_general;

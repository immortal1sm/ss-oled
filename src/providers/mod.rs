pub(crate) mod clock;

#[cfg(feature = "image")]
pub(crate) mod image;
#[cfg(any(feature = "dbus-support", target_os = "windows"))]
pub(crate) mod music;
#[cfg(feature = "sysinfo")]
pub(crate) mod sysinfo;
#[cfg(feature = "weather")]
pub(crate) mod weather_data;
#[cfg(feature = "weather")]
pub(crate) mod weather_icons;
#[cfg(feature = "weather")]
pub(crate) mod weather;
#[cfg(all(feature = "weather", feature = "image"))]
pub(crate) mod forecast;

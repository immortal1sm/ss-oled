#![allow(incomplete_features)]
#![feature(inherent_associated_types, impl_trait_in_assoc_type)]
#![warn(clippy::pedantic)]
// `clippy::mut_mut` is disabled because `futures::stream::select!` causes the lint to fire
// The other lints are just awfully tedious to implement especially when dealing with pixel
// coordinates. I'll fix them if I'm ever that bored.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
#![deny(
    missing_debug_implementations,
    nonstandard_style,
    missing_copy_implementations,
    unused_qualifications
)]

extern crate embedded_graphics;

use anyhow::Result;
use log::warn;

// This is kind of pointless on non-Linux platforms
#[cfg(all(feature = "dbus-support", target_os = "linux"))]
mod dbus;
mod ipc;

mod providers;
mod render;

#[cfg(all(feature = "simulator", feature = "usb"))]
compile_error!(
    "The features `simulator` and `usb` are mutually exclusive. Use --no-default-features!"
);

#[cfg(feature = "simulator")]
use apex_simulator::Simulator;

use crate::render::{scheduler, scheduler::Scheduler};
#[cfg(feature = "engine")]
use apex_engine::Engine;
use apex_hardware::AsyncDevice;
#[cfg(all(feature = "usb", target_os = "linux", not(feature = "engine")))]
#[cfg(all(feature = "usb", target_family = "unix", not(feature = "engine")))]
use apex_hardware::ReconnectingDevice;
use log::{info, LevelFilter};
use simplelog::{Config as LoggerConfig, SimpleLogger};
use tokio::sync::broadcast;

use apex_input::Command;

#[tokio::main]
#[allow(clippy::missing_errors_doc)]
#[allow(clippy::missing_panics_doc)]
pub async fn main() -> Result<()> {
    // Log level comes from `log.level` in settings.toml (settable in the GUI),
    // falling back to the APEX_LOG environment variable, then Info.
    //
    // The settings file is peeked here rather than via the merged `settings`
    // below because the logger has to be initialised before anything else
    // logs. The GUI restarts the daemon on Apply, so writing the key is enough
    // to change it.
    let mut logger_config = config::Config::default();
    if let Some(dir) = dirs::config_dir() {
        let _ = logger_config.merge(
            config::File::with_name(&dir.join("apex-tux/settings").to_string_lossy())
                .required(false),
        );
    }
    let level_name = logger_config
        .get_str("log.level")
        .ok()
        .filter(|s| !s.is_empty())
        .or(std::env::var("APEX_LOG").ok())
        .unwrap_or_else(|| "info".to_string())
        .to_ascii_lowercase();
    let level = match level_name.as_str() {
        "debug" => LevelFilter::Debug,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        "off" => LevelFilter::Off,
        _ => LevelFilter::Info,
    };
    SimpleLogger::init(level, LoggerConfig::default())?;
    log::info!("log level: {level_name}");

    // This channel is used to send commands to the scheduler
    let (tx, rx) = broadcast::channel::<Command>(100);
    // ReconnectingDevice rather than USBDevice: it starts disconnected and
    // attaches when the panel appears, instead of making an absent display a
    // fatal startup error (which with Restart=on-failure became a crash loop).
    #[cfg(all(feature = "usb", target_family = "unix", not(feature = "engine")))]
    let mut device = ReconnectingDevice::new();

    #[cfg(feature = "engine")]
    let mut device = Engine::new().await?;

    let mut settings = config::Config::default();
    // Add in `$USER_CONFIG_DIR/apex-tux/settings.toml`
    if let Some(user_config_dir) = dirs::config_dir() {
        settings.merge(
            config::File::with_name(&user_config_dir.join("apex-tux/settings").to_string_lossy())
                .required(false),
        )?;
    }
    settings
        // Add in `./settings.toml`
        .merge(config::File::with_name("settings").required(false))?
        // Add in settings from the environment (with a prefix of APEX)
        // Eg.. `APEX_DEBUG=1 ./target/app` would set the `debug` key
        .merge(config::Environment::with_prefix("APEX_"))?;

    #[cfg(feature = "hotkeys")]
    let hkm = {
        let defaults = apex_input::HotkeyBindings::default();
        let bindings = apex_input::HotkeyBindings {
            previous: settings
                .get_str("hotkeys.previous")
                .unwrap_or(defaults.previous),
            next: settings.get_str("hotkeys.next").unwrap_or(defaults.next),
            lock_toggle: settings
                .get_str("hotkeys.lock_toggle")
                .or_else(|_| settings.get_str("hotkeys.lock"))
                .unwrap_or(defaults.lock_toggle),
            item_next: settings
                .get_str("hotkeys.item_next")
                .unwrap_or(defaults.item_next),
            item_previous: settings
                .get_str("hotkeys.item_previous")
                .unwrap_or(defaults.item_previous),
            scroll_up: settings.get_str("hotkeys.scroll_up").unwrap_or(defaults.scroll_up),
            scroll_down: settings
                .get_str("hotkeys.scroll_down")
                .unwrap_or(defaults.scroll_down),
            detail_toggle: settings
                .get_str("hotkeys.detail_toggle")
                .unwrap_or(defaults.detail_toggle),
        };
        match apex_input::InputManager::new(tx.clone(), bindings) {
            Ok(manager) => Some(manager),
            Err(e) => {
                log::warn!("hotkeys unavailable: {e}");
                None
            }
        }
    };

    #[cfg(feature = "simulator")]
    let mut device = Simulator::connect(tx.clone());

    device.clear().await?;

    let mut scheduler = Scheduler::new(device);
    scheduler.start(tx.clone(), rx, settings).await?;

    ctrlc::set_handler(move || {
        info!("Ctrl + C received, shutting down!");
        tx.send(Command::Shutdown)
            .expect("Failed to send shutdown signal!");
    })?;

    #[cfg(feature = "hotkeys")]
    drop(hkm);

    Ok(())
}

use anyhow::{anyhow, Result};
use std::{
    marker::PhantomData,
    pin::Pin,
    time::{Duration, Instant},
};

use crate::render::{
    display::ContentProvider,
    notifications::{Notification, NotificationProvider},
    stream::multiplex,
};
use apex_hardware::{AsyncDevice, FrameBuffer};
use apex_input::Command;
use config::Config;
use futures::{pin_mut, stream, stream::Stream, StreamExt};
use itertools::Itertools;
use linkme::distributed_slice;
use log::{error, info};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    sync::broadcast,
    time::{self, MissedTickBehavior},
};

pub const TICK_LENGTH: usize = 50;
pub const TICKS_PER_SECOND: usize = 1000 / TICK_LENGTH;

/// Signal a provider can send to ask the scheduler to switch to it.
///
/// Used by the MPRIS provider to pull focus onto the OLED display whenever a
/// track changes or playback resumes. Pause/stop do NOT fire this signal —
/// the scheduler continues with whatever provider was active and rotates
/// normally.
#[derive(Debug, Clone, Copy)]
pub struct ProviderWantsFocus;

/// Broadcast channel between providers and the scheduler. Capacity 16 is
/// plenty for our use case (at most a few events per second).
pub type FocusChannel = broadcast::Sender<ProviderWantsFocus>;

#[distributed_slice]
pub static CONTENT_PROVIDERS: [fn(&Config, FocusChannel) -> Result<Box<dyn ContentWrapper>>] = [..];

#[distributed_slice]
pub static NOTIFICATION_PROVIDERS: [fn() -> Result<Box<dyn NotificationWrapper>>] = [..];

pub trait NotificationWrapper {
    fn proxy_stream<'a>(&'a mut self) -> Result<Box<dyn Stream<Item = Result<Notification>> + 'a>>;
}

impl<T: NotificationProvider> NotificationWrapper for T {
    fn proxy_stream<'this>(
        &'this mut self,
    ) -> Result<Box<dyn Stream<Item = Result<Notification>> + 'this>> {
        let x = <T as NotificationProvider>::stream(self)?;
        Ok(Box::new(x.fuse()))
    }
}

pub trait ContentWrapper {
    fn proxy_stream<'a>(&'a mut self) -> Result<Box<dyn Stream<Item = Result<FrameBuffer>> + 'a>>;
    fn provider_name(&self) -> &'static str;
}

impl<T: ContentProvider> ContentWrapper for T {
    // `provider_locked` is loop-carried state: it is assigned inside the
    // `select!` arm and read on a LATER iteration (the auto-rotation
    // suppression further down). The unused_assignments lint cannot see a read
    // that happens after a `continue`, so these assignments look dead and are
    // not. Removing them would silently break the lock.
    #[allow(unused_assignments)]
    fn proxy_stream<'this>(
        &'this mut self,
    ) -> Result<Box<dyn Stream<Item = Result<FrameBuffer>> + 'this>> {
        let x = <T as ContentProvider>::stream(self)?;
        Ok(Box::new(x.fuse()))
    }

    fn provider_name(&self) -> &'static str {
        self.name()
    }
}

/// Resolve a per-provider setting from wherever that provider's section lives.
///
/// A provider's config path depends on its layout, and the GUI writes to the
/// matching location:
///
/// - `[sysinfo]`            → `sysinfo.<key>`
/// - `[providers.lyrics]`   → `providers.<name>.<key>`
/// - `[providers.custom.x]` → `providers.custom.<name>.<key>`
///
/// Looking only at the top level made every nested provider invisible: the GUI
/// would show lyrics first while the daemon, finding no `lyrics.priority`,
/// defaulted it to 99 and sorted it last.
///
/// Most specific section wins, so a custom provider's own settings are never
/// shadowed by a same-named top-level section.
fn provider_setting(config: &Config, name: &str, key: &str) -> Option<config::Value> {
    for section in [
        format!("providers.custom.{name}"),
        format!("providers.{name}"),
        name.to_string(),
    ] {
        if let Ok(tbl) = config.get_table(&section) {
            if let Some(v) = tbl.get(key) {
                return Some(v.clone());
            }
        }
    }
    None
}

/// Scheduler for provider rotation and notifications.
pub struct Scheduler<'a, T: AsyncDevice + 'a> {
    device: T,
    _marker: PhantomData<&'a T>,
}

/// A notification being shown right now, drawn one frame per scheduler
/// iteration.
///
/// The budget is a hard bound on the whole display, tracked against wall-clock
/// rather than a `timeout` around the stream: the stream no longer runs to
/// completion inside one `select!` branch, so there is nothing left to wrap, and
/// an elapsed check is what actually enforces the limit now.
struct ActiveNotification {
    stream: Pin<Box<dyn Stream<Item = Result<FrameBuffer>> + Send>>,
    shown_at: Instant,
    budget: Duration,
    frames: u32,
}

impl ActiveNotification {
    /// # Memory note
    ///
    /// The notification is deliberately leaked for the life of the process.
    /// `ContentProvider::stream` returns an opaque `impl Stream + '_` that
    /// borrows the notification, so a struct holding both would be
    /// self-referential and cannot be built safely. One leak per notification
    /// is small -- a frame buffer and a few wrapped lines -- and it buys a
    /// stream that can be polled from inside `select!` alongside the hotkey
    /// channel. The alternative is `self_cell`/`ouroboros`, i.e. new
    /// dependencies, which is a worse trade for a handful of allocations on a
    /// process that displays a few dozen notifications an hour.
    fn start(notification: Notification) -> Result<Self> {
        // A new notification always starts at the top, whatever the last one
        // was scrolled to.
        crate::render::notifications::reset_notification_scroll();
        // The stream's own countdown is `duration`; on top of that a scroll
        // can hold it open for up to SCROLL_HOLD_CAP. The scheduler's budget is
        // the ceiling on both, so it has to be the SUM or the hold is silently
        // truncated to whatever slack `expected_duration` left over.
        let budget = notification.expected_duration()
            + crate::render::notifications::SCROLL_HOLD_CAP
            + crate::render::notifications::NOTIF_LOCK_CAP;
        // `notification` is owned here and is
        // never touched again; only the stream that borrows it is used.
        let notification: &'static mut Notification = Box::leak(Box::new(notification));
        let stream: Pin<Box<dyn Stream<Item = Result<FrameBuffer>> + Send>> =
            Box::pin(notification.stream()?);
        Ok(Self {
            stream,
            shown_at: Instant::now(),
            budget,
            frames: 0,
        })
    }

    /// Whether the notification must end regardless of scrolling.
    ///
    /// The budget is a bound on the WHOLE display, base duration plus the
    /// scroll hold allowance -- it is not the countdown the reader sees. The
    /// stream enforces that countdown itself (it owns the tick loop), so if
    /// this checked only `budget` it would cut a notification off while the
    /// reader was still scrolling, which is exactly the "I could scroll once
    /// and then it died" symptom.
    fn expired(&self) -> bool {
        self.shown_at.elapsed() >= self.budget
    }
}

impl<'a, T: 'a + AsyncDevice> Scheduler<'a, T> {
    pub fn new(device: T) -> Self {
        Self {
            device,
            _marker: PhantomData,
        }
    }

    #[allow(clippy::too_many_lines)]
    // `provider_locked` is loop-carried state: assigned inside the `select!`
    // arm and read on a LATER iteration (the auto-rotation suppression at the
    // bottom of the loop). unused_assignments cannot see a read that happens
    // after a `continue`, so these assignments look dead and are not --
    // removing them would silently break the lock.
    #[allow(unused_assignments)]
    pub async fn start(
        &mut self,
        tx: broadcast::Sender<Command>,
        rx: broadcast::Receiver<Command>,
        mut config: Config,
    ) -> Result<()> {
        // Channel providers use to request the scheduler focus on them.
        // We subscribe below (focus_rx) and react by jumping the active
        // provider index to the requester.
        let (focus_tx, _) = broadcast::channel::<ProviderWantsFocus>(16);

        #[cfg(not(target_os = "macos"))]
        // A provider that is disabled in config reports that by returning an
        // error from its register function. That must NOT be fatal: weather,
        // lyrics, image and friends all bail with "disabled" when their
        // `enabled` flag is off. Propagating the error killed the whole daemon,
        // and systemd restarted it into the same crash ~3s later -- an endless
        // restart loop from a deliberately disabled provider.
        let mut providers = Vec::new();
        for f in CONTENT_PROVIDERS.iter() {
            match (f)(&mut config, focus_tx.clone()) {
                Ok(p) => providers.push(p),
                // Skip disabled/unconfigured providers; anything else is a
                // genuine failure and should still surface.
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("disabled") {
                        log::info!("skipping disabled provider: {msg}");
                    } else {
                        log::warn!("provider failed to register: {msg}");
                    }
                }
            }
        }

        // Dynamic custom providers from [providers.custom.*] sections.
        #[cfg(feature = "custom")]
        for name in crate::providers::custom::list_custom_sections(&config) {
            match crate::providers::custom::from_config_section(&name, &config) {
                Ok(Some(p)) => providers.push(Box::new(p)),
                Ok(None) => {}
                Err(e) => log::warn!("skipping custom provider '{name}': {e}"),
            }
        }

        // Optional lyrics provider, configured via [providers.lyrics].
        #[cfg(feature = "lyrics")]
        {
            if let Some(p) = crate::providers::lyrics::from_config(&config)? {
                providers.push(Box::new(p));
            }
        }

        #[cfg(target_os = "macos")]
        let mut providers = [
            crate::providers::clock::PROVIDER_INIT(&mut config)?,
            #[cfg(feature = "crypto")]
            crate::providers::coindesk::PROVIDER_INIT(&mut config)?,
        ];

        let mut notifications = NOTIFICATION_PROVIDERS
            .iter()
            .map(|f| (f)())
            .collect::<Result<Vec<_>>>()?;

        let (notifications, errors): (Vec<_>, Vec<_>) = notifications
            .iter_mut()
            .map(|s| s.proxy_stream().map(Box::into_pin))
            .partition_result();

        for e in errors {
            error!("{e}");
        }

        let mut notifications = stream::select_all(notifications.into_iter());

        // Subscribe to provider focus requests. Held outside the loop so we
        // don't create a new receiver on every iteration.
        let focus_rx = focus_tx.subscribe();
        pin_mut!(focus_rx);

        // Subscribe to kglobalaccel's media-shortcut signal and jump to
        // the mpris2 provider when a media key is pressed. Forwarding
        // Player.PlayPause/Next/etc. to the active player is intentionally
        // NOT done here — KDE handles that natively, and adding our own
        // forward raced with kded6's routing on some setups.
        let media_key_focus_enabled = Arc::new(AtomicBool::new(
            config.get_bool("mpris2.event_focus").unwrap_or(true),
        ));
        tokio::spawn(subscribe_media_keys(
            focus_tx.clone(),
            Arc::clone(&media_key_focus_enabled),
        ));

        let current = Arc::new(AtomicUsize::new(0));
        info!("Found {} registered providers", providers.len());

        pin_mut!(rx);

        let (named_providers, errors): (Vec<_>, Vec<_>) = providers
            .iter_mut()
            .map(|i| (i.provider_name(), i.proxy_stream()))
            .filter(|(name, _)| {
                provider_setting(&config, name, "enabled")
                    .and_then(|v| v.clone().into_bool().ok())
                    .unwrap_or(true)
            })
            .map(|(name, i)| {
                let prio = provider_setting(&config, name, "priority")
                    .and_then(|v| v.clone().into_int().ok())
                    .unwrap_or(99i64);
                (name.to_string(), i, prio)
            })
            .sorted_by_key(|(_, _, prio)| *prio)
            .map(|(name, i, _)| {
                let name_for_err = name.clone();
                i.map(|stream| (name, stream)).map_err(|e| {
                    anyhow!(
                        "Failed to initialize provider: {}. Error: {}",
                        name_for_err,
                        e
                    )
                })
            })
            .partition_result();

        for e in errors {
            error!("{e}");
        }

        // Split names from streams so we can keep them parallel for per-provider
        // interval lookups in the change-tick arm.
        let provider_names: Vec<String> = named_providers
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let providers = named_providers
            .into_iter()
            .map(|(_, stream)| stream)
            .map(Box::into_pin)
            .map(StreamExt::fuse)
            .collect::<Vec<_>>();
        let size = providers.len();
        let z = current.clone();

        let mut y = multiplex(providers, move || z.load(Ordering::SeqCst));

        // Flag to know if auto-change is enabled at all. With per-provider
        // intervals, "enabled" means any provider has a non-zero interval set.
        // If everything is 0, we skip the tick to save CPU.
        // Provider lock state (Ctrl+Shift+Numpad- / Numpad+, or IPC lock).
        // Shared with the IPC server so tray/CLI toggles reflect instantly.
        let mut provider_locked = false;

        let is_auto_change_enabled = config
            .get_int("interval.refresh")
            .map(|v| v != 0)
            .unwrap_or(true);
        let mut change = time::interval(Duration::from_secs(if is_auto_change_enabled {
            1
        } else {
            // this is done for performance (don't know if it actually has a big impact)
            300
        }));
        change.set_missed_tick_behavior(MissedTickBehavior::Skip);
        //the last time the screen was changed
        let time_last_change = Arc::new(std::sync::Mutex::new(Instant::now()));

        // IPC control interface (tray/CLI clients). Shares `current` and a
        // lock flag with the scheduler so external commands and hotkeys stay
        // in sync. Errors here are non-fatal: the daemon runs headless.
        let ipc_locked = Arc::new(AtomicBool::new(false));
        {
            let handle = crate::ipc::IpcHandle {
                tx: broadcast::Sender::new(16),
                locked: Arc::clone(&ipc_locked),
                provider_names: Arc::new(provider_names.clone()),
                current: Arc::clone(&current),
                last_change: Arc::clone(&time_last_change),
            };
            let socket_dir =
                std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
            if let Err(e) = crate::ipc::spawn(std::path::Path::new(&socket_dir), handle) {
                log::warn!("IPC server unavailable: {e}");
            }
        }

        // Set when a focus jump just happened: skip the immediately following
        // rotation tick so the jump isn't undone by an already-elapsed dwell.
        let mut suppress_next_rotation = false;
        // `notifications.override` decides whether an incoming notification
        // interrupts the rotation (true, default) or waits for the current
        // provider's dwell to expire (false).
        let notif_override = config
            .get_bool("notifications.override")
            .unwrap_or(true);
        log::info!("notifications.override = {notif_override}");
        // One-slot queue for the non-override path; newest wins.
        let mut pending_notification: Option<Notification> = None;
        // The notification currently being shown, if any.
        //
        // This was previously a blocking `timeout(...).await` inside the
        // `select!` branch that received the notification, which meant the
        // hotkey channel was never polled for the whole time it displayed:
        // `select!` polls branches, it does not run them concurrently. Holding
        // the stream here and drawing ONE frame per `select!` iteration keeps
        // `rx.recv()` live, so a scroll hotkey pressed mid-notification is
        // seen on the very next frame.
        let mut active_notif: Option<ActiveNotification> = None;
        loop {
            provider_locked = ipc_locked.load(Ordering::SeqCst);
            tokio::select! {
                cmd = rx.recv() => {
                    //update the last time the screen was updated to now
                    *time_last_change.lock().unwrap() = Instant::now();
                    match cmd {
                        Ok(Command::Shutdown) => break,
                        Ok(Command::NextSource) => {
                            let new = current.load(Ordering::SeqCst).wrapping_add(1) % size;
                            current.store(new, Ordering::SeqCst);
                            log::info!(
                                "Provider switched via hotkey: {}",
                                provider_names.get(new).map(String::as_str).unwrap_or("?")
                            );
                            self.device.clear().await?;
                        },
                        Ok(Command::PreviousSource) => {
                            let new = match current.load(Ordering::SeqCst) {
                                0 => size - 1,
                                n => (n - 1) % size
                            };
                            current.store(new, Ordering::SeqCst);
                            log::info!(
                                "Provider switched via hotkey: {}",
                                provider_names.get(new).map(String::as_str).unwrap_or("?")
                            );
                            self.device.clear().await?;
                        },
                        Ok(Command::NextItem) | Ok(Command::PreviousItem) => {
                            // Steps the array cursor of the custom-API
                            // provider currently on screen. A no-op for every
                            // other provider, so the hotkey is safe to leave
                            // bound globally.
                            let delta = matches!(cmd, Ok(Command::PreviousItem)) as isize;
                            let delta = if delta == 1 { -1 } else { 1 };
                            let name = provider_names
                                .get(current.load(Ordering::SeqCst))
                                .cloned()
                                .unwrap_or_default();
                            let stepped = crate::providers::custom::step_item(&name, delta);
                            if stepped {
                                log::info!(
                                    "item {} on '{name}'",
                                    if delta > 0 { "next" } else { "previous" }
                                );
                            } else {
                                log::debug!("item hotkey: '{name}' has no items");
                            }
                        }
                        Ok(Command::ScrollUp) | Ok(Command::ScrollDown) => {
                            let delta = if matches!(cmd, Ok(Command::ScrollUp)) { -1 } else { 1 };
                            // A notification outranks every provider, so while
                            // one is up the scroll belongs to it: its body can
                            // be far longer than any provider's. Falls through
                            // to the custom provider otherwise.
                            if active_notif.is_some() {
                                crate::render::notifications::scroll_notification(delta);
                                log::info!("scrolled notification body by {delta}");
                            } else {
                                let name = provider_names
                                    .get(current.load(Ordering::SeqCst))
                                    .cloned()
                                    .unwrap_or_default();
                                // Logged: this branch used to be silent, so a
                                // scroll that did nothing was indistinguishable
                                // from one that worked.
                                if crate::providers::custom::scroll_item(&name, delta) {
                                    // Registered, so the offset moved. Whether
                                    // that CHANGED the picture is the renderer's
                                    // call -- it clamps the offset to the real
                                    // content height -- so this log deliberately
                                    // claims only the offset, not visible motion.
                                    log::info!("scroll offset for '{name}' {delta}");
                                } else {
                                    log::info!(
                                        "scroll ignored: '{name}' is not a custom provider"
                                    );
                                }
                                // A hotkey step resets the dwell, so a provider
                                // being scrolled stays put for a full interval
                                // instead of being replaced by the next item
                                // mid-read.
                                *time_last_change.lock().unwrap() = Instant::now();
                            }
                        }
                        Ok(Command::EightBall) => {
                            // On-demand source: it sits outside the provider
                            // rotation and only ever produces frames when
                            // asked, so this just raises a flag that the 8ball
                            // stream is polling. No provider index moves and
                            // rotation resumes normally afterwards.
                            if crate::providers::eightball::request() {
                                log::info!("8ball: reading requested");
                            } else {
                                log::info!("8ball: request already in flight");
                            }
                        }
                        Ok(Command::ToggleNotificationLock) => {
                            // Only meaningful while a notification is actually
                            // on screen. Pinning an empty screen sets a flag
                            // with nothing to pin, and that flag would then
                            // make the NEXT notification queue and never be
                            // displayed -- a notification silently vanishing is
                            // far worse than a pin being a no-op.
                            let on_screen = active_notif.is_some();
                            if on_screen {
                                let now =
                                    crate::render::notifications::toggle_notification_lock();
                            if now {
                                log::info!(
                                    "Notification PINNED — timer frozen, scroll still works"
                                );
                            } else {
                                log::info!("Notification unpinned — timer resumed");
                                // Restart the dwell so releasing a pin does not
                                // immediately rotate off the provider that was
                                // hidden behind it.
                                *time_last_change.lock().unwrap() = Instant::now();
                            }
                                // Cheapest of the two options: with a notification
                                // pinned the provider stream is not polled at all,
                                // so no provider rendering, no JSON polling and no
                                // USB writes happen underneath it. The pinned
                                // notification is the only thing drawing.
                            } else {
                                // Pinning an empty screen sets a flag with
                                // nothing to pin, and that flag would make the
                                // NEXT notification queue and never display --
                                // a notification silently vanishing is far
                                // worse than a pin being a no-op.
                                log::info!(
                                    "Notification lock ignored: no notification on screen"
                                );
                            }
                        }
                        Ok(Command::ToggleDetail) => {
                            let name = provider_names
                                .get(current.load(Ordering::SeqCst))
                                .cloned()
                                .unwrap_or_default();
                            match crate::providers::custom::toggle_view(&name) {
                                Some(view) => log::info!(
                                    "'{name}' -> {}",
                                    if view == crate::providers::custom::View::Article {
                                        "ARTICLE"
                                    } else {
                                        "HIGHLIGHTS"
                                    }
                                ),
                                None => log::debug!("toggle: '{name}' has no article fields"),
                            }
                        }
                        Ok(Command::LockSource) => {
                            if !provider_locked {
                                provider_locked = true;
                                ipc_locked.store(true, Ordering::SeqCst);
                                log::info!(
                                    "Provider LOCKED on '{}' — auto-rotation suspended",
                                    provider_names
                                        .get(current.load(Ordering::SeqCst))
                                        .map(String::as_str)
                                        .unwrap_or("?")
                                );
                            }
                        },
                        Ok(Command::ToggleLockSource) => {
                            provider_locked = !provider_locked;
                            ipc_locked.store(provider_locked, Ordering::SeqCst);
                            if provider_locked {
                                log::info!(
                                    "Provider LOCKED on '{}' — auto-rotation suspended",
                                    provider_names
                                        .get(current.load(Ordering::SeqCst))
                                        .map(String::as_str)
                                        .unwrap_or("?")
                                );
                            } else {
                                // Restart the dwell from now so unlock doesn't
                                // instantly rotate away.
                                *time_last_change.lock().unwrap() = Instant::now();
                                log::info!("Provider UNLOCKED — auto-rotation resumed");
                            }
                        },
                        Ok(Command::UnlockSource) => {
                            if provider_locked {
                                provider_locked = false;
                                ipc_locked.store(false, Ordering::SeqCst);
                                // Restart the dwell from now so unlock doesn't
                                // instantly rotate away.
                                *time_last_change.lock().unwrap() = Instant::now();
                                log::info!("Provider UNLOCKED — auto-rotation resumed");
                            }
                        },
                        _ => {}
                    }
                },
                notification = notifications.next(), if !notifications.is_empty() => {
                    if let Some(notification) = notification {
                        match notification {
                            Ok(notification) => {
                                // A pinned notification refuses to be replaced.
                                // This is what makes the pin usable for the
                                // case it exists for -- reading a 2FA code --
                                // because with `override = true` any stray
                                // notification would otherwise displace it
                                // mid-read. Queue it instead.
                                if crate::render::notifications::notification_locked() {
                                    log::info!(
                                        "Notification queued: one is pinned"
                                    );
                                    pending_notification = Some(notification);
                                } else if notif_override {
                                    // Interrupt the rotation and show it now.
                                    // A lock suspends auto-rotation, not this:
                                    // with `override = true` a notification has
                                    // the highest priority and is shown even
                                    // while the provider list is locked.
                                    log::info!("Notification received — displaying (override)");
                                    // Arm it. Frames are drawn by the
                                    // `notif_frame` branch below so this branch
                                    // stays non-blocking.
                                    active_notif = Some(ActiveNotification::start(notification)?);
                                } else {
                                    // Queued: hold it until the current
                                    // provider's dwell expires, then show it
                                    // without advancing the rotation.
                                    log::info!("Notification received — queued for next rotation");
                                    pending_notification = Some(notification);
                                }
                            }
                            Err(e) => log::warn!("Notification stream error: {e}"),
                        }
                    }
                }
                // Draw ONE notification frame, then yield back into `select!`
                // so the hotkey branch is polled again. Holding the stream
                // here rather than draining it inside the branch that received
                // the notification is the whole point: a scroll key pressed
                // mid-notification has to be seen on the next frame.
                notif_frame = async {
                    match active_notif.as_mut() {
                        Some(a) => a.stream.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let mut done = false;
                    match notif_frame {
                        Some(Ok(display)) => {
                            self.device.draw(&display).await?;
                            if let Some(a) = active_notif.as_mut() {
                                a.frames += 1;
                            }
                        }
                        Some(Err(e)) => {
                            log::warn!("Notification stream error: {e}");
                            done = true;
                        }
                        None => done = true,
                    }
                    let expired = active_notif.as_ref().is_some_and(|a| a.expired());
                    if done || expired {
                        // Suppress the rotation the elapsed dwell would
                        // otherwise fire on the very next tick: time spent
                        // showing a notification is not time the provider was
                        // actually on screen.
                        suppress_next_rotation = true;
                        if let Some(a) = active_notif.take() {
                            if expired && !done {
                                log::warn!(
                                    "notification stream exceeded {:?} after {} frames; resuming rotation",
                                    a.budget, a.frames
                                );
                            } else {
                                log::info!(
                                    "Notification display finished after {:?} ({} frames); resuming rotation",
                                    a.shown_at.elapsed(), a.frames
                                );
                            }
                        }
                        // Restart the provider dwell: time spent showing a
                        // notification must not count towards the current
                        // provider's interval, or rotation fires immediately
                        // afterwards.
                        *time_last_change.lock().unwrap() = Instant::now();
                    }
                },
                content = y.next(), if active_notif.is_none() => {
                    // Disabled while a notification is up. Both futures stay
                    // armed otherwise and whichever ticks last wins, so the
                    // provider frame overwrote the notification ~20x a second
                    // -- the flicker. A notification outranks every provider,
                    // so suppressing this is the correct precedence.
                    if let Some(Ok(content)) = &content {
                        self.device.draw(content).await?;
                    }
                }
                focus_event = focus_rx.recv() => {
                    log::info!("Scheduler received focus event: {:?}", focus_event);
                    // A provider asked for focus. Find it by name and jump to it.
                    // If we're already showing mpris2, this is a no-op for the
                    // active index AND we don't reset the dwell timer — the
                    // rotation cycle continues normally. This is what makes
                    // the OLED behave as a cycling display that ALSO responds
                    // to media events: music activity briefly pulls focus,
                    // then the cycle resumes from there.
                    //
                    // User intent: any music state change should jump to MPRIS
                    // so the user can see what's playing. After that brief
                    // view, rotation continues so other providers get screen
                    // time too. Pause/stop will also fire PropertiesChanged
                    // (Firefox does this on pause), which keeps the focus
                    // behavior consistent.
                    if focus_event.is_ok() {
                        // Lock semantics: a locked provider is pinned
                        // absolutely — even MPRIS/media-key focus jumps are
                        // suppressed so the screen shows exactly what the
                        // user chose, no surprises.
                        if provider_locked {
                            log::debug!("Focus event ignored (provider locked)");
                        } else {
                            let active_idx = current.load(Ordering::SeqCst);
                            log::info!(
                                "FOCUS DEBUG: active_idx={} target={:?} order={:?}",
                                active_idx,
                                provider_names.iter().position(|n| n == "mpris2"),
                                provider_names
                            );
                            if let Some(target_idx) = provider_names
                                .iter()
                                .position(|n| n == "mpris2")
                            {
                                if target_idx != active_idx {
                                    current.store(target_idx, Ordering::SeqCst);
                                    let _ = self.device.clear().await;
                                    // Reset dwell on transition only — so the
                                    // OLED sits on MPRIS for its full 30s dwell
                                    // before rotating away. Also skip the next
                                    // rotation tick to win the focus-vs-rotate
                                    // race when the previous dwell had expired.
                                    *time_last_change.lock().unwrap() = Instant::now();
                                    suppress_next_rotation = true;
                                    log::info!(
                                        "Provider focused: mpris2 (was idx {}, name={})",
                                        active_idx,
                                        provider_names.get(active_idx).map(String::as_str).unwrap_or("?")
                                    );
                                } else {
                                    // No-op: already showing mpris2. We intentionally
                                    // log this at INFO so that running
                                    // `journalctl --user -u ss-oled -f` shows a
                                    // heartbeat that events are arriving.
                                    log::info!("Focus event on mpris2 (already showing, no-op)");
                                }
                                // Note: we intentionally do NOT reset the dwell
                                // timer when already on mpris2. This lets the
                                // normal rotation cycle continue so other
                                // providers get screen time even when music is
                                // active.
                            } else {
                                log::warn!("Focus requested but mpris2 not in providers list");
                            }
                        }
                    } else {
                        log::warn!("Focus event recv error: {:?}", focus_event);
                    }
                }
                _ = change.tick() => {
                    if suppress_next_rotation {
                        suppress_next_rotation = false;
                    } else if is_auto_change_enabled && !provider_locked {
                        let current_time = Instant::now();
                        let elapsed_time = current_time - *time_last_change.lock().unwrap();
                        // Look up the dwell time for the CURRENT provider, not a
                        // global value. Priority of lookup:
                        //   1. `interval.<provider_name>` (e.g. `interval.clock`)
                        //   2. `interval.refresh` (global fallback)
                        // If the resolved interval is 0, that provider is treated
                        // as "manual only" — never auto-rotated away.
                        let active_idx = current.load(Ordering::SeqCst);
                        let active_name = provider_names
                            .get(active_idx)
                            .map(String::as_str)
                            .unwrap_or("");
                        let interval_secs = Self::interval_for(&config, active_name);

                        // Heartbeat every 5 seconds: log the current active
                        // provider so the user can verify which provider is
                        // currently on the OLED without staring at it.
                        if elapsed_time.as_secs() % 5 == 0
                            && elapsed_time.as_millis() % 1000 < 100
                        {
                            log::debug!(
                                "Currently showing {} (idx {}), {}s of {}s elapsed",
                                active_name,
                                active_idx,
                                elapsed_time.as_secs(),
                                interval_secs
                            );
                        }

                        if interval_secs > 0
                            && elapsed_time > Duration::from_secs(interval_secs)
                        {
                            // A queued notification takes this slot instead of
                            // a rotation, so it is shown without advancing the
                            // provider list.
                            if let Some(queued) = pending_notification.take() {
                                log::info!("Showing queued notification before rotation");
                                // Armed rather than drained inline, for the
                                // same reason as the override path: draining
                                // here would block the hotkey channel again.
                                active_notif = Some(ActiveNotification::start(queued)?);
                            } else {
                                log::info!(
                                    "Rotation timer: rotating from {} (idx {}) after {}s (limit {}s)",
                                    active_name,
                                    active_idx,
                                    elapsed_time.as_secs(),
                                    interval_secs
                                );
                                let _ = tx.send(Command::NextSource);
                            }
                        }
                    }
                }
            };
        }

        self.device.clear().await?;
        self.device.shutdown().await?;
        Ok(())
    }

    /// Look up the dwell time for a provider, in seconds.
    ///
    /// Precedence:
    /// 1. `interval.<provider_name>` (e.g. `interval.clock = 5`)
    /// 2. `interval.refresh` (global fallback)
    /// 3. 30 seconds (hard-coded default if neither key exists)
    ///
    /// A value of 0 means "do not auto-rotate this provider away".
    fn interval_for(config: &Config, provider_name: &str) -> u64 {
        let key = format!("interval.{provider_name}");
        config
            .get_int(&key)
            .ok()
            .filter(|v| *v >= 0)
            .map(|v| v as u64)
            .or_else(|| {
                config
                    .get_int("interval.refresh")
                    .ok()
                    .filter(|v| *v >= 0)
                    .map(|v| v as u64)
            })
            .unwrap_or(30)
    }
}

/// Listen to KDE kglobalaccel's media-shortcut signals and forward
/// play/pause/stop to the active MPRIS player. next/prev are passed
/// through (kded6 handles them natively).

/// Listen to KDE kglobalaccel's media-shortcut signals and request focus
/// on the mpris2 provider so the user sees the change on the OLED. The
/// active player action itself (PlayPause / Next / etc.) is handled by
/// KDE natively — we only react to the focus jump.
async fn subscribe_media_keys(focus_tx: FocusChannel, event_focus_enabled: Arc<AtomicBool>) {
    use dbus::{
        arg::messageitem::MessageItem,
        message::{MatchRule, MessageType},
    };
    use dbus_tokio::connection;
    use futures::stream::StreamExt;

    let (resource, conn) = match connection::new_session_sync() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("media-keys: dbus connect failed: {e}");
            return;
        }
    };
    tokio::spawn(async move {
        let err = resource.await;
        panic!("media-keys: lost DBus connection: {err}");
    });

    let mr = MatchRule::new()
        .with_type(MessageType::Signal)
        .with_path("/component/mediacontrol")
        .with_interface("org.kde.kglobalaccel.Component")
        .with_member("globalShortcutPressed");
    let msg_match = match conn.add_match(mr).await {
        Ok(m) => m,
        Err(e) => {
            log::warn!("media-keys: add_match failed: {e}");
            return;
        }
    };
    log::info!("media-keys: kglobalaccel listener registered");
    let (_msg_match, mut msg_stream) = msg_match.msg_stream();

    while let Some(msg) = msg_stream.next().await {
        let items = msg.get_items();
        let mut strs = Vec::new();
        for item in items {
            if let MessageItem::Str(s) = item {
                strs.push(s);
                if strs.len() == 2 {
                    break;
                }
            }
        }
        let component = strs.get(0).map(|s| s.as_str()).unwrap_or("");
        let shortcut = strs.get(1).map(|s| s.as_str()).unwrap_or("");
        if component != "mediacontrol" {
            continue;
        }
        if !event_focus_enabled.load(Ordering::SeqCst) {
            log::debug!("media-keys: {} (event_focus off, no jump)", shortcut);
            continue;
        }
        log::info!("media-keys: {} -> jumping to mpris2", shortcut);
        let _ = focus_tx.send(ProviderWantsFocus);
    }
    log::warn!("media-keys: listener stream ended");
}

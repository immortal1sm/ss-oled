use anyhow::{anyhow, Result};
use std::{
    cell::RefCell,
    marker::PhantomData,
    rc::Rc,
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
    atomic::{AtomicUsize, Ordering},
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

pub struct Scheduler<'a, T: AsyncDevice + 'a> {
    device: T,
    _marker: PhantomData<&'a T>,
}

impl<'a, T: 'a + AsyncDevice> Scheduler<'a, T> {
    pub fn new(device: T) -> Self {
        Self {
            device,
            _marker: PhantomData,
        }
    }

    #[allow(clippy::too_many_lines)]
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
        let mut providers = CONTENT_PROVIDERS
            .iter()
            .map(|f| (f)(&mut config, focus_tx.clone()))
            .collect::<Result<Vec<_>>>()?;

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

        let current = Arc::new(AtomicUsize::new(0));
        info!("Found {} registered providers", providers.len());

        pin_mut!(rx);

        let (named_providers, errors): (Vec<_>, Vec<_>) = providers
            .iter_mut()
            .map(|i| (i.provider_name(), i.proxy_stream()))
            .filter(|(name, _)| {
                let key = format!("{name}.enabled");
                config.get_bool(&key).unwrap_or(true)
            })
            .map(|(name, i)| {
                let key = format!("{name}.priority");
                let prio = config.get_int(&key).unwrap_or(99i64);
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
        let time_last_change = Rc::new(RefCell::new(Instant::now()));
        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    //update the last time the screen was updated to now
                    *time_last_change.borrow_mut() = Instant::now();
                    match cmd {
                        Ok(Command::Shutdown) => break,
                        Ok(Command::NextSource) => {
                            let new = current.load(Ordering::SeqCst).wrapping_add(1) % size;
                            current.store(new, Ordering::SeqCst);
                            self.device.clear().await?;
                        },
                        Ok(Command::PreviousSource) => {
                            let new = match current.load(Ordering::SeqCst) {
                                0 => size - 1,
                                n => (n - 1) % size
                            };
                            current.store(new, Ordering::SeqCst);
                            self.device.clear().await?;
                        },
                        _ => {}
                    }
                },
                notification = notifications.next(), if !notifications.is_empty() => {
                    if let Some(Ok(mut notification)) = notification {
                        let mut stream = Box::pin(notification.stream()?);
                        while let Some(display) = stream.next().await {
                            self.device.draw(&display?).await?;
                        }
                    }
                }
                content = y.next() => {
                    if let Some(Ok(content)) = &content {
                        self.device.draw(content).await?;
                    }
                }
                focus_event = focus_rx.recv() => {
                    log::debug!("Scheduler received focus event: {:?}", focus_event);
                    // A provider asked for focus. Find it by name and jump to it.
                    // If we're already showing mpris2, this is a no-op for the
                    // active index — but we still reset the dwell timer so the
                    // OLED stays on MPRIS while music events keep firing.
                    // When the music stops (no more PropertiesChanged events
                    // arrive), the 30s rotation timer naturally takes over.
                    if focus_event.is_ok() {
                        let active_idx = current.load(Ordering::SeqCst);
                        if let Some(target_idx) = provider_names
                            .iter()
                            .position(|n| n == "mpris2")
                        {
                            let was_active = target_idx == active_idx;
                            if !was_active {
                                current.store(target_idx, Ordering::SeqCst);
                                let _ = self.device.clear().await;
                                log::info!("Provider focused: mpris2 (was idx {})", active_idx);
                            } else {
                                log::debug!("Refresh focus on mpris2 (events still firing)");
                            }
                            // Always reset the dwell timer so the OLED stays
                            // on MPRIS while music events keep arriving. This
                            // is what makes the OLED behave like a music HUD —
                            // it shows the music screen as long as music is
                            // active, and only rotates away after 30s of
                            // silence (i.e., music stopped or paused without
                            // further state changes).
                            *time_last_change.borrow_mut() = Instant::now();
                        } else {
                            log::warn!("Focus requested but mpris2 not in providers list");
                        }
                    } else {
                        log::warn!("Focus event recv error: {:?}", focus_event);
                    }
                }
                _ = change.tick() => {
                    if is_auto_change_enabled {
                        let current_time = Instant::now();
                        let elapsed_time = current_time - *time_last_change.borrow();
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
                        if interval_secs > 0
                            && elapsed_time > Duration::from_secs(interval_secs)
                        {
                            let _ = tx.send(Command::NextSource);
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

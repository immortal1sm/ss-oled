//! A [`Device`] wrapper that survives the display being unplugged.
//!
//! Without this, every layer above treats "the OLED went away" as fatal: a
//! `draw()` error propagates out of the scheduler and kills the process, and
//! `try_connect()` failing at boot kills it too. With `Restart=on-failure`
//! that becomes a crash loop every few seconds, and each cycle re-registers
//! the D-Bus notification monitors and re-fetches lyrics.
//!
//! Here a disconnected display is an ordinary state instead: the handle is
//! dropped, reconnection is attempted on a bounded backoff, and `draw()`
//! reports success while the panel is absent so the frame loop keeps running.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::device::{Device, FrameBuffer};
use crate::usb::USBDevice;

/// Shortest gap between reconnection attempts.
const MIN_BACKOFF: Duration = Duration::from_millis(500);
/// Longest gap between reconnection attempts. A long plug-out should not
/// leave us waiting minutes after the device comes back.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Backoff after the next failed attempt: double, capped at [`MAX_BACKOFF`].
pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// Whether enough time has passed to try connecting again.
///
/// Returns true when `now` has reached `next_attempt`, or when the deadline has
/// never been set (disconnected for the first time).
pub fn should_attempt(next_attempt: Option<Instant>, now: Instant) -> bool {
    match next_attempt {
        None => true,
        Some(t) => now >= t,
    }
}

/// A [`USBDevice`] that reconnects instead of failing.
///
/// Implements the synchronous [`Device`] trait; the blanket impl in
/// [`crate::device`] gives it [`AsyncDevice`](crate::device::AsyncDevice) for
/// free, exactly as `USBDevice` gets it.
pub struct ReconnectingDevice {
    inner: Option<USBDevice>,
    /// When the next connection attempt is allowed. `None` means "disconnected
    /// and no attempt has been scheduled yet" — attempt immediately.
    next_attempt: Option<Instant>,
    backoff: Duration,
    /// True once a connection has succeeded. Kept for tests and for callers
    /// that want to tell "first attach" from "recovered".
    ever_connected: bool,
}

impl Default for ReconnectingDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl ReconnectingDevice {
    pub fn new() -> Self {
        Self {
            inner: None,
            next_attempt: None,
            backoff: MIN_BACKOFF,
            ever_connected: false,
        }
    }

    /// Try to establish a handle. Returns true when one is now held.
    fn try_reconnect(&mut self, now: Instant) -> bool {
        match USBDevice::try_connect() {
            Ok(dev) => {
                // No confirmation flash here: the keyboard's own firmware plays
                // its boot splash on replug and overwrites anything we draw in
                // that window, so a white flash is 5 wasted USB feature
                // reports that the user never sees. `OLED reconnected` in the
                // log is the actual confirmation.
                self.inner = Some(dev);
                self.backoff = MIN_BACKOFF;
                self.next_attempt = None;
                self.ever_connected = true;
                log::info!("OLED reconnected");
                true
            }
            Err(e) => {
                log::debug!("OLED not present yet: {e}");
                self.backoff = next_backoff(self.backoff);
                self.next_attempt = Some(now + self.backoff);
                false
            }
        }
    }
}

impl Device for ReconnectingDevice {
    fn draw(&mut self, display: &FrameBuffer) -> Result<()> {
        let now = Instant::now();

        if self.inner.is_none() {
            if !should_attempt(self.next_attempt, now) {
                // Still waiting out the backoff. Report success: the panel is
                // simply not there, which is not an error for the frame loop.
                return Ok(());
            }
            if !self.try_reconnect(now) {
                return Ok(());
            }
        }

        let Some(dev) = self.inner.as_mut() else {
            return Ok(());
        };

        match dev.draw(display) {
            Ok(()) => Ok(()),
            Err(e) => {
                // One immediate retry on the same handle. A single odd USB
                // error should not throw away a perfectly good connection.
                match dev.draw(display) {
                    Ok(()) => Ok(()),
                    Err(e2) => {
                        log::warn!("OLED write failed ({e}); retry also failed ({e2}) — dropping handle");
                        self.inner = None;
                        self.backoff = MIN_BACKOFF;
                        self.next_attempt = Some(Instant::now() + self.backoff);
                        // Swallow: the panel being unplugged must not kill the
                        // daemon. Reconnection is handled above.
                        Ok(())
                    }
                }
            }
        }
    }

    fn clear(&mut self) -> Result<()> {
        match self.inner.as_mut() {
            Some(dev) => match dev.clear() {
                Ok(()) => Ok(()),
                Err(e) => {
                    log::warn!("OLED clear failed ({e}); dropping handle");
                    self.inner = None;
                    self.backoff = MIN_BACKOFF;
                    self.next_attempt = Some(Instant::now() + self.backoff);
                    Ok(())
                }
            },
            None => Ok(()),
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        self.inner = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = MIN_BACKOFF;
        assert_eq!(b, Duration::from_millis(500));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_secs(1));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_secs(2));
        // Keep doubling well past the cap; it must never exceed MAX_BACKOFF.
        for _ in 0..20 {
            b = next_backoff(b);
            assert!(b <= MAX_BACKOFF, "backoff grew past the cap: {b:?}");
        }
        assert_eq!(b, MAX_BACKOFF);
    }

    #[test]
    fn first_disconnect_attempts_immediately() {
        let now = Instant::now();
        assert!(should_attempt(None, now), "no deadline means try now");
    }

    #[test]
    fn backoff_deadline_is_respected() {
        let now = Instant::now();
        let future = now + Duration::from_secs(5);
        assert!(
            !should_attempt(Some(future), now),
            "must not attempt before the deadline"
        );
        assert!(
            should_attempt(Some(future), future + Duration::from_millis(1)),
            "must attempt once the deadline passes"
        );
    }

    #[test]
    fn starts_disconnected_and_recovers_nothing_on_its_own() {
        let d = ReconnectingDevice::new();
        assert!(d.inner.is_none(), "must not connect at construction");
        assert!(!d.ever_connected);
    }
}
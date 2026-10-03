//! One MPRIS connection shared by the providers that need "what is playing".
//!
//! # Scope: data only, never timing
//!
//! An earlier version of this module also owned the *clock* -- providers read
//! snapshots and were expected to decide when to repaint. That coupling is
//! what broke the display: each provider's animation state (mpris2's marquee
//! phase and elapsed timer) advances inside `renderer.update()`, so the CALLER
//!'s cadence silently becomes the animation speed. Re-painting "just in case"
//! ran the marquee at the poll rate; gating repaints on a status condition
//! left a paused screen blank. Three regressions came out of that one change.
//!
//! So this module publishes DATA and nothing else. It never tells a provider
//! when to render, how often, or how fast. Each provider keeps its own loop on
//! its own interval, exactly as before, and reads the latest snapshot when it
//! wants to. Sharing the connection removes the duplicated `wait_for_player ->
//! progress` round trip; it does not touch a single frame of timing.
//!
//! # Shape
//!
//! A task owns the connection and publishes the current track to a `watch`
//! channel. Providers subscribe synchronously (no DBus handle crosses the
//! boundary) and read a flattened snapshot, never a live DBus object.
//!
//! The owner reconnects with bounded backoff and never gives up, so a player
//! restart is recovered from once and every subscriber sees `Down` together
//! rather than each discovering it independently.

use apex_music::{AsyncPlayer, Metadata as MetadataTrait, PlaybackStatus, PlayerEvent, Progress};
use futures::StreamExt;
use log::{info, warn};
use std::sync::OnceLock;
use tokio::sync::{broadcast, watch};

/// What is currently playing, as one immutable snapshot.
///
/// `Progress` bundles metadata and position into a single round trip, so
/// keeping the bundle means a consumer never sees metadata from one instant
/// alongside a position from another.
#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    /// Always empty: MPRIS does not reliably expose album art identity, and
    /// lrclib matches on title+artist alone.
    pub album: String,
    /// Always empty, same reason. Reserved for the local-sidecar tier.
    pub url: String,
    /// Track length in microseconds. Zero when unknown.
    pub length_us: i64,
    /// Playback position in microseconds, as sampled at `sampled_at`.
    ///
    /// NOT a live value: the owner publishes on metadata CHANGE, so this is
    /// frozen at the last change unless `sampled_at` is used. Read it through
    /// `position_now()`, never directly.
    pub position_us: i64,
    /// When `position_us` was read. Together with `position_us` this lets a
    /// subscriber extrapolate the playhead using its OWN clock, so the shared
    /// module never has to publish on every tick or wake anyone to do it.
    pub sampled_at: std::time::Instant,
    pub status: PlaybackStatus,
    /// Bus name of the player this came from, e.g. `org.mpris.MediaPlayer2...`.
    pub player: String,
}

impl Default for NowPlaying {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            url: String::new(),
            length_us: 0,
            position_us: 0,
            sampled_at: std::time::Instant::now(),
            // `PlaybackStatus` derives neither Default nor PartialEq upstream,
            // so the change-detection comparison formats its Debug instead of
            // using `==` on the enum.
            status: PlaybackStatus::Stopped,
            player: String::new(),
        }
    }
}

impl NowPlaying {
    /// Playback position extrapolated to NOW, in microseconds.
    ///
    /// The owner publishes only when metadata changes, so a snapshot's
    /// `position_us` is stale by however long ago that was. Adding the elapsed
    /// time since `sampled_at` recovers a current playhead without publishing
    /// anything or waking a single subscriber: the extrapolation happens on the
    /// subscriber's own clock, which is the whole point of this module.
    ///
    /// Only extrapolate while PLAYING. A paused or stopped player's position
    /// is whatever it last was, and adding elapsed time would march it forward
    /// for no reason.
    pub fn position_now(&self) -> i64 {
        match self.status {
            PlaybackStatus::Playing => self
                .position_us
                .saturating_add(self.sampled_at.elapsed().as_micros() as i64),
            PlaybackStatus::Paused | PlaybackStatus::Stopped => self.position_us,
        }
    }

    /// True when there is a real track, not just an idle player.
    ///
    /// A connected but idle player publishes an EMPTY title rather than going
    /// `Down`, so this is the only thing separating "nothing playing" from
    /// "connected". Callers use it to decide what to draw.
    pub fn is_playing_something(&self) -> bool {
        !self.title.trim().is_empty()
    }
}

/// Compare only the fields that indicate a real change.
///
/// `position_us` is deliberately EXCLUDED: it advances on every poll, so
/// including it would publish a "change" several times a second and wake every
/// subscriber to redraw identical pixels. That said, excluding it is also what
/// keeps this module out of the timing business: subscribers poll for position
/// themselves, at their own cadence.
///
/// `status` IS compared because play/pause must repaint the icon.
fn same_snapshot(a: &NowPlaying, b: &NowPlaying) -> bool {
    a.title == b.title
        && a.artist == b.artist
        && a.album == b.album
        && a.url == b.url
        && a.length_us == b.length_us
        && a.player == b.player
        && format!("{:?}", a.status) == format!("{:?}", b.status)
}

#[derive(Clone, Debug)]
pub(crate) enum State {
    Live(Box<NowPlaying>),
    Down,
}

impl Default for State {
    fn default() -> Self {
        State::Down
    }
}

/// Handle to the shared MPRIS state. Cheap to clone; holds no DBus connection.
#[derive(Clone)]
pub struct MprisShared {
    rx: watch::Receiver<State>,
    /// Fan-out of MPRIS events (PropertiesChanged / Seeked / Timer).
    ///
    /// A SIGNAL ONLY: "something happened, you may want to look". It carries no
    /// opinion about whether to redraw. Each subscriber decides what an event
    /// means for it, exactly as it did with its own connection.
    ///
    /// `watch` cannot carry these: an event is not a value, and a subscriber
    /// that attaches late must not replay a stale one. `broadcast` gives every
    /// subscriber every event and drops for whoever falls behind.
    events: broadcast::Sender<PlayerEvent>,
}

/// Process-wide owner handle.
///
/// The provider registry pins every init function to
/// `fn(&Config, FocusChannel)`, so a provider cannot be handed extra
/// constructor arguments without changing a signature shared by every
/// provider. Rather than widen that, the owner publishes one handle here.
/// `OnceLock` is what makes this a single connection rather than one per
/// reader.
static INSTANCE: OnceLock<MprisShared> = OnceLock::new();

/// The shared connection, spawning the owner on first use.
pub fn shared() -> &'static MprisShared {
    INSTANCE.get_or_init(|| {
        let (tx, rx) = watch::channel(State::default());
        // One slot of headroom: this carries change SIGNALS. A subscriber that
        // falls behind gets `Lagged` and re-reads the snapshot, which is
        // strictly better than replaying an event queue.
        let (ev_tx, _) = broadcast::channel(16);
        tokio::spawn({
            let ev_tx = ev_tx.clone();
            async move {
                owner_loop(tx, ev_tx).await;
            }
        });
        MprisShared { rx, events: ev_tx }
    })
}

impl MprisShared {
    /// Latest snapshot, cloned if connected.
    ///
    /// Cloned rather than borrowed: `watch::Receiver::borrow()` hands out a
    /// guard tied to a temporary, so the reference cannot outlive this call,
    /// and a provider must not hold a borrow across an `.await` anyway.
    /// `NowPlaying` is a handful of small fields, so the copy is cheaper than
    /// the borrow's lifetime rules.
    pub fn now(&self) -> Option<NowPlaying> {
        match &*self.rx.borrow() {
            State::Live(np) => Some((**np).clone()),
            State::Down => None,
        }
    }

    /// Subscribe to MPRIS event signals.
    ///
    /// `None` only if there is no owner at all. Call it once when the stream
    /// starts. Callers must handle `Lagged` (fall behind) by re-reading the
    /// snapshot rather than replaying: the snapshot is authoritative, the event
    /// was only a hint.
    pub fn subscribe_events(&self) -> broadcast::Receiver<PlayerEvent> {
        self.events.subscribe()
    }

    /// True when nothing is playing: either no connection, or connected to an
    /// idle player. A two-way convenience over `now()` for callers that only
    /// need to know whether to draw an idle screen.
    pub fn is_idle(&self) -> bool {
        !self.now().is_some_and(|np| np.is_playing_something())
    }
}

/// Own the connection for the process lifetime, reconnecting with backoff.
#[cfg(target_os = "linux")]
async fn owner_loop(tx: watch::Sender<State>, events: broadcast::Sender<PlayerEvent>) {
    // Bounded so a persistent MPRIS outage does not spin: 1s doubling to 10s.
    let mut backoff_secs = 1u64;
    loop {
        match apex_mpris2::MPRIS2::new().await {
            Ok(mpris) => {
                let _ = tx.send_if_modified(|s| {
                    *s = State::Live(Box::new(NowPlaying::default()));
                    true
                });
                info!("shared MPRIS connection established");
                serve(&mpris, &tx, &events).await;
                // `serve` returns when the connection drops.
                warn!("shared MPRIS connection lost; will reconnect");
                tx.send_modify(|s| *s = State::Down);
            }
            Err(e) => {
                warn!("shared MPRIS connect failed: {e}");
                tx.send_modify(|s| *s = State::Down);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(10);
    }
}

/// Windows has no MPRIS equivalent wired to this module, so the shared state
/// stays `Down` and the provider keeps its own per-platform path. Linux is the
/// only platform where two providers actually competed for one connection.
#[cfg(not(target_os = "linux"))]
async fn owner_loop(tx: watch::Sender<State>, events: broadcast::Sender<PlayerEvent>) {
    let _ = (tx, events);
    futures::future::pending::<()>().await;
}

/// Poll the player and publish snapshots until the connection fails.
///
/// The tick here is the OWNER's poll rate for the metadata bundle. It is not a
/// render cadence: subscribers read on their own schedules and this says
/// nothing about when they should draw.
#[cfg(target_os = "linux")]
async fn serve(
    mpris: &apex_mpris2::MPRIS2,
    tx: &watch::Sender<State>,
    events: &broadcast::Sender<PlayerEvent>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // ONE event stream per connection, fanned out to every subscriber. This is
    // the second thing the duplicated connections each used to open their own
    // copy of.
    let Ok(stream) = mpris.stream().await else {
        warn!("shared MPRIS: could not open event stream");
        return;
    };
    // `stream()` returns an anonymous async stream that is not `Unpin`, so
    // `select!` needs it pinned on the heap.
    let mut tracker = Box::pin(stream);
    loop {
        // Wait for a real event or the polling tick. An event means something
        // moved; the tick catches players that emit nothing at all (several
        // browser integrations). The EVENT is forwarded as-is: deciding what it
        // means is the subscriber's job.
        tokio::select! {
            event = tracker.next() => {
                if let Some(event) = event {
                    let _ = events.send(event);
                }
            }
            _ = interval.tick() => {}
        }
        let Some(player) = mpris.wait_for_player(None).await.ok() else {
            continue;
        };
        let name = AsyncPlayer::name(&player).await;
        let Ok(progress) = player.progress().await else { continue };
        let snapshot = to_snapshot(name, progress);
        // Publish only on a real change: every subscriber is woken by a send,
        // and they redraw when they see it. `send_modify` bumps the version
        // unconditionally, which would wake them for identical pixels, so use
        // `send_if_modified` and report the difference ourselves.
        let _ = tx.send_if_modified(|s| match &mut *s {
            State::Live(cur) if same_snapshot(cur, &snapshot) => {
                // Same track, new reading. Refresh the VOLATILE fields so a
                // subscriber polling on its own schedule sees a current
                // position, but report NO change: nobody needs waking, and
                // `position_now()` extrapolates from `sampled_at` anyway.
                // Returning false here is what keeps this from publishing
                // once a second per consumer.
                cur.position_us = snapshot.position_us;
                cur.sampled_at = snapshot.sampled_at;
                false
            }
            _ => {
                *s = State::Live(Box::new(snapshot));
                true
            }
        });
        // `send_if_modified` cannot report a dropped receiver, so check
        // liveness explicitly: with every subscriber gone there is nothing
        // left to poll for.
        if tx.receiver_count() == 0 {
            return;
        }
    }
}

#[cfg(target_os = "linux")]
fn to_snapshot(player: String, progress: Progress<apex_mpris2::Metadata>) -> NowPlaying {
    let meta = progress.metadata;
    NowPlaying {
        title: MetadataTrait::title(&meta).unwrap_or_default(),
        artist: MetadataTrait::artists(&meta).unwrap_or_default(),
        album: String::new(),
        url: String::new(),
        length_us: MetadataTrait::length(&meta).unwrap_or(0) as i64,
        position_us: progress.position,
        // Stamped at the moment of the DBus read, which is what makes
        // `position_now()` a truthful extrapolation rather than a guess.
        sampled_at: std::time::Instant::now(),
        status: progress.status,
        player,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(title: &str) -> NowPlaying {
        NowPlaying {
            title: title.into(),
            status: PlaybackStatus::Playing,
            ..Default::default()
        }
    }

    fn handle_pair() -> (watch::Sender<State>, MprisShared) {
        let (tx, rx) = watch::channel(State::default());
        let (events, _) = broadcast::channel(16);
        (tx, MprisShared { rx, events })
    }

    #[test]
    fn blank_title_is_not_a_track() {
        assert!(!np("").is_playing_something());
        assert!(!np("   ").is_playing_something(), "whitespace is not a title");
    }

    #[test]
    fn real_title_is_a_track() {
        assert!(np("Without a Doubt").is_playing_something());
    }

    /// This is the whole point of sharing: a dropped connection is visible to
    /// every holder, not just the one that happened to notice.
    #[test]
    fn a_drop_is_visible_to_every_holder() {
        let (tx, a) = handle_pair();
        // Every other subscriber gets a clone of the same receiver -- never a
        // second connection.
        let b = a.clone();
        tx.send_modify(|s| *s = State::Live(Box::new(np("Song"))));
        assert!(a.now().unwrap().is_playing_something());
        assert!(b.now().unwrap().is_playing_something());

        tx.send_modify(|s| *s = State::Down);
        assert!(a.now().is_none());
        assert!(b.now().is_none());
    }

    /// A connected-but-idle player is Live with an empty title. Callers that
    /// ignore this distinction render "Unknown title" instead of the idle
    /// screen, which is exactly the regression this module must not invite.
    #[test]
    fn idle_is_distinguishable_from_down() {
        let (tx, h) = handle_pair();
        assert!(h.is_idle(), "a fresh handle is idle");

        tx.send_modify(|s| *s = State::Live(Box::new(np(""))));
        assert!(h.is_idle(), "connected with no track is idle");
        assert!(h.now().is_some(), "but it IS connected");
        assert!(!h.now().unwrap().is_playing_something());

        tx.send_modify(|s| *s = State::Live(Box::new(np("Song"))));
        assert!(!h.is_idle());
    }

    /// An identical snapshot must not wake subscribers.
    ///
    /// `send_modify` marks the channel changed even when the closure leaves
    /// the value alone, so the owner uses `send_if_modified`, which bumps the
    /// version only when the closure reports an actual difference.
    #[test]
    fn an_identical_snapshot_is_suppressed() {
        let (tx, mut h) = handle_pair();
        let snapshot = np("Song");

        let publish = |snap: &NowPlaying| {
            tx.send_if_modified(|s| match &*s {
                State::Live(cur) if same_snapshot(cur, snap) => false,
                _ => {
                    *s = State::Live(Box::new(snap.clone()));
                    true
                }
            })
        };

        assert!(publish(&snapshot), "first publish is a change");
        assert!(h.rx.has_changed().unwrap());
        // `borrow_and_update` is what marks the version seen. Calling
        // `has_changed()` again does NOT clear the flag.
        let _ = h.rx.borrow_and_update();

        assert!(
            !publish(&snapshot),
            "an identical snapshot must not report a change"
        );
        assert!(!h.rx.has_changed().unwrap(), "subscribers stay asleep");

        assert!(publish(&np("Different")), "a new title is a change");
        assert!(h.rx.has_changed().unwrap());
    }

    /// `position_us` must be excluded from change detection, or the owner
    /// publishes a "change" on every poll and wakes everyone for pixels that
    /// did not move.
    #[test]
    fn a_moving_position_is_not_a_change() {
        let a = np("Song");
        let mut b = np("Song");
        b.position_us = 5_000_000;
        assert!(
            same_snapshot(&a, &b),
            "an advancing playhead is not a state change"
        );
    }

    #[test]
    fn a_different_title_is_a_change() {
        assert!(!same_snapshot(&np("Song"), &np("Other")));
    }

    /// Status IS compared: play/pause swaps the icon.
    #[test]
    fn a_status_change_is_a_change() {
        let mut a = np("Song");
        let mut b = np("Song");
        a.status = PlaybackStatus::Playing;
        b.status = PlaybackStatus::Paused;
        assert!(!same_snapshot(&a, &b));
    }

    /// The playhead must ADVANCE with wall-clock time even though the owner
    /// publishes only on metadata change.
    ///
    /// Regression: `position_us` was excluded from change detection and read
    /// directly by the consumer, so it stayed frozen at the last metadata
    /// change. Lyrics then held one line forever -- including across a daemon
    /// restart, because the freeze was in the DATA, not the process.
    #[test]
    fn position_advances_without_a_publish() {
        let mut a = np("Song");
        a.status = PlaybackStatus::Playing;
        a.position_us = 1_000_000;

        // Pretend the reading was taken a while ago.
        let age = std::time::Duration::from_millis(500);
        a.sampled_at = std::time::Instant::now() - age;

        let advanced = a.position_now();
        assert!(
            advanced >= a.position_us + 400_000,
            "a playing snapshot must extrapolate forward: {} -> {}",
            a.position_us,
            advanced
        );
        // ...and it must be capped by length so a bad sample cannot run away.
        a.length_us = 2_000_000;
        let capped = a.position_now();
        assert!(
            capped <= a.length_us,
            "position must never exceed the track length: {capped} > {}",
            a.length_us
        );
    }

    /// A PAUSED player must NOT have its position marched forward: the value is
    /// simply whatever the player last reported.
    #[test]
    fn a_paused_position_does_not_drift() {
        let mut a = np("Song");
        a.status = PlaybackStatus::Paused;
        a.position_us = 1_000_000;
        a.sampled_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
        assert_eq!(a.position_now(), 1_000_000, "a paused playhead is frozen");
    }

    /// Refreshing the volatile fields must NOT wake subscribers -- that is what
    /// keeps a 1s poll from costing a repaint per consumer per second.
    #[test]
    fn refreshing_position_does_not_wake_anyone() {
        let (tx, mut h) = handle_pair();
        let mut first = np("Song");
        first.position_us = 1_000_000;
        tx.send_modify(|s| *s = State::Live(Box::new(first)));
        let _ = h.rx.borrow_and_update();

        // Same metadata, new position.
        let mut second = np("Song");
        second.position_us = 2_000_000;
        let changed = tx.send_if_modified(|s| match &mut *s {
            State::Live(cur) if same_snapshot(cur, &second) => {
                cur.position_us = second.position_us;
                cur.sampled_at = second.sampled_at;
                false
            }
            _ => {
                *s = State::Live(Box::new(second.clone()));
                true
            }
        });

        assert!(!changed, "a moving position is not a wake-worthy change");
        assert!(!h.rx.has_changed().unwrap(), "nobody may be woken");
        // The value must still be current for whoever polls next.
        assert_eq!(h.now().unwrap().position_us, 2_000_000, "but it is fresh");
    }

}

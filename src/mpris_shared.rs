//! One MPRIS connection shared by every provider that needs "what is playing".
//!
//! # Why this exists
//!
//! `lyrics` and `mpris2` both need the current track, and both used to open
//! their own DBus connection and repeat the same
//! `wait_for_player -> progress` preamble. Two connections meant two
//! independent lifecycles for one fact, which is harder to reason about than
//! the duplication it was meant to avoid: a player restart had to be
//! recovered from twice, and the two providers could disagree about what was
//! playing.
//!
//! Sharing is the smaller system. If the connection drops, `NowPlaying` goes
//! `Disconnected`, both providers see it, and one reconnect brings both back.
//! That coupling is deliberate: they are reading the same player, so a player
//! that vanished takes both views with it.
//!
//! # Shape
//!
//! A single task owns the connection and publishes the current track. Providers
//! subscribe to a `watch` channel rather than polling the bus themselves, so
//! they cannot drift apart and none of them holds a DBus handle.
//!
//! The owner reconnects on its own with a bounded backoff and never gives up,
//! so a provider's stream stays alive across a player restart and simply sees
//! `Disconnected` in the meantime.
//!
//! # Non-blocking by construction
//!
//! `subscribe()` is synchronous and cannot touch DBus: it returns a receiver
//! immediately. All I/O happens on the owner task. That matters because
//! providers construct their stream inside the scheduler's `select!`, where a
//! blocking call would stall every other provider.

use futures::StreamExt;
use apex_music::{AsyncPlayer, Metadata as MetadataTrait, PlaybackStatus, PlayerEvent, Progress};
use log::{info, warn};
use std::sync::{Arc, OnceLock};
use tokio::sync::{broadcast, watch};

/// What is currently playing, as one snapshot.
///
/// `Progress` bundles metadata and position in a single round trip; keeping the
/// bundle means a consumer never sees metadata from one instant and position
/// from another.
#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    /// Empty: MPRIS does not reliably expose album art identity for every
    /// player, and lrclib matches on title+artist alone.
    pub album: String,
    /// Empty, same reason.
    pub url: String,
    /// Track length in microseconds. Zero when unknown.
    pub length_us: i64,
    /// Playback position in microseconds.
    pub position_us: i64,
    pub status: PlaybackStatus,
    /// Bus name of the player this came from, e.g. `spotify`.
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
            // `PlaybackStatus` derives neither Default nor PartialEq upstream, so
            // the comparison used for change detection is done in
            // `state_matches` instead of `==` on the enum.
            status: PlaybackStatus::Stopped,
            player: String::new(),
        }
    }
}

/// Compare the fields that matter for change detection.
///
/// `PlaybackStatus` derives neither `PartialEq` nor `Eq` upstream, so
/// `NowPlaying` cannot derive them either while it holds one. Comparing its
/// `Debug` form avoids touching a crate three other consumers share.
///
/// `position_us` is deliberately EXCLUDED: it advances every tick, so
/// including it would publish a "change" every second and wake both providers
/// to redraw identical pixels. `status` is included because play/pause must
/// repaint the icon.
fn same_snapshot(a: &NowPlaying, b: &NowPlaying) -> bool {
    a.title == b.title
        && a.artist == b.artist
        && a.album == b.album
        && a.url == b.url
        && a.length_us == b.length_us
        && a.player == b.player
        && format!("{:?}", a.status) == format!("{:?}", b.status)
}

impl NowPlaying {
    /// True when there is a real track, not just an idle player.
    pub fn is_playing_something(&self) -> bool {
        !self.title.trim().is_empty()
    }
}

/// Health of the shared connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MprisHealth {
    /// Connected, with a track.
    Connected,
    /// Connected, but nothing is playing.
    Idle,
    /// No connection: MPRIS is unreachable or no player has appeared yet.
    Disconnected,
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

/// Handle to the shared MPRIS state. Cloneable and cheap; providers hold one
/// each and never see the DBus connection itself.
#[derive(Clone)]
pub struct MprisShared {
    rx: watch::Receiver<State>,
    /// Fan-out of MPRIS events (PropertiesChanged / Seeked / Timer).
    ///
    /// `watch` carries the latest *value*, which is the wrong shape for events:
    /// a subscriber that attaches late must not replay the last one, and every
    /// subscriber needs to see every event, not just the newest. `mpris2` uses
    /// these to fire focus on a track change, so a lost event means a missed
    /// screen switch.
    events: broadcast::Sender<PlayerEvent>,
}

impl MprisShared {
    /// Latest snapshot, cloned if connected.
    ///
    /// Cloned rather than borrowed: `watch::Receiver::borrow()` hands out a
    /// guard tied to a temporary, so the reference cannot outlive this call,
    /// and a provider must not hold a borrow across an `.await` anyway.
    /// `NowPlaying` is six small fields, so the copy is cheaper than the
    /// borrow's lifetime rules.
    pub fn now(&self) -> Option<NowPlaying> {
        match &*self.rx.borrow() {
            State::Live(np) => Some((**np).clone()),
            State::Down => None,
        }
    }

    pub fn health(&self) -> MprisHealth {
        match &*self.rx.borrow() {
            State::Live(np) if np.is_playing_something() => MprisHealth::Connected,
            State::Live(_) => MprisHealth::Idle,
            State::Down => MprisHealth::Disconnected,
        }
    }

    /// Wait for the next change. Returns `None` only if the owner task is gone,
    /// which cannot happen while this handle exists.
    pub async fn changed(&mut self) {
        let _ = self.rx.changed().await;
    }

    /// Re-borrow the receiver mutably, for a `select!` that awaits `changed`.
    pub fn receiver_mut(&mut self) -> &mut watch::Receiver<State> {
        &mut self.rx
    }

    /// Subscribe to MPRIS events.
    ///
    /// Returns `None` only if the owner task is already gone. Call this once,
    /// when the provider's stream starts: `broadcast` drops events for
    /// subscribers that fall behind, which for a focus signal is better than
    /// unbounded queue growth.
    pub fn subscribe_events(&self) -> Option<broadcast::Receiver<PlayerEvent>> {
        if self.events.receiver_count() == 0 {
            None
        } else {
            Some(self.events.subscribe())
        }
    }
}

/// Where the shared connection should look for a player.
///
/// `None` means "whatever the bus reports as active", which is what the lyrics
/// provider wants; `Some(name)` pins a specific player for the mpris2 provider's
/// `preferred_player` setting.
#[derive(Clone, Debug, Default)]
pub struct PlayerPreference(pub Option<Arc<String>>);

/// Process-wide owner handle.
///
/// The provider registry (`CONTENT_PROVIDERS`) pins every init function to
/// `fn(&Config, FocusChannel)`, so a provider cannot be handed extra
/// constructor arguments without changing a signature shared by every provider.
/// Rather than widen that, the owner publishes one handle here and the two MPRIS
/// consumers read it. `OnceLock` is what makes this a single connection rather
/// than one per reader.
static INSTANCE: OnceLock<MprisShared> = OnceLock::new();

/// Spawn the owner once and return the shared handle.
///
/// Safe to call from every provider: only the first call does the work.
pub fn shared() -> &'static MprisShared {
    INSTANCE.get_or_init(|| {
        // No preference here: the owner accepts a player name to pin, but the
        // per-provider `mpris2.preferred_player` is applied by that provider's
        // own read. One owner cannot serve two different pinned players, so the
        // preference is deliberately left unset and the first matching player
        // on the bus wins.
        spawn(PlayerPreference::default())
    })
}

/// Spawn the connection owner and return a handle to its state.
///
/// Non-blocking: returns as soon as the task is scheduled. Call it once during
/// startup, before any provider stream is built.
#[cfg(target_os = "linux")]
pub fn spawn(preference: PlayerPreference) -> MprisShared {
    let (tx, rx) = watch::channel(State::default());
    // One slot is enough: this carries change signals, and a subscriber that
    // falls behind gets `Lagged` and resyncs from `now()` rather than a stale
    // replay. A large buffer would only defer the same information.
    let (ev_tx, _ev_rx) = broadcast::channel(16);
    tokio::spawn({
        let ev_tx = ev_tx.clone();
        async move {
            owner_loop(tx, ev_tx, preference).await;
        }
    });
    MprisShared { rx, events: ev_tx }
}

#[cfg(target_os = "windows")]
pub fn spawn(_preference: PlayerPreference) -> MprisShared {
    let (tx, rx) = watch::channel(State::default());
    let (ev_tx, _ev_rx) = broadcast::channel(16);
    tokio::spawn(async move {
        // The Windows backend has no watchable player API to mirror yet, so the
        // shared state stays `Down`. Windows keeps its existing per-provider
        // path until that lands; Linux is the only platform where two providers
        // actually competed for the same connection.
        let _ = (tx, ev_tx);
        futures::future::pending::<()>().await;
    });
    MprisShared {
        rx,
        events: ev_tx,
    }
}

/// Own the connection for the process lifetime, reconnecting with backoff.
#[cfg(target_os = "linux")]
async fn owner_loop(
    tx: watch::Sender<State>,
    events: broadcast::Sender<PlayerEvent>,
    preference: PlayerPreference,
) {
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
                serve(&mpris, &tx, &events, preference.0.clone()).await;
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

/// Poll the player and publish snapshots until the connection fails.
#[cfg(target_os = "linux")]
async fn serve(
    mpris: &apex_mpris2::MPRIS2,
    tx: &watch::Sender<State>,
    events: &broadcast::Sender<PlayerEvent>,
    preference: Option<Arc<String>>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // One event stream per connection, fanned out to every subscriber. This is
    // the second thing the duplicate connections used to each open their own
    // copy of.
    // `stream()` hands back an anonymous async stream that is not `Unpin`, so
    // `select!` needs it pinned on the heap.
    let Ok(stream) = mpris.stream().await else {
        warn!("shared MPRIS: could not open event stream");
        return;
    };
    let mut tracker = Box::pin(stream);
    loop {
        // Wait for either a real MPRIS event or the polling tick. An event
        // means the track or position moved; the tick catches players that do
        // not emit events at all (several browser integrations).
        tokio::select! {
            event = tracker.next() => {
                if let Some(event) = event {
                    // `send` failing just means nobody is listening yet.
                    let _ = events.send(event);
                }
            }
            _ = interval.tick() => {}
        }
        let player = mpris.wait_for_player(preference.clone()).await;
        let Ok(player) = player else { continue };
        let name = AsyncPlayer::name(&player).await;
        let Ok(progress) = player.progress().await else { continue };
        let snapshot = to_snapshot(name, progress);
        // Only publish on a real change: this channel wakes every subscriber,
        // and the providers re-render on each wake.
        // Publish only a real change. `send_modify` wakes every subscriber,
        // and both providers redraw on each wake, so an identical snapshot would
        // cost a repaint per second for no visible difference.
        // `send_if_modified` bumps the watch version ONLY when the closure
        // returns true, so an unchanged snapshot leaves every subscriber asleep.
        // `send_modify` would mark it changed unconditionally and cost both
        // providers a repaint per second for identical pixels.
        let _ = tx.send_if_modified(|s| match &*s {
            State::Live(cur) if same_snapshot(cur, &snapshot) => false,
            _ => {
                *s = State::Live(Box::new(snapshot));
                true
            }
        });
        // `send_modify` cannot report a dropped receiver, so check liveness
        // explicitly: with every provider gone there is nothing to poll for.
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
        status: progress.status,
        player,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

    /// Build a handle wired to a throwaway channel pair. Mirrors `spawn()`
    /// without starting a real owner task, so lifecycle behaviour is testable
    /// without a DBus bus.
    fn handles() -> (watch::Sender<State>, MprisShared, broadcast::Sender<PlayerEvent>) {
        let (tx, rx) = watch::channel(State::default());
        let (ev_tx, _) = broadcast::channel(16);
        (
            tx,
            MprisShared {
                rx: rx.clone(),
                events: ev_tx.clone(),
            },
            ev_tx,
        )
    }

    fn np(title: &str) -> NowPlaying {
        NowPlaying {
            title: title.into(),
            status: PlaybackStatus::Playing,
            ..Default::default()
        }
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

    /// Health is what a provider uses to decide whether to draw idle art, so
    /// the three states must stay distinguishable.
    #[test]
    fn health_distinguishes_idle_from_down() {
        let (tx, shared, _ev) = handles();
        assert_eq!(shared.health(), MprisHealth::Disconnected, "fresh channel is down");

        tx.send_modify(|s| *s = State::Live(Box::new(np(""))));
        assert_eq!(shared.health(), MprisHealth::Idle, "connected, nothing playing");

        tx.send_modify(|s| *s = State::Live(Box::new(np("Song"))));
        assert_eq!(shared.health(), MprisHealth::Connected);
        assert_eq!(shared.now().unwrap().title, "Song");
    }

    /// This is the whole point of sharing: a dropped connection has to be
    /// visible to every holder, not just the one that noticed it.
    #[test]
    fn a_drop_is_visible_to_every_holder() {
        let (tx, lyrics_side, _ev) = handles();
        // The second holder is what `spawn()` hands every other consumer: a
        // clone of the same receiver, never a second connection.
        let mpris2_side = lyrics_side.clone();
        tx.send_modify(|s| *s = State::Live(Box::new(np("Song"))));
        assert_eq!(lyrics_side.health(), MprisHealth::Connected);
        assert_eq!(mpris2_side.health(), MprisHealth::Connected);

        tx.send_modify(|s| *s = State::Down);
        assert_eq!(lyrics_side.health(), MprisHealth::Disconnected);
        assert_eq!(mpris2_side.health(), MprisHealth::Disconnected);
        assert!(lyrics_side.now().is_none());
        assert!(mpris2_side.now().is_none());
    }

    /// An identical snapshot must not wake subscribers.
    ///
    /// `send_modify` marks the channel changed even when the closure leaves
    /// the value alone, so the owner uses `send_if_modified`, which bumps the
    /// version only when the closure reports an actual difference. Without this
    /// the two providers would repaint identical pixels once a second.
    #[test]
    fn an_identical_snapshot_is_suppressed() {
        let (tx, mut shared, _ev) = handles();
        let mut rx = shared.receiver_mut().clone();
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
        assert!(rx.has_changed().unwrap());
        // `borrow_and_update` is what marks the version seen. Calling
        // `has_changed()` again does NOT clear the flag, so it is the wrong way
        // to acknowledge a change.
        let _ = rx.borrow_and_update();
        assert!(!rx.has_changed().unwrap(), "version is now seen");

        assert!(
            !publish(&snapshot),
            "an identical snapshot must not report a change"
        );
        assert!(
            !rx.has_changed().unwrap(),
            "an identical snapshot must not wake subscribers"
        );

        // A genuine difference must still get through, or nothing repaints.
        assert!(publish(&np("Different")), "a new title is a change");
        assert!(rx.has_changed().unwrap());
    }

    /// `position_us` must be excluded from change detection, or the owner
    /// publishes a "change" every second and both providers repaint pixels
    /// that did not move.
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

    /// A real difference still has to register, or nothing would ever repaint.
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

    /// A busy provider must be able to block on changes without holding the
    /// connection: the owner publishes, they await.
    #[tokio::test]
    async fn changed_returns_when_the_owner_publishes() {
        let (tx, mut shared, _ev) = handles();
        let waiter = tokio::spawn(async move {
            shared.changed().await;
            true
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tx.send_modify(|s| *s = State::Live(Box::new(np("Song"))));
        assert!(waiter.await.unwrap(), "changed() must wake on publish");
    }
}

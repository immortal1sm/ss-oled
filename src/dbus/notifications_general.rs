//! General desktop notification provider.
//!
//! Listens for `org.freedesktop.Notifications.Notify` from *any* application and
//! renders summary + body on the OLED. This is the counterpart to
//! `dbus::notifications`, which only accepts messages whose bus source is
//! literally `discord` (that is the path SteelSeries GG-style clients use).
//!
//! Both providers can be enabled independently: this one renders ordinary
//! desktop notifications, the other renders Discord's richer payload.

use crate::{
    render::{
        notifications::{Notification, NotificationBuilder, NotificationProvider},
        scheduler::NotificationWrapper,
    },
    scheduler::NOTIFICATION_PROVIDERS,
};
use anyhow::Result;
use async_stream::try_stream;
use crate::render::notifications::{Align, Icon, Layout, LineSpec, Part, SizeClass};
use dbus::{
    arg::messageitem::MessageItem,
    channel::MatchingReceiver,
    message::MatchRule,
    nonblock,
    strings::{Interface, Member},
    Message,
};
use dbus_tokio::connection;

use futures::{channel::mpsc, StreamExt};
use log::{info, warn};
use std::time::Duration;
use super::notifications::unescape_entities;

/// Registered into the scheduler's notification slot at startup.
#[linkme::distributed_slice(NOTIFICATION_PROVIDERS)]
static PROVIDER_INIT: fn() -> Result<Box<dyn NotificationWrapper>> = register_callback;

#[allow(clippy::unnecessary_wraps)]
fn register_callback() -> Result<Box<dyn NotificationWrapper>> {
    info!("Registering general desktop notification source.");

    // The distributed-slice constructor takes no arguments, so the settings
    // are read here rather than injected. `notifications.default_duration` is
    // How long every notification displays. Authoritative.
    let mut settings = config::Config::default();
    if let Some(dir) = dirs::config_dir() {
        let _ = settings.merge(
            config::File::with_name(&dir.join("apex-tux/settings").to_string_lossy())
                .required(false),
        );
    }
    // `duration` is canonical. `default_duration` is still read so existing
    // configs keep working; remove that fallback once configs have migrated.
    let duration_seconds = settings
        .get_int("notifications.duration")
        .or_else(|_| settings.get_int("notifications.default_duration"))
        .unwrap_or(5)
        .clamp(1, 300) as u64;

    // Layout comes from the same table, one entry per text part:
    //
    //   [notifications.lines.title]
    //   size = "xl"; align = "center"; row = 4; bold = false; shown = true
    //
    // Each part is independent — there is no shared align, so a centred title
    // with a left-aligned body is expressible. Absent sections fall back to the
    // built-in defaults, so an empty [notifications] behaves like no config.
    let default_layout = Layout::default();
    let mut lines = Vec::new();

    for (key, part) in [
        ("app", Part::App),
        ("title", Part::Title),
        ("content", Part::Content),
    ] {
        let prefix = format!("notifications.lines.{key}");
        let fallback = default_layout.line(part);
        // A `[notifications.lines.<part>]` table is entirely optional.
        if settings.get_table(&prefix).is_err() {
            lines.push(fallback);
            continue;
        }
        lines.push(LineSpec {
            part,
            shown: settings
                .get_bool(&format!("{prefix}.shown"))
                .unwrap_or(fallback.shown),
            size: settings
                .get_str(&format!("{prefix}.size"))
                .ok()
                .and_then(|v| SizeClass::parse(&v))
                .unwrap_or(fallback.size),
            align: settings
                .get_str(&format!("{prefix}.align"))
                .ok()
                .and_then(|v| Align::parse(&v))
                .unwrap_or(fallback.align),
            row: settings.get_int(&format!("{prefix}.row")).ok().map(|v| v as i32),
            bold: settings
                .get_bool(&format!("{prefix}.bold"))
                .unwrap_or(fallback.bold),
            wrap: settings
                .get_bool(&format!("{prefix}.wrap"))
                .unwrap_or(fallback.wrap),
            dy: settings.get_int(&format!("{prefix}.dy")).ok().map(|v| v as i32).unwrap_or(0),
        });
    }

    let layout = Layout {
        lines,
        show_timer: settings
            .get_bool("notifications.show_timer")
            .unwrap_or(true),
        timer_border: settings
            .get_bool("notifications.timer_border")
            .unwrap_or(true),
        // DBus notifications carry no badge; the 8ball sets this on its own
        // layout instance.
        badge_box: None,
    };

    Ok(Box::new(Dbus {
        duration_seconds,
        layout,
    }))
}

pub struct Dbus {
    /// How long every notification displays, from `notifications.duration`.
    /// This is authoritative — the sender's request is informational only.
    duration_seconds: u64,
    /// Text placement, configured under `[notifications]`.
    layout: Layout,
}

/// A decoded `Notify` call, reduced to what fits on a 128x40 panel.
struct Generic {
    app_name: String,
    summary: String,
    body: String,
    /// Absolute path from `hints["image-path"]`, if the sender supplied one.
    image_path: Option<String>,
}

impl Generic {
    /// Notification titles double as the OLED's single text row; body text is
    /// appended when the summary leaves room. Fall back to the body, then the
    /// app name, so the panel is never blank.
    fn title(&self) -> String {
        let summary = unescape_entities(self.summary.trim());
        if !summary.is_empty() {
            return summary.to_string();
        }
        let body = unescape_entities(self.body.trim());
        if !body.is_empty() {
            return body;
        }
        self.app_name.clone()
    }

    fn content(&self) -> String {
        let summary = unescape_entities(self.summary.trim());
        let body = unescape_entities(self.body.trim());
        match (summary.is_empty(), body.is_empty()) {
            (false, false) => body,
            _ => String::new(),
        }
    }

    fn render(&self, duration_seconds: u64, layout: Layout) -> Result<Notification> {
        // `with_title` borrows, so the title must outlive the builder.
        let title = self.title();
        let mut builder = NotificationBuilder::new().with_title(&title);
        let content = self.content();
        if !content.is_empty() {
            builder = builder.with_content(content);
        }
        if let Some(path) = &self.image_path {
            if let Some(icon) = load_icon(path) {
                builder = builder.with_icon(icon);
            }
        }
        log::info!(
            "notification content: app={:?} title={:?} body_len={}",
            self.app_name,
            self.title(),
            self.body.chars().count()
        );

        // The configured duration is authoritative and the sender's
        // `expire_timeout` is deliberately ignored. One knob, one behaviour:
        // what you set is what every notification shows. The sender's value is
        // not even parsed -- it is milliseconds per the freedesktop spec but
        // seconds for notify-send's -t, and honouring it either way is how a
        // 2500ms request became a 41-minute notification that wedged the
        // scheduler for good.
        builder = builder.with_hold_seconds(duration_seconds);
        builder = builder.with_layout(layout).with_app_name(&self.app_name);
        builder.build()
    }
}

/// Decode `hints["image-path"]` into the 24x24 BMP the builder expects.
///
/// `NotificationBuilder` hard-requires 24x24 icons, so anything else is scaled
/// or skipped rather than failing the whole notification.
/// Load an app icon from `hints["image-path"]`.
///
/// NOTE: not wired up yet. `NotificationBuilder`'s `Icon` wraps `Bmp<'a>`, which
/// borrows a byte slice, so a runtime-decoded icon would need either a leaked
/// buffer or an owned-icon type added to the builder. Until one of those
/// exists, notifications render text-only — which is what the builder already
/// handles (`offset()` returns zero without an icon).
#[allow(dead_code)]
fn load_icon(_path: &str) -> Option<Icon<'static>> {
    None
}

impl TryFrom<Message> for Generic {
    type Error = anyhow::Error;

    fn try_from(value: Message) -> Result<Self> {
        // Notify(app_name, replaces_id, app_icon, summary, body, actions,
        //        hints, expire_timeout). Read the five leading strings we
        // actually use, then pull `hints` out of the raw items — `read6`
        // cannot express `a{sv}`.
        let (app_name, _replaces_id, app_icon, summary, body): (String, u32, String, String, String) =
            value.read5()?;

        // `app_icon` may be a themed icon name, an absolute path, or empty.
        // Prefer an explicit hints["image-path"], then an app_icon that looks
        // like a path we can actually open. `hints` is argument index 6.
        let mut image_path: Option<String> = None;
        // `into_vec` consumes the dict; the borrow from `get_items()` ends with
        // the match arm, so clone the item out first.
        if let Some(MessageItem::Dict(dict)) = value.get_items().get(6).cloned() {
            for (key, item) in dict.into_vec() {
                let MessageItem::Str(key) = key else {
                    continue;
                };
                if key != "image-path" && key != "image_path" {
                    continue;
                }
                if let MessageItem::Str(path) = item {
                    if !path.is_empty() {
                        image_path = Some(path);
                    }
                }
            }
        }
        if image_path.is_none() && app_icon.contains('/') {
            image_path = Some(app_icon.clone());
        }

        Ok(Generic {
            app_name,
            summary,
            body,
            image_path,
        })
    }
}

/// Open a bus connection, become a `Notify` monitor, and resolve when that
/// connection is lost. The caller re-runs this to recover. Shared with
/// `notifications.rs`; see the comment there about polling `resource`.
async fn connect_and_monitor(
    rule: MatchRule<'static>,
    mut tx: mpsc::Sender<Message>,
) -> Result<()> {
    let (resource, conn) = connection::new_session_sync()?;
    let conn2 = conn.clone();

    let proxy = nonblock::Proxy::new(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        Duration::from_millis(5000),
        conn,
    );

    // BecomeMonitor lets us watch every Notify on the session bus. The flag
    // argument is 0 (not eavesdropping) per the DBus spec.
    let setup = proxy.method_call::<(), _, _, _>(
        "org.freedesktop.DBus.Monitoring",
        "BecomeMonitor",
        (vec![rule.match_str()], 0_u32),
    );
    let resource = resource;
    tokio::pin!(setup);
    tokio::pin!(resource);

    tokio::select! {
        r = &mut setup => r?,
        err = &mut resource => {
            return Err(anyhow::anyhow!("connection lost during setup: {err}"))
        }
    }

    conn2.start_receive(
        rule,
        Box::new(move |msg, _| { tx.try_send(msg).is_ok() }),
    );

    let err = resource.await;
    warn!("DBus general-notify connection lost ({err}); will re-establish");
    Ok(())
}

impl NotificationProvider for Dbus {
    type NotificationStream<'a>
        = impl futures::Stream<Item = Result<Notification>> + 'a;

    // This needs to be enabled until full GAT support is here
    #[allow(clippy::needless_lifetimes)]
    fn stream(&mut self) -> Result<Self::NotificationStream<'_>> {
        let mut rule = MatchRule::new();
        rule.interface = Some(Interface::from("org.freedesktop.Notifications"));
        rule.member = Some(Member::from("Notify"));

        let duration_seconds = self.duration_seconds;
        let layout = self.layout.clone();

        let (tx, mut rx) = mpsc::channel(16);

        // Same supervisor as notifications.rs: re-establish the monitor when
        // the connection drops, and never panic on it. See that file for why
        // `resource` has to be raced against `BecomeMonitor` with `select!`
        // rather than awaited afterwards.
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            loop {
                match connect_and_monitor(rule.clone(), tx.clone()).await {
                    Ok(()) => backoff = Duration::from_millis(500),
                    Err(e) => {
                        warn!("DBus general-notify monitor unavailable: {e}; retrying");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(10));
                    }
                }
            }
        });

        Ok(try_stream! {
            while let Some(msg) = rx.next().await {
                let generic = match Generic::try_from(msg) {
                    Ok(g) => g,
                    Err(e) => {
                        log::debug!("skipping undecodable notification: {e}");
                        continue;
                    }
                };
                // Empty notifications carry no text; nothing to render.
                if generic.title().is_empty() {
                    continue;
                }
                match generic.render(duration_seconds, layout.clone()) {
                    Ok(n) => yield n,
                    // A bad icon or an over-long title shouldn't kill the stream.
                    Err(e) => log::warn!("could not render notification: {e}"),
                }
            }
        })
    }
}

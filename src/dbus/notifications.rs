use crate::{
    render::{
        notifications::{Icon, Notification, NotificationBuilder, NotificationProvider},
        scheduler::NotificationWrapper,
    },
    scheduler::NOTIFICATION_PROVIDERS,
};
use anyhow::{anyhow, Result};
use async_stream::try_stream;
use dbus::{
    arg::messageitem::MessageItem,
    channel::MatchingReceiver,
    message::MatchRule,
    nonblock,
    strings::{Interface, Member},
    Message,
};
use dbus_tokio::connection;
use embedded_graphics::pixelcolor::BinaryColor;
use futures::{channel::mpsc, StreamExt};
use futures_core::Stream;
use linkme::distributed_slice;
use log::{debug, info, warn};
use std::{convert::TryFrom, sync::LazyLock, time::Duration};
use tinybmp::Bmp;

#[distributed_slice(NOTIFICATION_PROVIDERS)]
static PROVIDER_INIT: fn() -> Result<Box<dyn NotificationWrapper>> = register_callback;

#[allow(clippy::unnecessary_wraps)]
fn register_callback() -> Result<Box<dyn NotificationWrapper>> {
    info!("Registering DBUS notification source.");
    let dbus = Box::new(Dbus {});
    Ok(dbus)
}

static DISCORD_ICON: &[u8] = include_bytes!("./../../assets/discord.bmp");

static DISCORD_ICON_BMP: LazyLock<Bmp<'static, BinaryColor>> =
    LazyLock::new(|| Bmp::<BinaryColor>::from_slice(DISCORD_ICON).expect("Failed to parse BMP"));

pub struct Dbus {}

enum NotificationType {
    Discord { title: String, content: String },
    Unsupported,
}

impl NotificationType {
    pub fn render(&self) -> Result<Notification> {
        let builder = NotificationBuilder::new();

        match self {
            NotificationType::Discord { title, content } => {
                let icon = Icon::new(*DISCORD_ICON_BMP);
                builder
                    .with_icon(icon)
                    .with_content(content)
                    .with_title(title)
                    .build()
            }
            NotificationType::Unsupported => Err(anyhow!("Unsupported notification type!")),
        }
    }
}

impl TryFrom<Message> for NotificationType {
    type Error = anyhow::Error;

    fn try_from(value: Message) -> Result<Self, <Self as TryFrom<Message>>::Error> {
        let source = value.get_source()?;

        Ok(match source.as_str() {
            "discord" => {
                let (_, _, _, title, content) =
                    value.read5::<String, u32, String, String, String>()?;
                if let Some(MessageItem::Dict(dict)) = value.get_items().get(6) {
                    if let Some((MessageItem::Str(key), _)) = dict.last() {
                        if key != "sender-pid" {
                            return Ok(NotificationType::Unsupported);
                        }
                    }
                }

                NotificationType::Discord { title, content }
            }
            _ => NotificationType::Unsupported,
        })
    }
}

trait MessageExt {
    fn get_source(&self) -> Result<String>;
}

impl MessageExt for Message {
    fn get_source(&self) -> Result<String> {
        self.get1::<String>()
            .ok_or_else(|| anyhow!("Couldn't get source"))
    }
}

/// Open a bus connection, become a `Notify` monitor, and resolve when that
/// connection is lost. The caller re-runs this to recover.
/// Decode the XML character references the notification spec permits in the
/// summary and body strings.
///
/// The spec calls these "body markup", and real senders escape them --
/// `notify-send` turns `&` into `&amp;`, which arrived here verbatim and drew
/// a literal "amp" on the panel. Only the five predefined entities plus numeric
/// character references are handled; anything else is left exactly as sent, so
/// a bare `&` (or a malformed `&foo`) is never mangled or dropped.
///
/// Parsed by hand rather than pulled from an XML parser: the strings are a
/// single run of text with no tags, so there is nothing to parse, and an
/// unterminated reference must degrade to literal text rather than error.
pub(crate) fn unescape_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let Some(semi) = tail.find(';') else {
            // No terminator before the end: not a reference, just an ampersand.
            out.push('&');
            out.push_str(tail);
            return out;
        };
        let entity = &tail[..semi];
        // Bounded so a long run of text between `&` and `;` is not treated as
        // one enormous entity name.
        if entity.len() > 12 {
            out.push('&');
            out.push_str(tail);
            return out;
        }
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    if let Some(hex) = n.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        n.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

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

    // `BecomeMonitor` is the modern approach to monitoring messages on the bus.
    // There used to be `eavesdrop` but it's since been deprecated.
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
        Box::new(move |msg, _| {
            debug!("DBus event from {:?}", msg.sender());
            tx.try_send(msg).is_ok()
        }),
    );

    // Monitor is live; wait for the connection to go away.
    let err = resource.await;
    warn!("DBus Notify connection lost ({err}); will re-establish");
    Ok(())
}

impl NotificationProvider for Dbus {
    type NotificationStream<'a> = impl Stream<Item = Result<Notification>> + 'a;

    // This needs to be enabled until full GAT support is here
    #[allow(clippy::needless_lifetimes)]
    fn stream(&mut self) -> Result<<Self as NotificationProvider>::NotificationStream<'_>> {
        let mut rule = MatchRule::new();
        rule.interface = Some(Interface::from("org.freedesktop.Notifications"));
        rule.member = Some(Member::from("Notify"));

        let (tx, mut rx) = mpsc::channel(10);

        // Supervisor: keep a Notify monitor on the bus, rebuilding it if the
        // connection drops.
        //
        // The critical detail, learned the hard way: the `Resource` returned by
        // `new_session_sync()` must be POLLED while `BecomeMonitor` is in
        // flight -- it is what drives the connection. Holding it and awaiting it
        // afterwards stalls the socket, the reply never arrives, and setup times
        // out. So it is pinned and raced against the method call with
        // `select!`, then awaited afterwards to detect the drop.
        //
        // This used to `panic!` instead, which both logged a panic and left
        // notifications dead for the rest of the process's life.
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            loop {
                match connect_and_monitor(rule.clone(), tx.clone()).await {
                    Ok(()) => backoff = Duration::from_millis(500),
                    Err(e) => {
                        warn!("DBus Notify monitor unavailable: {e}; retrying");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(10));
                    }
                }
            }
        });

        Ok(try_stream! {
             while let Some(msg) = rx.next().await {
                let ty = NotificationType::try_from(msg)?;

                if let NotificationType::Unsupported = &ty {
                    continue;
                }
                if let Ok(notif) = ty.render() {
                    yield notif;
                }
            }
            println!("WTF?");
        })
    }
}

#[cfg(test)]
mod unescape_tests {
    use super::unescape_entities;

    #[test]
    fn decodes_predefined_entities() {
        assert_eq!(unescape_entities("fish &amp; chips"), "fish & chips");
        assert_eq!(unescape_entities("a&lt;b&gt;c"), "a<b>c");
        assert_eq!(unescape_entities("&quot;quoted&quot;"), "\"quoted\"");
        assert_eq!(unescape_entities("it&apos;s"), "it's");
    }

    #[test]
    fn decodes_numeric_references() {
        assert_eq!(unescape_entities("caf&#233;"), "caf\u{e9}");
        assert_eq!(unescape_entities("&#x41;"), "A");
        // Out of Unicode range: must stay literal, not vanish or panic.
        assert_eq!(unescape_entities("&#1114112;"), "&#1114112;");
    }

    /// The failure mode that motivated this: a bare `&` is legal text and must
    /// survive untouched, as must anything that only looks like a reference.
    #[test]
    fn leaves_bare_ampersands_and_junk_alone() {
        assert_eq!(unescape_entities("R&D 50% & more"), "R&D 50% & more");
        assert_eq!(unescape_entities("&"), "&");
        assert_eq!(unescape_entities("&notanentity;"), "&notanentity;");
        // Unterminated: no `;` anywhere after the `&`.
        assert_eq!(unescape_entities("a & b"), "a & b");
        assert_eq!(unescape_entities("trailing &"), "trailing &");
        // A long run before `;` is text, not one giant entity name.
        let long_run = format!("&{};", "x".repeat(40));
        assert_eq!(unescape_entities(&long_run), long_run);
        assert_eq!(unescape_entities(""), "");
        assert_eq!(unescape_entities("no entities here"), "no entities here");
    }

    /// Decoding must be idempotent for already-plain text, so a body that was
    /// NOT escaped is not corrupted by our own pass.
    #[test]
    fn plain_text_round_trips() {
        for t in ["plain", "100% done", "a & b", "<tag>", "x & y & z"] {
            assert_eq!(unescape_entities(t), t);
        }
    }
}

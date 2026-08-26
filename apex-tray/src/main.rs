//! ss-oled system tray.
//!
//! A StatusNotifierItem (KDE Plasma / most Wayland bars) exposing daemon
//! control: provider switching, rotation lock, restart, settings edit.
//! Talks to the running daemon over its unix socket.

use anyhow::Result;
use ksni::{
    menu::{CheckmarkItem, MenuItem, StandardItem, SubMenu},
    Tray,
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::time::{interval, Duration};

/// One request/response over the daemon socket (blocking; called from
/// ksni's menu-activation callbacks which run on their own thread).
fn ipc(cmd: &str, path: &PathBuf) -> Result<String> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    let mut writer = &stream;
    writer.write_all(format!("{cmd}\n").as_bytes())?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim().to_string())
}

struct SsOledTray {
    socket_path: PathBuf,
    locked: Arc<std::sync::atomic::AtomicBool>,
    current_provider: Arc<Mutex<String>>,
    providers: Vec<String>,
}

impl SsOledTray {
    fn send_and_refresh(&self, cmd: &str) {
        if let Ok(resp) = ipc(cmd, &self.socket_path) {
            // Responses like "ok <provider>" / "locked <provider>" let us
            // refresh local state from the daemon's answer.
            let mut it = resp.split_whitespace().peekable();
            if it.peek() == Some(&"ok") || it.peek() == Some(&"err") {
                it.next();
            }
            if let Some(state) = it.next() {
                match state {
                    "locked" | "unlocked" => self
                        .locked
                        .store(state == "locked", std::sync::atomic::Ordering::SeqCst),
                    name => {
                        *self.current_provider.lock().unwrap() = name.to_string();
                    }
                }
                if let Some(name) = it.next() {
                    *self.current_provider.lock().unwrap() = name.to_string();
                }
            }
        }
    }
}

impl Tray for SsOledTray {
    fn icon_name(&self) -> String {
        "input-keyboard".into()
    }

    fn title(&self) -> String {
        format!("ss-oled — {}", self.current_provider.lock().unwrap())
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![];

        // Lock toggle
        items.push(
            CheckmarkItem {
                label: "Lock provider".into(),
                checked: self.locked.load(std::sync::atomic::Ordering::SeqCst),
                activate: Box::new(|tray: &mut Self| {
                    let cmd = if tray.locked.load(std::sync::atomic::Ordering::SeqCst) {
                        "unlock"
                    } else {
                        "lock"
                    };
                    tray.send_and_refresh(cmd);
                }),
                ..Default::default()
            }
            .into(),
        );

        // Provider submenu
        let submenu: Vec<MenuItem<Self>> = self
            .providers
            .iter()
            .map(|p| {
                let name = p.clone();
                StandardItem {
                    label: p.clone(),
                    activate: Box::new(move |tray: &mut Self| {
                        tray.send_and_refresh(&format!("goto {}", name));
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        items.push(
            SubMenu {
                label: "Provider".into(),
                submenu,
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // Open the settings GUI (single instance: focus existing via pkill-less
        // check is overkill; spawning a second window is harmless but avoid it
        // by testing for a running apex-gui first).
        items.push(
            StandardItem {
                label: "Open settings…".into(),
                activate: Box::new(|_tray: &mut Self| {
                    let already = std::process::Command::new("pgrep")
                        .arg("-f")
                        .arg("apex-gui")
                        .output()
                        .map(|o| !o.stdout.is_empty())
                        .unwrap_or(false);
                    if already {
                        return;
                    }
                    // The GUI needs a display connection; if this tray was
                    // started without one in its environment, supply the usual
                    // Plasma session defaults.
                    let mut cmd = std::process::Command::new("sh");
                    cmd.arg("-c").arg(
                        "nohup ~/.config/apex-tux/../../projects/apex-tux/target/release/apex-gui >/dev/null 2>&1 &",
                    );
                    let have_display = std::env::var_os("WAYLAND_DISPLAY").is_some()
                        || std::env::var_os("DISPLAY").is_some();
                    if !have_display {
                        cmd.env("WAYLAND_DISPLAY", "wayland-0");
                        cmd.env("XDG_SESSION_TYPE", "wayland");
                    }
                    let _ = cmd.spawn();
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(
            StandardItem {
                label: "Restart daemon".into(),
                activate: Box::new(|_tray: &mut Self| {
                    let _ = std::process::Command::new("systemctl")
                        .args(["--user", "restart", "apex-tux"])
                        .spawn();
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_tray: &mut Self| {
                    // Full-suite shutdown: config editor, then daemon, then us.
                    let _ = std::process::Command::new("pkill")
                        .args(["-f", "apex-gui"])
                        .spawn();
                    let _ = std::process::Command::new("systemctl")
                        .args(["--user", "stop", "apex-tux"])
                        .spawn();
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let socket_path = PathBuf::from(runtime_dir).join("apex-tux.sock");

    // Initial state from the daemon.
    let status = ipc("status", &socket_path).unwrap_or_else(|_| "unlocked ?".into());
    let mut parts = status.split_whitespace();
    let locked_state = parts.next() == Some("locked");
    let current = parts.next().unwrap_or("?").to_string();

    let providers: Vec<String> = ipc("providers", &socket_path)
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();

    println!(
        "apex-tray: {} providers, current={}, locked={}",
        providers.len(),
        current,
        locked_state
    );

    let tray = SsOledTray {
        socket_path: socket_path.clone(),
        locked: Arc::new(std::sync::atomic::AtomicBool::new(locked_state)),
        current_provider: Arc::new(Mutex::new(current)),
        providers,
    };

    // Poll status every 3s to keep the title in sync with external changes
    // (hotkeys pressed on the keyboard, other clients).
    let poll_locked = Arc::clone(&tray.locked);
    let poll_current = Arc::clone(&tray.current_provider);
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            if let Ok(status) = ipc("status", &socket_path) {
                let mut it = status.split_whitespace();
                let state = it.next() == Some("locked");
                let name = it.next().unwrap_or("?").to_string();
                poll_locked.store(state, std::sync::atomic::Ordering::SeqCst);
                *poll_current.lock().unwrap() = name;
            }
        }
    });

    let service = ksni::TrayService::new(tray);
    service.spawn();

    // Park forever.
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

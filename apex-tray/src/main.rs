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
    providers: Arc<Mutex<Vec<String>>>,
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

        // Next / previous provider. The daemon has always accepted `next` and
        // `prev` over IPC; these entries expose it from the tray the same way
        // the hotkeys do. Disabled when there is nothing to switch between.
        let multi_provider = self.providers.lock().unwrap().len() > 1;
        items.push(
            StandardItem {
                label: "Next provider".into(),
                enabled: multi_provider,
                activate: Box::new(|tray: &mut Self| {
                    tray.send_and_refresh("next");
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Previous provider".into(),
                enabled: multi_provider,
                activate: Box::new(|tray: &mut Self| {
                    tray.send_and_refresh("prev");
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

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
            .lock()
            .unwrap()
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
                    if gui_running() {
                        return;
                    }
                    // The GUI needs a display connection; if this tray was
                    // started without one in its environment, supply the usual
                    // Plasma session defaults.
                    let mut cmd = std::process::Command::new("sh");
                    cmd.arg("-c").arg(
                        "nohup ~/.config/apex-tux/../../projects/apex-tux/target/release/apex-gui \
                         >/dev/null 2>&1 &",
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
        providers: Arc::new(Mutex::new(providers)),
    };

    // Poll status every 3s to keep the title in sync with external changes
    // (hotkeys pressed on the keyboard, other clients).
    // Clone the shared state out of `tray` BEFORE it moves into the service
    // (`ksni::Handle::model` is private, so it cannot be read back later).
    let poll_locked = Arc::clone(&tray.locked);
    let poll_current = Arc::clone(&tray.current_provider);
    let poll_providers = Arc::clone(&tray.providers);

    let service = ksni::TrayService::new(tray);
    // Grab the handle before `spawn` consumes the service.
    let handle = service.state();
    service.spawn();

    // The SNI menu is cached DBus-side; ksni only rebuilds it when asked. The
    // poll loop mutates shared state, which is NOT enough on its own — without
    // `Handle::update` the checkmark keeps showing whatever was true when the
    // menu was first built.
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            if let Ok(status) = ipc("status", &socket_path) {
                let mut it = status.split_whitespace();
                let state = it.next() == Some("locked");
                let name = it.next().unwrap_or("?").to_string();
                let prev = poll_locked.swap(state, std::sync::atomic::Ordering::SeqCst);
                let mut changed = false;
                if prev != state {
                    eprintln!("apex-tray: lock state {prev} -> {state} (via poll)");
                    changed = true;
                }
                {
                    let mut cur = poll_current.lock().unwrap();
                    if *cur != name {
                        *cur = name;
                        changed = true;
                    }
                }
                if changed {
                    // Mutating the atomics is NOT enough: the SNI menu is
                    // cached on the DBus side and only rebuilt when ksni is
                    // told to update. Without this the checkmark keeps showing
                    // whatever was true when the menu was first built.
                    handle.update(|_| ());
                }
            }
            // The provider list is NOT static: enabling, disabling, adding or
            // removing one in the GUI changes what the daemon reports, and the
            // daemon only re-reads the config on restart. Without this the
            // dropdown stays frozen at whatever it saw at startup.
            if let Ok(list) = ipc("providers", &socket_path) {
                let fresh: Vec<String> =
                    list.split_whitespace().map(String::from).collect();
                let mut guard = poll_providers.lock().unwrap();
                if *guard != fresh {
                    eprintln!(
                        "apex-tray: provider list changed ({} -> {}): {:?}",
                        guard.len(),
                        fresh.len(),
                        fresh
                    );
                    *guard = fresh;
                    drop(guard);
                    handle.update(|_| ());
                }
            }
        }
    });

    // Park forever.
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// Is an apex-gui actually running?
///
/// `pgrep -f apex-gui` alone is not enough: a process that already exited but
/// has not been reaped by its parent still shows up as `[apex-gui] <defunct>`.
/// Treating that as "already running" makes this menu item silently do nothing,
/// with no window and no error -- it just looks like the GUI is broken.
/// Only a process still in a running state counts.
fn gui_running() -> bool {
    let out = match std::process::Command::new("pgrep").args(["-f", "apex-gui"]).output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .any(|pid| {
            // Field 3 of /proc/<pid>/stat is the single-letter state, and it
            // sits after the comm field in parentheses -- which may itself
            // contain spaces or ')'. Split on the LAST ')' to find it safely.
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false; // gone between pgrep and now
            };
            match stat.rfind(')') {
                Some(i) => stat[i + 1..]
                    .split_whitespace()
                    .next()
                    .map(|state| state != "Z")
                    .unwrap_or(false),
                None => false,
            }
        })
}

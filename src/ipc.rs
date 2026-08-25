//! IPC control interface for ss-oled.
//!
//! A small unix-socket server that lets external tools (tray, CLI, scripts)
//! drive the running daemon: switch providers, toggle lock, query status.
//!
//! Socket path: `$XDG_RUNTIME_DIR/apex-tux.sock` (typically
//! `/run/user/1000/apex-tux.sock`). Line-based protocol, one command per
//! line; responses are single lines ending in `\n`.
//!
//! Commands:
//!   next          -> switch to the next provider
//!   prev          -> switch to the previous provider
//!   lock          -> pin current provider (suppresses rotation + focus jumps)
//!   unlock        -> resume auto-rotation and event reactions
//!   status        -> returns `locked|unlocked <provider_name>`
//!   providers     -> returns space-separated provider names in rotation order
//!
//! Unknown commands get `err unknown command`. Every request gets exactly
//! one response line, so clients can do request/response over one socket.

use anyhow::Result;
use log::{info, warn};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

/// Commands the scheduler understands from IPC.
#[derive(Debug, Clone, PartialEq)]
pub enum IpcCommand {
    Next,
    Prev,
    Lock,
    Unlock,
}

/// Handle the scheduler keeps so it can react to IPC-driven state changes.
#[derive(Clone)]
pub struct IpcHandle {
    pub tx: broadcast::Sender<IpcCommand>,
    pub locked: Arc<AtomicBool>,
    /// Names of registered providers in rotation order.
    pub provider_names: Arc<Vec<String>>,
    pub current: Arc<std::sync::atomic::AtomicUsize>,
}

impl IpcHandle {
    pub fn new(provider_names: Vec<String>) -> Self {
        let (tx, _) = broadcast::channel(16);
        Self {
            tx,
            locked: Arc::new(AtomicBool::new(false)),
            provider_names: Arc::new(provider_names),
            current: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn status_line(&self) -> String {
        let state = if self.locked.load(Ordering::SeqCst) {
            "locked"
        } else {
            "unlocked"
        };
        let idx = self.current.load(Ordering::SeqCst);
        let name = self
            .provider_names
            .get(idx)
            .map(String::as_str)
            .unwrap_or("?");
        format!("{state} {name}")
    }
}

/// Spawn the IPC server. `handle` is shared with the scheduler so commands
/// and state stay in sync.
pub fn spawn(socket_dir: &std::path::Path, handle: IpcHandle) -> Result<()> {
    let path = socket_dir.join("apex-tux.sock");
    // Stale socket from a previous run would bind() to fail.
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)?;
    info!("IPC listening on {}", path.display());

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve(stream, handle).await {
                            warn!("ipc connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    warn!("ipc accept error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });

    Ok(())
}

async fn serve(stream: UnixStream, handle: IpcHandle) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }

        let response = match cmd {
            "next" | "prev" => {
                // Rotation is allowed even when locked — manual movement
                // carries the lock, matching hotkey semantics.
                let c = if cmd == "next" {
                    IpcCommand::Next
                } else {
                    IpcCommand::Prev
                };
                let idx_before = handle.current.load(Ordering::SeqCst);
                let size = handle.provider_names.len();
                let idx_after = if cmd == "next" {
                    (idx_before + 1) % size
                } else if idx_before == 0 {
                    size - 1
                } else {
                    idx_before - 1
                };
                handle.current.store(idx_after, Ordering::SeqCst);
                let _ = handle.tx.send(c);
                format!(
                    "ok {}",
                    handle
                        .provider_names
                        .get(idx_after)
                        .map(String::as_str)
                        .unwrap_or("?")
                )
            }
            "lock" => {
                handle.locked.store(true, Ordering::SeqCst);
                info!("IPC: provider LOCKED");
                "ok locked".to_string()
            }
            "unlock" => {
                handle.locked.store(false, Ordering::SeqCst);
                info!("IPC: provider UNLOCKED");
                "ok unlocked".to_string()
            }
            "status" => handle.status_line(),
            "providers" => handle.provider_names.join(" "),
            other => format!("err unknown command '{other}'"),
        };

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }

    Ok(())
}

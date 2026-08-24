#[derive(Debug, Copy, Clone)]
pub enum Command {
    PreviousSource,
    NextSource,
    /// Pin the current provider: no auto-rotation countdown. / and * still
    /// move between providers while staying locked.
    LockSource,
    /// Return to normal auto-rotation.
    UnlockSource,
    Shutdown,
}

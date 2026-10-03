#[derive(Debug, Copy, Clone)]
pub enum Command {
    PreviousSource,
    NextSource,
    /// Pin the current provider: no auto-rotation countdown. / and * still
    /// move between providers while staying locked.
    LockSource,
    /// Toggle pinned-provider state.
    ToggleLockSource,
    /// Return to normal auto-rotation.
    UnlockSource,
    /// Step to the next item of the current custom-API provider (i.e. the next
    /// element of the JSON array). No-op for any other provider.
    NextItem,
    /// Step to the previous item of the current custom-API provider.
    PreviousItem,
    /// Scroll the current custom-API provider's article down one line.
    ScrollDown,
    /// Scroll the current custom-API provider's article up one line.
    ScrollUp,
    /// Enter/leave the article view of the current custom-API provider.
    ToggleDetail,
    Shutdown,
}

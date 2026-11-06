# Patched dependencies

Copies of crates.io crates with small changes, used through `[patch.crates-io]` in the workspace `Cargo.toml`. Each keeps its upstream license. Delete a folder and its `[patch.crates-io]` line once upstream ships the fix.

## gpui-pre-windows 0.3.11

GPUI's Windows backend from [Zed](https://github.com/zed-industries/zed) (`crates/gpui_windows`), Apache-2.0.

On Windows, GPUI's vsync thread wakes every window on every display refresh, even when nothing changed. An open but idle window used 3 to 4% of a CPU core on a high refresh rate display. GPUI already tells the platform when a window needs a frame (`frame_waker`), and the macOS backend draws only then. The patch does the same on Windows: the vsync thread parks after 30 refreshes without a frame request and wakes on the next one.

Changed files, each marked at the top:

- `src/vsync.rs`: `FRAME_DEMAND`, the wake and park signal.
- `src/platform.rs`: the vsync loop parks when idle, and stopping wakes it.
- `src/window.rs`: `frame_waker` wakes the vsync thread, and so does a failed draw, so a lost GPU device is still recovered.
- `src/events.rs`: a paint deferred during a nested message loop wakes it.

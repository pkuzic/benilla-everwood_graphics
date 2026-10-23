# benilla's `wgpu-hal` fork — what differs from upstream, and how to check

Upstream: [`gfx-rs/wgpu`](https://github.com/gfx-rs/wgpu) `wgpu-hal`, MIT OR Apache-2.0, version
`27.0.4` — the version the workspace lock resolves to (via `bevy_render` 0.18.1 → `wgpu` 27).
Wired in through `[patch.crates-io]` in the workspace root, the way `lua-src` and `kira` are. The
crate is copied from the cargo registry as published (`src/`, `build.rs`, `Cargo.toml`,
`README.md`, the two licence files); the published `Cargo.toml`'s three `[[example]]` targets are
dropped, since `examples/` is not copied. Every hunk is marked `BENILLA:`.

## Why a fork exists at all

A presentation-engine stall longer than wgpu-core's `FRAME_TIMEOUT_MS` (1 s) killed the client:

```
bevy_render .../view/window/mod.rs:304: Couldn't get swap chain texture, operation
unrecoverable: Acquiring a texture failed with a generic error
```

followed by a cascade of `uniform_buffer.rs:312 unwrap None` / `Buffer with '' label is invalid`
panics. The chain, on Vulkan (the Windows default):

1. `vulkan::Surface::acquire_texture` waits up to the 1 s timeout on the fence that says the
   acquired image is ready (the DXGI-swapchain frame-pacing wait added for gfx-rs/wgpu#8310).
2. Upstream maps that wait's `VK_TIMEOUT` through `map_host_device_oom_and_lost_err`, which turns
   any non-OOM, non-lost result into `DeviceError::Unexpected`.
3. wgpu-core's `handle_hal_error` treats `Unexpected` (like OOM and Lost) as fatal and **loses the
   device**; `wgpu` drops device-lost errors from the error sink (`ErrorType::DeviceLost =>
   return`), so nothing is logged, and `Surface::get_current_texture` reports `SurfaceStatus::
   Unknown` = `SurfaceError::Other`, the "generic error" Bevy panics on.

The stall itself is outside the process — no TDR (`Display` 4101) or `nvlddmkm` event in the
System log, so the GPU never hung: on the dev box every crash came after 5-15 s main-thread hitches
while the machine was loaded (other sessions compiling and linking multi-GB PDBs; the page file
peaked at 14 GB), the kind of system-wide stall that holds the presentation engine past 1 s.

## Patch 1 — `src/vulkan/instance.rs`, `Surface::acquire_texture`

- `vkAcquireNextImageKHR` → `VK_TIMEOUT`: retried until `STALL_BUDGET` (30 s) instead of returning
  `Ok(None)` at once (Bevy's `prepare_windows` panics on `SurfaceError::Timeout` off Linux). A
  timed-out acquire leaves the semaphore and fence unsignalled, so the retry is legal.
- The post-acquire `vkWaitForFences` → `VK_TIMEOUT`: retried until `STALL_BUDGET`. The image is
  already acquired, so waiting for it is the only correct move; any other result is logged with its
  raw `vk::Result` and mapped as upstream does.

Each 1 s retry logs an `error!` (the client's filter is `wgpu=error`), so a stall is visible in the
log and the crash report's ring instead of silent.

## Patch 2 — `src/vulkan/mod.rs`, `get_unexpected_err` / `get_oom_err` / `get_lost_err`

Log the raw `vk::Result` before returning, so a future device loss names its cause.

## How to check

`grep -rn BENILLA third_party/wgpu-hal/src` lists every hunk; `diff -r` against
`~/.cargo/registry/src/*/wgpu-hal-27.0.4/src` shows nothing else. Drop the fork when the
workspace moves to a `wgpu-hal` whose `acquire_texture` handles the timeout itself.

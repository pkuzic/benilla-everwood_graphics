//! MONKEY (EME): name a lost GPU device in the log before Bevy's render systems panic over it.
//!
//! wgpu routes a lost device (`DeviceError::Lost`, a hal out-of-memory, `Unexpected`) to the
//! device-lost callback and drops it from the error sink, and Bevy installs no callback: the first
//! sign of it is `prepare_windows` panicking "Couldn't get swap chain texture ... generic error",
//! then a cascade of invalid-buffer panics, with the reason nowhere. This logs the reason (it
//! reaches the crash report's log ring) and keeps wgpu's uncaptured-error behaviour (log, then
//! panic) with the error text in the log first.

use std::sync::Arc;

use bevy::prelude::*;
use bevy::render::renderer::RenderDevice;

pub(crate) struct GpuLostPlugin;

impl Plugin for GpuLostPlugin {
    fn build(&self, _app: &mut App) {}

    fn finish(&self, app: &mut App) {
        let Some(device) = app.world().get_resource::<RenderDevice>() else {
            // Headless (`backends: None`): no device to watch.
            return;
        };
        info!("gpu: device-lost and uncaptured-error hooks installed");
        let device = device.wgpu_device();
        device.set_device_lost_callback(|reason, message| {
            error!("GPU device lost ({reason:?}): {message}");
        });
        device.on_uncaptured_error(Arc::new(|err: wgpu::Error| {
            error!("wgpu uncaptured error: {err}");
            panic!("wgpu error: {err}\n");
        }));
    }
}

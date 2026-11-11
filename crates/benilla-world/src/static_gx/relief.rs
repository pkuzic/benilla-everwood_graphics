//! MONKEY (model relief): the retained pass's relief maps — `benilla_assets::model_relief`'s
//! offline WMO / doodad normal + cavity + height maps, for the statics `static_gx.wgsl` draws
//! (which never see a `WowModelMaterial`, so the entity lane's per-material binding cannot reach
//! them).
//!
//! The base textures live in per-(size, format, mips) array classes ([`super::pool`]); a relief
//! map is a different format (Rgba8Unorm, linear) at its texture's native size, so it cannot share
//! its base's class. Instead ONE fixed array holds every static's relief: [`RELIEF_POOL_SIZE`]²,
//! a full mip chain, [`RELIEF_POOL_LAYERS`] layers. It is allocated LAZILY, the first time
//! `modelRelief` is non-zero and an item whose texture has a map asks for a layer, so with the
//! dial at 0 (or without a relief directory) it costs no VRAM; until then group 1 binds a 1×1
//! one-layer dummy, so the layout never changes. Full, further items simply get no relief (one log
//! line). It resets with the map, like the base pool.
//!
//! A map arrives as a `GpuImage` of its own size, so it is RESAMPLED into its layer on the GPU
//! (`static_gx_relief_blit.wgsl`, one fullscreen triangle per destination mip reading the source
//! level of matching size) rather than copied; nothing is kept on the CPU. A layer is published
//! to records only once its blit has been submitted ([`GxReliefPool::layer_bits`]), so a record
//! never points at an unwritten layer.
//!
//! The live dials (`modelRelief` / `modelReliefDepth`, mirrored process-wide by
//! `benilla_assets::model_relief::set_model_relief_dials`) reach the shader through a 16-byte
//! uniform at group 1 binding 6 ([`GxReliefPool::dial`]), one persistent buffer every region's
//! bind group shares, so a dial change is one `write_buffer` and re-bakes nothing.

use bevy::asset::AssetId;
use bevy::image::Image;
use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{sampler, texture_2d, uniform_buffer_sized};
use bevy::render::render_resource::*;
use bevy::render::renderer::{RenderDevice, RenderQueue};
use bevy::render::texture::GpuImage;

/// MONKEY (model relief): the relief array's layer edge in texels (square).
pub(super) const RELIEF_POOL_SIZE: u32 = 256;
/// MONKEY (model relief): its mip count, a full chain down to 1×1.
const RELIEF_POOL_MIPS: u32 = 9;
/// MONKEY (model relief): its fixed capacity in layers (clamped to the device's array limit):
/// 256² × 4 B × 4/3 ≈ 341 KiB a layer, ≈ 341 MiB (357 MB) at 1024.
pub(super) const RELIEF_POOL_LAYERS: u32 = 1024;
/// MONKEY (model relief): record column `x` carries the base layer in bits 0..16 (a class holds
/// at most the device's 2048 layers, so the high half was always zero) and `relief layer + 1` in
/// bits 16..32 (0 = no relief). **Keep in sync with `static_gx.wgsl`'s `RECORD_RELIEF_SHIFT`.**
pub(super) const RECORD_RELIEF_SHIFT: u32 = 16;
/// MONKEY (model relief): the base-layer half of record column `x`.
pub(super) const RECORD_BASE_LAYER_MASK: u32 = 0xffff;

/// MONKEY (model relief): what a region's relief columns were last resolved against — the relief
/// index's registration count, the pool's landed-blit count, and whether the dial was on. A region
/// whose key differs re-resolves (see `render.rs` `sync_relief`); while the dial is off the key is
/// constant, so a loading burst of new maps costs nothing.
pub(super) type ReliefKey = (u32, u32, bool);

/// MONKEY (model relief): the render world's relief array, its blit pipeline and the dial uniform.
#[derive(Resource)]
pub(super) struct GxReliefPool {
    /// The 1×1 one-layer stand-in bound until the array exists (never sampled: no record names a
    /// layer then). Its texture is kept alive by the view.
    dummy_view: TextureView,
    /// The array and its D2Array view, once allocated.
    array: Option<(Texture, TextureView)>,
    capacity: u32,
    /// Relief image id → its layer.
    layers: HashMap<AssetId<Image>, u16>,
    /// Per layer: its blit has been submitted.
    ready: Vec<bool>,
    /// Queued (relief image id, layer) blits, waiting for the source's `GpuImage` and the pipeline.
    pending: Vec<(AssetId<Image>, u16)>,
    full_logged: bool,
    /// Bumped each frame some blit lands (and on reset), a [`ReliefKey`] input.
    landed: u32,
    /// Group 1 binding 6: `[mode, depth, 0, 0]`.
    pub(super) dial: Buffer,
    dial_written: Option<[f32; 4]>,
    blit_pipeline: CachedRenderPipelineId,
    blit_src_layout: BindGroupLayoutDescriptor,
    blit_mip_layout: BindGroupLayoutDescriptor,
    blit_sampler: Sampler,
    /// One `[edge, mip, 0, 0]` uniform per destination mip, made once.
    blit_mip_buffers: Vec<Buffer>,
    blit_mip_groups: Vec<BindGroup>,
}

impl GxReliefPool {
    pub(super) fn new(
        render_device: &RenderDevice,
        pipeline_cache: &PipelineCache,
        asset_server: &AssetServer,
    ) -> Self {
        let dummy = render_device.create_texture(&TextureDescriptor {
            label: Some("static_gx_relief_dummy"),
            size: Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy_view = dummy.create_view(&TextureViewDescriptor {
            dimension: Some(TextureViewDimension::D2Array),
            ..Default::default()
        });
        let dial = render_device.create_buffer(&BufferDescriptor {
            label: Some("static_gx_relief_dial"),
            size: 16,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let blit_src_layout = BindGroupLayoutDescriptor::new(
            "static_gx_relief_blit_src",
            &BindGroupLayoutEntries::sequential(
                ShaderStages::FRAGMENT,
                (
                    texture_2d(TextureSampleType::Float { filterable: true }),
                    sampler(SamplerBindingType::Filtering),
                ),
            ),
        );
        let blit_mip_layout = BindGroupLayoutDescriptor::new(
            "static_gx_relief_blit_mip",
            &BindGroupLayoutEntries::sequential(
                ShaderStages::FRAGMENT,
                (uniform_buffer_sized(false, Some(std::num::NonZero::new(16).unwrap())),),
            ),
        );
        let shader: Handle<Shader> =
            asset_server.load("embedded://benilla_world/shaders/static_gx_relief_blit.wgsl");
        let blit_pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("static_gx_relief_blit".into()),
            layout: vec![blit_src_layout.clone(), blit_mip_layout.clone()],
            vertex: VertexState {
                shader: shader.clone(),
                shader_defs: vec![],
                entry_point: Some("vertex".into()),
                buffers: vec![],
            },
            fragment: Some(FragmentState {
                shader,
                shader_defs: vec![],
                entry_point: Some("fragment".into()),
                targets: vec![Some(ColorTargetState {
                    format: TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            ..default()
        });
        let blit_sampler = render_device.create_sampler(&SamplerDescriptor {
            label: Some("static_gx_relief_blit"),
            min_filter: FilterMode::Linear,
            mag_filter: FilterMode::Linear,
            mipmap_filter: FilterMode::Linear,
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            ..Default::default()
        });
        let blit_mip_buffers = (0..RELIEF_POOL_MIPS)
            .map(|mip| {
                let edge = (RELIEF_POOL_SIZE >> mip).max(1) as f32;
                render_device.create_buffer_with_data(&BufferInitDescriptor {
                    label: Some("static_gx_relief_blit_mip"),
                    contents: bytemuck::cast_slice(&[edge, mip as f32, 0.0, 0.0]),
                    usage: BufferUsages::UNIFORM,
                })
            })
            .collect();
        Self {
            dummy_view,
            array: None,
            capacity: 0,
            layers: HashMap::default(),
            ready: Vec::new(),
            pending: Vec::new(),
            full_logged: false,
            landed: 0,
            dial,
            dial_written: None,
            blit_pipeline,
            blit_src_layout,
            blit_mip_layout,
            blit_sampler,
            blit_mip_buffers,
            blit_mip_groups: Vec::new(),
        }
    }

    /// MONKEY (model relief): drop the array and every assignment (a map change); the dial, the
    /// dummy and the blit pipeline stay.
    pub(super) fn reset(&mut self) {
        if self.array.is_none() && self.layers.is_empty() {
            return;
        }
        self.array = None;
        self.capacity = 0;
        self.layers.clear();
        self.ready.clear();
        self.pending.clear();
        self.full_logged = false;
        self.landed = self.landed.wrapping_add(1);
    }

    /// MONKEY (model relief): the view group 1 binding 5 binds: the array, or the dummy.
    pub(super) fn view(&self) -> &TextureView {
        self.array.as_ref().map_or(&self.dummy_view, |(_, v)| v)
    }

    /// MONKEY (model relief): whether [`Self::view`] is the real array.
    pub(super) fn is_real(&self) -> bool {
        self.array.is_some()
    }

    /// MONKEY (model relief): push the live dials into the uniform (on change only) and return
    /// the mode; 0 without a relief directory, whatever the dial says.
    pub(super) fn sync_dials(&mut self, render_queue: &RenderQueue) -> u8 {
        let (mode, depth) = if benilla_assets::monkey_relief_dir().is_some() {
            benilla_assets::model_relief::model_relief_dials()
        } else {
            (0, 0.0)
        };
        // MONKEY (model gloss): z = the metal gloss strength (0 = off).
        let words = [
            f32::from(mode),
            depth,
            benilla_assets::model_relief::model_gloss_dial(),
            0.0,
        ];
        if self.dial_written != Some(words) {
            render_queue.write_buffer(&self.dial, 0, bytemuck::cast_slice(&words));
            self.dial_written = Some(words);
        }
        mode
    }

    /// MONKEY (model relief): the current [`ReliefKey`].
    pub(super) fn key(&self, mode_on: bool) -> ReliefKey {
        if mode_on {
            (
                benilla_assets::model_relief::model_relief_generation(),
                self.landed,
                true,
            )
        } else {
            (0, 0, false)
        }
    }

    /// MONKEY (model relief): the record column `x` high half for an item with base texture
    /// `texture`: `(layer + 1) << 16` once its map's layer is written, else 0. A map seen for the
    /// first time takes a layer (allocating the array on the very first) and queues its blit; the
    /// item picks the layer up on the re-resolve after the blit lands.
    pub(super) fn layer_bits(
        &mut self,
        texture: Option<AssetId<Image>>,
        render_device: &RenderDevice,
    ) -> u32 {
        let Some(relief) = texture.and_then(benilla_assets::model_relief::relief_for) else {
            return 0;
        };
        let id = relief.id();
        if let Some(&layer) = self.layers.get(&id) {
            return if self.ready[usize::from(layer)] {
                (u32::from(layer) + 1) << RECORD_RELIEF_SHIFT
            } else {
                0
            };
        }
        if self.array.is_none() {
            self.allocate(render_device);
        }
        let next = self.layers.len() as u32;
        if next >= self.capacity {
            if !self.full_logged {
                self.full_logged = true;
                warn!(
                    "MONKEY (model relief): the static relief array is full ({} layers); further \
                     statics draw without relief until the map changes",
                    self.capacity
                );
            }
            return 0;
        }
        let layer = u16::try_from(next).expect("relief layers under u16");
        self.layers.insert(id, layer);
        self.ready.push(false);
        self.pending.push((id, layer));
        0
    }

    fn allocate(&mut self, render_device: &RenderDevice) {
        let capacity = RELIEF_POOL_LAYERS.min(render_device.limits().max_texture_array_layers);
        let array = render_device.create_texture(&TextureDescriptor {
            label: Some("static_gx_relief_array"),
            size: Extent3d {
                width: RELIEF_POOL_SIZE,
                height: RELIEF_POOL_SIZE,
                depth_or_array_layers: capacity,
            },
            mip_level_count: RELIEF_POOL_MIPS,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = array.create_view(&TextureViewDescriptor {
            dimension: Some(TextureViewDimension::D2Array),
            ..Default::default()
        });
        let level0 = u64::from(RELIEF_POOL_SIZE) * u64::from(RELIEF_POOL_SIZE) * 4;
        let bytes = level0 * 4 / 3 * u64::from(capacity);
        info!(
            "MONKEY (model relief): static relief array allocated: {capacity} layers of \
             {RELIEF_POOL_SIZE}² Rgba8Unorm + mips, {:.0} MB",
            bytes as f64 / 1.0e6
        );
        super::GX_VRAM.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        self.array = Some((array, view));
        self.capacity = capacity;
    }

    /// MONKEY (model relief): encode and submit every queued blit whose source is resident (once
    /// the blit pipeline has compiled); the rest wait. Bumps the landed count when any lands.
    pub(super) fn drain(
        &mut self,
        render_device: &RenderDevice,
        render_queue: &RenderQueue,
        pipeline_cache: &PipelineCache,
        images: &RenderAssets<GpuImage>,
    ) {
        if self.pending.is_empty() {
            return;
        }
        let Some(array) = self.array.as_ref().map(|(t, _)| t.clone()) else {
            return;
        };
        let Some(pipeline) = pipeline_cache.get_render_pipeline(self.blit_pipeline) else {
            return;
        };
        if self.blit_mip_groups.is_empty() {
            let layout = pipeline_cache.get_bind_group_layout(&self.blit_mip_layout);
            self.blit_mip_groups = self
                .blit_mip_buffers
                .iter()
                .map(|b| {
                    render_device.create_bind_group(
                        "static_gx_relief_blit_mip",
                        &layout,
                        &BindGroupEntries::sequential((b.as_entire_binding(),)),
                    )
                })
                .collect();
        }
        let src_layout = pipeline_cache.get_bind_group_layout(&self.blit_src_layout);
        let mut encoder = render_device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("static_gx_relief_blits"),
        });
        let mut landed = false;
        let mut waiting = Vec::new();
        for (id, layer) in std::mem::take(&mut self.pending) {
            let Some(src) = images.get(id) else {
                waiting.push((id, layer));
                continue;
            };
            let src_group = render_device.create_bind_group(
                "static_gx_relief_blit_src",
                &src_layout,
                &BindGroupEntries::sequential((&src.texture_view, &self.blit_sampler)),
            );
            for mip in 0..RELIEF_POOL_MIPS {
                let target = array.create_view(&TextureViewDescriptor {
                    label: Some("static_gx_relief_layer"),
                    format: Some(TextureFormat::Rgba8Unorm),
                    dimension: Some(TextureViewDimension::D2),
                    base_mip_level: mip,
                    mip_level_count: Some(1),
                    base_array_layer: u32::from(layer),
                    array_layer_count: Some(1),
                    ..default()
                });
                let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                    label: Some("static_gx_relief_blit"),
                    color_attachments: &[Some(RenderPassColorAttachment {
                        view: &target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: Operations::default(),
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &*src_group, &[]);
                pass.set_bind_group(1, &*self.blit_mip_groups[mip as usize], &[]);
                pass.draw(0..3, 0..1);
            }
            self.ready[usize::from(layer)] = true;
            landed = true;
        }
        self.pending = waiting;
        if landed {
            render_queue.submit([encoder.finish()]);
            self.landed = self.landed.wrapping_add(1);
        }
    }
}

//! Surface material for the optional MONKEY voxel build site.
//!
//! MONKEY (voxel): it binds the REAL ADT tileset of the ground the volume replaces — the covering
//! tile's `layer_array`, per-chunk `alpha_array` and MCSH `shadow_array` — plus the SHARED global
//! light buffer the terrain reads, and `monkey_voxel.wgsl` reproduces `terrain.wgsl`'s mapping, blend
//! and lighting so a voxel top pixel-matches the ground beside it. The chunk is picked PER FRAGMENT
//! from a 3×3 neighbourhood of the covering tile's MCNK chunks (see [`MonkeyVoxelParams`]). Until
//! the covering tile is resident it falls back to a PROCEDURAL grass/rock/dirt slope-blend (built
//! once at startup into the same `texture_2d_array`), so the volume always renders something.
//!
//! `monkey_voxel_texture_from_terrain` (in `voxel.rs`) swaps the handles, the 3×3 chunk table and
//! the shared light buffer over once they exist. Still `unlit` as a StandardMaterial — the fragment
//! stage does the terrain's own (fixed-function-faithful) light sum from the shared buffer.

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::pbr::{ExtendedMaterial, MaterialExtension};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, Buffer, Extent3d, ShaderType, TextureDataOrder, TextureDimension, TextureFormat,
    TextureViewDescriptor, TextureViewDimension,
};
use bevy::render::renderer::RenderDevice;
use bevy::shader::ShaderRef;

pub type MonkeyVoxelMaterial = ExtendedMaterial<StandardMaterial, MonkeyVoxelExt>;

/// Side of the per-fragment chunk neighbourhood: 3×3 around the chunk under the volume centre (the
/// 32-yd volume never reaches past the neighbours of a 33.33-yd chunk).
pub(super) const MONKEY_VOXEL_GRID: usize = 3;

/// MONKEY (voxel): the uniform at binding 104 — field order MUST match the WGSL `MonkeyVoxelParams`.
#[derive(Clone, Default, ShaderType)]
pub struct MonkeyVoxelParams {
    /// Each neighbourhood chunk's 4 layer-array indices, row-major (row = south, col = east).
    pub layers: [Vec4; 9],
    /// `x` = alpha-array index; `y` = shadow-array index (`< 0` = no MCSH map); `zw` unused.
    pub aux: [Vec4; 9],
    /// `x`/`y` = WoW X (north) / WoW Y (west) of the grid's NW corner; `z` = chunk size (yd);
    /// `w` = layer repeats per chunk ([`benilla_formats::TERRAIN_LAYER_TILES`]).
    pub grid: Vec4,
    /// `x` = procedural tiles per yard; `y` = has the real tileset (≥0.5); `z` = the shared light
    /// buffer is bound (≥0.5, else a baked sun); `w` unused.
    pub misc: Vec4,
}

/// The extension bindings: the tile's ground `layer_array` (100, with ITS repeating sampler at 101 —
/// the one sampler terrain also uses for alpha + shadow), `alpha_array` (102), `shadow_array` (103),
/// the packed params (104) and the shared global light (90, as terrain).
#[derive(Asset, AsBindGroup, Clone, TypePath)]
pub struct MonkeyVoxelExt {
    #[texture(100, dimension = "2d_array")]
    #[sampler(101)]
    layer_array: Handle<Image>,
    #[texture(102, dimension = "2d_array")]
    alpha_array: Handle<Image>,
    #[texture(103, dimension = "2d_array")]
    shadow_array: Handle<Image>,
    #[uniform(104)]
    params: MonkeyVoxelParams,
    /// The shared global light (rows: light, fog, points, MonkeyFrame, shelter) — a zeroed startup
    /// placeholder until [`monkey_voxel_bind_light`] points it at `SharedLightBuffer`.
    #[storage(90, read_only, buffer)]
    light_buf: Buffer,
}

impl MaterialExtension for MonkeyVoxelExt {
    fn fragment_shader() -> ShaderRef {
        "embedded://benilla_world/shaders/monkey_voxel.wgsl".into()
    }
}

/// The one shared voxel material (a single volume at a time), plus the procedural fallback image
/// handles kept resident so the material can be rebuilt without re-synthesising them. `.0` is the
/// material handle (the field every existing call site reads).
#[derive(Resource)]
pub struct MonkeyVoxelMaterialHandle(
    pub Handle<MonkeyVoxelMaterial>,
    /// Procedural fallback `layer_array` (grass/rock/dirt), held so a revert never re-synthesises.
    #[allow(dead_code)] pub(super) Handle<Image>,
    /// Procedural fallback `alpha_array`/`shadow_array` (unread by the fallback path, bound so the
    /// group is valid).
    #[allow(dead_code)] pub(super) Handle<Image>,
);

/// The procedural fallback's own tiling (one repeat per 2 yd), preserving the pre-tileset look.
const MONKEY_VOXEL_PROC_TILES_PER_YARD: f32 = 0.5;

pub fn monkey_voxel_create_material(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<MonkeyVoxelMaterial>>,
    device: Res<RenderDevice>,
    shared_light: Option<Res<crate::lighting::SharedLightBuffer>>,
) {
    // A 4-layer procedural array: [0] fallback/grass, [1] grass, [2] rock, [3] dirt (the shader's
    // fallback path reads 1/2/3 and slope-blends them, exactly as the old three-texture material).
    let layer_array = images.add(monkey_voxel_proc_layers());
    let alpha_array = images.add(monkey_voxel_dummy_alpha());
    // The shared light if the assets startup already made it; else a zeroed placeholder of the same
    // size (wgpu validates the bound size), with `misc.z = 0` so the shader keeps its baked sun
    // until `monkey_voxel_bind_light` swaps the real one in.
    let (light_buf, has_light) = match shared_light {
        Some(l) => (l.0.clone(), 1.0),
        None => (crate::lighting::new_shared_light_buffer(&device).0, 0.0),
    };
    let handle = materials.add(MonkeyVoxelMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            unlit: true,
            cull_mode: None,
            ..default()
        },
        extension: MonkeyVoxelExt {
            layer_array: layer_array.clone(),
            alpha_array: alpha_array.clone(),
            shadow_array: alpha_array.clone(),
            params: MonkeyVoxelParams {
                layers: [Vec4::new(1.0, 2.0, 3.0, 0.0); 9],
                aux: [Vec4::new(0.0, -1.0, 0.0, 0.0); 9],
                grid: Vec4::new(
                    0.0,
                    0.0,
                    benilla_formats::CHUNK_SIZE,
                    benilla_formats::TERRAIN_LAYER_TILES,
                ),
                // Start procedural (no real tile bound yet).
                misc: Vec4::new(MONKEY_VOXEL_PROC_TILES_PER_YARD, 0.0, has_light, 0.0),
            },
            light_buf,
        },
    });
    commands.insert_resource(MonkeyVoxelMaterialHandle(handle, layer_array, alpha_array));
}

/// Whether the material already reads the shared light buffer.
pub(super) fn monkey_voxel_has_light(ext: &MonkeyVoxelExt) -> bool {
    ext.params.misc.z >= 0.5
}

/// Point the material at the shared global light buffer (the terrain's), switching the shader from
/// its baked sun to the terrain light sum.
pub(super) fn monkey_voxel_bind_light(ext: &mut MonkeyVoxelExt, light: Buffer) {
    ext.light_buf = light;
    ext.params.misc.z = 1.0;
}

/// One neighbourhood chunk's shading for [`monkey_voxel_apply_real`].
#[derive(Clone, Copy)]
pub(super) struct MonkeyVoxelChunkShade {
    pub layers: [u32; 4],
    pub alpha: u32,
    pub shadow: f32,
}

/// Update the shared material to wear the real tileset: the tile's own arrays, the 3×3 chunk table
/// (row-major, row = south, col = east; out-of-tile neighbours already clamped by the caller) and
/// the grid's NW corner in WoW (x = north, y = west).
pub(super) fn monkey_voxel_apply_real(
    ext: &mut MonkeyVoxelExt,
    layer_array: Handle<Image>,
    alpha_array: Handle<Image>,
    shadow_array: Handle<Image>,
    grid: &[MonkeyVoxelChunkShade; MONKEY_VOXEL_GRID * MONKEY_VOXEL_GRID],
    grid_nw: [f32; 2],
) {
    ext.layer_array = layer_array;
    ext.alpha_array = alpha_array;
    ext.shadow_array = shadow_array;
    for (i, c) in grid.iter().enumerate() {
        ext.params.layers[i] = Vec4::new(
            c.layers[0] as f32,
            c.layers[1] as f32,
            c.layers[2] as f32,
            c.layers[3] as f32,
        );
        ext.params.aux[i] = Vec4::new(c.alpha as f32, c.shadow, 0.0, 0.0);
    }
    ext.params.grid = Vec4::new(
        grid_nw[0],
        grid_nw[1],
        benilla_formats::CHUNK_SIZE,
        benilla_formats::TERRAIN_LAYER_TILES,
    );
    ext.params.misc.y = 1.0;
}

fn monkey_voxel_hash(x: u32, y: u32, seed: u32) -> f32 {
    let mut h = seed ^ x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    h as f32 / u32::MAX as f32
}

/// Periodic value noise: the lattice wraps at the edge, so linear texture filtering can repeat it.
fn monkey_voxel_noise(x: u32, y: u32, period: u32, seed: u32) -> f32 {
    let step = 256 / period;
    let ix = x / step;
    let iy = y / step;
    let fx = (x % step) as f32 / step as f32;
    let fy = (y % step) as f32 / step as f32;
    let sx = fx * fx * (3.0 - 2.0 * fx);
    let sy = fy * fy * (3.0 - 2.0 * fy);
    let a = monkey_voxel_hash(ix, iy, seed);
    let b = monkey_voxel_hash((ix + 1) % period, iy, seed);
    let c = monkey_voxel_hash(ix, (iy + 1) % period, seed);
    let d = monkey_voxel_hash((ix + 1) % period, (iy + 1) % period, seed);
    (a + (b - a) * sx) * (1.0 - sy) + (c + (d - c) * sx) * sy
}

/// One 256² RGBA8 procedural ground tile (grass/rock/dirt by `kind`), the pixels of the old tiles.
fn monkey_voxel_tile_rgba(base: [f32; 3], seed: u32, kind: u8) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(256 * 256 * 4);
    for y in 0..256 {
        for x in 0..256 {
            let broad = monkey_voxel_noise(x, y, 16, seed) - 0.5;
            let fine = monkey_voxel_noise(x, y, 64, seed ^ 0xC0FF_EE11) - 0.5;
            let grain = monkey_voxel_hash(x, y, seed ^ 0xDEAD_BEEF);
            let mut variation = 1.0 + broad * 0.22 + fine * 0.12;
            match kind {
                0 if grain < 0.025 => variation *= 0.63, // scattered dark grass specks
                1 if broad < -0.29 && fine < -0.12 => variation *= 0.65, // rock fissures
                2 => variation += (grain - 0.5) * 0.12,  // fine dirt grain
                _ => {}
            }
            for channel in base {
                rgba.push((channel * variation * 255.0).clamp(0.0, 255.0) as u8);
            }
            rgba.push(255);
        }
    }
    rgba
}

/// The procedural fallback `layer_array`: a 4-layer, single-mip `texture_2d_array` (LayerMajor),
/// [0]/[1] grass, [2] rock, [3] dirt, with a repeat/linear sampler so triplanar tiling can repeat.
fn monkey_voxel_proc_layers() -> Image {
    let grass = monkey_voxel_tile_rgba([0.29, 0.40, 0.17], 0xA1B2_C3D4, 0);
    let rock = monkey_voxel_tile_rgba([0.40, 0.40, 0.43], 0x3141_5926, 1);
    let dirt = monkey_voxel_tile_rgba([0.44, 0.35, 0.22], 0x2718_2818, 2);
    let mut data = Vec::with_capacity(4 * grass.len());
    data.extend_from_slice(&grass); // layer 0 (fallback)
    data.extend_from_slice(&grass); // layer 1 (grass)
    data.extend_from_slice(&rock); // layer 2 (rock)
    data.extend_from_slice(&dirt); // layer 3 (dirt)

    let mut image = Image::new(
        Extent3d {
            width: 256,
            height: 256,
            depth_or_array_layers: 4,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data_order = TextureDataOrder::LayerMajor;
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::D2Array),
        ..default()
    });
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        ..default()
    });
    image
}

/// A 1-layer 64² zero `alpha_array` (`texture_2d_array`), bound so the fallback group is valid; the
/// fallback path never reads it (it slope-blends), and the real path swaps in the tile's own.
fn monkey_voxel_dummy_alpha() -> Image {
    let mut image = Image::new(
        Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        vec![0u8; 64 * 64 * 4],
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data_order = TextureDataOrder::LayerMajor;
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::D2Array),
        ..default()
    });
    image
}

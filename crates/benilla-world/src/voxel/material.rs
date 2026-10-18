//! Surface material for the optional MONKEY voxel build site.
//!
//! It binds the REAL ADT tileset of the ground the volume sits on — the tile's `layer_array`
//! (a `texture_2d_array` of the ground BLPs) and per-chunk `alpha_array` — and triplanar-samples
//! them so the sculpted voxel surface wears the surrounding Blizzlike ground. Until the covering
//! tile is resident it falls back to a PROCEDURAL grass/rock/dirt slope-blend (built once at
//! startup into the same `texture_2d_array`), so the volume always renders something.
//!
//! `monkey_voxel::material::monkey_voxel_texture_from_terrain` (in `voxel.rs`) swaps the handles +
//! the per-chunk uniforms over once the real tile loads. Kept UNLIT with its own baked sun/ambient,
//! because benilla's custom world light blacks out lit materials.

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::pbr::{ExtendedMaterial, MaterialExtension};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, Extent3d, TextureDataOrder, TextureDimension, TextureFormat,
    TextureViewDescriptor, TextureViewDimension,
};
use bevy::shader::ShaderRef;

pub type MonkeyVoxelMaterial = ExtendedMaterial<StandardMaterial, MonkeyVoxelExt>;

/// The extension bindings. `layer_array` (100) is the tile's ground `texture_2d_array` — either the
/// real ADT one or the procedural fallback built at startup — and `alpha_array` (102) the per-chunk
/// blend maps. The three `Vec4`s pack into ONE uniform buffer at binding 104, in the field order of
/// the WGSL `MonkeyVoxelParams` (the [`WowModelExt`](benilla_assets) multi-field pattern).
#[derive(Asset, AsBindGroup, Clone, TypePath, Default)]
pub struct MonkeyVoxelExt {
    #[texture(100, dimension = "2d_array")]
    #[sampler(101)]
    layer_array: Handle<Image>,
    #[texture(102, dimension = "2d_array")]
    #[sampler(103)]
    alpha_array: Handle<Image>,

    /// The covering chunk's 4 layer-array indices (real path), as floats.
    #[uniform(104)]
    layers: Vec4,
    /// `x` = alpha-array layer index; `y` = covering chunk NW-corner WoW X (north); `z` = its WoW Y
    /// (west); `w` = chunk size (yd). The shader maps a fragment's world XZ into the chunk's 0..1
    /// alpha UV from these.
    #[uniform(104)]
    chunk: Vec4,
    /// `x` = texture tiles per world yard; `y` = `has_real` (≥0.5 → sample the real tileset, else the
    /// procedural slope-blend); `zw` unused.
    #[uniform(104)]
    misc: Vec4,
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
    #[allow(dead_code)]
    pub(super) Handle<Image>,
    /// Procedural fallback `alpha_array` (unused by the fallback path, bound so the group is valid).
    #[allow(dead_code)]
    pub(super) Handle<Image>,
);

/// Repeats per world yard that matches the real ground: the terrain samples its BLPs `uv*8` over a
/// 33.33-yd chunk, i.e. `8 / CHUNK_SIZE` tiles per yard.
pub(super) const MONKEY_VOXEL_TILES_PER_YARD: f32 =
    benilla_formats::TERRAIN_LAYER_TILES / benilla_formats::CHUNK_SIZE;

/// The procedural fallback's own tiling (one repeat per 2 yd), preserving the pre-tileset look.
const MONKEY_VOXEL_PROC_TILES_PER_YARD: f32 = 0.5;

pub fn monkey_voxel_create_material(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<MonkeyVoxelMaterial>>,
) {
    // A 4-layer procedural array: [0] fallback/grass, [1] grass, [2] rock, [3] dirt (the shader's
    // fallback path reads 1/2/3 and slope-blends them, exactly as the old three-texture material).
    let layer_array = images.add(monkey_voxel_proc_layers());
    let alpha_array = images.add(monkey_voxel_dummy_alpha());
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
            layers: Vec4::new(1.0, 2.0, 3.0, 0.0),
            chunk: Vec4::new(0.0, 0.0, 0.0, benilla_formats::CHUNK_SIZE),
            // Start procedural (no real tile bound yet).
            misc: Vec4::new(MONKEY_VOXEL_PROC_TILES_PER_YARD, 0.0, 0.0, 0.0),
        },
    });
    commands.insert_resource(MonkeyVoxelMaterialHandle(handle, layer_array, alpha_array));
}

/// Update the shared material to wear the real tileset of the covering chunk. `layers`/`alpha` are
/// the [`benilla_assets::ChunkShading`] indices, `chunk_nw` the chunk's NW-corner WoW (x = north,
/// y = west), and the handles the tile's own arrays.
#[allow(clippy::too_many_arguments)]
pub(super) fn monkey_voxel_apply_real(
    ext: &mut MonkeyVoxelExt,
    layer_array: Handle<Image>,
    alpha_array: Handle<Image>,
    layers: [u32; 4],
    alpha: u32,
    chunk_nw: [f32; 2],
) {
    ext.layer_array = layer_array;
    ext.alpha_array = alpha_array;
    ext.layers = Vec4::new(
        layers[0] as f32,
        layers[1] as f32,
        layers[2] as f32,
        layers[3] as f32,
    );
    ext.chunk = Vec4::new(
        alpha as f32,
        chunk_nw[0],
        chunk_nw[1],
        benilla_formats::CHUNK_SIZE,
    );
    ext.misc = Vec4::new(MONKEY_VOXEL_TILES_PER_YARD, 1.0, 0.0, 0.0);
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

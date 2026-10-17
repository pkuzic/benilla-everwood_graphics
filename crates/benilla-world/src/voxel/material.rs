//! Procedural, world-projected surface for the optional MONKEY voxel build site.

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::pbr::{ExtendedMaterial, MaterialExtension};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, Extent3d, TextureDimension, TextureFormat};
use bevy::shader::ShaderRef;

pub type MonkeyVoxelMaterial = ExtendedMaterial<StandardMaterial, MonkeyVoxelExt>;

#[derive(Asset, AsBindGroup, Clone, TypePath, Default)]
pub struct MonkeyVoxelExt {
    #[texture(100)]
    #[sampler(101)]
    grass: Handle<Image>,
    #[texture(102)]
    #[sampler(103)]
    rock: Handle<Image>,
    #[texture(104)]
    #[sampler(105)]
    dirt: Handle<Image>,
}

impl MaterialExtension for MonkeyVoxelExt {
    fn fragment_shader() -> ShaderRef {
        "embedded://benilla_world/shaders/monkey_voxel.wgsl".into()
    }
}

#[derive(Resource)]
pub struct MonkeyVoxelMaterialHandle(pub Handle<MonkeyVoxelMaterial>);

pub fn monkey_voxel_create_material(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<MonkeyVoxelMaterial>>,
) {
    let grass = images.add(monkey_voxel_tile([0.29, 0.40, 0.17], 0xA1B2_C3D4, 0));
    let rock = images.add(monkey_voxel_tile([0.40, 0.40, 0.43], 0x3141_5926, 1));
    let dirt = images.add(monkey_voxel_tile([0.44, 0.35, 0.22], 0x2718_2818, 2));
    let handle = materials.add(MonkeyVoxelMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            unlit: true,
            cull_mode: None,
            ..default()
        },
        extension: MonkeyVoxelExt { grass, rock, dirt },
    });
    commands.insert_resource(MonkeyVoxelMaterialHandle(handle));
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

fn monkey_voxel_tile(base: [f32; 3], seed: u32, kind: u8) -> Image {
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
    let mut image = Image::new(
        Extent3d {
            width: 256,
            height: 256,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        ..default()
    });
    image
}

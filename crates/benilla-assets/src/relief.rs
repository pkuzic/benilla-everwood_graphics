//! MONKEY (ground relief): the per-tile `relief_array` — one offline-baked relief map per ground
//! layer, indexed exactly like the tile's `layer_array`, that `terrain.wgsl` reads for per-pixel
//! normal mapping, parallax occlusion mapping and parallax self-shadowing (`groundRelief`).
//!
//! The maps are LOOSE PNGs, not MPQ content: for the layer `Tileset\Elwynn\ElwynnCobblestoneBase.blp`
//! the map is `<relief dir>/tileset/elwynn/elwynncobblestonebase.png` (the normalised key
//! lowercased, `\` → `/`, `.blp` → `.png`; an `_s.blp` key shares its base's map). `<relief dir>`
//! is `$MONKEY_RELIEF_DIR`, else `MData\Relief` beside the executable, else `MData\Relief` beside
//! the `Data` folder — the same two install bases (and order) `monkey-anims`' `Library` resolves
//! `MData\Anims` from.
//!
//! The PNG channels are LINEAR data (never colour): R/G the tangent-space normal X (+u) / Y (+v,
//! image-down) as `n*0.5+0.5`, B cavity (1 open), A height (1 top). Mips are a plain box filter,
//! which is right for this linear data; the shader renormalises the normal.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use bevy::image::{Image, ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::prelude::*;
use bevy::render::render_resource::{TextureDataOrder, TextureFormat};

use crate::terrain::{array_image, mip_count_for};

/// MONKEY (ground relief): the flat texel — normal (0, 0, 1), fully open, top height. The shader
/// decodes the normal as `(byte - 128) / 127`, so 128 is EXACTLY zero tilt.
pub const RELIEF_FLAT: [u8; 4] = [128, 128, 255, 255];

/// MONKEY (ground relief): edge of the 1-layer dummy a tile with no relief map at all binds.
const DUMMY_SIZE: u32 = 4;

/// MONKEY (ground relief): the relief directory, resolved once per process (`None` = no relief
/// anywhere: every tile binds the flat dummy and the shader takes today's path).
pub fn monkey_relief_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let found = relief_dir_candidates().into_iter().find(|p| p.is_dir());
        match &found {
            Some(p) => info!("MONKEY (ground relief): relief maps from {}", p.display()),
            None => debug!("MONKEY (ground relief): no relief directory (ground stays flat)"),
        }
        found
    })
    .as_deref()
}

/// MONKEY (ground relief): every place [`monkey_relief_dir`] looks, in order.
fn relief_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("MONKEY_RELIEF_DIR").filter(|p| !p.is_empty()) {
        out.push(PathBuf::from(p));
    }
    // The install bases `monkey_anims::Library::client_bases` uses: the executable's folder, then
    // the folder holding `Data`.
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        bases.push(d);
    }
    if let Some(d) = benilla_formats::wow_data().and_then(|d| d.parent().map(Path::to_path_buf)) {
        if !bases.contains(&d) {
            bases.push(d);
        }
    }
    out.extend(bases.into_iter().map(|b| b.join("MData").join("Relief")));
    out
}

/// MONKEY (ground relief): the relative PNG path for a normalised layer key (see the module doc).
pub fn relief_rel_path(key: &str) -> String {
    let mut k = key.replace('\\', "/").to_ascii_lowercase();
    if let Some(stem) = k.strip_suffix("_s.blp") {
        k = format!("{stem}.blp");
    }
    blp_key_to_png(&k)
}

/// MONKEY (model relief): the shared tail of both relief path rules: a lowercased `/` key's
/// `.blp` swapped for `.png` (appended when the key has no `.blp`).
pub(crate) fn blp_key_to_png(k: &str) -> String {
    match k.strip_suffix(".blp") {
        Some(stem) => format!("{stem}.png"),
        None => format!("{k}.png"),
    }
}

/// MONKEY (ground relief): one layer's relief chain (`size`², a full mip chain, RGBA8, level 0
/// first; `size` is the tile's layer edge so it lines up with the colour array), or `None` when the dir has no (readable) map for `key`.
pub fn read_relief_layer(dir: &Path, key: &str, size: u32) -> Option<Vec<u8>> {
    let path = dir.join(relief_rel_path(key));
    let bytes = std::fs::read(&path).ok()?;
    let decoded = match image::load_from_memory_with_format(&bytes, image::ImageFormat::Png) {
        Ok(img) => img.into_rgba8(),
        Err(e) => {
            warn!("MONKEY (ground relief): {} unreadable: {e}", path.display());
            return None;
        }
    };
    Some(relief_chain(decoded, size))
}

/// MONKEY (ground relief): resize (triangle filter) to `size`² and box-filter the mips; an exact
/// size passes through (the 512 maps on disk with the upscale pack).
fn relief_chain(img: image::RgbaImage, size: u32) -> Vec<u8> {
    let mip_count = mip_count_for(size);
    let top = if img.width() == size && img.height() == size {
        img
    } else {
        image::imageops::resize(
            &img,
            size,
            size,
            image::imageops::FilterType::Triangle,
        )
    };
    let mut chain = Vec::with_capacity(crate::terrain::mip_chain_byte_size(size, mip_count));
    let mut level = top.into_raw();
    let mut w = size;
    chain.extend_from_slice(&level);
    for _ in 1..mip_count {
        let nw = (w / 2).max(1);
        level = box_down(&level, w, nw);
        w = nw;
        chain.extend_from_slice(&level);
    }
    chain
}

/// MONKEY (ground relief): a square RGBA8 level `w`² halved to `nw`² by a 2×2 average (rounded).
fn box_down(src: &[u8], w: u32, nw: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((nw * nw * 4) as usize);
    let w = w as usize;
    for y in 0..nw as usize {
        for x in 0..nw as usize {
            let (x0, y0) = ((2 * x).min(w - 1), (2 * y).min(w - 1));
            let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(w - 1));
            for c in 0..4 {
                let at = |xx: usize, yy: usize| u32::from(src[(yy * w + xx) * 4 + c]);
                let sum = at(x0, y0) + at(x1, y0) + at(x0, y1) + at(x1, y1);
                out.push(((sum + 2) / 4) as u8);
            }
        }
    }
    out
}

/// MONKEY (ground relief): build a tile's relief array from its `layer_names` (index 0 = the
/// fallback slot, flat). Returns the image and `relief_layers`: the array's layer count, or 0 when
/// no layer has a map (the image is then a 1-layer 4×4 flat dummy and the shader never samples it).
pub fn monkey_relief_array(names: &[String], dir: Option<&Path>, size: u32) -> (Image, u32) {
    let maps: Vec<Option<Vec<u8>>> = match dir {
        Some(dir) => names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                if i == 0 || n.is_empty() {
                    None
                } else {
                    read_relief_layer(dir, n, size)
                }
            })
            .collect(),
        None => Vec::new(),
    };
    relief_array_from(names.len().max(1) as u32, maps, size)
}

/// MONKEY (ground relief): [`monkey_relief_array`] over already-read chains (testable without IO).
fn relief_array_from(layer_count: u32, maps: Vec<Option<Vec<u8>>>, size: u32) -> (Image, u32) {
    if !maps.iter().any(Option::is_some) {
        let data = RELIEF_FLAT.repeat((DUMMY_SIZE * DUMMY_SIZE) as usize);
        let mut image = array_image(DUMMY_SIZE, 1, TextureFormat::Rgba8Unorm);
        image.data = Some(data);
        set_sampler(&mut image);
        return (image, 0);
    }
    let mip_count = mip_count_for(size);
    let per_layer = crate::terrain::mip_chain_byte_size(size, mip_count);
    let flat = RELIEF_FLAT.repeat(per_layer / 4);
    let mut data = Vec::with_capacity(per_layer * layer_count as usize);
    for i in 0..layer_count as usize {
        match maps.get(i).and_then(Option::as_ref) {
            Some(chain) if chain.len() == per_layer => data.extend_from_slice(chain),
            _ => data.extend_from_slice(&flat),
        }
    }
    let mut image = array_image(size, layer_count, TextureFormat::Rgba8Unorm);
    image.data = Some(data);
    image.texture_descriptor.mip_level_count = mip_count;
    image.data_order = TextureDataOrder::LayerMajor;
    set_sampler(&mut image);
    (image, layer_count)
}

/// MONKEY (ground relief): repeat + trilinear. Informational only — `terrain.wgsl` samples the
/// relief array through the layer array's sampler (binding 105), one sampler for every array.
fn set_sampler(image: &mut Image) {
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        ..default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::LAYER_TEX_SIZE;

    #[test]
    fn the_relief_path_is_the_lowercased_base_key_as_png() {
        assert_eq!(
            relief_rel_path(r"Tileset\Elwynn\ElwynnCobblestoneBase.blp"),
            "tileset/elwynn/elwynncobblestonebase.png"
        );
        assert_eq!(
            relief_rel_path(r"tileset\elwynn\elwynncobblestonebase_s.blp"),
            "tileset/elwynn/elwynncobblestonebase.png"
        );
    }

    #[test]
    fn a_tile_without_maps_binds_the_small_flat_dummy() {
        let (img, layers) = relief_array_from(5, vec![None, None, None, None, None], 256);
        assert_eq!(layers, 0);
        assert_eq!(img.texture_descriptor.size.width, DUMMY_SIZE);
        assert_eq!(img.texture_descriptor.size.depth_or_array_layers, 1);
        let data = img.data.as_ref().unwrap();
        assert!(data.chunks(4).all(|t| t == RELIEF_FLAT));
    }

    #[test]
    fn a_tile_with_one_map_keeps_every_index_aligned_and_flat_elsewhere() {
        let chain = relief_chain(image::RgbaImage::from_pixel(
            64,
            64,
            image::Rgba([10, 20, 30, 40]),
        ), LAYER_TEX_SIZE);
        let per_layer = crate::terrain::mip_chain_byte_size(LAYER_TEX_SIZE, mip_count_for(LAYER_TEX_SIZE));
        assert_eq!(
            chain.len(),
            per_layer,
            "resized to the layer size with a full chain"
        );
        let (img, layers) = relief_array_from(3, vec![None, None, Some(chain)], LAYER_TEX_SIZE);
        assert_eq!(layers, 3);
        assert_eq!(img.texture_descriptor.size.depth_or_array_layers, 3);
        assert_eq!(img.texture_descriptor.mip_level_count, mip_count_for(LAYER_TEX_SIZE));
        let data = img.data.as_ref().unwrap();
        assert_eq!(data.len(), per_layer * 3);
        assert_eq!(&data[..4], &RELIEF_FLAT, "layer 0 is the flat fallback");
        assert_eq!(
            &data[per_layer..per_layer + 4],
            &RELIEF_FLAT,
            "a layer with no map is flat"
        );
        // A solid map stays exactly solid through the triangle resize and every box mip.
        assert!(data[2 * per_layer..]
            .chunks(4)
            .all(|t| t == [10, 20, 30, 40]));
    }

    #[test]
    fn the_box_mip_averages_linear_data() {
        // 2×2 → 1×1: (0 + 255 + 255 + 0) / 4 rounds to 128 on every channel.
        let src = [
            0u8, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 0, 0,
        ];
        assert_eq!(box_down(&src, 2, 1), vec![128, 128, 128, 128]);
    }

    // MONKEY (texture upscale): the relief chain follows the tile's layer size.
    #[test]
    fn relief_at_512_passes_a_512_map_through_and_resizes_a_256_one() {
        let want = crate::terrain::mip_chain_byte_size(512, mip_count_for(512));
        let native = relief_chain(image::RgbaImage::from_pixel(512, 512, image::Rgba([7, 8, 9, 10])), 512);
        assert_eq!(native.len(), want);
        assert_eq!(&native[..4], &[7, 8, 9, 10], "exact size is untouched");
        let old = relief_chain(image::RgbaImage::from_pixel(256, 256, image::Rgba([7, 8, 9, 10])), 512);
        assert_eq!(old.len(), want);
        let (img, n) = relief_array_from(2, vec![None, Some(native)], 512);
        assert_eq!(n, 2);
        assert_eq!(img.texture_descriptor.size.width, 512);
        assert_eq!(img.texture_descriptor.mip_level_count, 10);
    }
}

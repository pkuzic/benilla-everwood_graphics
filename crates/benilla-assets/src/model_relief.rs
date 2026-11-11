//! MONKEY (model relief): offline-baked relief maps for WORLD ART on the model lane — WMO
//! buildings and M2 doodads (`WowModelMaterial`, `wow_model.wgsl`), behind `modelRelief`.
//!
//! The data contract is the terrain one (`crate::relief`): for a texture with MPQ path
//! `Dungeons\Textures\Walls\StormwindBrick.blp` the map is
//! `<relief dir>/dungeons/textures/walls/stormwindbrick.png` (lowercased, `\` → `/`, `.blp` →
//! `.png`), `<relief dir>` from [`crate::monkey_relief_dir`]. RGBA8, LINEAR data: RG the tangent
//! normal X (+u) / Y (+v, image-down) as `n*0.5+0.5`, B cavity (1 open), A height (1 top), at the
//! source texture's NATIVE size (maybe non-square). Unlike terrain nothing is resized: the map is
//! uploaded as-is with a full box-filtered mip chain down to 1×1.
//!
//! **How a material finds its map.** Model textures are loaded three ways — the M2 / WMO loaders'
//! `ctx.load` of a `mpq://…blp` URL (decoded by [`crate::BlpImageLoader`], `BlpVariant::WorldArt`,
//! the main path), and [`crate::WorldAssets::texture`]'s synchronous upload (clutter, skybox). The
//! material builders live in other crates and only see the texture `Handle<Image>`, so the link is
//! a PROCESS-GLOBAL index here, `texture AssetId<Image> → relief Handle<Image>`
//! ([`relief_for`]):
//! - the loader decodes the PNG on the IO pool ([`model_relief_image`]) and parks the finished
//!   `Image` under the texture's asset-path string ([`park_model_relief`]), because a loader cannot
//!   know the id its asset will get;
//! - a main-thread system ([`monkey_track_model_relief`]) sees the texture's `AssetEvent::Added`,
//!   looks its path up, uploads the parked relief and indexes it ([`register_model_relief`]), and
//!   drops the entry again on `Removed`;
//! - [`WorldAssets::texture`](crate::WorldAssets::texture) knows its id at once and registers
//!   directly.
//!
//! Material builders call [`model_relief_lane`]; a material built before its texture's relief
//! was indexed is patched later from [`take_fresh_model_relief`] (`benilla-world`'s
//! `monkey_sync_model_relief`). Without a relief directory nothing here does any IO.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use bevy::asset::RenderAssetUsages;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::LockRecover;

/// MONKEY (model relief): the relative PNG path for a model texture's archive path, per the data
/// contract (no `_s` folding: that is a terrain-layer convention), with any `texture_url`
/// sampler marker (`…@cc.blp`) taken off first.
pub fn model_relief_rel_path(path: &str) -> String {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let unmarked = crate::strip_sampler_marker(&lower).unwrap_or(lower);
    crate::relief::blp_key_to_png(&unmarked)
}

/// MONKEY (model relief): rel paths already found to have no map, so each path probes the disk
/// once per process however often its texture reloads.
fn misses() -> &'static Mutex<HashSet<String>> {
    static M: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// MONKEY (model relief): the relief `Image` for the texture at archive / asset path `path`
/// (`None` without a relief dir, without a map, or with an unreadable one). Decoded and mipped
/// here, so the loader's IO thread pays for it, not the main thread.
pub fn model_relief_image(path: &str) -> Option<Image> {
    let dir = crate::monkey_relief_dir()?;
    let rel = model_relief_rel_path(path);
    if misses().lock_recover().contains(&rel) {
        return None;
    }
    let file = dir.join(&rel);
    let Ok(bytes) = std::fs::read(&file) else {
        misses().lock_recover().insert(rel);
        return None;
    };
    match image::load_from_memory_with_format(&bytes, image::ImageFormat::Png) {
        Ok(img) => Some(relief_image(img.into_rgba8())),
        Err(e) => {
            warn!("MONKEY (model relief): {} unreadable: {e}", file.display());
            misses().lock_recover().insert(rel);
            None
        }
    }
}

/// MONKEY (model relief): mip count of a full chain down to 1×1 for a `w`×`h` level 0.
fn full_mip_count(w: u32, h: u32) -> u32 {
    32 - w.max(h).max(1).leading_zeros()
}

/// MONKEY (model relief): RGBA8 level 0 plus its full box-filtered chain, level 0 first.
fn relief_chain(img: image::RgbaImage) -> (u32, u32, u32, Vec<u8>) {
    let (w0, h0) = (img.width().max(1), img.height().max(1));
    let levels = full_mip_count(w0, h0);
    let mut level = img.into_raw();
    let mut chain = level.clone();
    let (mut w, mut h) = (w0, h0);
    for _ in 1..levels {
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        level = box_down(&level, w, h, nw, nh);
        chain.extend_from_slice(&level);
        (w, h) = (nw, nh);
    }
    (w0, h0, levels, chain)
}

/// MONKEY (model relief): a `w`×`h` RGBA8 level halved to `nw`×`nh` by a 2×2 average (rounded);
/// a dimension already at 1 averages its single row / column with itself (non-square aware).
fn box_down(src: &[u8], w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut out = Vec::with_capacity(nw as usize * nh as usize * 4);
    for y in 0..nh as usize {
        let y0 = (2 * y).min(h - 1);
        let y1 = (y0 + 1).min(h - 1);
        for x in 0..nw as usize {
            let x0 = (2 * x).min(w - 1);
            let x1 = (x0 + 1).min(w - 1);
            for c in 0..4 {
                let at = |xx: usize, yy: usize| u32::from(src[(yy * w + xx) * 4 + c]);
                let sum = at(x0, y0) + at(x1, y0) + at(x0, y1) + at(x1, y1);
                out.push(((sum + 2) / 4) as u8);
            }
        }
    }
    out
}

/// MONKEY (model relief): the GPU image: `Rgba8Unorm` (linear data, never sRGB), full chain.
/// No sampler of its own matters: `wow_model.wgsl` samples it through the base colour texture's
/// sampler, so wrap and filtering follow the batch's texture.
fn relief_image(img: image::RgbaImage) -> Image {
    let (w, h, levels, data) = relief_chain(img);
    let mut image = Image::new_uninit(
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data = Some(data);
    image.texture_descriptor.mip_level_count = levels;
    image
}

/// MONKEY (model relief): reliefs decoded by the loader, waiting for their texture's id (keyed
/// by the texture's asset-path string, `AssetPath`'s `Display`).
fn parked() -> &'static Mutex<HashMap<String, Image>> {
    static P: OnceLock<Mutex<HashMap<String, Image>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// MONKEY (model relief): park a loader-decoded relief under its texture's asset path.
pub fn park_model_relief(asset_path: String, relief: Image) {
    parked().lock_recover().insert(asset_path, relief);
}

/// MONKEY (model relief): the index (texture id → relief handle) and the ids indexed since the
/// last [`take_fresh_model_relief`].
#[derive(Default)]
struct Index {
    map: HashMap<AssetId<Image>, Handle<Image>>,
    fresh: Vec<AssetId<Image>>,
}

fn index() -> &'static Mutex<Index> {
    static I: OnceLock<Mutex<Index>> = OnceLock::new();
    I.get_or_init(Default::default)
}

/// MONKEY (model relief): bumped on every [`register_model_relief`]. Unlike
/// [`take_fresh_model_relief`] (drained by the material sync) any number of readers can poll it:
/// `benilla-world`'s retained static pass re-resolves its items' relief layers when it moves.
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// MONKEY (model relief): index `relief` as the map of the texture `texture`.
pub fn register_model_relief(texture: AssetId<Image>, relief: Handle<Image>) {
    let mut ix = index().lock_recover();
    ix.map.insert(texture, relief);
    ix.fresh.push(texture);
    // MONKEY (model relief): under the lock, so a reader that sees the new count finds the map.
    GENERATION.fetch_add(1, Ordering::Release);
}

/// MONKEY (model relief): the index's registration count; a change means some texture may have
/// gained a relief map since the last read.
pub fn model_relief_generation() -> u32 {
    GENERATION.load(Ordering::Acquire)
}

/// MONKEY (model relief): the relief map of the texture `texture`, if it has one (indexed yet).
pub fn relief_for(texture: AssetId<Image>) -> Option<Handle<Image>> {
    index().lock_recover().map.get(&texture).cloned()
}

/// MONKEY (model relief): the texture ids indexed since the last call (drained).
pub fn take_fresh_model_relief() -> Vec<AssetId<Image>> {
    std::mem::take(&mut index().lock_recover().fresh)
}

/// MONKEY (model relief): the live `modelRelief` / `modelReliefDepth` dials, mirrored here (as
/// bits) so a material built in any crate starts with them ([`model_relief_lane`]).
static DIAL_MODE: AtomicU32 = AtomicU32::new(0);
static DIAL_DEPTH: AtomicU32 = AtomicU32::new(0);

/// MONKEY (model relief): publish the dials (`benilla-world`'s sync system, on change).
pub fn set_model_relief_dials(mode: u8, depth: f32) {
    DIAL_MODE.store(u32::from(mode), Ordering::Relaxed);
    DIAL_DEPTH.store(depth.to_bits(), Ordering::Relaxed);
}

/// MONKEY (model gloss): the live `modelGloss` * `modelGlossStrength` product (0 = off), as bits.
static DIAL_GLOSS: AtomicU32 = AtomicU32::new(0);

/// MONKEY (model gloss): publish the gloss dial: `strength` is 0 with `modelGloss 0`, else the
/// `modelGlossStrength` value (0..2).
pub fn set_model_gloss_dial(strength: f32) {
    DIAL_GLOSS.store(strength.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
}

/// MONKEY (model gloss): the gloss strength as last published (0 until then: off).
pub fn model_gloss_dial() -> f32 {
    f32::from_bits(DIAL_GLOSS.load(Ordering::Relaxed))
}

/// MONKEY (model relief): the dials as last published (`0, 0.0` until then: off).
pub fn model_relief_dials() -> (u8, f32) {
    (
        DIAL_MODE.load(Ordering::Relaxed) as u8,
        f32::from_bits(DIAL_DEPTH.load(Ordering::Relaxed)),
    )
}

/// MONKEY (model relief): the `WowModelExt::relief` lane for a material with this map:
/// `x` = 1, `y` = mode, `z` = depth, `w` = the model gloss strength (MONKEY (model gloss): 0 = off;
/// `Vec4::ZERO` without a map — such a material never reads
/// the dials, so a dial change does not touch it).
pub fn model_relief_params(has_map: bool) -> Vec4 {
    if !has_map {
        return Vec4::ZERO;
    }
    let (mode, depth) = model_relief_dials();
    Vec4::new(1.0, f32::from(mode), depth, model_gloss_dial())
}

/// MONKEY (model relief): what a `WowModelExt` builder fills for base texture `texture`: the
/// relief handle (`None` unless the texture has an indexed map) and the `relief` uniform lane.
pub fn model_relief_lane(texture: Option<&Handle<Image>>) -> (Option<Handle<Image>>, Vec4) {
    let relief = texture.and_then(|t| relief_for(t.id()));
    let params = model_relief_params(relief.is_some());
    (relief, params)
}

/// MONKEY (model relief): upload parked reliefs as their textures arrive, and forget a texture's
/// relief when the texture is removed. Idle (one emptiness check) without a relief directory or
/// with nothing parked.
pub fn monkey_track_model_relief(
    mut events: MessageReader<AssetEvent<Image>>,
    server: Option<Res<AssetServer>>,
    images: Option<ResMut<Assets<Image>>>,
) {
    if crate::monkey_relief_dir().is_none() {
        events.clear();
        return;
    }
    let (Some(server), Some(mut images)) = (server, images) else {
        events.clear();
        return;
    };
    let mut removed: Vec<AssetId<Image>> = Vec::new();
    let mut added: Vec<AssetId<Image>> = Vec::new();
    for ev in events.read() {
        match ev {
            AssetEvent::Added { id } => added.push(*id),
            AssetEvent::Removed { id } => removed.push(*id),
            _ => {}
        }
    }
    if !removed.is_empty() {
        let mut ix = index().lock_recover();
        for id in &removed {
            ix.map.remove(id);
        }
    }
    if added.is_empty() || parked().lock_recover().is_empty() {
        return;
    }
    for id in added {
        let Some(path) = server.get_path(id) else {
            continue;
        };
        let Some(relief) = parked().lock_recover().remove(&path.to_string()) else {
            continue;
        };
        let handle = images.add(relief);
        register_model_relief(id, handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_relief_path_is_the_lowercased_key_as_png_without_marker_or_s_folding() {
        assert_eq!(
            model_relief_rel_path(r"Dungeons\Textures\Walls\StormwindBrick.blp"),
            "dungeons/textures/walls/stormwindbrick.png"
        );
        assert_eq!(
            model_relief_rel_path("world/generic/crate@cc.blp"),
            "world/generic/crate.png"
        );
        assert_eq!(
            model_relief_rel_path(r"World\Foo\Bark_S.blp"),
            "world/foo/bark_s.png"
        );
    }

    #[test]
    fn a_non_square_map_keeps_its_size_and_mips_to_one_texel() {
        let img = image::RgbaImage::from_pixel(256, 128, image::Rgba([10, 20, 30, 40]));
        let (w, h, levels, data) = relief_chain(img);
        assert_eq!((w, h, levels), (256, 128, 9));
        // 256x128, 128x64, ..., 2x1, 1x1.
        let mut expect = 0usize;
        let (mut lw, mut lh) = (256usize, 128usize);
        for _ in 0..levels {
            expect += lw * lh * 4;
            (lw, lh) = ((lw / 2).max(1), (lh / 2).max(1));
        }
        assert_eq!(data.len(), expect);
        assert!(
            data.chunks(4).all(|t| t == [10, 20, 30, 40]),
            "a solid map stays solid"
        );
        let image = relief_image(image::RgbaImage::from_pixel(
            3,
            5,
            image::Rgba([0, 0, 0, 0]),
        ));
        assert_eq!(image.texture_descriptor.mip_level_count, 3);
        assert_eq!(image.texture_descriptor.format, TextureFormat::Rgba8Unorm);
    }

    #[test]
    fn the_box_mip_averages_a_one_texel_wide_column() {
        // 1×2 → 1×1: (0 + 0 + 255 + 255) / 4 rounds to 128.
        let src = [0u8, 0, 0, 0, 255, 255, 255, 255];
        assert_eq!(box_down(&src, 1, 2, 1, 1), vec![128, 128, 128, 128]);
    }

    #[test]
    fn a_material_without_a_map_has_a_zero_lane() {
        assert_eq!(model_relief_params(false), Vec4::ZERO);
        assert_eq!(model_relief_params(true).x, 1.0);
    }

    // ── MONKEY (model gloss): Rust mirrors of `wow_model.wgsl`'s gloss formulas ─────────────────
    fn ss(e0: f32, e1: f32, x: f32) -> f32 {
        let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }

    fn mirror_gloss_mask(c: [f32; 3], cavity: f32) -> f32 {
        let mx = c[0].max(c[1]).max(c[2]);
        let mn = c[0].min(c[1]).min(c[2]);
        let sat = (mx - mn) / mx.max(1e-3);
        let neutral = (1.0 - ss(0.15, 0.35, sat)) * ss(0.18, 0.45, mx);
        let warm = ss(0.0, 0.08, c[0] - c[1]) * ss(0.0, 0.08, c[1] - c[2]);
        let gold = 0.8 * warm * ss(0.35, 0.5, sat) * (1.0 - ss(0.75, 0.95, sat)) * ss(0.55, 0.8, mx);
        neutral.max(gold) * cavity.clamp(0.0, 1.0)
    }

    fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    fn norm3(a: [f32; 3]) -> [f32; 3] {
        let l = dot3(a, a).sqrt();
        [a[0] / l, a[1] / l, a[2] / l]
    }

    #[allow(clippy::too_many_arguments)]
    fn mirror_gloss_add(
        mask: f32,
        strength: f32,
        n: [f32; 3],
        v: [f32; 3],
        l: [f32; 3],
        base: [f32; 3],
        sun: [f32; 3],
        sky: [f32; 3],
        shadow: f32,
    ) -> [f32; 3] {
        let h = norm3([l[0] + v[0], l[1] + v[1] + 1.0e-4, l[2] + v[2]]);
        let nl = dot3(n, l).max(0.0);
        let fres = (1.0 - dot3(n, v).clamp(0.0, 1.0)).powf(4.0);
        let spec = dot3(n, h).max(0.0).powf(32.0) * (1.0 + 2.0 * fres) * ss(0.0, 0.15, nl);
        let day = ss(0.2, 0.5, sun[0].max(sun[1]).max(sun[2]));
        let mut out = [0.0; 3];
        for i in 0..3 {
            let tint = 0.5 + 0.5 * base[i]; // mix(1, base, 0.5)
            let s = tint * sun[i] * (0.55 * spec * shadow * day) + sky[i] * (fres * 0.10);
            out[i] = (s * (mask * strength)).min(0.6);
        }
        out
    }

    #[test]
    fn the_gloss_mask_picks_metal_and_skips_the_rest() {
        let steel = mirror_gloss_mask([0.6, 0.62, 0.65], 1.0);
        let gold = mirror_gloss_mask([0.85, 0.65, 0.25], 1.0);
        let wood = mirror_gloss_mask([0.45, 0.28, 0.12], 1.0);
        let red = mirror_gloss_mask([0.8, 0.1, 0.1], 1.0);
        let dark = mirror_gloss_mask([0.08, 0.08, 0.09], 1.0);
        let green = mirror_gloss_mask([0.2, 0.7, 0.2], 1.0);
        assert!(steel > 0.95, "steel {steel}");
        assert!(gold > 0.4, "gold {gold}");
        assert!(wood < 0.05, "wood {wood}");
        assert!(red < 0.05 && green < 0.05, "saturated cloth {red} {green}");
        assert!(dark < 0.05, "dark metal/leather {dark}");
        // The cavity scales it; a closed crevice shines nothing.
        assert_eq!(mirror_gloss_mask([0.6, 0.62, 0.65], 0.0), 0.0);
        assert!((mirror_gloss_mask([0.6, 0.62, 0.65], 0.5) - steel * 0.5).abs() < 1e-6);
    }

    #[test]
    fn the_gloss_highlight_peaks_at_the_half_vector_and_respects_shadow_night_and_cap() {
        let n = [0.0, 0.0, 1.0];
        let l = norm3([0.4, 0.0, 1.0]);
        let v = norm3([-0.4, 0.0, 1.0]); // half vector = n: the mirror direction
        let base = [0.6, 0.6, 0.6];
        let day = [0.9, 0.9, 0.85];
        let sky = [0.5, 0.6, 0.8];
        let peak = mirror_gloss_add(1.0, 1.0, n, v, l, base, day, sky, 1.0);
        let off = mirror_gloss_add(1.0, 1.0, n, norm3([-0.9, 0.0, 0.3]), l, base, day, sky, 1.0);
        assert!(peak[0] > off[0] * 3.0, "tight highlight: {peak:?} vs {off:?}");
        assert!(peak.iter().all(|c| *c <= 0.6 && *c > 0.0), "{peak:?}");
        // In shadow only the faint sky term remains (the face-on fresnel is ~0).
        let shaded = mirror_gloss_add(1.0, 1.0, n, v, l, base, day, sky, 0.0);
        assert!(shaded[0] < 0.01, "{shaded:?}");
        // The moon's dim sun light gives no highlight.
        let night = mirror_gloss_add(1.0, 1.0, n, v, l, base, [0.1, 0.1, 0.2], sky, 1.0);
        assert!(night[0] < 0.01, "{night:?}");
        // No mask or no strength: exactly nothing; strength scales linearly below the cap.
        assert_eq!(mirror_gloss_add(0.0, 1.0, n, v, l, base, day, sky, 1.0), [0.0; 3]);
        assert_eq!(mirror_gloss_add(1.0, 0.0, n, v, l, base, day, sky, 1.0), [0.0; 3]);
        let half = mirror_gloss_add(0.5, 1.0, n, v, l, base, day, sky, 1.0);
        assert!((half[0] - peak[0] * 0.5).abs() < 1e-5);
        // A grazing view picks up the fresnel sky term.
        let g = norm3([0.99, 0.0, 0.1]);
        let graze = mirror_gloss_add(1.0, 1.0, n, g, g, base, day, sky, 1.0);
        assert!(graze[2] > 0.0);
    }

    #[test]
    fn the_wgsl_uses_the_mirrored_gloss_formulas() {
        let model = include_str!("shaders/wow_model.wgsl");
        for needle in [
            "const GLOSS_POWER: f32 = 32.0;",
            "const GLOSS_GAIN: f32 = 0.55;",
            "const GLOSS_RIM: f32 = 2.0;",
            "const GLOSS_ENV: f32 = 0.10;",
            "const GLOSS_MAX_ADD: f32 = 0.6;",
            "let sat = (mx - mn) / max(mx, 1e-3);",
            "let neutral = (1.0 - smoothstep(0.15, 0.35, sat)) * smoothstep(0.18, 0.45, mx);",
            "let warm = smoothstep(0.0, 0.08, c.r - c.g) * smoothstep(0.0, 0.08, c.g - c.b);",
            "let gold = 0.8 * warm * smoothstep(0.35, 0.5, sat) * (1.0 - smoothstep(0.75, 0.95, sat))",
            "* smoothstep(0.55, 0.8, mx);",
            "return max(neutral, gold) * clamp(cavity, 0.0, 1.0);",
            "let fres = pow(1.0 - clamp(dot(n, to_view), 0.0, 1.0), 4.0);",
            "let spec = pow(max(dot(n, h), 0.0), GLOSS_POWER) * (1.0 + GLOSS_RIM * fres)",
            "* smoothstep(0.0, 0.15, nl);",
            "let day = smoothstep(0.2, 0.5, max(max(sun_rgb.r, sun_rgb.g), sun_rgb.b));",
            "let tint = mix(vec3<f32>(1.0), base, 0.5);",
            "let sun = tint * sun_rgb * (GLOSS_GAIN * spec * shadow * day);",
            "let sky = sky_rgb * (fres * GLOSS_ENV);",
            "return min((sun + sky) * (mask * strength), vec3<f32>(GLOSS_MAX_ADD));",
            // The gate: relief lane, the strength lane, never a WMO / interior / rig.
            "let gloss_on = relief_on && m.relief.w > 0.0 && !is_wmo && !is_interior && !is_rig",
            "spec_add = max(spec_add, gloss_add(gloss_mask(base.rgb, relief.b), m.relief.w, n_shade,",
        ] {
            assert!(model.contains(needle), "wow_model.wgsl drifted from the gloss mirror: {needle}");
        }
    }

    #[test]
    fn the_gloss_dial_rides_the_relief_lane_w_of_a_mapped_material_only() {
        // One test owns the process-wide dials (the other test here reads only `.x` / zero).
        set_model_gloss_dial(1.5);
        assert_eq!(model_relief_params(true).w, 1.5);
        assert_eq!(model_relief_params(false), Vec4::ZERO);
        set_model_gloss_dial(9.0);
        assert_eq!(model_gloss_dial(), 2.0, "clamped to 0..2");
        set_model_gloss_dial(0.0);
        assert_eq!(model_relief_params(true).w, 0.0);
    }
}

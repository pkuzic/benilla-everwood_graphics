//! MONKEY (model relief): the `modelRelief` / `modelReliefDepth` dials on the model lane, and the
//! system that keeps every live `WowModelMaterial`'s relief binding and `relief` uniform lane in
//! step with them and with `benilla_assets::model_relief`'s texture → relief index.

use std::collections::HashSet;

use bevy::prelude::*;

use benilla_assets::materials::WowModelMaterial;
use benilla_assets::model_relief::{
    model_relief_params, relief_for, set_model_relief_dials, take_fresh_model_relief,
};

/// MONKEY (model relief): the `modelRelief` / `modelReliefDepth` cvars as the model lane reads
/// them (written by the app's settings bridge). `mode` 0 = off — today's model render, bit for
/// bit — 1 normal mapping, 2 / 3 + parallax occlusion on WMO batches, 4 / 5 debug (height /
/// normal). `depth` is the parallax depth in world yards. Packed into the `relief`
/// lane (`.y` / `.z`) of every material that HAS a relief map, by [`monkey_sync_model_relief`]
/// on change; materials without a map keep a zero lane and are never touched.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct ModelRelief {
    pub mode: u8,
    pub depth: f32,
    /// MONKEY (model gloss): the metal gloss strength, `modelGloss` * `modelGlossStrength`
    /// (0 = off, 0..2). Rides the `relief` lane's `w`, so it only reaches materials with a relief
    /// map. `MONKEY_MODEL_GLOSS=0|1`, `MONKEY_MODEL_GLOSS_STRENGTH=<0..2>` seed it.
    pub gloss: f32,
}

/// MONKEY (model gloss): the env seeds, parsed apart so a test can drive them (`None` = unset):
/// on by default at strength 1; the result is the single strength dial (0 when switched off).
pub fn gloss_from_env(on: Option<&str>, strength: Option<&str>) -> f32 {
    let on = on.and_then(|v| v.trim().parse::<f32>().ok()).map_or(true, |v| v != 0.0);
    let s = strength.and_then(|v| v.trim().parse::<f32>().ok()).map_or(1.0, |v| v.clamp(0.0, 2.0));
    if on { s } else { 0.0 }
}

impl Default for ModelRelief {
    fn default() -> Self {
        // MONKEY (model relief): `MONKEY_MODEL_RELIEF=<mode>[,<depth>]` seeds the dials for
        // captures and the cvar-less world viewer; the client's cvar bridge overrides it.
        let env: Vec<f32> = std::env::var("MONKEY_MODEL_RELIEF")
            .ok()
            .map(|v| v.split(',').filter_map(|p| p.trim().parse().ok()).collect())
            .unwrap_or_default();
        Self {
            mode: env.first().map_or(0, |m| m.clamp(0.0, 5.0) as u8),
            depth: env.get(1).map_or(0.05, |d| d.clamp(0.0, 0.2)), // MONKEY (model relief): yards
            gloss: gloss_from_env(
                std::env::var("MONKEY_MODEL_GLOSS").ok().as_deref(),
                std::env::var("MONKEY_MODEL_GLOSS_STRENGTH").ok().as_deref(),
            ),
        }
    }
}

/// MONKEY (model relief): `clutter_fade.z` bit 9, the depth-prime twin (`ZFILL_MARKER`): it
/// writes no colour, so it never takes a relief map.
const ZFILL_BIT: u32 = 0x200;

/// MONKEY (model relief): the relief binding + lane `m` should have now, or `None` when it
/// already has them. A material keeps a map once attached (the index only drops a map with its
/// texture, and then the material is going too).
fn wanted(m: &WowModelMaterial) -> Option<(Option<Handle<Image>>, Vec4)> {
    if (m.extension.clutter_fade.z as u32) & ZFILL_BIT != 0 {
        return None;
    }
    let relief = m.extension.relief_texture.clone().or_else(|| {
        m.base
            .base_color_texture
            .as_ref()
            .and_then(|t| relief_for(t.id()))
    });
    let params = model_relief_params(relief.is_some());
    let same =
        relief.is_some() == m.extension.relief_texture.is_some() && m.extension.relief == params;
    (!same).then_some((relief, params))
}

/// MONKEY (model relief): publish changed dials, and bring materials up to date — every live one
/// on a dial change or when the index grew (a texture's relief arrived after its material was
/// built), else only the materials added since last frame (a parked material realized late).
/// Only materials whose binding or lane actually differ are touched (a touched material
/// re-prepares its bind group), and without a relief directory nothing is scanned at all.
pub(super) fn monkey_sync_model_relief(
    relief: Res<ModelRelief>,
    mut events: MessageReader<AssetEvent<WowModelMaterial>>,
    materials: Option<ResMut<Assets<WowModelMaterial>>>,
) {
    let dials_changed = relief.is_changed();
    if dials_changed {
        set_model_relief_dials(relief.mode, relief.depth);
        benilla_assets::model_relief::set_model_gloss_dial(relief.gloss); // MONKEY (model gloss)
    }
    let added: Vec<AssetId<WowModelMaterial>> = events
        .read()
        .filter_map(|e| match e {
            AssetEvent::Added { id } => Some(*id),
            _ => None,
        })
        .collect();
    if benilla_assets::monkey_relief_dir().is_none() {
        return;
    }
    let Some(mut materials) = materials else {
        return;
    };
    let fresh: HashSet<AssetId<Image>> = take_fresh_model_relief().into_iter().collect();
    let candidates: Vec<AssetId<WowModelMaterial>> = if dials_changed || !fresh.is_empty() {
        materials.ids().collect()
    } else if added.is_empty() {
        return;
    } else {
        added
    };
    let stale: Vec<(AssetId<WowModelMaterial>, Option<Handle<Image>>, Vec4)> = candidates
        .into_iter()
        .filter_map(|id| {
            let m = materials.get(id)?;
            wanted(m).map(|(tex, lane)| (id, tex, lane))
        })
        .collect();
    for (id, tex, lane) in stale {
        if let Some(m) = materials.get_mut(id) {
            m.extension.relief_texture = tex;
            m.extension.relief = lane;
        }
    }
}

#[cfg(test)]
mod gloss_tests {
    use super::gloss_from_env;

    /// MONKEY (model gloss): on at strength 1 by default; off wins; the strength clamps to 0..2.
    #[test]
    fn the_gloss_env_seeds_parse_and_clamp() {
        assert_eq!(gloss_from_env(None, None), 1.0);
        assert_eq!(gloss_from_env(Some("0"), None), 0.0);
        assert_eq!(gloss_from_env(Some("1"), Some("0.5")), 0.5);
        assert_eq!(gloss_from_env(None, Some("9")), 2.0);
        assert_eq!(gloss_from_env(Some("0"), Some("2")), 0.0);
        assert_eq!(gloss_from_env(Some("x"), Some("y")), 1.0);
    }

    /// MONKEY (model gloss): the retained pass carries the same gloss constants and gate as the
    /// entity shader, and never glosses a WMO surface.
    #[test]
    fn static_gx_carries_the_same_gloss_and_skips_wmo() {
        let src = include_str!("../shaders/static_gx.wgsl");
        for needle in [
            "const GLOSS_POWER: f32 = 32.0;",
            "const GLOSS_GAIN: f32 = 0.55;",
            "const GLOSS_RIM: f32 = 2.0;",
            "const GLOSS_ENV: f32 = 0.10;",
            "const GLOSS_MAX_ADD: f32 = 0.6;",
            "return max(neutral, gold) * clamp(cavity, 0.0, 1.0);",
            "return min((sun + sky) * (mask * strength), vec3<f32>(GLOSS_MAX_ADD));",
            "let gloss_on = relief_on && relief_dial.dial.z > 0.0 && (in.word & WORD_WMO) == 0u",
        ] {
            assert!(src.contains(needle), "static_gx.wgsl drifted from the gloss mirror: {needle}");
        }
    }
}

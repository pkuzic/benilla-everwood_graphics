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
/// normal). `depth` is the parallax depth in base-texture UV units. Packed into the `relief`
/// lane (`.y` / `.z`) of every material that HAS a relief map, by [`monkey_sync_model_relief`]
/// on change; materials without a map keep a zero lane and are never touched.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct ModelRelief {
    pub mode: u8,
    pub depth: f32,
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
            depth: env.get(1).map_or(0.03, |d| d.clamp(0.0, 0.1)),
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

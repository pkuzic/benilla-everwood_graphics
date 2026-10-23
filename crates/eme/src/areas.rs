//! The `area` tool's picker catalog: the current map's `AreaTable.dbc` rows (id + the client's
//! localized name), refilled on every map change. The DBC is read once, lazily, the first time EME
//! is open in the world (nothing is read for a player who never opens EME).

use bevy::prelude::*;

use benilla_assets::{LockRecover, WorldAssets};
use benilla_world::world_map::CurrentMap;

use crate::tools::area::EmeAreaCatalog;
use crate::EmeActive;

/// The catalog the `area` tool picks from (shared with the tool).
#[derive(Resource, Debug, Clone, Default)]
pub struct EmeAreas(pub EmeAreaCatalog);

pub(crate) fn register(app: &mut App) {
    app.init_resource::<EmeAreas>()
        .add_systems(Update, fill_catalog);
}

/// `(map the catalog holds, the whole table)`.
#[derive(Default)]
struct Loaded {
    map: Option<u32>,
    table: Option<benilla_formats::AreaTableCatalog>,
    failed: bool,
}

fn fill_catalog(
    active: Res<EmeActive>,
    areas: Res<EmeAreas>,
    map: Option<Res<CurrentMap>>,
    assets: Option<Res<WorldAssets>>,
    mut loaded: Local<Loaded>,
) {
    let (true, Some(map)) = (active.0, map) else {
        return;
    };
    if loaded.map == Some(map.0) || loaded.failed {
        return;
    }
    if loaded.table.is_none() {
        let Some(assets) = assets else { return };
        let result = {
            let mut chain = assets.chain.lock_recover();
            benilla_formats::load_area_table_catalog(&mut chain)
        };
        match result {
            Ok(t) => loaded.table = Some(t),
            Err(e) => {
                warn!("EME: AreaTable.dbc failed to load ({e:#}) - the area picker is empty");
                loaded.failed = true;
                return;
            }
        }
    }
    let Some(table) = loaded.table.as_ref() else { return };
    let rows: Vec<(u16, String)> = table
        .entries()
        .filter(|(id, r)| r.map_id == map.0 && *id != 0 && *id <= u32::from(u16::MAX))
        .map(|(id, r)| (id as u16, r.name.clone()))
        .collect();
    info!("EME: area picker - {} AreaTable rows on map {}", rows.len(), map.0);
    areas.0.set(rows);
    loaded.map = Some(map.0);
}

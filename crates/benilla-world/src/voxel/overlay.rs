//! MONKEY (EME overlay): the client side of the v3 overlay layers (CONTRACT §11) — liquid cells and
//! area cells on top of the immutable ADT baseline, server-ordered, journaled per region with
//! epochs like the voxels (engine-free journal: [`benilla_voxel::overlay::OverlayJournal`]).
//!
//! - [`MonkeyOverlayNet`]: the network's mailbox (benilla-app `net/voxel_relay.rs` decodes `L` /
//!   `A` / `O` / `Z` into [`MonkeyOverlayNetEvent`]s, in arrival order).
//! - [`MonkeyOverlay`]: the journal + the current map's derived state. Persisted to
//!   `$BENILLA_HOME/monkey_overlay.bin`.
//! - **Liquid** (lane K): every wet overlay cell draws as an ordinary ADT-style liquid surface (one
//!   `benilla_formats::LiquidMesh` per MCNK block and level, spawned through the same
//!   `liquid::spawn_liquids` as streamed MCLQ, so the shader, waves, foam, mirror, sounds AND every
//!   liquid query — swimming, wading, the underwater eye — treat it as real water). Covered ADT
//!   cells yield ([`crate::liquid::MonkeyLiquidMask`]). Depth (the swatch ramp) is measured down to
//!   the voxel-aware ground, so a pit dug by voxels fills to the level with its floor visible.
//! - **Area** (lane L): the 16-yd area cells are published to the terrain streamer, which answers
//!   them before the MCNK and WMO-interior area ids (`CurrentArea`: zone / subzone / minimap text,
//!   area-keyed grading / music).
//!
//! Undo / revert arrive as `Z` + a re-OSYNC without the reverted rows: the cells drop, their blocks
//! rebuild, and the baseline (water and area) comes back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use bevy::prelude::*;

use benilla_assets::AdtTile;
use benilla_formats::{LiquidKind, LiquidMesh};
use benilla_voxel::overlay::{
    LiquidCell, OverlayJournal, OverlayRow, OverlayValue, KIND_MAGMA, KIND_OCEAN, KIND_SLIME,
    LAYER_AREA, LAYER_LIQUID, LIQ_CELL,
};
use benilla_voxel::world::{raymarch, RegionSync};

use super::{monkey_voxel_aim_density, monkey_voxel_home_file, MonkeyVoxelWorld};
use crate::liquid::{LiquidAssets, MonkeyLiquidMask, MonkeyOverlayLiquid};
use crate::terrain_stream::TerrainStreamer;

/// Liquid cells per MCNK block edge (the MCLQ grid).
const BLOCK: i32 = 8;
/// The river/lake swatch saturates at depth byte 42 ≈ 4.7 yd of water (8.96 bytes/yd authored).
const DEPTH_V_PER_YARD: f32 = 8.96 / 42.0;
/// How deep the depth probe looks under a surface, yards.
const DEPTH_PROBE: f32 = 12.0;

/// One overlay record from the network, in arrival order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MonkeyOverlayNetEvent {
    /// `L` / `A`: a cell row (`layer` 1 liquid, 2 area).
    Cell {
        layer: u8,
        map: u16,
        cell: (i16, i16),
        row: OverlayRow,
    },
    /// `Z`: drop the region's cells of that layer, adopt `epoch`.
    Reset {
        layer: u8,
        map: u16,
        rx: i16,
        ry: i16,
        epoch: u16,
    },
    /// The region's persisted sync point (an `O`, or a live cell in a synced region).
    SyncPoint {
        layer: u8,
        map: u16,
        rx: i16,
        ry: i16,
        sync: RegionSync,
    },
}

/// The network → overlay mailbox.
#[derive(Resource, Default)]
pub struct MonkeyOverlayNet {
    /// Records to apply next frame, in arrival order.
    pub events: Vec<MonkeyOverlayNetEvent>,
}

/// The overlay layers' state (see the module docs).
#[derive(Resource, Default)]
pub struct MonkeyOverlay {
    journal: OverlayJournal,
    loaded: bool,
    /// The map the derived state is built for.
    map: Option<u32>,
    /// MCNK blocks whose liquid surfaces must be rebuilt.
    liquid_dirty: HashSet<(i32, i32)>,
    /// The area overrides changed (republish to the streamer).
    area_dirty: bool,
    /// The covered liquid cells changed (refit the ADT mask).
    mask_dirty: bool,
    journal_dirty: bool,
    /// The overlay surfaces spawned per block.
    entities: HashMap<(i32, i32), Vec<Entity>>,
    /// Bumped on every change of a live cell (UI / tests watch it).
    generation: u64,
}

impl MonkeyOverlay {
    /// The map the overlay mirrors.
    pub fn map(&self) -> Option<u32> {
        self.map
    }

    /// The whole journal (every map).
    pub fn journal(&self) -> &OverlayJournal {
        &self.journal
    }

    /// Bumped whenever a live cell changes.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The persisted sync point of `(layer, map, region)` for an OSYNC entry.
    pub fn region_sync(&self, layer: u8, map: u16, rx: i16, ry: i16) -> RegionSync {
        self.journal.region_sync(layer, map, rx, ry)
    }

    /// The live liquid cell at a WoW XY on the current map (`kind 0` = "no liquid here").
    pub fn liquid_at(&self, x: f32, y: f32) -> Option<LiquidCell> {
        self.journal.liquid_at(self.map? as u16, x, y)
    }

    /// The overriding area id at a WoW XY on the current map.
    pub fn area_at(&self, x: f32, y: f32) -> Option<u16> {
        self.journal.area_at(self.map? as u16, x, y)
    }

    /// Apply one network record; marks what it changed.
    pub fn apply_event(&mut self, ev: MonkeyOverlayNetEvent) {
        match ev {
            MonkeyOverlayNetEvent::Cell {
                layer,
                map,
                cell,
                row,
            } => {
                if self.journal.apply(layer, map, cell, row) {
                    self.journal_dirty = true;
                    self.touch(layer, map, std::iter::once(cell));
                }
            }
            MonkeyOverlayNetEvent::Reset {
                layer,
                map,
                rx,
                ry,
                epoch,
            } => {
                let dropped = self.journal.reset(layer, map, rx, ry, epoch);
                self.journal_dirty = true;
                if !dropped.is_empty() {
                    info!(
                        "MONKEY (EME overlay): Z layer {layer} region ({rx}, {ry}) epoch {epoch} - \
                         {} cell(s) dropped, re-OSYNC",
                        dropped.len()
                    );
                }
                self.touch(layer, map, dropped.into_iter());
            }
            MonkeyOverlayNetEvent::SyncPoint {
                layer,
                map,
                rx,
                ry,
                sync,
            } => {
                if self.journal.region_sync(layer, map, rx, ry) != sync {
                    self.journal.set_region(layer, map, rx, ry, sync);
                    self.journal_dirty = true;
                }
            }
        }
    }

    fn touch(&mut self, layer: u8, map: u16, cells: impl Iterator<Item = (i16, i16)>) {
        if self.map != Some(map as u32) {
            return; // another map: derived state rebuilds on arrival
        }
        let mut any = false;
        for (a, b) in cells {
            any = true;
            if layer == LAYER_LIQUID {
                self.liquid_dirty.insert(block_of(a, b));
                self.mask_dirty = true;
            }
        }
        if any {
            self.generation += 1;
            if layer == LAYER_AREA {
                self.area_dirty = true;
            }
        }
    }

    /// Switch the derived state to `map` (every block of the new map rebuilds).
    fn set_map(&mut self, map: u32) {
        self.map = Some(map);
        let blocks: Vec<(i32, i32)> = self
            .journal
            .liquid_cells(map as u16)
            .map(|((a, b), _)| block_of(a, b))
            .collect();
        self.liquid_dirty.extend(self.entities.keys().copied());
        self.liquid_dirty.extend(blocks);
        self.area_dirty = true;
        self.mask_dirty = true;
        self.generation += 1;
    }
}

/// The MCNK block (8×8 liquid cells) of liquid cell `(cx, cy)`.
pub fn block_of(cx: i16, cy: i16) -> (i32, i32) {
    (
        (cx as i32).div_euclid(BLOCK),
        (cy as i32).div_euclid(BLOCK),
    )
}

/// The renderer's kind for a wire liquid kind (`None` = no surface: kind 0 erases).
fn render_kind(kind: u8) -> Option<LiquidKind> {
    match kind {
        0 => None,
        KIND_OCEAN => Some(LiquidKind::Ocean),
        KIND_MAGMA => Some(LiquidKind::Magma),
        KIND_SLIME => Some(LiquidKind::Slime),
        _ => Some(LiquidKind::Still),
    }
}

/// World coordinate of liquid-lattice line `i` (exact in f64, like the ADT lattice).
fn lattice(i: i32) -> f32 {
    (f64::from(i) * 25.0 / 6.0) as f32
}

/// The flat surfaces of one block: one [`LiquidMesh`] per (kind, level) group of its wet cells, in
/// the MCLQ layout (9×9 vertices from the block's north-west corner, rows step −X, columns −Y).
/// `depth(x, y, level)` gives the water depth at a vertex (yards).
pub fn block_meshes(
    block: (i32, i32),
    cells: &BTreeMap<(i16, i16), LiquidCell>,
    mut depth: impl FnMut(f32, f32, f32) -> f32,
) -> Vec<LiquidMesh> {
    let (bx, by) = block;
    // Group by (kind, level bits) so each grid is one flat plane.
    let mut groups: BTreeMap<(u8, u32), Vec<usize>> = BTreeMap::new();
    for row in 0..BLOCK {
        for col in 0..BLOCK {
            let cx = (bx * BLOCK + BLOCK - 1 - row) as i16;
            let cy = (by * BLOCK + BLOCK - 1 - col) as i16;
            if let Some(c) = cells.get(&(cx, cy)) {
                if c.is_wet() && render_kind(c.kind).is_some() {
                    groups
                        .entry((c.kind, c.level.to_bits()))
                        .or_default()
                        .push((row * BLOCK + col) as usize);
                }
            }
        }
    }
    let g = (BLOCK + 1) as usize;
    let x0 = bx * BLOCK + BLOCK;
    let y0 = by * BLOCK + BLOCK;
    groups
        .into_iter()
        .filter_map(|((kind, bits), wet_cells)| {
            let level = f32::from_bits(bits);
            let kind = render_kind(kind)?;
            let mut wet = vec![false; (BLOCK * BLOCK) as usize];
            let mut indices = Vec::with_capacity(wet_cells.len() * 6);
            for &c in &wet_cells {
                wet[c] = true;
                let (row, col) = (c / BLOCK as usize, c % BLOCK as usize);
                let tl = (row * g + col) as u32;
                let (tr, bl) = (tl + 1, ((row + 1) * g + col) as u32);
                let br = bl + 1;
                indices.extend_from_slice(&[tl, bl, br, tl, br, tr]);
            }
            let mut positions = Vec::with_capacity(g * g);
            let mut uvs = Vec::with_capacity(g * g);
            let mut depths = Vec::with_capacity(g * g);
            for n in 0..g * g {
                let (row, col) = ((n / g) as i32, (n % g) as i32);
                let (x, y) = (lattice(x0 - row), lattice(y0 - col));
                positions.push([x, y, level]);
                uvs.push([col as f32 * 0.25, row as f32 * 0.25]);
                depths.push((depth(x, y, level) * DEPTH_V_PER_YARD).clamp(0.0, 1.0));
            }
            Some(LiquidMesh {
                grid: [g as u32, g as u32],
                shared: vec![false; wet.len()],
                wet,
                positions,
                uvs,
                depths,
                indices,
                sound_nibble: 0,
                material_id: None,
                kind,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Journal file
// ---------------------------------------------------------------------------------------------

fn monkey_overlay_journal_path() -> std::path::PathBuf {
    monkey_voxel_home_file("monkey_overlay.bin")
}

fn monkey_overlay_journal_load() -> OverlayJournal {
    let path = monkey_overlay_journal_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return OverlayJournal::default();
    };
    match OverlayJournal::decode(&bytes) {
        Ok(j) => {
            let n: usize = j.layers.values().map(|l| l.cells.len()).sum();
            info!(
                "MONKEY (EME overlay): journal loaded - {n} cell(s) in {} layer-map(s) from {}",
                j.layers.len(),
                path.display()
            );
            j
        }
        Err(e) => {
            warn!(
                "MONKEY (EME overlay): ignoring {} ({e:?}) - starting an empty overlay journal",
                path.display()
            );
            OverlayJournal::default()
        }
    }
}

fn monkey_overlay_journal_save(j: &OverlayJournal) {
    static SAVE: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let bytes = j.encode();
    let my = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let path = monkey_overlay_journal_path();
    let write = move || {
        let mut last = SAVE.lock().unwrap_or_else(|e| e.into_inner());
        if *last > my {
            return;
        }
        *last = my;
        let tmp = path.with_extension("bin.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    };
    match bevy::tasks::IoTaskPool::try_get() {
        Some(pool) => pool.spawn(async move { write() }).detach(),
        None => write(),
    }
}

// ---------------------------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------------------------

/// Load the journal once, follow the map, apply the network's records.
pub(super) fn monkey_overlay_apply(
    map: Option<Res<crate::world_map::CurrentMap>>,
    mut ov: ResMut<MonkeyOverlay>,
    mut net: ResMut<MonkeyOverlayNet>,
) {
    if !ov.loaded {
        ov.loaded = true;
        ov.journal = monkey_overlay_journal_load();
    }
    if let Some(map) = map {
        if ov.map != Some(map.0) {
            ov.set_map(map.0);
        }
    }
    for ev in std::mem::take(&mut net.events) {
        ov.apply_event(ev);
    }
}

/// Rebuild the dirty blocks' liquid surfaces, refit the ADT mask, publish the area overrides.
#[allow(clippy::too_many_arguments)]
pub(super) fn monkey_overlay_publish(
    mut commands: Commands,
    mut ov: ResMut<MonkeyOverlay>,
    liquid_assets: Option<Res<LiquidAssets>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mask: ResMut<MonkeyLiquidMask>,
    mut streamer: ResMut<TerrainStreamer>,
    voxels: Res<MonkeyVoxelWorld>,
    adts: Res<Assets<AdtTile>>,
) {
    let Some(map) = ov.map else {
        return;
    };
    let map16 = map as u16;
    if ov.area_dirty {
        ov.area_dirty = false;
        let cells: HashMap<(i16, i16), u16> = ov.journal.area_cells(map16).collect();
        info!(
            "MONKEY (EME overlay): {} area override cell(s) on map {map}",
            cells.len()
        );
        streamer.set_monkey_area_overlay(Some(Arc::new(cells)));
    }
    if ov.mask_dirty {
        ov.mask_dirty = false;
        // Every overlay liquid cell (any kind) takes over the ADT cell under it.
        mask.set(ov.journal.liquid_cells(map16).map(|(c, _)| c).collect());
    }
    if ov.liquid_dirty.is_empty() {
        return;
    }
    let Some(liquid_assets) = liquid_assets else {
        return; // the liquid materials are not up yet: keep the blocks dirty
    };
    let dirty: Vec<(i32, i32)> = ov.liquid_dirty.drain().collect();
    let all: BTreeMap<(i16, i16), LiquidCell> = ov.journal.liquid_cells(map16).collect();
    let (mut spawned, mut cleared) = (0usize, 0usize);
    for block in dirty {
        if let Some(old) = ov.entities.remove(&block) {
            cleared += old.len();
            for e in old {
                commands.entity(e).despawn();
            }
        }
        let (bx, by) = block;
        let lo = ((bx * BLOCK) as i16, (by * BLOCK) as i16);
        let hi = ((bx * BLOCK + BLOCK - 1) as i16, (by * BLOCK + BLOCK - 1) as i16);
        let cells: BTreeMap<(i16, i16), LiquidCell> = all
            .range(lo..=hi)
            .filter(|((a, b), _)| *a >= lo.0 && *a <= hi.0 && *b >= lo.1 && *b <= hi.1)
            .map(|(k, v)| (*k, *v))
            .collect();
        if cells.is_empty() {
            continue;
        }
        let lqs = block_meshes(block, &cells, |x, y, level| {
            raymarch(
                [x, y, level],
                [0.0, 0.0, -1.0],
                DEPTH_PROBE,
                0.25,
                |p| monkey_voxel_aim_density(&voxels, &streamer, &adts, p),
            )
            .map_or(DEPTH_PROBE, |p| level - p[2])
        });
        let mut ents = Vec::new();
        crate::liquid::spawn_liquids(
            &mut commands,
            lqs.iter(),
            Some(&liquid_assets),
            &mut *meshes,
            &mut ents,
        );
        for &e in &ents {
            commands.entity(e).insert((MonkeyOverlayLiquid, Name::new("MonkeyOverlayLiquid")));
        }
        spawned += ents.len();
        if !ents.is_empty() {
            ov.entities.insert(block, ents);
        }
    }
    info!(
        "MONKEY (EME overlay): liquid surfaces rebuilt - {spawned} spawned, {cleared} cleared, \
         {} overlay cell(s) on map {map}",
        all.len()
    );
}

/// Persist the journal (≤ 1 write per 2 s while it changes).
pub(super) fn monkey_overlay_autosave(
    time: Res<Time>,
    mut ov: ResMut<MonkeyOverlay>,
    mut since: Local<f32>,
) {
    if !ov.journal_dirty {
        *since = 0.0;
        return;
    }
    *since += time.delta_secs();
    if *since < 2.0 {
        return;
    }
    *since = 0.0;
    ov.journal_dirty = false;
    monkey_overlay_journal_save(&ov.journal);
}

/// `WOW_OVERLAY_PROBE=1`: once a second, one log line whenever what the overlay means for the
/// player changes — the area override and `CurrentArea` under the body, the liquid surface over the
/// feet (and the depth), the eye's submersion, the overlay cell counts. The live check's evidence.
pub(super) fn monkey_overlay_probe(
    time: Res<Time>,
    focus: Res<crate::terrain_stream::ViewFocus>,
    ov: Res<MonkeyOverlay>,
    world: crate::world_point::WorldPoint,
    mut last: Local<String>,
    mut acc: Local<f32>,
    mut on: Local<Option<bool>>,
) {
    if !*on.get_or_insert_with(|| std::env::var_os("WOW_OVERLAY_PROBE").is_some()) {
        return;
    }
    *acc += time.delta_secs();
    if *acc < 1.0 {
        return;
    }
    *acc = 0.0;
    let Some(body) = focus.body_pos() else { return };
    let map = ov.map().unwrap_or(0) as u16;
    let liquid = world.liquid_at(crate::world_point::Subject::Player, body);
    let line = format!(
        "body ({:.0}, {:.0}, {:.1}) area override {:?} CurrentArea {:?} | overlay liquid cell {:?} |          liquid over feet {} | eye {:?} | cells liquid {} area {}",
        body[0],
        body[1],
        body[2],
        ov.area_at(body[0], body[1]),
        world.area(),
        ov.liquid_at(body[0], body[1]).map(|c| (c.kind, c.level)),
        liquid.map_or("none".to_string(), |h| format!(
            "{:?} surface {:.2} depth {:.2}",
            h.kind,
            h.surface_z,
            h.surface_z - body[2]
        )),
        world.submersion(),
        ov.journal.liquid_cells(map).count(),
        ov.journal.area_cells(map).count(),
    );
    // Depth wobbles while swimming: compare without it.
    let key: String = line.split(" depth ").next().unwrap_or("").to_string()
        + &line.split(" | eye ").nth(1).unwrap_or("").to_string();
    if *last != key {
        *last = key;
        info!("MONKEY (EME overlay) PROBE: {line}");
    }
}

/// Helper for the net side: the cell key + row of an `L` record.
pub fn liquid_event(
    seq: u32,
    stroke: u16,
    map: u16,
    cx: i16,
    cy: i16,
    cell: LiquidCell,
    actor: u32,
) -> MonkeyOverlayNetEvent {
    MonkeyOverlayNetEvent::Cell {
        layer: LAYER_LIQUID,
        map,
        cell: (cx, cy),
        row: OverlayRow {
            seq,
            stroke,
            actor,
            value: OverlayValue::Liquid(cell),
        },
    }
}

/// Helper for the net side: the cell key + row of an `A` record.
pub fn area_event(
    seq: u32,
    stroke: u16,
    map: u16,
    ax: i16,
    ay: i16,
    area_id: u16,
    actor: u32,
) -> MonkeyOverlayNetEvent {
    MonkeyOverlayNetEvent::Cell {
        layer: LAYER_AREA,
        map,
        cell: (ax, ay),
        row: OverlayRow {
            seq,
            stroke,
            actor,
            value: OverlayValue::Area(area_id),
        },
    }
}

/// `LIQ_CELL` re-exported for the editor (cell maths).
pub const LIQUID_CELL_YARDS: f32 = LIQ_CELL;

#[cfg(test)]
mod tests {
    use super::*;
    use benilla_voxel::overlay::{liquid_cell_of, KIND_NONE, KIND_WATER};

    fn wcell(level: f32) -> LiquidCell {
        LiquidCell {
            kind: KIND_WATER,
            level,
            flags: 0,
            fade: 0,
        }
    }

    #[test]
    fn block_mesh_covers_exactly_its_cells_at_the_level() {
        // Two cells of block (12, -3) at 81, one at 79, one erase (kind 0).
        let mut cells = BTreeMap::new();
        cells.insert((100, -20), wcell(81.0));
        cells.insert((101, -20), wcell(81.0));
        cells.insert((100, -19), wcell(79.0));
        cells.insert(
            (101, -19),
            LiquidCell {
                kind: KIND_NONE,
                ..wcell(81.0)
            },
        );
        assert_eq!(block_of(100, -20), (12, -3));
        let lqs = block_meshes((12, -3), &cells, |_, _, _| 2.0);
        assert_eq!(lqs.len(), 2, "one plane per level; kind 0 draws nothing");
        for lq in &lqs {
            assert!(lq.positions.iter().all(|p| p[2] == 79.0 || p[2] == 81.0));
            assert_eq!(lq.positions.len(), 81);
            // Every wet cell's centre maps back to an overlay cell of that level.
            let z = lq.positions[0][2];
            for (i, _) in lq.wet.iter().enumerate().filter(|(_, w)| **w) {
                let (row, col) = (i / 8, i % 8);
                let p = |r: usize, c: usize| lq.positions[r * 9 + c];
                let cx = (p(row, col)[0] + p(row + 1, col + 1)[0]) * 0.5;
                let cy = (p(row, col)[1] + p(row + 1, col + 1)[1]) * 0.5;
                let key = liquid_cell_of(cx, cy);
                assert_eq!(cells[&key].level, z, "cell {key:?}");
            }
        }
        let n_wet: usize = lqs.iter().map(|l| l.wet.iter().filter(|w| **w).count()).sum();
        assert_eq!(n_wet, 3);
        assert!(lqs.iter().all(|l| l.indices.len() == 6 * l.wet.iter().filter(|w| **w).count()));
        assert!((lqs[0].depths[0] - 2.0 * DEPTH_V_PER_YARD).abs() < 1e-6);
    }

    #[test]
    fn overlay_events_track_dirty_blocks_resets_and_area() {
        let mut ov = MonkeyOverlay {
            loaded: true,
            ..Default::default()
        };
        ov.set_map(0);
        ov.liquid_dirty.clear();
        ov.area_dirty = false;
        let g0 = ov.generation();
        ov.apply_event(liquid_event(5, 3, 0, 100, -20, wcell(12.5), 42));
        assert!(ov.liquid_dirty.contains(&(12, -3)));
        assert!(ov.generation() > g0);
        assert_eq!(ov.liquid_at(418.0, -80.0).map(|c| c.level), Some(12.5));
        // A duplicate changes nothing.
        ov.liquid_dirty.clear();
        let g1 = ov.generation();
        ov.apply_event(liquid_event(5, 3, 0, 100, -20, wcell(12.5), 42));
        assert!(ov.liquid_dirty.is_empty());
        assert_eq!(ov.generation(), g1);
        // Area override → dirty; Z removes it again (restore).
        ov.apply_event(area_event(1, 4, 0, -557, -8, 87, 42));
        assert!(ov.area_dirty);
        assert_eq!(ov.area_at(-8911.0, -117.0), Some(87));
        ov.area_dirty = false;
        ov.apply_event(MonkeyOverlayNetEvent::Reset {
            layer: LAYER_AREA,
            map: 0,
            rx: -140,
            ry: -2,
            epoch: 1,
        });
        assert!(ov.area_dirty);
        assert_eq!(ov.area_at(-8911.0, -117.0), None);
        // Undo of the water: Z on its region drops the cell and marks the block.
        ov.liquid_dirty.clear();
        let (rx, ry) = benilla_voxel::overlay::cell_region(LAYER_LIQUID, 100, -20);
        ov.apply_event(MonkeyOverlayNetEvent::Reset {
            layer: LAYER_LIQUID,
            map: 0,
            rx,
            ry,
            epoch: 1,
        });
        assert_eq!(ov.liquid_at(418.0, -80.0), None);
        assert!(ov.liquid_dirty.contains(&(12, -3)));
        assert_eq!(ov.region_sync(LAYER_LIQUID, 0, rx, ry).epoch, 1);
        // Another map's cell doesn't touch this map's derived state.
        ov.liquid_dirty.clear();
        ov.apply_event(liquid_event(6, 3, 1, 0, 0, wcell(1.0), 42));
        assert!(ov.liquid_dirty.is_empty());
    }
}

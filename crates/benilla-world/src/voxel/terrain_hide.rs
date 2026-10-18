//! MONKEY (voxel): hide the real ADT terrain under a voxel build site, so a region placed on the
//! real ground REPLACES it — a hole dug into the voxel shows the voxel interior (dark walls going
//! down), not the old flat heightmap ground poking through at its original height.
//!
//! The terrain renderer draws one mesh per CELL = 4×4 MCNK chunks (`terrain_stream::furnish`). This
//! module finds the cells the voxel footprint overlaps and re-meshes them with the covered MCNK
//! chunks carved out via [`benilla_formats::chunk_indices_holed`] (`chunk.holes | extra_holes`) — a
//! block-precise hole, the same mechanism a loaded ADT hole uses. It is reversible: a cell is
//! restored (re-meshed un-holed) when the region moves off it or is removed, and re-carved when a
//! tile streams out and back in (the furnisher respawns the cell fresh/visible).
//!
//! Only 2×2-cell hole blocks lying FULLY inside the footprint are carved: a block reaching past the
//! voxel's edge would show the sky through a sliver, while a leftover ADT rim UNDER the voxel is
//! invisible (the voxel surface is lifted `SEED_LIFT` above it).
//!
//! Dig-through: the tile's one static terrain collider is rebuilt with the same carve
//! ([`sync_tile_colliders`]), so a pit dug in the voxel is walkable down to the voxel's own floor,
//! and the carve is published to `TerrainStreamer` so the MCNK HEIGHT query answers `None` over it
//! (area / ground effect / shade keep the un-holed data; the voxel seed samplers read the raw path).
//!
//! Everything here is gated behind the crate's `voxel` cargo feature, like the rest of `voxel`.

use std::collections::{HashMap, HashSet};

use avian3d::prelude::{Collider, SpatialQuery, SpatialQueryFilter};
use bevy::camera::primitives::Aabb;
use bevy::math::Vec3A;
use bevy::prelude::*;

use super::{MonkeyVoxelData, MonkeyVoxelVolume};
use crate::terrain_stream::{build_collider_task, PendingCollider, TerrainStreamer};
use benilla_assets::coords::bevy_to_wow;
use benilla_assets::materials::TerrainMaterial;
use benilla_assets::{chunks_to_mesh, AdtTile, ChunkShading};
use benilla_formats::{chunk_indices_holed, ChunkMesh};

/// `(tile_x, tile_y, cell_idx)`: a render cell (4×4 MCNK chunks) of one tile, the unit the
/// furnisher spawns one `Mesh3d` + `MeshMaterial3d<TerrainMaterial>` per.
type CellKey = (i32, i32, usize);

/// What to carve out of one render cell: the extra hole bits per MCNK chunk (`chunk_idx` within the
/// tile, 0..256) plus a cheap signature so a moved footprint re-carves only when the bits change.
struct CellPlan {
    chunk_holes: Vec<(usize, u16)>,
    sig: u64,
}

/// MONKEY (voxel): the full per-tile carve used by its single terrain collider.
#[derive(Clone, Default)]
struct TilePlan {
    chunk_holes: Vec<(usize, u16)>,
    sig: u64,
}

/// The build site's terrain-hiding state: the footprint signature (recompute the plan only when it
/// changes), the per-cell carve plan, and which cell entities are currently carved (so they are
/// restored when the footprint leaves them and re-carved after a stream-out/in respawns them).
#[derive(Resource, Default)]
pub(super) struct MonkeyVoxelHideState {
    /// Quantized footprint signature (centre ×2 rounded, chunk dims); `None` = no volume / cleared.
    sig: Option<(i64, i64, [u32; 3])>,
    /// The carve plan per affected render cell.
    desired: HashMap<CellKey, CellPlan>,
    /// MONKEY (voxel): collider plan per tile, including the combined chunk hole bits.
    desired_tiles: HashMap<(i32, i32), TilePlan>,
    /// Carved cell entities → (key, plan-sig) they were carved for.
    applied: HashMap<Entity, (CellKey, u64)>,
    /// MONKEY (voxel): collider root -> (hole signature), also detects streamed roots.
    applied_tiles: HashMap<(i32, i32), (Entity, u64)>,
    /// MONKEY (voxel): re-plan until resident — the last plan found 0 cells or a footprint corner's
    /// tile wasn't resident, so re-plan every ~0.5 s even with an unchanged signature.
    incomplete: bool,
    replan_timer: f32,
}

/// `WOW_VOXEL_NO_HIDE=1`: keep the real ADT ground under the build site (the pre-hiding look), a
/// dev A/B switch read once.
fn monkey_voxel_hide_disabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WOW_VOXEL_NO_HIDE").is_some())
}

/// Half the yard-size of a 2-cell hole block (= `CHUNK_SIZE / 4` ≈ 8.33 yd), the granularity the
/// `holes` bitmap drops: bit `(row>>1)*4 + (col>>1)` for the 8×8 cell grid.
#[inline]
fn block_size() -> f32 {
    benilla_formats::CHUNK_SIZE / 4.0
}

/// MONKEY (voxel): require a complete hole block inside the footprint, within floating error.
#[inline]
fn fully_inside(lo: f32, hi: f32, footprint_lo: f32, footprint_hi: f32, eps: f32) -> bool {
    lo >= footprint_lo - eps && hi <= footprint_hi + eps
}

/// The world-centre of the 64×64 tile grid in WoW yards; the global MCNK grid counts out from here.
#[inline]
fn map_center() -> f32 {
    32.0 * benilla_formats::TILE_SIZE
}

/// The `(tile_x, tile_y, cell_idx)` a WoW XY column falls in, by the global MCNK grid. Rows step
/// south (−X) and are `tile_y`'s axis; columns step east (−Y) and are `tile_x`'s axis — the same
/// split `benilla_formats::world_to_tile` uses (`tile_x` from Y, `tile_y` from X).
fn cell_key_at(wow_x: f32, wow_y: f32) -> CellKey {
    let cs = benilla_formats::CHUNK_SIZE;
    let mc = map_center();
    let gx = (((mc - wow_x) / cs).floor() as i32).clamp(0, 1023); // global chunk row (X / south)
    let gy = (((mc - wow_y) / cs).floor() as i32).clamp(0, 1023); // global chunk col (Y / east)
    let (tx, ty) = (gy / 16, gx / 16);
    let (row, col) = ((gx % 16) as usize, (gy % 16) as usize);
    (tx, ty, (row / 4) * 4 + (col / 4))
}

/// The render cell an entity's world-space `Aabb` names, from its horizontal centre.
fn cell_key_of(center: Vec3A) -> CellKey {
    // Bevy → WoW horizontal: wow_x (north) = −bevy_z, wow_y (west) = −bevy_x.
    cell_key_at(-center.z, -center.x)
}

/// Build the per-cell carve plan for the footprint `[fx0,fx1]×[fy0,fy1]` (WoW X×Y). Pure geometry:
/// cells of tiles not yet resident keep their plan and are carved once they stream in.
fn compute_desired(fx0: f32, fx1: f32, fy0: f32, fy1: f32) -> HashMap<CellKey, CellPlan> {
    let cs = benilla_formats::CHUNK_SIZE;
    let mc = map_center();
    let block = block_size();
    const EPS: f32 = 1.0e-3;

    // The global MCNK chunk index range the footprint's bounding box touches.
    let gx_lo = (((mc - fx1) / cs).floor() as i32).clamp(0, 1023);
    let gx_hi = (((mc - fx0) / cs).floor() as i32).clamp(0, 1023);
    let gy_lo = (((mc - fy1) / cs).floor() as i32).clamp(0, 1023);
    let gy_hi = (((mc - fy0) / cs).floor() as i32).clamp(0, 1023);

    // Accumulate extra hole bits per (tile, chunk index within tile). A 2-cell hole block (8.33 yd)
    // is holed only when its WHOLE [x0,x1]×[y0,y1] lies inside the footprint (`EPS` tolerance for a
    // block edge landing exactly on the footprint edge). Rounding the carve DOWN never opens the
    // sky past the voxel's edge; the un-carved rim (< one block per side) sits under the voxel's
    // lifted surface, invisible.
    let mut per_tile: HashMap<(i32, i32), HashMap<usize, u16>> = HashMap::new();
    for gx in gx_lo..=gx_hi {
        let chunk_x_hi = mc - gx as f32 * cs; // chunk's north edge (max X)
        for gy in gy_lo..=gy_hi {
            let chunk_y_hi = mc - gy as f32 * cs; // chunk's west edge (max Y)
            let (tx, ty) = (gy / 16, gx / 16);
            let chunk_idx = (gx % 16) as usize * 16 + (gy % 16) as usize;
            let mut bits: u16 = 0;
            for brow in 0..4i32 {
                let bx1 = chunk_x_hi - brow as f32 * block;
                let bx0 = bx1 - block;
                if !fully_inside(bx0, bx1, fx0, fx1, EPS) {
                    continue; // the full block is not inside the footprint's X span
                }
                for bcol in 0..4i32 {
                    let by1 = chunk_y_hi - bcol as f32 * block;
                    let by0 = by1 - block;
                    if !fully_inside(by0, by1, fy0, fy1, EPS) {
                        continue;
                    }
                    bits |= 1u16 << (brow * 4 + bcol);
                }
            }
            if bits != 0 {
                *per_tile
                    .entry((tx, ty))
                    .or_default()
                    .entry(chunk_idx)
                    .or_insert(0) |= bits;
            }
        }
    }

    // Group every affected chunk into a render cell. Unresident tiles retain a plan and are carved
    // as soon as their root and cell entities stream in.
    let mut desired: HashMap<CellKey, CellPlan> = HashMap::new();
    for ((tx, ty), chunks) in per_tile {
        for (chunk_idx, bits) in chunks {
            let (row, col) = (chunk_idx / 16, chunk_idx % 16);
            let cell = (row / 4) * 4 + (col / 4);
            desired
                .entry((tx, ty, cell))
                .or_insert_with(|| CellPlan {
                    chunk_holes: Vec::new(),
                    sig: 0,
                })
                .chunk_holes
                .push((chunk_idx, bits));
        }
    }
    for plan in desired.values_mut() {
        plan.chunk_holes.sort_unstable();
        // FNV-1a over the (chunk, bits) pairs: changes when the covered blocks change.
        let mut h = 0xcbf29ce484222325u64;
        for (ci, bits) in &plan.chunk_holes {
            for b in (*ci as u64)
                .to_le_bytes()
                .into_iter()
                .chain(bits.to_le_bytes())
            {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
        }
        plan.sig = h;
    }
    desired
}

/// MONKEY (voxel): optional numeric ray check for the centre and two yards inside the footprint edge.
fn monkey_voxel_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("WOW_VOXEL_PROBE_RAY").is_ok_and(|value| value == "1"))
}

/// MONKEY (voxel): log collision ownership above the carved centre and near its edge, at most ten times.
pub(super) fn monkey_voxel_probe_rays(
    time: Res<Time>,
    spatial: SpatialQuery,
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    volumes: Query<(Entity, &Transform, &MonkeyVoxelData), With<MonkeyVoxelVolume>>,
    mut probe: Local<(f32, u8)>,
) {
    if !monkey_voxel_probe_enabled() || probe.1 >= 10 {
        return;
    }
    probe.0 += time.delta_secs();
    if probe.0 < 2.0 {
        return;
    }
    probe.0 = 0.0;
    let Ok((voxel, tf, data)) = volumes.single() else {
        return;
    };
    let half = super::half_extent_for(data.0.chunk_dims());
    let centre = tf.translation + half;
    let points = [
        ("centre", centre),
        ("edge", centre + Vec3::X * (half.x - 2.0)),
    ];
    for (name, point) in points {
        let origin = point + Vec3::Y * 60.0;
        // The column's tile root: the ray is vertical, so the tile under `point` is the only one
        // whose terrain trimesh it can hit.
        let w = bevy_to_wow(point);
        let (tx, ty) = benilla_formats::world_to_tile(w[0], w[1]);
        let tile_root = streamer.tile_entity((tx as i32, ty as i32));
        let kind_of = |e: Entity| {
            if e == voxel {
                "voxel"
            } else if tile_root == Some(e) {
                "terrain-tile"
            } else {
                "other"
            }
        };
        let filter = SpatialQueryFilter::default();
        let height = crate::terrain_stream::terrain_height_under(&streamer, &adts, point);
        let raw = crate::terrain_stream::terrain_height_under_raw(&streamer, &adts, point);
        match spatial.cast_ray(origin, Dir3::NEG_Y, 160.0, true, &filter) {
            Some(hit) => info!(
                "MONKEY (voxel): probe ray {} hit {} entity {:?} at y={:.3}; \
                 terrain_height_under={:?} (raw ADT {:?})",
                name,
                kind_of(hit.entity),
                hit.entity,
                origin.y - hit.distance,
                height,
                raw
            ),
            None => info!(
                "MONKEY (voxel): probe ray {} hit none; terrain_height_under={:?} (raw ADT {:?})",
                name, height, raw
            ),
        }
        // The first GROUND hit (voxel or this tile's terrain), past any canopy / doodad hull.
        let ground = spatial.cast_ray_predicate(origin, Dir3::NEG_Y, 160.0, true, &filter, &|e| {
            kind_of(e) != "other"
        });
        if let Some(hit) = ground {
            info!(
                "MONKEY (voxel): probe ray {} ground = {} at y={:.3}",
                name,
                kind_of(hit.entity),
                origin.y - hit.distance
            );
        }
    }
    probe.1 += 1;
}

/// MONKEY (voxel): fold cell plans into the tile-level terrain collider plans.
fn tile_plans(cells: &HashMap<CellKey, CellPlan>) -> HashMap<(i32, i32), TilePlan> {
    let mut out: HashMap<(i32, i32), TilePlan> = HashMap::new();
    for (&(tx, ty, _), plan) in cells {
        out.entry((tx, ty))
            .or_default()
            .chunk_holes
            .extend(plan.chunk_holes.iter().copied());
    }
    for plan in out.values_mut() {
        plan.chunk_holes.sort_unstable();
        let mut h = 0xcbf29ce484222325u64;
        for (ci, bits) in &plan.chunk_holes {
            for b in (*ci as u64)
                .to_le_bytes()
                .into_iter()
                .chain(bits.to_le_bytes())
            {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
        }
        plan.sig = h;
    }
    out
}

/// MONKEY (voxel): build one tile collider with extra hole bits applied to the affected chunks.
fn tile_collider_data(adt: &AdtTile, holes: &[(usize, u16)]) -> Option<(Vec<Vec3>, Vec<[u32; 3]>)> {
    collider_data_with_holes(&adt.chunks, holes)
}

/// MONKEY (voxel): make collider indices match the render carve, preserving each source hole bit.
fn collider_data_with_holes(
    source: &[ChunkMesh],
    holes: &[(usize, u16)],
) -> Option<(Vec<Vec3>, Vec<[u32; 3]>)> {
    let mut chunks = source.to_vec();
    for &(idx, extra) in holes {
        if let Some(chunk) = chunks.get_mut(idx) {
            chunk.indices = chunk_indices_holed(chunk.holes | extra);
        }
    }
    crate::terrain_stream::voxel_terrain_collider_data(&chunks)
}

/// Re-mesh one render cell of `adt`, applying `extra` (chunk-index → extra hole bits) to the
/// chunks it names; `extra` empty restores the un-holed cell. `None` if every chunk is a hole (a
/// 32-yd footprint never covers a whole 133-yd cell, so in practice this always builds a mesh).
fn rebuild_cell_mesh(adt: &AdtTile, cell: usize, extra: &[(usize, u16)]) -> Option<Mesh> {
    let (cy, cx) = (cell / 4, cell % 4);
    // Clone only the chunks we hole; the rest are referenced from the asset.
    let mut clones: HashMap<usize, ChunkMesh> = HashMap::new();
    for &(ci, bits) in extra {
        if bits == 0 {
            continue;
        }
        let (row, col) = (ci / 16, ci % 16);
        if row / 4 != cy || col / 4 != cx {
            continue; // a chunk of another cell, ignore here
        }
        if let Some(src) = adt.chunks.get(ci) {
            let mut c = src.clone();
            c.indices = chunk_indices_holed(src.holes | bits);
            clones.insert(ci, c);
        }
    }
    let mut parts: Vec<(&ChunkMesh, &ChunkShading)> = Vec::with_capacity(16);
    for dy in 0..4usize {
        for dx in 0..4usize {
            let ci = (cy * 4 + dy) * 16 + (cx * 4 + dx);
            let Some(shading) = adt.shading.get(ci) else {
                continue;
            };
            if let Some(chunk) = clones.get(&ci).or_else(|| adt.chunks.get(ci)) {
                parts.push((chunk, shading));
            }
        }
    }
    chunks_to_mesh(&parts)
}

/// Swap the cell entity's rendered mesh in place (carve with `extra`, or restore with `&[]`).
/// Returns whether the swap happened (the tile must still be resident to re-read its chunks).
fn swap_cell(
    adts: &Assets<AdtTile>,
    streamer: &TerrainStreamer,
    meshes: &mut Assets<Mesh>,
    key: CellKey,
    extra: &[(usize, u16)],
    mesh3d: &Mesh3d,
) -> bool {
    let (tx, ty, cell) = key;
    let Some(handle) = streamer.tile_handle((tx, ty)) else {
        return false;
    };
    let Some(adt) = adts.get(&handle) else {
        return false;
    };
    let Some(mesh) = rebuild_cell_mesh(adt, cell, extra) else {
        return false;
    };
    // The vertices are unchanged (a hole only drops triangles), so the entity's cull Aabb stays
    // valid — no need to touch it.
    let _ = meshes.insert(&mesh3d.0, mesh);
    true
}

/// Carve the ADT terrain under the voxel build site, and keep the carve in step with the region:
/// recompute the plan when the footprint moves, carve newly-resident cells, restore cells the
/// footprint has left (or whose covered blocks changed), and re-carve cells the streamer respawned.
#[allow(clippy::type_complexity)]
pub(super) fn monkey_voxel_hide_terrain(
    mut commands: Commands,
    mut streamer: ResMut<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut state: ResMut<MonkeyVoxelHideState>,
    time: Res<Time>, // MONKEY (voxel): re-plan until resident
    volume: Query<(&Transform, &MonkeyVoxelData), With<MonkeyVoxelVolume>>,
    cells: Query<(Entity, &Mesh3d, &Aabb), With<MeshMaterial3d<TerrainMaterial>>>,
    settled: Query<(), (With<Collider>, Without<PendingCollider>)>, // MONKEY (voxel)
) {
    // Dev A/B switch (`WOW_VOXEL_NO_HIDE=1`): leave the real ground showing under the patch, as the
    // pre-hiding behaviour, so a capture can compare carved vs un-carved. Restores first so a live
    // toggle reverts cleanly.
    let no_hide = monkey_voxel_hide_disabled();

    // No volume (or hiding disabled): restore every still-living carved cell, then forget the plan.
    let volume = if no_hide {
        Err(())
    } else {
        volume.single().map_err(|_| ())
    };
    let Ok((tf, data)) = volume else {
        if state.sig.is_some() {
            streamer.set_voxel_holes(HashMap::new());
        }
        if !state.applied.is_empty() {
            let living: HashMap<Entity, Mesh3d> =
                cells.iter().map(|(e, m, _)| (e, m.clone())).collect();
            for (e, (key, _)) in std::mem::take(&mut state.applied) {
                if let Some(mesh3d) = living.get(&e) {
                    swap_cell(&adts, &streamer, &mut meshes, key, &[], mesh3d);
                }
            }
        }
        state.desired.clear();
        state.desired_tiles.clear();
        state.sig = None;
        state.incomplete = false;
        // An empty plan restores every carved tile collider to its original ADT trimesh.
        sync_tile_colliders(&mut commands, &streamer, &adts, &settled, &mut state);
        return;
    };

    // Footprint in WoW yards: the volume centre ± the horizontal half-extent. Bevy X ↔ WoW Y, Bevy
    // Z ↔ WoW X, so the WoW X span is half.z and the WoW Y span is half.x.
    let half = super::half_extent_for(data.0.chunk_dims());
    let centre = tf.translation + half;
    let wow = bevy_to_wow(centre);
    let (fx0, fx1) = (wow[0] - half.z, wow[0] + half.z);
    let (fy0, fy1) = (wow[1] - half.x, wow[1] + half.x);

    // Recompute the plan only when the footprint meaningfully moves (quantised to 0.5 yd).
    let q = |v: f32| (v * 2.0).round() as i64;
    let sig = (q(wow[0]), q(wow[1]), data.0.chunk_dims());
    // MONKEY (voxel): re-plan until resident (the plan is XZ-only, so a re-seed's centre.y change
    // needs no re-plan).
    let mut retry = false;
    if state.incomplete && state.sig == Some(sig) {
        state.replan_timer += time.delta_secs();
        if state.replan_timer >= 0.5 {
            state.replan_timer = 0.0;
            retry = true;
        }
    }
    if state.sig != Some(sig) || retry {
        let resident = |x: f32, y: f32| {
            let (tx, ty, _) = cell_key_at(x, y);
            streamer
                .tile_handle((tx, ty))
                .is_some_and(|h| adts.get(&h).is_some())
        };
        let all_resident = [(fx0, fy0), (fx0, fy1), (fx1, fy0), (fx1, fy1)]
            .iter()
            .all(|&(x, y)| resident(x, y));
        let was_retry = retry && state.sig == Some(sig);
        state.sig = Some(sig);
        state.desired = compute_desired(fx0, fx1, fy0, fy1);
        state.desired_tiles = tile_plans(&state.desired);
        // The height queries' carve mask, published only when the plan is rebuilt (not every
        // frame, so the streamer resource isn't marked changed each frame).
        streamer.set_voxel_holes(
            state
                .desired_tiles
                .iter()
                .map(|(tile, plan)| (*tile, plan.chunk_holes.iter().copied().collect()))
                .collect(),
        );
        state.incomplete = !all_resident || state.desired.is_empty();
        if was_retry && state.incomplete {
            // still waiting on the stream: stay quiet (no per-0.5 s log spam)
        } else {
            let plan: Vec<_> = state
                .desired
                .iter()
                .map(|(k, p)| (*k, p.chunk_holes.clone()))
                .collect();
            info!(
            "MONKEY (voxel): hide plan — footprint WoW X[{:.1},{:.1}] Y[{:.1},{:.1}] covers {} \
             render cell(s): {:?}",
            fx0,
            fx1,
            fy0,
            fy1,
            state.desired.len(),
            plan,
        );
        }
    }

    // Index the resident terrain cells by their render-cell key.
    let mut by_key: HashMap<CellKey, (Entity, Mesh3d)> = HashMap::new();
    let mut living: HashSet<Entity> = HashSet::new();
    for (e, mesh3d, aabb) in &cells {
        living.insert(e);
        by_key.insert(cell_key_of(aabb.center), (e, mesh3d.clone()));
    }

    // Restore cells no longer carved the way the plan wants (moved off, bits changed), and forget
    // carved cells the streamer despawned (their mesh asset went with them).
    let snapshot: Vec<(Entity, CellKey, u64)> = state
        .applied
        .iter()
        .map(|(e, (k, s))| (*e, *k, *s))
        .collect();
    for (e, key, applied_sig) in snapshot {
        if !living.contains(&e) {
            state.applied.remove(&e);
            continue;
        }
        if state.desired.get(&key).map(|p| p.sig) != Some(applied_sig) {
            if let Some((_, mesh3d)) = by_key.get(&key) {
                swap_cell(&adts, &streamer, &mut meshes, key, &[], mesh3d);
            }
            state.applied.remove(&e);
        }
    }

    // Carve desired cells that aren't already carved on a live entity for the current plan.
    let desired_keys: Vec<CellKey> = state.desired.keys().copied().collect();
    for key in desired_keys {
        let plan_sig = state.desired[&key].sig;
        let already = state
            .applied
            .iter()
            .any(|(e, (k, s))| *k == key && *s == plan_sig && living.contains(e));
        if already {
            continue;
        }
        let Some((e, mesh3d)) = by_key.get(&key).map(|(e, m)| (*e, m.clone())) else {
            continue; // the cell hasn't streamed in yet; caught on a later frame
        };
        if swap_cell(
            &adts,
            &streamer,
            &mut meshes,
            key,
            &state.desired[&key].chunk_holes,
            &mesh3d,
        ) {
            state.applied.insert(e, (key, plan_sig));
            // `debug!`, not `info!`: this re-fires whenever a tile streams out and back in and the
            // cell respawns, so it must not spam the player's log on every pass near a build site.
            debug!(
                "MONKEY (voxel): carved terrain cell {key:?} (entity {e:?}) — the real ADT ground \
                 under the build site is now holed."
            );
        }
    }

    sync_tile_colliders(&mut commands, &streamer, &adts, &settled, &mut state);
}

/// MONKEY (voxel): keep each tile's one static terrain collider in step with the carve plan — holed
/// with the render cells' MCNK bits while the footprint covers it, the original ADT trimesh again
/// once it no longer does. Rebuilds only when a tile's (root entity, plan signature) changes.
///
/// No frame without ground: the replacement is a [`PendingCollider`] on the SAME root, and
/// `finish_colliders` swaps the `Collider` component in place when the off-thread build lands (and
/// bumps `ColliderEpoch`), so the old trimesh stays live until then. A FRESH root (tile streamed
/// back in) is left alone until its own original build has attached (`Collider`, no
/// `PendingCollider`) — inserting ours while the streamer's spawn/insert is still queued or in
/// flight would race it (whichever `PendingCollider` insert applies last wins).
fn sync_tile_colliders(
    commands: &mut Commands,
    streamer: &TerrainStreamer,
    adts: &Assets<AdtTile>,
    settled: &Query<(), (With<Collider>, Without<PendingCollider>)>,
    state: &mut MonkeyVoxelHideState,
) {
    let mut tiles: HashSet<(i32, i32)> = state.applied_tiles.keys().copied().collect();
    tiles.extend(state.desired_tiles.keys().copied());
    for tile in tiles {
        let Some(root) = streamer.tile_entity(tile) else {
            state.applied_tiles.remove(&tile); // streamed out; a fresh root is carved later
            continue;
        };
        let plan = state.desired_tiles.get(&tile);
        let want_sig = plan.map_or(0, |p| p.sig);
        let applied = state.applied_tiles.get(&tile).copied();
        if applied == Some((root, want_sig)) {
            continue;
        }
        let fresh_root = applied.is_none_or(|(r, _)| r != root);
        if fresh_root {
            if !settled.contains(root) {
                continue; // the streamer's original build hasn't attached yet
            }
            if want_sig == 0 {
                // A fresh root already carries the original trimesh: nothing to restore.
                state.applied_tiles.insert(tile, (root, 0));
                continue;
            }
        }
        let Some(handle) = streamer.tile_handle(tile) else {
            continue;
        };
        let Some(adt) = adts.get(&handle) else {
            continue;
        };
        let data = if let Some(plan) = plan {
            tile_collider_data(adt, &plan.chunk_holes)
        } else {
            crate::terrain_stream::voxel_terrain_collider_data(&adt.chunks)
        };
        let Some((verts, tris)) = data else { continue };
        let ntris = tris.len();
        let blocks = plan.map_or(0, |p| {
            p.chunk_holes
                .iter()
                .map(|(_, bits)| bits.count_ones())
                .sum()
        });
        commands.entity(root).insert(PendingCollider::new(
            build_collider_task(verts, tris),
            None,
            true,
        ));
        info!(
            "MONKEY (voxel): tile ({},{}) collider rebuilt with {} extra hole block(s) ({} tris)",
            tile.0, tile.1, blocks, ntris
        );
        state.applied_tiles.insert(tile, (root, want_sig));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_chunk() -> ChunkMesh {
        let step = benilla_formats::CHUNK_SIZE / 8.0;
        let mut positions = Vec::with_capacity(145);
        for row in 0..9 {
            for col in 0..9 {
                positions.push([100.0 - row as f32 * step, 100.0 - col as f32 * step, 0.0]);
            }
            if row < 8 {
                for col in 0..8 {
                    positions.push([
                        100.0 - (row as f32 + 0.5) * step,
                        100.0 - (col as f32 + 0.5) * step,
                        0.0,
                    ]);
                }
            }
        }
        ChunkMesh {
            positions,
            normals: Vec::new(),
            uvs: Vec::new(),
            indices: chunk_indices_holed(0),
            holes: 0,
            base_texture: None,
            layer_textures: Vec::new(),
            layer_effect_ids: Vec::new(),
            alpha_map: None,
            shadow: None,
            pred_tex: [0; 64],
            no_effect_doodad: [false; 64],
            index_x: 0,
            index_y: 0,
            area_id: 0,
            impassable: false,
            liquids: Vec::new(),
        }
    }

    #[test]
    fn a_hole_block_must_be_fully_inside_the_footprint() {
        assert!(
            !fully_inside(0.0, 8.0, 1.0, 10.0, 1.0e-3),
            "edge-straddling block must remain solid"
        );
        assert!(
            fully_inside(2.0, 8.0, 1.0, 10.0, 1.0e-3),
            "interior block is carved"
        );
    }

    /// MONKEY (voxel): the planner itself — a footprint that cuts through the first block row/col
    /// of global chunk (512, 512) (whose NW corner is the map centre, WoW (0, 0)) and fully contains
    /// the second: only block (1, 1) is holed. The old "block centre inside" rule would also have
    /// holed the straddling row/col 0 blocks.
    #[test]
    fn planner_holes_only_blocks_fully_inside_the_footprint() {
        let b = block_size();
        // X/Y spans: block 0 = [-b, 0] straddles the -0.5 edge, block 1 = [-2b, -b] is inside,
        // block 2 = [-3b, -2b] straddles the -2b-1 edge.
        let desired = compute_desired(-2.0 * b - 1.0, -0.5, -2.0 * b - 1.0, -0.5);
        let all: Vec<_> = desired
            .iter()
            .flat_map(|(k, p)| p.chunk_holes.iter().map(move |h| (*k, *h)))
            .collect();
        assert_eq!(all, vec![((32, 32, 0), (0, 1u16 << 5))], "plan {all:?}");
        // And the tile plan carries the same single block.
        let tiles = tile_plans(&desired);
        assert_eq!(tiles[&(32, 32)].chunk_holes, vec![(0, 1u16 << 5)]);
        assert_ne!(
            tiles[&(32, 32)].sig,
            0,
            "a real plan's signature never reads as 'restored'"
        );
    }

    #[test]
    fn collider_hole_drops_sixteen_fan_triangles_per_block() {
        let chunk = flat_chunk();
        let (_, base) = collider_data_with_holes(std::slice::from_ref(&chunk), &[]).unwrap();
        let (_, holed) = collider_data_with_holes(&[chunk], &[(0, 1)]).unwrap();
        assert_eq!(base.len() - holed.len(), 16);
    }

    /// The render-cell key round-trips Bevy-centre → WoW → (tile, cell) consistently with the
    /// footprint planner's own `cell_key_at`, for a handful of tile/cell centres.
    #[test]
    fn cell_key_round_trips_through_bevy_centre() {
        let cs = benilla_formats::CHUNK_SIZE;
        let mc = super::map_center();
        for &(tx, ty, row, col) in &[
            (32i32, 49i32, 5usize, 10usize),
            (0, 0, 0, 0),
            (63, 63, 15, 15),
        ] {
            // The WoW centre of chunk (tile_y row, tile_x col) → its render cell.
            let gx = ty * 16 + row as i32;
            let gy = tx * 16 + col as i32;
            let wow_x = mc - (gx as f32 + 0.5) * cs;
            let wow_y = mc - (gy as f32 + 0.5) * cs;
            let expect = (
                (tx, ty, (row / 4) * 4 + (col / 4)),
                cell_key_at(wow_x, wow_y),
            );
            assert_eq!(expect.0, expect.1, "cell_key_at mismatch");
            // Bevy horizontal centre = wow_to_bevy: (−wow_y, _, −wow_x).
            let via_aabb = cell_key_of(Vec3A::new(-wow_y, 123.0, -wow_x));
            assert_eq!(via_aabb, expect.0, "aabb centre → cell key");
        }
    }
}

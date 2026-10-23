//! MONKEY (voxel): hide the real ADT terrain under the world voxel layer, so the voxel surface
//! REPLACES the ground wherever it is active — a hole dug into the voxel shows the voxel interior,
//! not the old heightmap ground poking through at its original height.
//!
//! The carve is the UNION of the layer's surface columns ([`VoxelCarve`], CONTRACT §1): each 16-yd
//! column a rectangle, inset 0.5 yd only along the union's OUTER boundary. The terrain renderer
//! draws one mesh per CELL = 4×4 MCNK chunks (`terrain_stream::furnish`); this module re-meshes the
//! cells the union reaches with every chunk CLIPPED to it (an exact Sutherland–Hodgman cut, see
//! `terrain_stream/voxel_clip.rs`). Each cell / tile carries the signature of the rectangles that
//! reach IT, so adding a column only re-clips the cells and tile colliders that column touches. It
//! is reversible: a cell is restored (re-meshed from the original chunks) when the union leaves it,
//! and re-clipped when a tile streams out and back in (the furnisher respawns the cell fresh).
//!
//! Dig-through: the tile's one static terrain collider is rebuilt from the same clipped chunks
//! ([`sync_tile_colliders`]), and the carve is published to `TerrainStreamer` so the MCNK HEIGHT
//! query answers `None` strictly inside it (area / ground effect / shade keep the un-clipped data;
//! the voxel seed reads the raw path) and the torch casters clip the ground the same way.
//!
//! Everything here is gated behind the crate's `voxel` cargo feature, like the rest of `voxel`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use avian3d::prelude::Collider;
use bevy::camera::primitives::Aabb;
use bevy::math::Vec3A;
use bevy::prelude::*;

use super::MonkeyVoxelWorld;
use crate::terrain_stream::voxel_clip::VoxelCarve;
use crate::terrain_stream::{PendingCollider, TerrainStreamer};
use benilla_assets::coords::wow_to_bevy;
use benilla_assets::materials::TerrainMaterial;
use benilla_assets::{chunks_to_mesh, AdtTile, ChunkShading};
use benilla_formats::ChunkMesh;

/// MONKEY (voxel): how far INSIDE the union's outer edge the real ground still reaches (yd). The
/// voxel surface must always overlap the remaining ADT, or the sky shows through a slit along the
/// edge. Surface-nets puts a chunk's outermost vertex within ±0.25 yd of its owned boundary (the
/// mesh window starts one sample before it), so one full voxel (0.5 yd) is the hard bound that is
/// always covered; the underlap is hidden by the seed's `SEED_LIFT` (0.15 yd).
pub(super) const CARVE_INSET: f32 = benilla_voxel::VOXEL_SIZE;

/// `(tile_x, tile_y, cell_idx)`: a render cell (4×4 MCNK chunks) of one tile, the unit the
/// furnisher spawns one `Mesh3d` + `MeshMaterial3d<TerrainMaterial>` per.
type CellKey = (i32, i32, usize);

/// The terrain-hiding state: the carve the plan was built for, the per-cell / per-tile plan (each
/// keyed by the signature of the rectangles reaching it) and which cell entities / tile roots are
/// currently clipped (so they are restored when the union leaves them and re-clipped after a
/// stream-out/in respawns them).
#[derive(Resource, Default)]
pub(super) struct MonkeyVoxelHideState {
    /// Signature of the carve the plan was built for (0 = none).
    sig: u64,
    carve: Option<Arc<VoxelCarve>>,
    /// Render cells the union reaches → the signature of the rectangles reaching that cell.
    desired: HashMap<CellKey, u64>,
    /// Tiles whose collider the union reaches → the signature of the rectangles reaching it.
    desired_tiles: HashMap<(i32, i32), u64>,
    /// Clipped cells → (entity, signature) they were clipped for.
    applied: HashMap<CellKey, (Entity, u64)>,
    /// Collider root → (entity, signature; 0 = original), also detects streamed roots.
    applied_tiles: HashMap<(i32, i32), (Entity, u64)>,
}

/// `WOW_VOXEL_NO_HIDE=1`: keep the real ADT ground under the voxel layer (the pre-hiding look), a
/// dev A/B switch read once.
fn monkey_voxel_hide_disabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WOW_VOXEL_NO_HIDE").is_some())
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

/// The WoW XY box `([x_lo, y_lo], [x_hi, y_hi])` of a render cell.
fn cell_box(key: CellKey) -> ([f32; 2], [f32; 2]) {
    let cs = benilla_formats::CHUNK_SIZE;
    let mc = map_center();
    let (tx, ty, cell) = key;
    let gx = ty * 16 + (cell / 4) as i32 * 4;
    let gy = tx * 16 + (cell % 4) as i32 * 4;
    (
        [mc - (gx + 4) as f32 * cs, mc - (gy + 4) as f32 * cs],
        [mc - gx as f32 * cs, mc - gy as f32 * cs],
    )
}

/// The WoW XY box of a tile.
fn tile_box(tile: (i32, i32)) -> ([f32; 2], [f32; 2]) {
    let ts = benilla_formats::TILE_SIZE;
    let mc = map_center();
    (
        [mc - (tile.1 + 1) as f32 * ts, mc - (tile.0 + 1) as f32 * ts],
        [mc - tile.1 as f32 * ts, mc - tile.0 as f32 * ts],
    )
}

/// The render cells the union reaches, each tagged with the signature of the rectangles reaching
/// it. Pure geometry: cells of tiles not yet resident keep their plan and are clipped once they
/// stream in.
fn compute_desired(carve: &VoxelCarve) -> HashMap<CellKey, u64> {
    let cs = benilla_formats::CHUNK_SIZE;
    let mc = map_center();
    let mut cells: HashSet<CellKey> = HashSet::new();
    for r in carve.rects() {
        let gx_lo = (((mc - r.x1) / cs).floor() as i32).clamp(0, 1023);
        let gx_hi = (((mc - r.x0) / cs).floor() as i32).clamp(0, 1023);
        let gy_lo = (((mc - r.y1) / cs).floor() as i32).clamp(0, 1023);
        let gy_hi = (((mc - r.y0) / cs).floor() as i32).clamp(0, 1023);
        for gx in gx_lo..=gx_hi {
            for gy in gy_lo..=gy_hi {
                let (tx, ty) = (gy / 16, gx / 16);
                let (row, col) = ((gx % 16) as usize, (gy % 16) as usize);
                cells.insert((tx, ty, (row / 4) * 4 + (col / 4)));
            }
        }
    }
    cells
        .into_iter()
        .filter_map(|k| {
            let (lo, hi) = cell_box(k);
            let s = carve.sig_for_box(lo, hi);
            (s != 0).then_some((k, s))
        })
        .collect()
}

/// The tile-level collider plan: every tile a planned cell lies in, tagged by the rectangles
/// reaching the whole tile.
fn tile_plans(cells: &HashMap<CellKey, u64>, carve: &VoxelCarve) -> HashMap<(i32, i32), u64> {
    cells
        .keys()
        .map(|&(tx, ty, _)| (tx, ty))
        .collect::<HashSet<_>>()
        .into_iter()
        .filter_map(|t| {
            let (lo, hi) = tile_box(t);
            let s = carve.sig_for_box(lo, hi);
            (s != 0).then_some((t, s))
        })
        .collect()
}

/// `chunk` clipped to the carve when it reaches it, else borrowed as-is.
fn clipped<'a>(chunk: &'a ChunkMesh, carve: Option<&VoxelCarve>) -> Cow<'a, ChunkMesh> {
    match carve.and_then(|c| c.clip_chunk(chunk)) {
        Some(c) => Cow::Owned(c),
        None => Cow::Borrowed(chunk),
    }
}

/// One tile's terrain collider data from its chunks clipped to `carve` (`None` = the original
/// trimesh) — the same weld as `terrain_stream`'s `terrain_collider_data`: draw space, each chunk's
/// indices rebased onto the running vertex count.
fn tile_collider_data(
    chunks: &[ChunkMesh],
    carve: Option<&VoxelCarve>,
) -> Option<(Vec<Vec3>, Vec<[u32; 3]>)> {
    let mut verts: Vec<Vec3> = Vec::new();
    let mut tris: Vec<[u32; 3]> = Vec::new();
    for chunk in chunks {
        let chunk = clipped(chunk, carve);
        let base = verts.len() as u32;
        verts.extend(chunk.positions.iter().map(|p| wow_to_bevy(*p)));
        tris.extend(
            chunk
                .indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| [base + c[0], base + c[1], base + c[2]]),
        );
    }
    (!tris.is_empty()).then_some((verts, tris))
}

/// Re-mesh one render cell of `adt` with its chunks clipped to `carve`; `None` restores the
/// original cell. `None` result if every chunk is a hole.
fn rebuild_cell_mesh(adt: &AdtTile, cell: usize, carve: Option<&VoxelCarve>) -> Option<Mesh> {
    let (cy, cx) = (cell / 4, cell % 4);
    let mut parts: Vec<(Cow<ChunkMesh>, &ChunkShading)> = Vec::with_capacity(16);
    for dy in 0..4usize {
        for dx in 0..4usize {
            let ci = (cy * 4 + dy) * 16 + (cx * 4 + dx);
            let (Some(shading), Some(chunk)) = (adt.shading.get(ci), adt.chunks.get(ci)) else {
                continue;
            };
            // MONKEY (voxel fix): drawn with the curtain under the cut edges (CARVE_SKIRT).
            let drawn = match carve.and_then(|c| c.clip_chunk_drawn(chunk)) {
                Some(c) => Cow::Owned(c),
                None => Cow::Borrowed(chunk),
            };
            parts.push((drawn, shading));
        }
    }
    let refs: Vec<(&ChunkMesh, &ChunkShading)> = parts.iter().map(|(c, s)| (&**c, *s)).collect();
    chunks_to_mesh(&refs)
}

/// Swap the cell entity's rendered mesh in place (clip with `carve`, or restore with `None`).
/// Returns whether the swap happened (the tile must still be resident to re-read its chunks).
fn swap_cell(
    adts: &Assets<AdtTile>,
    streamer: &TerrainStreamer,
    meshes: &mut Assets<Mesh>,
    key: CellKey,
    carve: Option<&VoxelCarve>,
    mesh3d: &Mesh3d,
) -> bool {
    let (tx, ty, cell) = key;
    let Some(handle) = streamer.tile_handle((tx, ty)) else {
        return false;
    };
    let Some(adt) = adts.get(&handle) else {
        return false;
    };
    let Some(mesh) = rebuild_cell_mesh(adt, cell, carve) else {
        return false;
    };
    // Clipping only removes area (every appended vertex lies on an original triangle); only the
    // cut-edge curtain (`CARVE_SKIRT`, 0.3 yd) may reach under the original bounds' floor, and it
    // is only seen together with the ground above it, so the entity's cull Aabb stays valid.
    let _ = meshes.insert(&mesh3d.0, mesh);
    true
}

/// Carve the ADT terrain under the voxel layer's surface columns and keep it in step: re-plan when
/// the union changes, clip newly-resident cells, restore cells the union has left (or whose
/// rectangles changed), and re-clip cells the streamer respawned.
#[allow(clippy::type_complexity)]
pub(super) fn monkey_voxel_hide_terrain(
    mut commands: Commands,
    mut streamer: ResMut<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut state: ResMut<MonkeyVoxelHideState>,
    world: Res<MonkeyVoxelWorld>,
    cells: Query<(Entity, &Mesh3d, &Aabb), With<MeshMaterial3d<TerrainMaterial>>>,
    settled: Query<(), (With<Collider>, Without<PendingCollider>)>,
) {
    // Dev A/B switch (`WOW_VOXEL_NO_HIDE=1`): leave the real ground showing under the layer.
    let carve = if monkey_voxel_hide_disabled() {
        None
    } else {
        world.carve()
    };
    let sig = carve.as_ref().map_or(0, |c| c.sig());
    if sig == 0 && state.sig == 0 && state.applied.is_empty() && state.applied_tiles.is_empty() {
        return; // nothing carved, nothing to restore
    }

    if sig != state.sig {
        state.sig = sig;
        state.carve = carve.clone();
        match &carve {
            Some(c) => {
                state.desired = compute_desired(c);
                state.desired_tiles = tile_plans(&state.desired, c);
                info!(
                    "MONKEY (voxel): hide plan — {} surface column(s) carved (outer inset \
                     {CARVE_INSET} yd) across {} render cell(s) / {} tile(s)",
                    c.columns(),
                    state.desired.len(),
                    state.desired_tiles.len(),
                );
            }
            None => {
                state.desired.clear();
                state.desired_tiles.clear();
            }
        }
        // The height queries' / torch casters' carve, published only when the plan changes (not
        // every frame, so the streamer resource isn't marked changed each frame).
        streamer.set_voxel_carve(carve);
    }
    let carve = state.carve.clone();
    let t_cells = std::time::Instant::now();
    let mut swapped = 0usize;

    // Index the resident terrain cells by their render-cell key.
    let mut by_key: HashMap<CellKey, (Entity, Mesh3d)> = HashMap::new();
    for (e, mesh3d, aabb) in &cells {
        by_key.insert(cell_key_of(aabb.center), (e, mesh3d.clone()));
    }

    // Restore cells no longer clipped the way the plan wants, and forget clipped cells the
    // streamer despawned (their mesh asset went with them).
    let snapshot: Vec<(CellKey, Entity, u64)> =
        state.applied.iter().map(|(k, (e, s))| (*k, *e, *s)).collect();
    for (key, e, applied_sig) in snapshot {
        let live = by_key.get(&key).filter(|(le, _)| *le == e);
        let Some((_, mesh3d)) = live else {
            state.applied.remove(&key);
            continue;
        };
        if state.desired.get(&key).copied() != Some(applied_sig) {
            if state.desired.get(&key).is_none()
                && swap_cell(&adts, &streamer, &mut meshes, key, None, mesh3d)
            {
                debug!("MONKEY (voxel): restored terrain cell {key:?} (the carve left it).");
            }
            state.applied.remove(&key);
        }
    }

    // Clip desired cells that aren't already clipped on a live entity for the current plan.
    let desired: Vec<(CellKey, u64)> = state.desired.iter().map(|(k, s)| (*k, *s)).collect();
    for (key, plan_sig) in desired {
        let Some((e, mesh3d)) = by_key.get(&key).map(|(e, m)| (*e, m.clone())) else {
            continue; // the cell hasn't streamed in yet; caught on a later frame
        };
        if state.applied.get(&key) == Some(&(e, plan_sig)) {
            continue;
        }
        if swap_cell(&adts, &streamer, &mut meshes, key, carve.as_deref(), &mesh3d) {
            swapped += 1;
            state.applied.insert(key, (e, plan_sig));
            // `debug!`: this re-fires whenever a tile streams out and back in.
            debug!("MONKEY (voxel): clipped terrain cell {key:?} (entity {e:?}).");
        }
    }

    let cells_ms = t_cells.elapsed().as_secs_f32() * 1000.0;
    let t_col = std::time::Instant::now();
    let rebuilt = sync_tile_colliders(&mut commands, &streamer, &adts, &settled, &mut state);
    if swapped > 0 || rebuilt > 0 {
        // MONKEY (voxel): the main-thread cost of a re-clip (the perf lane's hitch accounting).
        info!(
            "MONKEY (voxel): hide re-clip — {swapped} render cell(s) in {cells_ms:.2} ms, {rebuilt} \
             tile collider(s) queued in {:.2} ms",
            t_col.elapsed().as_secs_f32() * 1000.0
        );
    }
}

/// MONKEY (voxel): the collider-relevant part of a chunk (geometry only — the alpha / shadow maps
/// stay behind), so a tile's clip + weld can run on the async pool.
fn light_chunk(c: &ChunkMesh) -> ChunkMesh {
    ChunkMesh {
        positions: c.positions.clone(),
        normals: c.normals.clone(),
        uvs: c.uvs.clone(),
        indices: c.indices.clone(),
        holes: c.holes,
        base_texture: None,
        layer_textures: Vec::new(),
        layer_effect_ids: Vec::new(),
        alpha_map: None,
        shadow: None,
        pred_tex: c.pred_tex,
        no_effect_doodad: c.no_effect_doodad,
        index_x: c.index_x,
        index_y: c.index_y,
        area_id: c.area_id,
        impassable: c.impassable,
        liquids: Vec::new(),
    }
}

/// Keep each tile's one static terrain collider in step with the carve plan — built from the
/// clipped chunks while the union reaches it, the original ADT trimesh again once it no longer
/// does. Rebuilds only when a tile's (root entity, plan signature) changes.
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
) -> usize {
    let mut rebuilt = 0;
    let mut tiles: HashSet<(i32, i32)> = state.applied_tiles.keys().copied().collect();
    tiles.extend(state.desired_tiles.keys().copied());
    for tile in tiles {
        let Some(root) = streamer.tile_entity(tile) else {
            state.applied_tiles.remove(&tile); // streamed out; a fresh root is clipped later
            continue;
        };
        let want_sig = state.desired_tiles.get(&tile).copied().unwrap_or(0);
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
        let carve: Option<Arc<VoxelCarve>> = if want_sig != 0 {
            state.carve.clone()
        } else {
            None
        };
        if adt.chunks.iter().all(|c| c.indices.len() < 3) {
            continue;
        }
        // MONKEY (voxel): the clip + weld + parry build all run on the async pool (the clip of a
        // whole tile measured several ms on the frame); the old trimesh stays live until then.
        let chunks: Vec<ChunkMesh> = adt.chunks.iter().map(light_chunk).collect();
        let clipped_tile = carve.is_some();
        let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            let (verts, tris) = tile_collider_data(&chunks, carve.as_deref())
                .or_else(|| tile_collider_data(&chunks, None))
                .unwrap_or_default();
            Collider::trimesh(verts, tris)
        });
        commands
            .entity(root)
            .insert(PendingCollider::new(task, None, true));
        rebuilt += 1;
        if clipped_tile {
            info!(
                "MONKEY (voxel): tile ({},{}) collider rebuilding with the ground clipped to the \
                 carve (off-thread)",
                tile.0, tile.1
            );
        } else {
            info!(
                "MONKEY (voxel): tile ({},{}) collider restoring to the original ADT trimesh \
                 (off-thread)",
                tile.0, tile.1
            );
        }
        state.applied_tiles.insert(tile, (root, want_sig));
    }
    rebuilt
}

#[cfg(test)]
mod tests {
    use super::*;
    use benilla_formats::chunk_indices_holed;

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

    /// The planner — two adjacent columns just south-east of the map centre (WoW (0, 0) is global
    /// chunk (512, 512)'s NW corner) plan exactly render cell 0 of tile (32, 32), tagged non-zero,
    /// and fold into that one tile. A column far away does not change the cell's signature.
    #[test]
    fn planner_covers_the_cells_the_union_reaches() {
        let carve = VoxelCarve::from_columns([(-2, -2), (-2, -1)], 16.0, CARVE_INSET);
        let desired = compute_desired(&carve);
        let mut keys: Vec<_> = desired.keys().copied().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec![(32, 32, 0)], "plan {keys:?}");
        assert!(desired.values().all(|&s| s != 0));
        let tiles = tile_plans(&desired, &carve);
        assert_eq!(tiles.len(), 1);
        assert!(tiles.contains_key(&(32, 32)));
        let far = VoxelCarve::from_columns([(-2, -2), (-2, -1), (-300, -300)], 16.0, CARVE_INSET);
        assert_eq!(compute_desired(&far).get(&(32, 32, 0)), desired.get(&(32, 32, 0)));
    }

    #[test]
    fn collider_is_clipped_and_restores_without_a_carve() {
        let chunk = flat_chunk();
        let (_, base) = tile_collider_data(std::slice::from_ref(&chunk), None).unwrap();
        assert_eq!(base.len(), 256, "an unclipped chunk welds all its fan triangles");
        // Columns (5,5) and (4,5): X [80, 96] ∪ [64, 80], Y [80, 96].
        let carve = VoxelCarve::from_columns([(5, 5), (4, 5)], 16.0, CARVE_INSET);
        let (verts, clipped) =
            tile_collider_data(std::slice::from_ref(&chunk), Some(&carve)).unwrap();
        assert_ne!(clipped.len(), base.len());
        // No collider triangle's centroid lies inside the union (WoW x = −bevy z, y = −bevy x).
        for t in &clipped {
            let c = (verts[t[0] as usize] + verts[t[1] as usize] + verts[t[2] as usize]) / 3.0;
            assert!(!carve.contains_strict(-c.z, -c.x), "collider tri inside the carve");
        }
    }

    /// The render-cell key round-trips Bevy-centre → WoW → (tile, cell) consistently with the
    /// planner's own `cell_key_at` and `cell_box`, for a handful of tile/cell centres.
    #[test]
    fn cell_key_round_trips_through_bevy_centre() {
        let cs = benilla_formats::CHUNK_SIZE;
        let mc = super::map_center();
        for &(tx, ty, row, col) in &[
            (32i32, 49i32, 5usize, 10usize),
            (0, 0, 0, 0),
            (63, 63, 15, 15),
        ] {
            let gx = ty * 16 + row as i32;
            let gy = tx * 16 + col as i32;
            let wow_x = mc - (gx as f32 + 0.5) * cs;
            let wow_y = mc - (gy as f32 + 0.5) * cs;
            let expect = (tx, ty, (row / 4) * 4 + (col / 4));
            assert_eq!(expect, cell_key_at(wow_x, wow_y), "cell_key_at mismatch");
            let via_aabb = cell_key_of(Vec3A::new(-wow_y, 123.0, -wow_x));
            assert_eq!(via_aabb, expect, "aabb centre → cell key");
            let (lo, hi) = cell_box(expect);
            assert!(lo[0] < wow_x && wow_x < hi[0] && lo[1] < wow_y && wow_y < hi[1]);
            let (tlo, thi) = tile_box((tx, ty));
            assert!(tlo[0] <= lo[0] && hi[0] <= thi[0] && tlo[1] <= lo[1] && hi[1] <= thi[1]);
        }
    }
}

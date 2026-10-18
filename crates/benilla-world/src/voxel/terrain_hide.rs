//! MONKEY (voxel): hide the real ADT terrain under a voxel build site, so a region placed on the
//! real ground REPLACES it — a hole dug into the voxel shows the voxel interior (dark walls going
//! down), not the old flat heightmap ground poking through at its original height.
//!
//! The terrain renderer draws one mesh per CELL = 4×4 MCNK chunks (`terrain_stream::furnish`). This
//! module finds the cells the voxel footprint overlaps and re-meshes them with every chunk the
//! footprint reaches CLIPPED to it ([`clip_chunk_outside_rect`]): all ground strictly inside the
//! carve rectangle — the footprint inset by [`CARVE_INSET`] — is dropped, everything outside is
//! kept exactly (an exact cut, not the 8.33-yd MCNK hole blocks the first lane used, which left an
//! un-carved rim of up to a block per side where a dig was covered by the real ground). It is
//! reversible: a cell is restored (re-meshed from the original chunks) when the region moves off it
//! or is removed, and re-clipped when a tile streams out and back in (the furnisher respawns the
//! cell fresh/visible).
//!
//! Dig-through: the tile's one static terrain collider is rebuilt from the same clipped chunks
//! ([`sync_tile_colliders`]), so a pit dug in the voxel is walkable down to the voxel's own floor,
//! and the carve rectangle is published to `TerrainStreamer` so the MCNK HEIGHT query answers
//! `None` strictly inside it (area / ground effect / shade keep the un-clipped data; the voxel seed
//! samplers read the raw path) and the torch casters clip the ground the same way.
//!
//! Everything here is gated behind the crate's `voxel` cargo feature, like the rest of `voxel`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use avian3d::prelude::{Collider, SpatialQuery, SpatialQueryFilter};
use bevy::camera::primitives::Aabb;
use bevy::math::Vec3A;
use bevy::prelude::*;

use super::{MonkeyVoxelData, MonkeyVoxelVolume};
use crate::terrain_stream::voxel_clip::{clip_chunk_outside_rect, VoxelCarveRect};
use crate::terrain_stream::{build_collider_task, PendingCollider, TerrainStreamer};
use benilla_assets::coords::{bevy_to_wow, wow_to_bevy};
use benilla_assets::materials::TerrainMaterial;
use benilla_assets::{chunks_to_mesh, AdtTile, ChunkShading};
use benilla_formats::ChunkMesh;

/// MONKEY (voxel): how far INSIDE the footprint edge the real ground still reaches (yd). The voxel
/// surface must always overlap the remaining ADT, or the sky shows through a slit along the edge.
/// Surface-nets puts the outermost vertex in the volume's first cell, i.e. 0..1 voxel (0..0.5 yd)
/// inside the low-index faces (on flat ground exactly 0.25 yd, the cell centre) and 0.25 yd PAST
/// the high-index faces; so one full voxel (0.5 yd) is the hard bound that is always covered. The
/// underlap it leaves is hidden: the seeded voxel surface is lifted `SEED_LIFT` (0.15 yd) above it.
pub(super) const CARVE_INSET: f32 = benilla_voxel::VOXEL_SIZE;

/// `(tile_x, tile_y, cell_idx)`: a render cell (4×4 MCNK chunks) of one tile, the unit the
/// furnisher spawns one `Mesh3d` + `MeshMaterial3d<TerrainMaterial>` per.
type CellKey = (i32, i32, usize);

/// The build site's terrain-hiding state: the footprint signature (recompute the plan only when it
/// changes), the carve rectangle, the per-cell / per-tile plan (keyed by the rectangle's signature)
/// and which cell entities / tile roots are currently clipped (so they are restored when the
/// footprint leaves them and re-clipped after a stream-out/in respawns them).
#[derive(Resource, Default)]
pub(super) struct MonkeyVoxelHideState {
    /// Quantized footprint signature (centre ×2 rounded, chunk dims); `None` = no volume / cleared.
    sig: Option<(i64, i64, [u32; 3])>,
    /// The carve rectangle the plan was built for.
    rect: Option<VoxelCarveRect>,
    /// Render cells the rectangle reaches → the rectangle signature they must be clipped with.
    desired: HashMap<CellKey, u64>,
    /// Tiles whose collider the rectangle reaches → the rectangle signature.
    desired_tiles: HashMap<(i32, i32), u64>,
    /// Clipped cell entities → (key, rect signature) they were clipped for.
    applied: HashMap<Entity, (CellKey, u64)>,
    /// MONKEY (voxel): collider root -> (rect signature, 0 = original), also detects streamed roots.
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

/// The carve rectangle of the footprint `[fx0,fx1]×[fy0,fy1]` (WoW X×Y): inset by [`CARVE_INSET`].
fn carve_rect(fx0: f32, fx1: f32, fy0: f32, fy1: f32) -> VoxelCarveRect {
    VoxelCarveRect {
        x0: fx0 + CARVE_INSET,
        x1: fx1 - CARVE_INSET,
        y0: fy0 + CARVE_INSET,
        y1: fy1 - CARVE_INSET,
    }
}

/// The render cells the carve rectangle reaches, each tagged with the rectangle's signature. Pure
/// geometry: cells of tiles not yet resident keep their plan and are clipped once they stream in.
/// A chunk merely touching the rectangle's edge is planned too; clipping it is a no-op.
fn compute_desired(rect: &VoxelCarveRect) -> HashMap<CellKey, u64> {
    let cs = benilla_formats::CHUNK_SIZE;
    let mc = map_center();
    let sig = rect.sig();
    // The global MCNK chunk index range the rectangle touches.
    let gx_lo = (((mc - rect.x1) / cs).floor() as i32).clamp(0, 1023);
    let gx_hi = (((mc - rect.x0) / cs).floor() as i32).clamp(0, 1023);
    let gy_lo = (((mc - rect.y1) / cs).floor() as i32).clamp(0, 1023);
    let gy_hi = (((mc - rect.y0) / cs).floor() as i32).clamp(0, 1023);
    let mut desired = HashMap::new();
    for gx in gx_lo..=gx_hi {
        for gy in gy_lo..=gy_hi {
            let (tx, ty) = (gy / 16, gx / 16);
            let (row, col) = ((gx % 16) as usize, (gy % 16) as usize);
            desired.insert((tx, ty, (row / 4) * 4 + (col / 4)), sig);
        }
    }
    desired
}

/// MONKEY (voxel): fold the cell plan into the tile-level terrain collider plan.
fn tile_plans(cells: &HashMap<CellKey, u64>) -> HashMap<(i32, i32), u64> {
    cells
        .iter()
        .map(|(&(tx, ty, _), &sig)| ((tx, ty), sig))
        .collect()
}

/// `chunk` clipped to `rect` when the rectangle reaches it, else borrowed as-is.
fn clipped<'a>(chunk: &'a ChunkMesh, rect: Option<&VoxelCarveRect>) -> Cow<'a, ChunkMesh> {
    match rect.and_then(|r| clip_chunk_outside_rect(chunk, r)) {
        Some(c) => Cow::Owned(c),
        None => Cow::Borrowed(chunk),
    }
}

/// MONKEY (voxel): one tile's terrain collider data from its chunks clipped to `rect` (`None` = the
/// original trimesh) — the same weld as `terrain_stream`'s `terrain_collider_data`: draw space, each
/// chunk's indices rebased onto the running vertex count.
fn tile_collider_data(
    chunks: &[ChunkMesh],
    rect: Option<&VoxelCarveRect>,
) -> Option<(Vec<Vec3>, Vec<[u32; 3]>)> {
    let mut verts: Vec<Vec3> = Vec::new();
    let mut tris: Vec<[u32; 3]> = Vec::new();
    for chunk in chunks {
        let chunk = clipped(chunk, rect);
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

/// Re-mesh one render cell of `adt` with its chunks clipped to `rect`; `None` restores the
/// original cell. `None` result if every chunk is a hole (a 32-yd footprint never covers a whole
/// 133-yd cell, so in practice this always builds a mesh).
fn rebuild_cell_mesh(adt: &AdtTile, cell: usize, rect: Option<&VoxelCarveRect>) -> Option<Mesh> {
    let (cy, cx) = (cell / 4, cell % 4);
    let mut parts: Vec<(Cow<ChunkMesh>, &ChunkShading)> = Vec::with_capacity(16);
    for dy in 0..4usize {
        for dx in 0..4usize {
            let ci = (cy * 4 + dy) * 16 + (cx * 4 + dx);
            let (Some(shading), Some(chunk)) = (adt.shading.get(ci), adt.chunks.get(ci)) else {
                continue;
            };
            parts.push((clipped(chunk, rect), shading));
        }
    }
    let refs: Vec<(&ChunkMesh, &ChunkShading)> = parts.iter().map(|(c, s)| (&**c, *s)).collect();
    chunks_to_mesh(&refs)
}

/// Swap the cell entity's rendered mesh in place (clip with `rect`, or restore with `None`).
/// Returns whether the swap happened (the tile must still be resident to re-read its chunks).
fn swap_cell(
    adts: &Assets<AdtTile>,
    streamer: &TerrainStreamer,
    meshes: &mut Assets<Mesh>,
    key: CellKey,
    rect: Option<&VoxelCarveRect>,
    mesh3d: &Mesh3d,
) -> bool {
    let (tx, ty, cell) = key;
    let Some(handle) = streamer.tile_handle((tx, ty)) else {
        return false;
    };
    let Some(adt) = adts.get(&handle) else {
        return false;
    };
    let Some(mesh) = rebuild_cell_mesh(adt, cell, rect) else {
        return false;
    };
    // Clipping only removes area (every appended vertex lies on an original triangle), so the
    // clipped mesh stays inside the original bounds and the entity's cull Aabb stays valid.
    let _ = meshes.insert(&mesh3d.0, mesh);
    true
}

/// MONKEY (voxel): optional numeric ray check for the centre and two yards inside the footprint edge.
fn monkey_voxel_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("WOW_VOXEL_PROBE_RAY").is_ok_and(|value| value == "1"))
}

/// MONKEY (voxel): the probe's frame-to-frame state — the periodic site probes, plus the one-shot
/// probe of the OLD centre after the site moved (the restore check).
#[derive(Default)]
pub(super) struct MonkeyVoxelProbe {
    timer: f32,
    count: u8,
    last_centre: Option<Vec3>,
    /// The centre the site left, and the seconds since — probed once the restore has settled.
    old: Option<(Vec3, f32)>,
}

/// MONKEY (voxel): log collision ownership above the carved centre and near its edge (at most ten
/// times), and once after a move the OLD centre: PASS = the restored terrain tile at raw ADT height.
pub(super) fn monkey_voxel_probe_rays(
    time: Res<Time>,
    spatial: SpatialQuery,
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    volumes: Query<(Entity, &Transform, &MonkeyVoxelData), With<MonkeyVoxelVolume>>,
    mut probe: Local<MonkeyVoxelProbe>,
) {
    if !monkey_voxel_probe_enabled() {
        return;
    }
    let Ok((voxel, tf, data)) = volumes.single() else {
        return;
    };
    let half = super::half_extent_for(data.0.chunk_dims());
    let centre = tf.translation + half;
    // The tile root under a column: the ray is vertical, so it is the only terrain trimesh it can hit.
    let tile_root_under = |point: Vec3| {
        let w = bevy_to_wow(point);
        let (tx, ty) = benilla_formats::world_to_tile(w[0], w[1]);
        streamer.tile_entity((tx as i32, ty as i32))
    };
    let kind_of = |e: Entity, tile_root: Option<Entity>| {
        if e == voxel {
            "voxel"
        } else if tile_root == Some(e) {
            "terrain-tile"
        } else {
            "other"
        }
    };
    let filter = SpatialQueryFilter::default();

    // A horizontal move of the site arms the one-shot old-centre probe (a re-seed only moves y).
    if let Some(last) = probe.last_centre {
        if last.xz().distance(centre.xz()) > 1.0 {
            probe.old = Some((last, 0.0));
        }
    }
    probe.last_centre = Some(centre);
    if let Some((old, waited)) = probe.old {
        let waited = waited + time.delta_secs();
        // ~3 s for the restored collider's off-thread build to land.
        if waited < 3.0 {
            probe.old = Some((old, waited));
        } else {
            probe.old = None;
            let root = tile_root_under(old);
            let origin = old + Vec3::Y * 60.0;
            let height = crate::terrain_stream::terrain_height_under(&streamer, &adts, old);
            let raw = crate::terrain_stream::terrain_height_under_raw(&streamer, &adts, old);
            let ground =
                spatial.cast_ray_predicate(origin, Dir3::NEG_Y, 160.0, true, &filter, &|e| {
                    kind_of(e, root) != "other"
                });
            let hit = ground.map(|h| (kind_of(h.entity, root), origin.y - h.distance));
            let pass = matches!((hit, raw), (Some(("terrain-tile", y)), Some(r)) if (y - r).abs() < 0.05)
                && height == raw;
            let w = bevy_to_wow(old);
            info!(
                "MONKEY (voxel): old-centre probe {} at WoW ({:.1}, {:.1}): ground = {:?}; \
                 terrain_height_under={:?} (raw ADT {:?})",
                if pass { "PASS" } else { "FAIL" },
                w[0],
                w[1],
                hit,
                height,
                raw
            );
        }
    }

    if probe.count >= 10 {
        return;
    }
    probe.timer += time.delta_secs();
    if probe.timer < 2.0 {
        return;
    }
    probe.timer = 0.0;
    let points = [
        ("centre", centre),
        ("edge", centre + Vec3::X * (half.x - 2.0)),
    ];
    for (name, point) in points {
        let origin = point + Vec3::Y * 60.0;
        let tile_root = tile_root_under(point);
        let height = crate::terrain_stream::terrain_height_under(&streamer, &adts, point);
        let raw = crate::terrain_stream::terrain_height_under_raw(&streamer, &adts, point);
        match spatial.cast_ray(origin, Dir3::NEG_Y, 160.0, true, &filter) {
            Some(hit) => info!(
                "MONKEY (voxel): probe ray {} hit {} entity {:?} at y={:.3}; \
                 terrain_height_under={:?} (raw ADT {:?})",
                name,
                kind_of(hit.entity, tile_root),
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
            kind_of(e, tile_root) != "other"
        });
        if let Some(hit) = ground {
            info!(
                "MONKEY (voxel): probe ray {} ground = {} at y={:.3}",
                name,
                kind_of(hit.entity, tile_root),
                origin.y - hit.distance
            );
        }
    }
    probe.count += 1;
}

/// Carve the ADT terrain under the voxel build site, and keep the carve in step with the region:
/// recompute the plan when the footprint moves, clip newly-resident cells, restore cells the
/// footprint has left (or whose rectangle changed), and re-clip cells the streamer respawned.
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

    // No volume (or hiding disabled): restore every still-living clipped cell, then forget the plan.
    let volume = if no_hide {
        Err(())
    } else {
        volume.single().map_err(|_| ())
    };
    let Ok((tf, data)) = volume else {
        if state.sig.is_some() {
            streamer.set_voxel_carve(None);
        }
        if !state.applied.is_empty() {
            let living: HashMap<Entity, Mesh3d> =
                cells.iter().map(|(e, m, _)| (e, m.clone())).collect();
            for (e, (key, _)) in std::mem::take(&mut state.applied) {
                if let Some(mesh3d) = living.get(&e) {
                    if swap_cell(&adts, &streamer, &mut meshes, key, None, mesh3d) {
                        info!("MONKEY (voxel): restored terrain cell {key:?} (no build site).");
                    }
                }
            }
        }
        state.desired.clear();
        state.desired_tiles.clear();
        state.sig = None;
        state.rect = None;
        state.incomplete = false;
        // An empty plan restores every clipped tile collider to its original ADT trimesh.
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
        let rect = carve_rect(fx0, fx1, fy0, fy1);
        state.sig = Some(sig);
        state.rect = Some(rect);
        state.desired = compute_desired(&rect);
        state.desired_tiles = tile_plans(&state.desired);
        // The height queries' / torch casters' carve, published only when the plan is rebuilt (not
        // every frame, so the streamer resource isn't marked changed each frame).
        streamer.set_voxel_carve(Some(rect));
        state.incomplete = !all_resident || state.desired.is_empty();
        if was_retry && state.incomplete {
            // still waiting on the stream: stay quiet (no per-0.5 s log spam)
        } else {
            let mut keys: Vec<CellKey> = state.desired.keys().copied().collect();
            keys.sort_unstable();
            info!(
                "MONKEY (voxel): hide plan — footprint WoW X[{:.1},{:.1}] Y[{:.1},{:.1}], ground \
                 clipped to X[{:.2},{:.2}] Y[{:.2},{:.2}] (inset {CARVE_INSET} yd) in {} render \
                 cell(s): {:?}",
                fx0,
                fx1,
                fy0,
                fy1,
                rect.x0,
                rect.x1,
                rect.y0,
                rect.y1,
                keys.len(),
                keys,
            );
        }
    }
    let rect = state.rect;

    // Index the resident terrain cells by their render-cell key.
    let mut by_key: HashMap<CellKey, (Entity, Mesh3d)> = HashMap::new();
    let mut living: HashSet<Entity> = HashSet::new();
    for (e, mesh3d, aabb) in &cells {
        living.insert(e);
        by_key.insert(cell_key_of(aabb.center), (e, mesh3d.clone()));
    }

    // Restore cells no longer clipped the way the plan wants (moved off, rectangle changed), and
    // forget clipped cells the streamer despawned (their mesh asset went with them).
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
        if state.desired.get(&key).copied() != Some(applied_sig) {
            if let Some((_, mesh3d)) = by_key.get(&key) {
                if swap_cell(&adts, &streamer, &mut meshes, key, None, mesh3d) {
                    info!(
                        "MONKEY (voxel): restored terrain cell {key:?} (entity {e:?}) — the build \
                         site left it or moved."
                    );
                }
            }
            state.applied.remove(&e);
        }
    }

    // Clip desired cells that aren't already clipped on a live entity for the current plan.
    let desired: Vec<(CellKey, u64)> = state.desired.iter().map(|(k, s)| (*k, *s)).collect();
    for (key, plan_sig) in desired {
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
        if swap_cell(&adts, &streamer, &mut meshes, key, rect.as_ref(), &mesh3d) {
            state.applied.insert(e, (key, plan_sig));
            // `debug!`, not `info!`: this re-fires whenever a tile streams out and back in and the
            // cell respawns, so it must not spam the player's log on every pass near a build site.
            debug!(
                "MONKEY (voxel): clipped terrain cell {key:?} (entity {e:?}) — the real ADT ground \
                 under the build site is now cut out."
            );
        }
    }

    sync_tile_colliders(&mut commands, &streamer, &adts, &settled, &mut state);
}

/// MONKEY (voxel): keep each tile's one static terrain collider in step with the carve plan —
/// built from the clipped chunks while the footprint covers it, the original ADT trimesh again once
/// it no longer does. Rebuilds only when a tile's (root entity, plan signature) changes.
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
        let rect = if want_sig != 0 {
            state.rect.as_ref()
        } else {
            None
        };
        let Some((verts, tris)) = tile_collider_data(&adt.chunks, rect) else {
            continue;
        };
        let ntris = tris.len();
        commands.entity(root).insert(PendingCollider::new(
            build_collider_task(verts, tris),
            None,
            true,
        ));
        match rect {
            Some(r) => info!(
                "MONKEY (voxel): tile ({},{}) collider rebuilt with the ground clipped to WoW \
                 X[{:.2},{:.2}] Y[{:.2},{:.2}] ({} tris)",
                tile.0, tile.1, r.x0, r.x1, r.y0, r.y1, ntris
            ),
            None => info!(
                "MONKEY (voxel): tile ({},{}) collider restored to the original ADT trimesh ({} tris)",
                tile.0, tile.1, ntris
            ),
        }
        state.applied_tiles.insert(tile, (root, want_sig));
    }
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

    /// MONKEY (voxel): the planner — a 32-yd footprint whose inset rectangle reaches into four
    /// chunks across a chunk corner plans exactly the render cells of those chunks, all tagged with
    /// the rectangle's (non-zero) signature, and folds them into one tile.
    #[test]
    fn planner_covers_the_cells_the_rect_reaches() {
        let cs = benilla_formats::CHUNK_SIZE;
        // Global chunk (512, 512)'s NW corner is the map centre, WoW (0, 0); a footprint centred
        // one chunk south-east of it straddles global chunks 512..=513 on both axes.
        let c = -cs;
        let rect = carve_rect(c - 16.0, c + 16.0, c - 16.0, c + 16.0);
        assert!((rect.x1 - rect.x0 - (32.0 - 2.0 * CARVE_INSET)).abs() < 1.0e-4);
        let desired = compute_desired(&rect);
        let mut keys: Vec<_> = desired.keys().copied().collect();
        keys.sort_unstable();
        // Rows/cols 0..=1 of tile (32, 32) are all in render cell 0.
        assert_eq!(keys, vec![(32, 32, 0)], "plan {keys:?}");
        assert!(desired.values().all(|&s| s == rect.sig() && s != 0));
        let tiles = tile_plans(&desired);
        assert_eq!(tiles.get(&(32, 32)), Some(&rect.sig()));
    }

    #[test]
    fn collider_is_clipped_and_restores_without_a_rect() {
        let chunk = flat_chunk();
        let (_, base) = tile_collider_data(std::slice::from_ref(&chunk), None).unwrap();
        assert_eq!(
            base.len(),
            256,
            "an unclipped chunk welds all its fan triangles"
        );
        let rect = VoxelCarveRect {
            x0: 80.0,
            x1: 90.0,
            y0: 80.0,
            y1: 90.0,
        };
        let (verts, clipped) =
            tile_collider_data(std::slice::from_ref(&chunk), Some(&rect)).unwrap();
        assert_ne!(clipped.len(), base.len());
        // No collider triangle's centroid lies inside the rect (WoW x = −bevy z, y = −bevy x).
        for t in &clipped {
            let c = (verts[t[0] as usize] + verts[t[1] as usize] + verts[t[2] as usize]) / 3.0;
            assert!(
                !rect.contains_strict(-c.z, -c.x),
                "collider tri inside the carve"
            );
        }
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

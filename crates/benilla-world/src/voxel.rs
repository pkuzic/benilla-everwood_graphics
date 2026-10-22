//! MONKEY (voxel): the v2 WORLD voxel layer (CONTRACT §1–2, `disposable/voxel-v2/CONTRACT.md`).
//!
//! The real ADT ground is editable ANYWHERE: a GM aims a brush at the ground (or an existing voxel
//! surface) and the sparse, world-aligned voxel chunks there activate on demand, seeded from the
//! RAW ADT heights, so the edit lands seamlessly in the Blizzlike terrain. The data + mesher +
//! journal are engine-free in `benilla_voxel::world`; this module is the Bevy side:
//!
//! - [`MonkeyVoxelWorld`] (resource): the active chunks' densities, column seeds, the journal v2
//!   (`$BENILLA_HOME/monkey_voxel_world.bin`), the wanted / build / remesh queues, the chunk
//!   entities and the carve (the union of surface columns) published to `terrain_hide`.
//! - one entity per meshed chunk ([`MonkeyVoxelChunk`]): `Mesh3d` + the per-tile
//!   [`MonkeyVoxelMaterial`] + `RigidBody::Static` trimesh `Collider`, rebuilt on change.
//! - systems (chained, world-live only): map watch → brush → AUTOEDIT hook → inbox → stream
//!   (seed / build / mesh / carve) → per-tile materials → terrain hide → probe → journal save.
//!
//! Everything is behind the crate's `voxel` cargo feature (OFF by default).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use avian3d::prelude::{Collider, RigidBody, SpatialQuery, SpatialQueryFilter};
use bevy::asset::RenderAssetUsages;
use bevy::camera::primitives::{Aabb, MeshAabb};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::tasks::futures_lite::future;
use bevy::tasks::{block_on, AsyncComputeTaskPool, Task};

mod material;
pub use material::MonkeyVoxelMaterial;
use material::MonkeyVoxelMaterialHandle;

mod terrain_hide;
use terrain_hide::{monkey_voxel_hide_terrain, MonkeyVoxelHideState, CARVE_INSET};

use crate::terrain_stream::voxel_clip::VoxelCarve;
use crate::terrain_stream::TerrainStreamer;
use crate::view::WorldCamera;
use benilla_assets::coords::{bevy_to_wow, wow_to_bevy};
use benilla_assets::AdtTile;
pub use benilla_voxel::wire;
pub use benilla_voxel::world::{region_of, RegionSync, VoxelEditOp, REGION_YARDS};
use benilla_voxel::world::{
    chunk_key_at, raymarch, window_origin, ChunkKey, ColumnKey, ColumnSeed, JournalInsert,
    VoxelJournal, VoxelWorldData, CHUNK_YARDS, SEED_LIFT,
};

/// One edit as it travels the cross-crate mailbox (the v2 EDIT body + optional server seq).
pub type MonkeyVoxelEdit = VoxelEditOp;

/// Hard cap on active chunks (32 KB of density each → 128 MB at the cap).
pub const MONKEY_VOXEL_MAX_CHUNKS: usize = 4096;
/// MONKEY (voxel): main-thread wall clock per frame for snapshotting build / mesh inputs (the work
/// itself runs on the async compute pool); hot (live-edit) work is always dispatched.
const DISPATCH_MS: f32 = 2.0;
/// MONKEY (voxel): main-thread wall clock per frame for attaching finished meshes (≥ 1 per frame).
const ATTACH_MS: f32 = 2.0;
/// MONKEY (voxel): triangles attached per frame at most (≥ 1 mesh): the queued inserts — the
/// mesh asset swap and avian's trimesh collider insert — run at the command flush, outside the
/// [`ATTACH_MS`] clock, and cost ~2e-5 ms per triangle.
const ATTACH_TRIS: usize = 60_000;
/// MONKEY (voxel): frames a finished hot mesh may wait for its sibling hot tasks.
const HOT_HOLD_FRAMES: u32 = 3;
/// MONKEY (voxel): chunks per off-thread build group (built in lockstep, like the sync build).
const BUILD_GROUP: usize = 4;
/// MONKEY (voxel): bulk build groups / bulk meshes in flight at most, so a live edit's hot task
/// never queues behind a whole replay on the pool.
const MAX_BUILD_TASKS: usize = 4;
const MAX_MESH_TASKS: usize = 8;
/// MONKEY (voxel): remote edits per frame still applied LIVE (incrementally); more = a burst →
/// the bulk (off-thread rebuild) path. Also bounded by [`LIVE_APPLY_MS`] of inline applying.
const LIVE_EDITS_PER_FRAME: usize = 8;
const LIVE_APPLY_MS: f32 = 2.0;
/// Column seeds sampled per frame at most (each = 36² raw height queries).
const SEEDS_PER_FRAME: usize = 16;
/// MONKEY (voxel): how far a chunk's skirt hangs below its open lip (yd) — well past the
/// [`SEED_LIFT`] slit to the real ground under it, on any slope the 0.5-yd overlap can carry.
const SKIRT_DEPTH: f32 = 1.0;

/// Marks one world voxel chunk's entity.
#[derive(Component)]
pub struct MonkeyVoxelChunk(pub ChunkKey);

/// Cross-crate mailbox between the voxel layer (benilla-world) and the network (benilla-app,
/// behind the `voxel` feature).
///
/// SERVER-LANE API (CONTRACT §3–4): `outbox` holds local edits to uplink (`.mvox` EDIT or the
/// guild-relay fallback `MV2:`); every inbound record goes into the matching inbox and is applied
/// next frame through the canonical path:
/// - `inbox`: remote edits — `seq: Some` = a server `E` record (deduped by seq, ordered), `None` =
///   a guild relay line (appended in arrival order);
/// - `confirms`: `(nonce, seq)` — the server echo of OUR OWN edit (marks the prediction confirmed,
///   never re-applies it);
/// - `resets`: `(map, rx, ry, epoch)` — `X` REGION-RESET (drops that region's journal, rebuilds);
/// - `region_done`: `(map, rx, ry, epoch, last_seq)` — `D` REGION-DONE (persists the sync point).
///
/// MONKEY (voxel): the server lane uses `events` instead — the SAME records, applied in ARRIVAL
/// order (an `X` followed by the region's `E`s in one SYNC reply must reset first, then apply).
/// The typed vecs above are drained first each frame (the guild relay still uses `inbox`).
#[derive(Resource, Default)]
pub struct MonkeyVoxelNet {
    pub outbox: Vec<VoxelEditOp>,
    pub inbox: Vec<VoxelEditOp>,
    pub confirms: Vec<(u16, u32)>,
    pub resets: Vec<(u16, i16, i16, u16)>,
    pub region_done: Vec<(u16, i16, i16, u16, u32)>,
    pub events: Vec<MonkeyVoxelNetEvent>,
}

/// MONKEY (voxel): one server-lane record for [`MonkeyVoxelNet::events`] (applied in order).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MonkeyVoxelNetEvent {
    /// A server `E` that is not our own prediction (seq: Some).
    Edit(VoxelEditOp),
    /// The server echo of our prediction `nonce` on `map` as `seq` (no re-apply).
    Confirm { map: u16, nonce: u16, seq: u32 },
    /// `X` REGION-RESET.
    Reset { map: u16, rx: i16, ry: i16, epoch: u16 },
    /// The region's persisted sync point (a `D`, or a live edit in a synced region).
    SyncPoint { map: u16, rx: i16, ry: i16, epoch: u16, last_seq: u32 },
    /// Drop our prediction `nonce` on `map` (an `R` reject, or a stale one the server never got).
    DropLocal { map: u16, nonce: u16 },
}

/// Whether the local player is a GM (`PLAYER_FLAGS & 0x8`, i.e. `.gm on`). benilla-world can't read
/// the network descriptor, so benilla-app fills this each frame from the self player's flags. Gates
/// the KEY-driven brush so only a GM edits (owner decision 2); applying others' edits is NOT gated,
/// and the headless `WOW_VOXEL_AUTOEDIT` hook bypasses it. Defaults false until benilla-app sets it.
#[derive(Resource, Default)]
pub struct MonkeyLocalGm(pub bool);

/// Current brush radius in YARDS (adjustable live with `-`/`=`, 0.5 yd steps). Default 2 yd.
#[derive(Resource)]
pub struct MonkeyVoxelBrushRadius(pub f32);

impl Default for MonkeyVoxelBrushRadius {
    fn default() -> Self {
        Self(2.0)
    }
}

/// The world voxel layer's state on the current map (see the module docs). The `pub` methods are
/// the API the server-net lane drives.
#[derive(Resource)]
pub struct MonkeyVoxelWorld {
    /// The map the layer is built for (`None` until the first world entry).
    map: Option<u32>,
    data: VoxelWorldData,
    journal: VoxelJournal,
    /// Wanted (activated) chunks, by column → kz.
    wanted: HashMap<ColumnKey, BTreeSet<i32>>,
    wanted_count: usize,
    /// Columns with a wanted chunk whose seed is not loaded yet (tile not resident).
    need_seed: HashSet<ColumnKey>,
    /// Built chunks that must be reseeded + replayed (out-of-order arrival, reset).
    rebuild: HashSet<ChunkKey>,
    /// Chunks to remesh with the frame budget, and ones to remesh THIS frame (live edits).
    dirty: HashSet<ChunkKey>,
    hot: HashSet<ChunkKey>,
    entities: HashMap<ChunkKey, Entity>,
    meshed: HashSet<ChunkKey>,
    /// The carve published to `terrain_hide`.
    carve_cols: BTreeSet<ColumnKey>,
    carve: Option<Arc<VoxelCarve>>,
    carve_dirty: bool,
    journal_dirty: bool,
    next_nonce: u16,
    cap_warned: bool,
    /// WoW centre of the last local dig (the probe aims under it).
    last_dig: Option<[f32; 3]>,
    /// Remaining verbose per-edit timing lines.
    timing_logs: u32,
    /// MONKEY (voxel): the live local edits of this frame awaiting their perf line —
    /// `(radius, ingest ms)`; the stream adds its build + mesh + collider time.
    perf: Vec<(f32, f32)>,
    /// MONKEY (voxel): the drain of a live edit's hot chunks over frames: (frames, max stream ms).
    perf_drain: Option<(u32, f32)>,
    /// MONKEY (voxel): wanted chunks with a seed but no density yet, as of the last stream pass.
    unbuilt: usize,
    /// MONKEY (voxel): per-chunk build-input generation ([`Self::bump_edit_gen`]).
    edit_gen: HashMap<ChunkKey, u64>,
    /// MONKEY (voxel): a live edit's not-yet-active chunks (built ahead of the bulk queue).
    hot_build: HashSet<ChunkKey>,
    /// MONKEY (voxel): builds on the pool, and the chunks they cover.
    builds: Vec<MonkeyBuildTask>,
    building: HashSet<ChunkKey>,
    /// MONKEY (voxel): meshes on the pool, how many per chunk, finished ones awaiting attach,
    /// the generation each chunk shows, the dispatch counter (= generations).
    meshing: Vec<MonkeyMeshTask>,
    mesh_in_flight: HashMap<ChunkKey, u32>,
    mesh_ready: HashMap<ChunkKey, MonkeyMeshReady>,
    shown_gen: HashMap<ChunkKey, u64>,
    mesh_seq: u64,
    /// MONKEY (voxel): hot chunks dispatched whose new mesh is not shown yet (the perf drain).
    hot_wait: HashSet<ChunkKey>,
    /// MONKEY (voxel): counters for the logs / tests.
    stale_builds: usize,
    stale_meshes: usize,
    built_total: usize,
    meshed_total: usize,
    /// MONKEY (voxel): last stream pass, ms per phase: seeds, build poll, build dispatch, mesh
    /// poll + attach, mesh dispatch, carve, whole stream.
    phase_ms: [f32; 7],
    /// MONKEY (voxel): consecutive frames hot results have been held for their siblings.
    hot_hold: u32,
    /// MONKEY (voxel): per-column mesh generation (bumped when any chunk of the column attaches a
    /// new mesh) and a global epoch (any attach or carve change) — the ground-effect re-placement
    /// ([`monkey_voxel_clutter`]) redoes only the MCNKs whose columns / carve changed.
    clutter_col_gen: HashMap<ColumnKey, u64>,
    clutter_epoch: u64,
}

impl Default for MonkeyVoxelWorld {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.subsec_nanos() as u16 ^ (d.as_secs() as u16).rotate_left(7));
        Self {
            map: None,
            data: VoxelWorldData::default(),
            journal: VoxelJournal::default(),
            wanted: HashMap::new(),
            wanted_count: 0,
            need_seed: HashSet::new(),
            rebuild: HashSet::new(),
            dirty: HashSet::new(),
            hot: HashSet::new(),
            entities: HashMap::new(),
            meshed: HashSet::new(),
            carve_cols: BTreeSet::new(),
            carve: None,
            carve_dirty: false,
            journal_dirty: false,
            next_nonce: seed,
            cap_warned: false,
            last_dig: None,
            timing_logs: 40,
            perf: Vec::new(),
            perf_drain: None,
            unbuilt: 0,
            edit_gen: HashMap::new(),
            hot_build: HashSet::new(),
            builds: Vec::new(),
            building: HashSet::new(),
            meshing: Vec::new(),
            mesh_in_flight: HashMap::new(),
            mesh_ready: HashMap::new(),
            shown_gen: HashMap::new(),
            mesh_seq: 0,
            hot_wait: HashSet::new(),
            stale_builds: 0,
            stale_meshes: 0,
            built_total: 0,
            meshed_total: 0,
            phase_ms: [0.0; 7],
            hot_hold: 0,
            clutter_col_gen: HashMap::new(),
            clutter_epoch: 0,
        }
    }
}

impl MonkeyVoxelWorld {
    /// The carve (union of surface columns) for the terrain hiding; `None` = nothing carved.
    pub fn carve(&self) -> Option<Arc<VoxelCarve>> {
        self.carve.clone()
    }

    /// The map the layer currently mirrors.
    pub fn map(&self) -> Option<u32> {
        self.map
    }

    /// SERVER LANE: local edits still waiting for a server seq on `map` (send/resend these).
    pub fn pending_local(&self, map: u16) -> Vec<VoxelEditOp> {
        self.journal
            .maps
            .get(&map)
            .map_or_else(Vec::new, |j| j.pending_local())
    }

    /// SERVER LANE: the persisted sync point of a region (`epoch`, `last_seq`) for a SYNC entry.
    pub fn region_sync(&self, map: u16, rx: i16, ry: i16) -> RegionSync {
        self.journal
            .maps
            .get(&map)
            .and_then(|j| j.regions.get(&(rx, ry)).copied())
            .unwrap_or_default()
    }

    /// Number of active (built) chunks.
    pub fn active_chunks(&self) -> usize {
        self.data.chunks.len()
    }

    /// MONKEY (voxel): nothing queued or in flight — every wanted chunk built and meshed.
    pub fn settled(&self) -> bool {
        self.need_seed.is_empty()
            && self.rebuild.is_empty()
            && self.dirty.is_empty()
            && self.hot.is_empty()
            && self.hot_wait.is_empty()
            && self.bulk_idle()
    }

    /// MONKEY (voxel): the inputs of `keys`' canonical builds changed (an edit touching them was
    /// filed, reordered or dropped): any build of them in flight is now stale.
    fn bump_edit_gen(&mut self, keys: &[ChunkKey]) {
        for k in keys {
            *self.edit_gen.entry(*k).or_default() += 1;
        }
    }

    /// MONKEY (voxel): nothing building, meshing or queued for the bulk path (the carve waits for
    /// this during a replay so the terrain is re-clipped once).
    fn bulk_idle(&self) -> bool {
        self.dirty.is_empty()
            && self.rebuild.is_empty()
            && self.unbuilt == 0
            && self.hot_build.is_empty()
            && self.builds.is_empty()
            && self.meshing.is_empty()
            && self.mesh_ready.is_empty()
    }

    /// MONKEY (voxel): pick this frame's canonical builds and snapshot them into `Send` jobs (the
    /// caller runs them on the pool, or inline in tests). Hot groups (a live edit's fresh chunks)
    /// always go; bulk ones (rebuilds first, then never-built wanted chunks) while fewer than
    /// [`MAX_BUILD_TASKS`] bulk groups are in flight and before `deadline`. A chunk already in
    /// flight waits for its result. Also refreshes [`Self::unbuilt`].
    fn take_build_jobs(&mut self, deadline: Instant) -> Vec<MonkeyBuildDispatch> {
        let mut out = Vec::new();
        let Some(map) = self.current_map_u16() else {
            return out;
        };
        let seeded = |s: &Self, k: &ChunkKey| s.data.seeds.contains_key(&[k[0], k[1]]);
        // Hot: a live edit's not-yet-active chunks.
        let mut hot: Vec<ChunkKey> = self
            .hot_build
            .iter()
            .copied()
            .filter(|k| !self.building.contains(k) && seeded(self, k))
            .collect();
        hot.retain(|k| !self.data.is_active(*k));
        hot.sort();
        for k in &hot {
            self.hot_build.remove(k);
        }
        let active_hot: Vec<ChunkKey> = self
            .hot_build
            .iter()
            .copied()
            .filter(|k| self.data.is_active(*k))
            .collect();
        for k in active_hot {
            self.hot_build.remove(&k);
        }
        // Bulk: out-of-order / reset rebuilds, then the wanted chunks never built.
        let mut bulk: Vec<ChunkKey> = self
            .rebuild
            .iter()
            .copied()
            .filter(|k| !self.building.contains(k))
            .collect();
        bulk.retain(|k| self.data.is_active(*k));
        let stale: Vec<ChunkKey> = self
            .rebuild
            .iter()
            .copied()
            .filter(|k| !self.data.is_active(*k) && !self.building.contains(k))
            .collect();
        for k in stale {
            self.rebuild.remove(&k); // never built: the unbuilt scan below builds it canonically
        }
        bulk.sort();
        let mut fresh: Vec<ChunkKey> = Vec::new();
        for (col, kzs) in &self.wanted {
            if !self.data.seeds.contains_key(col) {
                continue;
            }
            for &kz in kzs {
                let k = [col[0], col[1], kz];
                if !self.data.is_active(k) && !self.building.contains(&k) && !hot.contains(&k) {
                    fresh.push(k);
                }
            }
        }
        fresh.sort();
        let mut groups: Vec<(Vec<ChunkKey>, bool)> = hot
            .chunks(BUILD_GROUP)
            .map(|g| (g.to_vec(), true))
            .collect();
        groups.extend(bulk.chunks(BUILD_GROUP).map(|g| (g.to_vec(), false)));
        groups.extend(fresh.chunks(BUILD_GROUP).map(|g| (g.to_vec(), false)));
        let mut bulk_in_flight = self.builds.iter().filter(|t| !t.hot).count();
        let mut unbuilt = fresh.len();
        for (keys, is_hot) in groups {
            if !is_hot && (bulk_in_flight >= MAX_BUILD_TASKS || Instant::now() >= deadline) {
                break;
            }
            if !is_hot {
                bulk_in_flight += 1;
            }
            let edits = self
                .journal
                .maps
                .get(&map)
                .map_or_else(Vec::new, |j| j.edits_for(&keys));
            let gens: Vec<u64> = keys
                .iter()
                .map(|k| self.edit_gen.get(k).copied().unwrap_or(0))
                .collect();
            for k in &keys {
                self.building.insert(*k);
                self.rebuild.remove(k);
                if !self.data.is_active(*k) && !is_hot {
                    unbuilt -= 1;
                }
            }
            let job = self.data.build_job(&keys, edits);
            out.push(MonkeyBuildDispatch {
                keys,
                gens,
                hot: is_hot,
                job,
            });
        }
        self.unbuilt = unbuilt;
        out
    }

    /// MONKEY (voxel): a build came back. Adopted only if none of its chunks' inputs changed since
    /// dispatch (per-chunk [`Self::edit_gen`]); a stale result is DISCARDED and its chunks requeued
    /// (active → rebuild; inactive → the unbuilt scan / hot set), so it can never overwrite newer
    /// data. Returns whether it was adopted.
    fn finish_build(
        &mut self,
        keys: &[ChunkKey],
        gens: &[u64],
        hot: bool,
        out: Vec<(ChunkKey, Box<[i8]>)>,
    ) -> bool {
        for k in keys {
            self.building.remove(k);
        }
        let stale = keys
            .iter()
            .zip(gens)
            .any(|(k, g)| self.edit_gen.get(k).copied().unwrap_or(0) != *g);
        if stale {
            self.stale_builds += 1;
            for &k in keys {
                if self.data.is_active(k) {
                    self.rebuild.insert(k);
                } else if hot {
                    self.hot_build.insert(k);
                }
            }
            return false;
        }
        let built = self.data.insert_built(out);
        self.dirty_around(&built, hot);
        self.carve_dirty = true;
        self.built_total += built.len();
        true
    }

    /// MONKEY (voxel): pick this frame's remeshes and snapshot their windows: every hot chunk (at
    /// most 2 in flight per chunk), then dirty ones while fewer than [`MAX_MESH_TASKS`] bulk meshes
    /// are in flight and before `deadline`. Each gets a fresh, monotonically increasing generation.
    fn take_mesh_jobs(&mut self, deadline: Instant) -> Vec<MonkeyMeshDispatch> {
        let mut out = Vec::new();
        let hot: Vec<ChunkKey> = self.hot.iter().copied().collect();
        for k in hot {
            if !self.data.is_active(k) {
                self.hot.remove(&k);
                continue;
            }
            if self.mesh_in_flight.get(&k).copied().unwrap_or(0) >= 2 {
                continue; // stays hot: dispatched when one returns
            }
            self.hot.remove(&k);
            self.dirty.remove(&k);
            self.hot_wait.insert(k);
            out.push(self.mesh_dispatch(k, true));
        }
        let mut bulk_in_flight = self.meshing.iter().filter(|t| !t.hot).count();
        if bulk_in_flight >= MAX_MESH_TASKS || self.dirty.is_empty() {
            return out;
        }
        let mut cand: Vec<ChunkKey> = self
            .dirty
            .iter()
            .copied()
            .filter(|k| self.mesh_in_flight.get(k).copied().unwrap_or(0) == 0)
            .collect();
        cand.sort();
        for k in cand {
            if bulk_in_flight >= MAX_MESH_TASKS || Instant::now() >= deadline {
                break;
            }
            self.dirty.remove(&k);
            if !self.data.is_active(k) {
                continue;
            }
            bulk_in_flight += 1;
            out.push(self.mesh_dispatch(k, false));
        }
        out
    }

    fn mesh_dispatch(&mut self, k: ChunkKey, hot: bool) -> MonkeyMeshDispatch {
        self.mesh_seq += 1;
        *self.mesh_in_flight.entry(k).or_default() += 1;
        MonkeyMeshDispatch {
            key: k,
            gen: self.mesh_seq,
            hot,
            window: self.data.window_i8(k),
            skirts: self.skirt_sides(k),
        }
    }

    /// MONKEY (voxel): the horizontal sides (−x, +x, −y, +y) of chunk `k` where the voxel layer
    /// ENDS — the neighbour chunk is not active, so the surface's lip hangs over the real ground
    /// and needs a skirt ([`benilla_voxel::world::add_skirts`]). A neighbour activating later
    /// remeshes this chunk (`dirty_around`), dropping the skirt there.
    fn skirt_sides(&self, k: ChunkKey) -> [bool; 4] {
        [(-1, 0), (1, 0), (0, -1), (0, 1)]
            .map(|(dx, dy)| !self.data.is_active([k[0] + dx, k[1] + dy, k[2]]))
    }

    /// MONKEY (voxel): a mesh came back (task done). Keeps only the newest pending result per
    /// chunk; the attach decides against what is shown.
    fn mesh_returned(&mut self, r: MonkeyMeshReady) {
        if let Some(n) = self.mesh_in_flight.get_mut(&r.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.mesh_in_flight.remove(&r.key);
            }
        }
        match self.mesh_ready.get(&r.key) {
            Some(old) if old.gen > r.gen => {
                self.stale_meshes += 1;
            }
            _ => {
                if self.mesh_ready.insert(r.key, r).is_some() {
                    self.stale_meshes += 1;
                }
            }
        }
    }

    /// MONKEY (voxel): whether a finished mesh of generation `gen` may replace what chunk `k`
    /// shows: the chunk is still active and nothing NEWER was attached (results can finish out of
    /// order; an older snapshot must never overwrite a newer one).
    fn mesh_is_fresh(&self, k: ChunkKey, gen: u64) -> bool {
        self.data.is_active(k) && self.shown_gen.get(&k).is_none_or(|&s| gen > s)
    }


    fn current_map_u16(&self) -> Option<u16> {
        self.map.map(|m| m as u16)
    }

    /// Activate chunk `k` (and, once its column's seed is known, the column's surface chunks).
    fn want(&mut self, k: ChunkKey) {
        let col = [k[0], k[1]];
        if self.wanted.get(&col).is_some_and(|s| s.contains(&k[2])) {
            return;
        }
        if self.wanted_count >= MONKEY_VOXEL_MAX_CHUNKS {
            if !self.cap_warned {
                warn!(
                    "MONKEY (voxel): active-chunk cap ({MONKEY_VOXEL_MAX_CHUNKS}) reached — further \
                     edits outside the active chunks are journalled but not shown this session."
                );
                self.cap_warned = true;
            }
            return;
        }
        self.wanted.entry(col).or_default().insert(k[2]);
        self.wanted_count += 1;
        if self.data.seeds.contains_key(&col) {
            self.expand_column(col);
        } else {
            self.need_seed.insert(col);
        }
    }

    /// CONTRACT §1: a column with an active chunk within ±1 chunk of the seeded surface also
    /// activates its surface chunk(s), so the carve below it is always covered.
    fn expand_column(&mut self, col: ColumnKey) {
        let Some((lo, hi)) = self.data.seeds.get(&col).and_then(|s| s.surface_chunks()) else {
            return;
        };
        let near = self
            .wanted
            .get(&col)
            .is_some_and(|s| s.range(lo - 1..=hi + 1).next().is_some());
        if near {
            for kz in lo..=hi {
                self.want([col[0], col[1], kz]);
            }
        }
    }

    /// Mark the meshes that can see `keys`' samples (the chunks + their 26 neighbours) dirty.
    fn dirty_around(&mut self, keys: &[ChunkKey], hot: bool) {
        for k in keys {
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let n = [k[0] + dx, k[1] + dy, k[2] + dz];
                        if self.data.is_active(n) {
                            if hot {
                                self.hot.insert(n);
                            } else {
                                self.dirty.insert(n);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Put one edit through the canonical path: journal (canonical order), activation, and the
    /// incremental apply when it is in order (else a rebuild of the chunks it touches).
    ///
    /// MONKEY (voxel): `live` = a live local / remote edit: applied incrementally on this thread
    /// (sub-ms) and remeshed hot, its fresh chunks built hot. Not live = BULK (a SYNC reply, a
    /// burst): never applied here — its active chunks are queued for an off-thread canonical
    /// rebuild instead, so a 500-edit resync costs the frame nothing. Either way every chunk it
    /// touches gets a new edit generation (an in-flight build of it is now stale).
    fn ingest(&mut self, e: VoxelEditOp, live: bool) -> JournalInsert {
        let how = self.journal.maps.entry(e.map).or_default().insert(e);
        self.journal_dirty |= how != JournalInsert::Duplicate;
        if self.current_map_u16() != Some(e.map) || how == JournalInsert::Duplicate {
            return how; // another map's edit: journalled for when we go there
        }
        let touched = e.touched_chunks();
        self.bump_edit_gen(&touched);
        for &k in &touched {
            self.want(k);
            if live && !self.data.is_active(k) {
                self.hot_build.insert(k);
            }
        }
        match how {
            JournalInsert::Appended if live => {
                self.data.apply_incremental(&e);
                // The meshes seeing the bbox (+ apron) — only chunks already active.
                let remesh: Vec<ChunkKey> = e
                    .remesh_chunks()
                    .into_iter()
                    .filter(|k| self.data.is_active(*k))
                    .collect();
                for k in remesh {
                    self.hot.insert(k);
                }
            }
            JournalInsert::Appended | JournalInsert::OutOfOrder => {
                for &k in &touched {
                    if self.data.is_active(k) {
                        self.rebuild.insert(k);
                    }
                }
            }
            JournalInsert::Duplicate => {}
        }
        how
    }

    /// MONKEY (voxel): `keys` changed their canonical inputs: new generation + rebuild the active.
    fn requeue(&mut self, keys: &[ChunkKey]) {
        self.bump_edit_gen(keys);
        for &k in keys {
            if self.data.is_active(k) {
                self.rebuild.insert(k);
            }
        }
    }

    /// A LOCAL edit (brush / AUTOEDIT): stamps map + nonce, ingests it, queues it for the uplink.
    fn local_edit(&mut self, net: &mut MonkeyVoxelNet, mut e: VoxelEditOp) {
        let Some(map) = self.current_map_u16() else {
            return;
        };
        e.map = map;
        e.seq = None;
        e.local = true;
        e.nonce = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1);
        if !e.is_valid() {
            return;
        }
        if e.mode == 0 {
            self.last_dig = Some(e.pos);
        }
        let t0 = Instant::now();
        self.ingest(e, true);
        self.perf.push((e.radius, t0.elapsed().as_secs_f32() * 1000.0));
        net.outbox.push(e);
    }

    /// SERVER LANE: our prediction `nonce` came back as `seq` — confirm it (no re-apply).
    pub fn confirm(&mut self, map: u16, nonce: u16, seq: u32) {
        let Some(j) = self.journal.maps.get_mut(&map) else {
            return;
        };
        if let Some((e, how)) = j.confirm(nonce, seq) {
            self.journal_dirty = true;
            if how == JournalInsert::OutOfOrder && self.current_map_u16() == Some(map) {
                self.requeue(&e.touched_chunks());
            }
        }
    }

    /// SERVER LANE: REGION-RESET — drop the region's journal, adopt `epoch`, rebuild its chunks.
    pub fn reset_region(&mut self, map: u16, rx: i16, ry: i16, epoch: u16) {
        let dropped = self
            .journal
            .maps
            .entry(map)
            .or_default()
            .reset_region(rx, ry, epoch);
        self.journal_dirty = true;
        if self.current_map_u16() == Some(map) {
            let keys: HashSet<ChunkKey> = dropped.iter().flat_map(|e| e.touched_chunks()).collect();
            self.requeue(&keys.into_iter().collect::<Vec<_>>());
        }
    }

    /// SERVER LANE: drop our refused / stale prediction `nonce` and rebuild what it touched.
    pub fn drop_local(&mut self, map: u16, nonce: u16) -> Option<VoxelEditOp> {
        let e = self.journal.maps.get_mut(&map)?.remove_pending(nonce)?;
        self.journal_dirty = true;
        if self.current_map_u16() == Some(map) {
            self.requeue(&e.touched_chunks());
        }
        Some(e)
    }

    /// SERVER LANE: REGION-DONE — persist the region's sync point.
    pub fn region_done(&mut self, map: u16, rx: i16, ry: i16, epoch: u16, last_seq: u32) {
        self.journal
            .maps
            .entry(map)
            .or_default()
            .regions
            .insert((rx, ry), RegionSync { epoch, last_seq });
        self.journal_dirty = true;
    }

    /// Forget everything built for the current map (entities are despawned by the caller).
    fn clear_map_state(&mut self) {
        self.data = VoxelWorldData::default();
        self.wanted.clear();
        self.wanted_count = 0;
        self.need_seed.clear();
        self.rebuild.clear();
        self.dirty.clear();
        self.hot.clear();
        self.entities.clear();
        self.meshed.clear();
        self.carve_cols.clear();
        self.carve = None;
        self.carve_dirty = false;
        self.cap_warned = false;
        // MONKEY (voxel): dropping a Task cancels it; nothing of the old map can land.
        self.unbuilt = 0;
        self.edit_gen.clear();
        self.hot_build.clear();
        self.builds.clear();
        self.building.clear();
        self.meshing.clear();
        self.mesh_in_flight.clear();
        self.mesh_ready.clear();
        self.shown_gen.clear();
        self.hot_wait.clear();
        self.clutter_col_gen.clear();
        self.clutter_epoch += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// Journal file
// ---------------------------------------------------------------------------------------------

/// `$BENILLA_HOME/<name>` (falls back to the CWD).
fn monkey_voxel_home_file(name: &str) -> std::path::PathBuf {
    std::env::var_os("BENILLA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(name)
}

/// The v2 journal file.
fn monkey_voxel_journal_path() -> std::path::PathBuf {
    monkey_voxel_home_file("monkey_voxel_world.bin")
}

/// Read the v2 journal (an absent file = empty). A v1 journal / site file, or a v2 file of an
/// unknown version, is IGNORED with one log line (v1 edits were site-local, meaningless now).
fn monkey_voxel_journal_load() -> VoxelJournal {
    for old in ["monkey_voxel_journal.bin", "monkey_voxel_site.bin"] {
        if monkey_voxel_home_file(old).exists() {
            info!(
                "MONKEY (voxel): ignoring the v1 file {old} (v1 site edits do not map to the v2 \
                 world layer)."
            );
        }
    }
    let path = monkey_voxel_journal_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return VoxelJournal::default();
    };
    match VoxelJournal::decode(&bytes) {
        Ok(j) => {
            let n: usize = j.maps.values().map(|m| m.edits().len()).sum();
            info!(
                "MONKEY (voxel): journal v2 loaded — {n} edit(s) on {} map(s) from {}",
                j.maps.len(),
                path.display()
            );
            j
        }
        Err(e) => {
            warn!(
                "MONKEY (voxel): ignoring {} ({e:?}) — starting an empty v2 journal.",
                path.display()
            );
            VoxelJournal::default()
        }
    }
}

/// Write the journal atomically (temp + rename).
fn monkey_voxel_journal_save(j: &VoxelJournal) {
    // MONKEY (voxel): encode here (µs), write + rename on the IO pool — a file write / rename on
    // Windows (AV scan) measured ~90 ms on the frame. Serialised by a lock so writes never
    // interleave; the newest bytes always land last.
    static SAVE: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let bytes = j.encode();
    let my = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let path = monkey_voxel_journal_path();
    let write = move || {
        let mut last = SAVE.lock().unwrap_or_else(|e| e.into_inner());
        if *last > my {
            return; // a newer save already landed
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
// Mesh / collider conversion
// ---------------------------------------------------------------------------------------------

/// WoW-axis offset → Bevy-axis offset (`wow_to_bevy` is linear: X→−Z, Y→−X, Z→+Y; det +1, so the
/// triangle winding is kept).
#[inline]
fn wow_vec_to_bevy(p: [f32; 3]) -> [f32; 3] {
    [-p[1], p[2], -p[0]]
}

/// A chunk's engine-neutral mesh (WoW axes, window-local yards) as a Bevy [`Mesh`] in the chunk
/// entity's local space (its translation = the window origin in Bevy).
pub fn monkey_voxel_mesh_to_bevy(vm: &benilla_voxel::VoxelMesh) -> Mesh {
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    let pos: Vec<[f32; 3]> = vm.positions.iter().map(|p| wow_vec_to_bevy(*p)).collect();
    let nrm: Vec<[f32; 3]> = vm.normals.iter().map(|n| wow_vec_to_bevy(*n)).collect();
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, nrm);
    mesh.insert_indices(Indices::U32(vm.indices.clone()));
    mesh
}

/// The avian trimesh collider of a chunk mesh (Bevy local space), so the player capsule can stand
/// in a dug pit or on a build. No `CollisionLayers` → `CollisionLayer::Default`.
fn monkey_voxel_collider(vm: &benilla_voxel::VoxelMesh) -> Option<Collider> {
    if vm.indices.len() < 3 {
        return None;
    }
    let verts: Vec<Vec3> = vm
        .positions
        .iter()
        .map(|p| Vec3::from_array(wow_vec_to_bevy(*p)))
        .collect();
    let tris: Vec<[u32; 3]> = vm
        .indices
        .chunks_exact(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect();
    Some(Collider::trimesh(verts, tris))
}

// ---------------------------------------------------------------------------------------------
// Per-tile materials
// ---------------------------------------------------------------------------------------------

/// One voxel material per ADT tile (the chunks wear the tile under their column).
#[derive(Resource, Default)]
pub struct MonkeyVoxelTileMaterials {
    by_tile: HashMap<(i32, i32), Handle<MonkeyVoxelMaterial>>,
    /// Tiles whose material already wears the real tileset.
    real: HashSet<(i32, i32)>,
}

/// The ADT tile key `(tile_x, tile_y)` under a WoW column.
fn tile_of(x: f32, y: f32) -> (i32, i32) {
    let (tx, ty) = benilla_formats::world_to_tile(x, y);
    (tx as i32, ty as i32)
}

/// The tile under a chunk column's centre.
fn tile_of_column(col: ColumnKey) -> (i32, i32) {
    tile_of(
        (col[0] as f32 + 0.5) * CHUNK_YARDS,
        (col[1] as f32 + 0.5) * CHUNK_YARDS,
    )
}

/// The material handle for `tile`, cloned from the template (procedural fallback) on first use.
fn tile_material(
    mats: &mut MonkeyVoxelTileMaterials,
    materials: &mut Assets<MonkeyVoxelMaterial>,
    template: &MonkeyVoxelMaterialHandle,
    tile: (i32, i32),
) -> Handle<MonkeyVoxelMaterial> {
    if let Some(h) = mats.by_tile.get(&tile) {
        return h.clone();
    }
    let h = match materials.get(&template.0).cloned() {
        Some(m) => materials.add(m),
        None => template.0.clone(),
    };
    mats.by_tile.insert(tile, h.clone());
    h
}

/// Dress each tile material in the REAL tileset of its tile once resident: the tile's own
/// `layer_array` / `alpha_array` / `shadow_array` and its full 16×16 chunk table (a hole or a
/// chunk the loader skipped is filled with the nearest drawing chunk), and bind the shared global
/// light buffer to every material once it exists.
fn monkey_voxel_tile_materials(
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    template: Res<MonkeyVoxelMaterialHandle>,
    mut mats: ResMut<MonkeyVoxelTileMaterials>,
    mut materials: ResMut<Assets<MonkeyVoxelMaterial>>,
    shared_light: Option<Res<crate::lighting::SharedLightBuffer>>,
) {
    if let Some(light) = &shared_light {
        let handles: Vec<Handle<MonkeyVoxelMaterial>> = std::iter::once(template.0.clone())
            .chain(mats.by_tile.values().cloned())
            .collect();
        for h in handles {
            let bound = materials
                .get(&h)
                .is_some_and(|m| material::monkey_voxel_has_light(&m.extension));
            if !bound {
                if let Some(mat) = materials.get_mut(&h) {
                    material::monkey_voxel_bind_light(&mut mat.extension, light.0.clone());
                }
            }
        }
    }
    let pending: Vec<((i32, i32), Handle<MonkeyVoxelMaterial>)> = mats
        .by_tile
        .iter()
        .filter(|(t, _)| !mats.real.contains(*t))
        .map(|(t, h)| (*t, h.clone()))
        .collect();
    for (tile, h) in pending {
        let Some(adt) = streamer.tile_handle(tile).and_then(|th| adts.get(&th)) else {
            continue; // not resident yet — keep the procedural fallback
        };
        let cs = benilla_formats::CHUNK_SIZE;
        let ts = benilla_formats::TILE_SIZE;
        let mc = 32.0 * ts;
        // The tile's NW corner (WoW x north, y west).
        let nw = [mc - tile.1 as f32 * ts, mc - tile.0 as f32 * ts];
        // Find chunks by their OWN NW corner (the loader skips an MCNK without MCVT, so the vec
        // index is not the grid index); usable = it draws.
        let mut grid_idx: [Option<usize>; 256] = [None; 256];
        for (i, c) in adt.chunks.iter().enumerate() {
            if c.indices.len() < 3 {
                continue;
            }
            let p = c.positions[0];
            let row = ((nw[0] - p[0]) / cs).round() as i32;
            let col = ((nw[1] - p[1]) / cs).round() as i32;
            if (0..16).contains(&row) && (0..16).contains(&col) {
                grid_idx[(row * 16 + col) as usize] = Some(i);
            }
        }
        let Some(any) = grid_idx.iter().flatten().next().copied() else {
            mats.real.insert(tile); // a tile with no drawing chunk: stay procedural
            continue;
        };
        let shade = |i: usize| {
            let s = adt.shading.get(i).copied().unwrap_or_default();
            material::MonkeyVoxelChunkShade {
                layers: s.layers,
                alpha: s.alpha,
                shadow: s.shadow,
            }
        };
        let mut grid = [shade(any); 256];
        for r in 0..16i32 {
            for c in 0..16i32 {
                // Nearest drawing chunk (Chebyshev rings) for a hole / missing slot.
                let pick = (0..16i32).find_map(|ring| {
                    for dr in -ring..=ring {
                        for dc in -ring..=ring {
                            if dr.abs().max(dc.abs()) != ring {
                                continue;
                            }
                            let (rr, cc) = (r + dr, c + dc);
                            if (0..16).contains(&rr) && (0..16).contains(&cc) {
                                if let Some(i) = grid_idx[(rr * 16 + cc) as usize] {
                                    return Some(i);
                                }
                            }
                        }
                    }
                    None
                });
                grid[(r * 16 + c) as usize] = shade(pick.unwrap_or(any));
            }
        }
        if let Some(mat) = materials.get_mut(&h) {
            material::monkey_voxel_apply_real(
                &mut mat.extension,
                adt.layer_array.clone(),
                adt.alpha_array.clone(),
                adt.shadow_array.clone(),
                &grid,
                nw,
            );
            mats.real.insert(tile);
            info!("MONKEY (voxel): tile {tile:?} voxel material now wears the tile's real tileset.");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Plugin + systems
// ---------------------------------------------------------------------------------------------

/// The world voxel layer.
pub struct MonkeyVoxelPlugin;

impl Plugin for MonkeyVoxelPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<MonkeyVoxelMaterial>::default());
        app.add_systems(Startup, material::monkey_voxel_create_material);
        app.init_resource::<MonkeyVoxelBrushRadius>();
        app.init_resource::<MonkeyVoxelNet>();
        app.init_resource::<MonkeyLocalGm>();
        app.init_resource::<MonkeyVoxelHideState>();
        app.init_resource::<MonkeyVoxelWorld>();
        app.init_resource::<MonkeyVoxelTileMaterials>();
        app.init_resource::<MonkeyVoxelPerf>();
        app.add_systems(
            Update,
            (
                monkey_voxel_clock_start,
                monkey_voxel_map_watch,
                monkey_voxel_brush_edit,
                monkey_voxel_autoedit,
                monkey_voxel_synth,
                monkey_voxel_apply_inbox,
                monkey_voxel_stream,
                monkey_voxel_tile_materials,
                // Carve the real ADT ground under the surface columns after this frame's meshes.
                monkey_voxel_hide_terrain,
                // Re-place the ground effects of carved MCNKs onto the voxel tops.
                monkey_voxel_clutter,
                monkey_voxel_probe_rays,
                monkey_voxel_journal_autosave,
                monkey_voxel_clock_end,
            )
                .chain()
                .run_if(crate::schedule::world_is_live),
        );
    }
}

/// Track the current map: on the first world entry and on every map change, drop the chunk
/// entities + state and re-activate the chunks the new map's journal edits touch (they are built
/// lazily once their tiles are resident).
fn monkey_voxel_map_watch(
    mut commands: Commands,
    map: Option<Res<crate::world_map::CurrentMap>>,
    mut world: ResMut<MonkeyVoxelWorld>,
    mut loaded: Local<bool>,
) {
    if !*loaded {
        *loaded = true;
        world.journal = monkey_voxel_journal_load();
    }
    let Some(map) = map else {
        return;
    };
    if world.map == Some(map.0) {
        return;
    }
    for (_, e) in world.entities.drain() {
        commands.entity(e).despawn();
    }
    world.clear_map_state();
    world.map = Some(map.0);
    // MONKEY (voxel): the journal index already knows every chunk an edit touches.
    let (n, keys): (usize, Vec<ChunkKey>) = world
        .journal
        .maps
        .get(&(map.0 as u16))
        .map(|j| (j.edits().len(), j.indexed_chunks().collect()))
        .unwrap_or_default();
    for k in keys {
        world.want(k);
    }
    info!(
        "MONKEY (voxel): world layer on map {} — {} journalled edit(s), {} chunk(s) to activate.",
        map.0,
        n,
        world.wanted_count
    );
}

/// The density the brush / AUTOEDIT aim through: an active chunk's voxels, else the RAW ADT ground
/// (+ the seed lift) — so the ray hits the real terrain anywhere and an existing voxel surface
/// (incl. a dug pit below the old ground) where the layer is active.
fn monkey_voxel_aim_density(
    world: &MonkeyVoxelWorld,
    streamer: &TerrainStreamer,
    adts: &Assets<AdtTile>,
    p: [f32; 3],
) -> f32 {
    if world.data.is_active(chunk_key_at(p)) {
        return world.data.density_at(p);
    }
    match crate::terrain_stream::terrain_height_under_raw(streamer, adts, wow_to_bevy(p)) {
        Some(h) => (p[2] - (h + SEED_LIFT)) / benilla_voxel::VOXEL_SIZE,
        None => 1.0,
    }
}

/// P1 brush (GM only). Hold **`[`** dig · **`]`** build · **`\`** smooth, aimed along the view ray at
/// whatever is under the crosshair — the real ADT ground or an existing voxel surface; **`-`/`=`**
/// change the brush size (0.5 yd steps). Edits are emitted at ≤ 8/s (when the brush centre moved
/// ≥ radius/2 or 125 ms passed — CONTRACT §3's uplink cadence), and EXACTLY the emitted edits are
/// applied locally, journalled and uplinked, so every client converges on the same shape.
#[allow(clippy::too_many_arguments)]
fn monkey_voxel_brush_edit(
    keys: Res<ButtonInput<KeyCode>>,
    gm: Res<MonkeyLocalGm>,
    time: Res<Time>,
    mut radius: ResMut<MonkeyVoxelBrushRadius>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    mut world: ResMut<MonkeyVoxelWorld>,
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    mut net: ResMut<MonkeyVoxelNet>,
    mut last: Local<Option<([f32; 3], f32)>>,
) {
    if !gm.0 {
        return;
    }
    if keys.just_pressed(KeyCode::Minus) {
        radius.0 = (radius.0 - 0.5).max(0.5);
        info!("MONKEY (voxel): brush radius {:.1} yd", radius.0);
    }
    if keys.just_pressed(KeyCode::Equal) {
        radius.0 = (radius.0 + 0.5).min(8.0);
        info!("MONKEY (voxel): brush radius {:.1} yd", radius.0);
    }
    let mode = if keys.pressed(KeyCode::BracketLeft) {
        0u8
    } else if keys.pressed(KeyCode::BracketRight) {
        1
    } else if keys.pressed(KeyCode::Backslash) {
        2
    } else {
        *last = None;
        return;
    };
    let Ok(cam) = camera.single() else {
        return;
    };
    let origin = bevy_to_wow(cam.translation());
    let dir = bevy_to_wow(*cam.forward());
    let Some(hit) = raymarch(origin, dir, 250.0, 0.25, |p| {
        monkey_voxel_aim_density(&world, &streamer, &adts, p)
    }) else {
        return;
    };
    let now = time.elapsed_secs();
    let r = radius.0;
    let due = last.is_none_or(|(p, t)| {
        let d = ((p[0] - hit[0]).powi(2) + (p[1] - hit[1]).powi(2) + (p[2] - hit[2]).powi(2))
            .sqrt();
        d >= r * 0.5 || now - t >= 0.125
    });
    if !due {
        return;
    }
    *last = Some((hit, now));
    let e = VoxelEditOp {
        map: 0,
        pos: hit,
        radius: r,
        mode,
        // Smooth: a firm half-strength blur per 125 ms tick; dig/build ignore it.
        strength: if mode == 2 { 128 } else { 255 },
        nonce: 0,
        seq: None,
        local: true,
    };
    world.local_edit(&mut net, e);
}

/// Test/dev hook `WOW_VOXEL_AUTOEDIT=1` (anchor = the ground under the camera's view ray, else
/// straight below it) or `WOW_VOXEL_AUTOEDIT=x,y` (a WoW anchor): ~8 s into the world, once every
/// scripted spot's ground is resident, run a scripted sequence through the SAME path as the brush
/// (journal + uplink): a 2-step crater at the anchor, a second crater 30 yd north, a dig on a
/// 16-yd chunk CORNER (spanning 2×2×2 chunks), a build mound with an overhanging lip, an 8-yd and
/// a 2-yd dig (the perf references). ~3 s after the layer has settled it then plays a HELD-BRUSH
/// stroke — 16 × 2-yd digs at the brush's 8/s cadence, 1 yd apart, crossing a chunk border — and
/// one live 8-yd dig, each logged as a `perf edit` line. Bypasses the GM gate.
#[allow(clippy::too_many_arguments)]
fn monkey_voxel_autoedit(
    time: Res<Time>,
    mut state: Local<(f32, bool)>,
    mut stroke: Local<MonkeyAutoStroke>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    mut world: ResMut<MonkeyVoxelWorld>,
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    mut net: ResMut<MonkeyVoxelNet>,
) {
    if state.1 {
        monkey_voxel_autoedit_stroke(&time, &mut stroke, &mut world, &streamer, &adts, &mut net);
        return;
    }
    let Ok(spec) = std::env::var("WOW_VOXEL_AUTOEDIT") else {
        state.1 = true;
        return;
    };
    state.0 += time.delta_secs();
    if state.0 < 8.0 || world.map.is_none() {
        return;
    }
    let Ok(cam) = camera.single() else {
        return;
    };
    let ground = |x: f32, y: f32| {
        crate::terrain_stream::terrain_height_under_raw(&streamer, &adts, wow_to_bevy([x, y, 0.0]))
    };
    let v: Vec<f32> = spec.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let anchor = if v.len() == 2 {
        [v[0], v[1]]
    } else {
        let o = bevy_to_wow(cam.translation());
        let d = bevy_to_wow(*cam.forward());
        match raymarch(o, d, 250.0, 0.25, |p| {
            monkey_voxel_aim_density(&world, &streamer, &adts, p)
        }) {
            Some(h) => [h[0], h[1]],
            None => [o[0], o[1]],
        }
    };
    // Every scripted spot must be resident before the script runs (re-tried each frame).
    let spots = [
        [anchor[0], anchor[1]],
        [anchor[0] + 30.0, anchor[1]],
        [anchor[0], anchor[1] + 30.0],
        [anchor[0] - 22.0, anchor[1]],
        [anchor[0], anchor[1] - 30.0],
    ];
    let mut zs = Vec::with_capacity(spots.len());
    for s in &spots {
        match ground(s[0], s[1]) {
            Some(z) => zs.push(z),
            None => return,
        }
    }
    state.1 = true;
    // The chunk-corner dig: snap the third spot onto a 16-yd chunk corner (x and y borders).
    let corner = [
        (spots[2][0] / CHUNK_YARDS).round() * CHUNK_YARDS,
        (spots[2][1] / CHUNK_YARDS).round() * CHUNK_YARDS,
    ];
    let zc = ground(corner[0], corner[1]).unwrap_or(zs[2]);
    // The corner dig's z on a 16-yd z border too, so it spans 2×2×2 chunks.
    let zc = ((zc / CHUNK_YARDS).round() * CHUNK_YARDS).clamp(zc - 4.0, zc + 1.0);
    let script: [(&str, [f32; 3], f32, u8); 8] = [
        ("crater", [spots[0][0], spots[0][1], zs[0]], 4.0, 0),
        ("crater (deeper)", [spots[0][0], spots[0][1], zs[0] - 3.0], 3.0, 0),
        ("crater +30 yd", [spots[1][0], spots[1][1], zs[1]], 4.0, 0),
        ("chunk-corner dig", [corner[0], corner[1], zc], 5.0, 0),
        ("mound", [spots[3][0], spots[3][1], zs[3] + 1.0], 4.0, 1),
        ("overhang lip", [spots[3][0] + 3.5, spots[3][1] + 3.5, zs[3] + 4.5], 3.0, 1),
        ("8-yd dig", [spots[4][0], spots[4][1], zs[4]], 8.0, 0),
        ("2-yd dig", [spots[0][0] + 3.0, spots[0][1], zs[0] - 1.0], 2.0, 0),
    ];
    for (name, pos, radius, mode) in script {
        let t0 = Instant::now();
        world.local_edit(
            &mut net,
            VoxelEditOp {
                map: 0,
                pos,
                radius,
                mode,
                strength: 255,
                nonce: 0,
                seq: None,
                local: true,
            },
        );
        info!(
            "MONKEY (voxel): AUTOEDIT {name} at WoW ({:.1}, {:.1}, {:.1}) r={radius} yd (ingest {:.2} ms)",
            pos[0],
            pos[1],
            pos[2],
            t0.elapsed().as_secs_f32() * 1000.0,
        );
    }
    // The probe aims under the first crater.
    world.last_dig = Some([spots[0][0], spots[0][1], zs[0]]);
    // The perf stroke: along +y from 8 yd south of the anchor, then the live 8-yd dig.
    stroke.armed = true;
    stroke.start = [spots[0][0] - 8.0, spots[0][1] - 6.0];
    stroke.big = [spots[4][0] + 4.0, spots[4][1], zs[4] - 1.0];
}

/// MONKEY (voxel): the AUTOEDIT perf stroke's progress.
#[derive(Default)]
struct MonkeyAutoStroke {
    armed: bool,
    quiet: f32,
    tick: f32,
    step: u32,
    start: [f32; 2],
    big: [f32; 3],
}

/// The AUTOEDIT held-brush stroke (see [`monkey_voxel_autoedit`]): waits for the layer to settle,
/// then emits one 2-yd dig per 125 ms (the brush cadence), 1 yd apart, and finally an 8-yd dig over
/// the already-active 8-yd crater.
fn monkey_voxel_autoedit_stroke(
    time: &Time,
    stroke: &mut MonkeyAutoStroke,
    world: &mut MonkeyVoxelWorld,
    streamer: &TerrainStreamer,
    adts: &Assets<AdtTile>,
    net: &mut MonkeyVoxelNet,
) {
    if !stroke.armed {
        return;
    }
    let settled = world.settled();
    if stroke.step == 0 && stroke.quiet < 3.0 {
        stroke.quiet = if settled { stroke.quiet + time.delta_secs() } else { 0.0 };
        return;
    }
    stroke.tick += time.delta_secs();
    if stroke.tick < 0.125 {
        return;
    }
    stroke.tick = 0.0;
    let (pos, radius) = if stroke.step < 16 {
        let (x, y) = (stroke.start[0], stroke.start[1] + stroke.step as f32);
        let Some(h) =
            crate::terrain_stream::terrain_height_under_raw(streamer, adts, wow_to_bevy([x, y, 0.0]))
        else {
            stroke.armed = false;
            return;
        };
        ([x, y, h], 2.0)
    } else {
        stroke.armed = false;
        (stroke.big, 8.0)
    };
    stroke.step += 1;
    world.local_edit(
        net,
        VoxelEditOp {
            map: 0,
            pos,
            radius,
            mode: 0,
            strength: 255,
            nonce: 0,
            seq: None,
            local: true,
        },
    );
}

/// Drain the network inboxes into the canonical path.
///
/// MONKEY (voxel): a trickle of remote edits (≤ [`LIVE_EDITS_PER_FRAME`], within
/// [`LIVE_APPLY_MS`]) is applied LIVE (incremental + hot remesh, like our own brush); a burst (a
/// SYNC reply, a resync after `X`) goes BULK — journalled and its chunks rebuilt off-thread — so the
/// frame never replays hundreds of edits inline.
fn monkey_voxel_apply_inbox(mut net: ResMut<MonkeyVoxelNet>, mut world: ResMut<MonkeyVoxelWorld>) {
    if world.map.is_none() {
        return; // keep buffering until the first world entry
    }
    let n_edits = net.inbox.len()
        + net
            .events
            .iter()
            .filter(|e| matches!(e, MonkeyVoxelNetEvent::Edit(_)))
            .count();
    let t0 = Instant::now();
    let live_for = |t0: &Instant| {
        n_edits <= LIVE_EDITS_PER_FRAME && t0.elapsed().as_secs_f32() * 1000.0 < LIVE_APPLY_MS
    };
    let edits: Vec<VoxelEditOp> = net.inbox.drain(..).collect();
    let n = edits.len();
    for mut e in edits {
        e.local = false; // MONKEY (voxel): a remote record is never our own prediction
        if e.is_valid() {
            let live = live_for(&t0);
            world.ingest(e, live);
        }
    }
    if n > 0 {
        info!("MONKEY (voxel): applied {n} remote edit(s)");
    }
    for (nonce, seq) in std::mem::take(&mut net.confirms) {
        if let Some(map) = world.current_map_u16() {
            world.confirm(map, nonce, seq);
        }
    }
    for (map, rx, ry, epoch) in std::mem::take(&mut net.resets) {
        world.reset_region(map, rx, ry, epoch);
    }
    for (map, rx, ry, epoch, last_seq) in std::mem::take(&mut net.region_done) {
        world.region_done(map, rx, ry, epoch, last_seq);
    }
    // MONKEY (voxel): the server lane, in arrival order.
    let events = std::mem::take(&mut net.events);
    if events.is_empty() {
        return;
    }
    let (mut applied, mut dup, mut confirmed) = (Vec::new(), 0usize, Vec::new());
    let mut bulk = 0usize;
    let mut resets: Vec<(u16, i16, i16, u16)> = Vec::new();
    for ev in events {
        match ev {
            MonkeyVoxelNetEvent::Edit(mut e) => {
                e.local = false;
                if !e.is_valid() {
                    continue;
                }
                let live = live_for(&t0);
                bulk += !live as usize;
                match world.ingest(e, live) {
                    JournalInsert::Duplicate => dup += 1,
                    _ => applied.push(e.seq.unwrap_or(0)),
                }
            }
            MonkeyVoxelNetEvent::Confirm { map, nonce, seq } => {
                world.confirm(map, nonce, seq);
                confirmed.push(seq);
            }
            MonkeyVoxelNetEvent::Reset { map, rx, ry, epoch } => {
                world.reset_region(map, rx, ry, epoch);
                // MONKEY (voxel): one summary line below — a log line per region measured ~1 ms
                // each on a redirected console (a 9-region resync was a 16 ms frame).
                resets.push((map, rx, ry, epoch));
            }
            MonkeyVoxelNetEvent::SyncPoint {
                map,
                rx,
                ry,
                epoch,
                last_seq,
            } => world.region_done(map, rx, ry, epoch, last_seq),
            MonkeyVoxelNetEvent::DropLocal { map, nonce } => {
                if let Some(e) = world.drop_local(map, nonce) {
                    info!(
                        "MONKEY (voxel): dropped local prediction nonce {nonce} at WoW ({:.1}, \
                         {:.1}, {:.1}) r={} — chunks rebuilding",
                        e.pos[0], e.pos[1], e.pos[2], e.radius
                    );
                }
            }
        }
    }
    if !applied.is_empty() || dup > 0 {
        if applied.len() > 16 {
            info!(
                "MONKEY (voxel): server edits APPLIED: {} (seq {}..={}, {bulk} via the bulk \
                 rebuild path; {} duplicate(s) skipped) in {:.2} ms",
                applied.len(),
                applied.iter().min().unwrap_or(&0),
                applied.iter().max().unwrap_or(&0),
                dup,
                t0.elapsed().as_secs_f32() * 1000.0
            );
        } else {
            info!(
                "MONKEY (voxel): server edits APPLIED seq {:?} ({} duplicate(s) skipped)",
                applied, dup
            );
        }
    }
    if !confirmed.is_empty() {
        info!("MONKEY (voxel): own edits CONFIRMED by seq {confirmed:?} (not re-applied)");
    }
    if !resets.is_empty() {
        info!(
            "MONKEY (voxel): {} region(s) RESET (map, rx, ry, epoch) {:?} — their journal dropped, \
             chunks rebuilding",
            resets.len(),
            resets
        );
    }
}

/// MONKEY (voxel): a canonical build picked by [`MonkeyVoxelWorld::take_build_jobs`].
struct MonkeyBuildDispatch {
    keys: Vec<ChunkKey>,
    gens: Vec<u64>,
    hot: bool,
    job: benilla_voxel::world::VoxelBuildJob,
}

/// MONKEY (voxel): a build running on the async compute pool → (densities, run ms).
struct MonkeyBuildTask {
    keys: Vec<ChunkKey>,
    gens: Vec<u64>,
    hot: bool,
    task: Task<(Vec<(ChunkKey, Box<[i8]>)>, f32)>,
}

/// MONKEY (voxel): a remesh picked by [`MonkeyVoxelWorld::take_mesh_jobs`] (its window snapshot).
struct MonkeyMeshDispatch {
    key: ChunkKey,
    gen: u64,
    hot: bool,
    window: Vec<i8>,
    /// MONKEY (voxel): the sides that get a skirt ([`MonkeyVoxelWorld::skirt_sides`]).
    skirts: [bool; 4],
}

/// MONKEY (voxel): what a mesh task hands back: the Bevy mesh + its AABB + the trimesh collider
/// (all built off-thread), or nothing for an empty surface.
struct MonkeyMeshOut {
    mesh: Option<(Mesh, Option<Aabb>)>,
    collider: Option<Collider>,
    tris: usize,
}

struct MonkeyMeshTask {
    key: ChunkKey,
    gen: u64,
    hot: bool,
    task: Task<MonkeyMeshOut>,
}

/// MONKEY (voxel): a finished mesh waiting for the attach budget.
struct MonkeyMeshReady {
    key: ChunkKey,
    gen: u64,
    hot: bool,
    out: MonkeyMeshOut,
}

/// MONKEY (voxel): the off-thread half of a remesh: surface nets over the snapshot, the Bevy mesh,
/// its AABB and the parry trimesh collider (the expensive BVH build).
fn monkey_voxel_mesh_job(window: &[i8], skirts: [bool; 4]) -> MonkeyMeshOut {
    let mut vm = benilla_voxel::world::mesh_window(window);
    benilla_voxel::world::add_skirts(&mut vm, skirts, SKIRT_DEPTH);
    if vm.indices.len() < 3 {
        return MonkeyMeshOut {
            mesh: None,
            collider: None,
            tris: 0,
        };
    }
    let mesh = monkey_voxel_mesh_to_bevy(&vm);
    let aabb = mesh.compute_aabb();
    MonkeyMeshOut {
        collider: monkey_voxel_collider(&vm),
        tris: vm.tri_count(),
        mesh: Some((mesh, aabb)),
    }
}

/// The stream: load column seeds once their tiles are resident (→ surface-column activation),
/// build wanted chunks (seed + their journal edits in canonical order), rebuild out-of-order ones,
/// remesh hot then dirty chunks, spawn / update the chunk entities (mesh + per-tile material +
/// collider), and republish the carve when the set of fully-meshed surface columns changes.
///
/// MONKEY (voxel): builds (seed + indexed journal replay) and remeshes (surface nets + Bevy mesh +
/// trimesh collider) run on the ASYNC COMPUTE POOL; the main thread only snapshots inputs
/// ([`DISPATCH_MS`]), adopts finished builds (generation-checked) and attaches finished meshes
/// ([`ATTACH_MS`], newest generation wins).
#[allow(clippy::too_many_arguments)]
fn monkey_voxel_stream(
    mut commands: Commands,
    mut world: ResMut<MonkeyVoxelWorld>,
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<MonkeyVoxelMaterial>>,
    template: Res<MonkeyVoxelMaterialHandle>,
    mut tile_mats: ResMut<MonkeyVoxelTileMaterials>,
    handles: Query<&Mesh3d, With<MonkeyVoxelChunk>>,
    epoch: Option<ResMut<crate::collision::ColliderEpoch>>,
) {
    let world = &mut *world;
    let t_stream = Instant::now();
    let mut t_phase = t_stream;
    // 1. Column seeds (raw ADT heights) for columns whose tiles are now resident.
    if !world.need_seed.is_empty() {
        let resident = |x: f32, y: f32| {
            streamer
                .tile_handle(tile_of(x, y))
                .is_some_and(|h| adts.get(&h).is_some())
        };
        let cols: Vec<ColumnKey> = world.need_seed.iter().copied().collect();
        let mut done = 0;
        for col in cols {
            if done >= SEEDS_PER_FRAME {
                break;
            }
            let (x0, y0) = (
                col[0] as f32 * CHUNK_YARDS - 1.0,
                col[1] as f32 * CHUNK_YARDS - 1.0,
            );
            let (x1, y1) = (x0 + CHUNK_YARDS + 2.0, y0 + CHUNK_YARDS + 2.0);
            if ![(x0, y0), (x0, y1), (x1, y0), (x1, y1)]
                .iter()
                .all(|&(x, y)| resident(x, y))
            {
                continue;
            }
            let mut cache = None;
            let seed = ColumnSeed::from_fn(col, |x, y| {
                crate::terrain_stream::terrain_height_under_raw_cached(
                    &streamer,
                    &adts,
                    wow_to_bevy([x, y, 0.0]),
                    &mut cache,
                )
            });
            world.data.seeds.insert(col, Arc::new(seed));
            world.need_seed.remove(&col);
            world.expand_column(col);
            done += 1;
        }
    }

    world.phase_ms[0] = t_phase.elapsed().as_secs_f32() * 1000.0;
    t_phase = Instant::now();
    // 2. Finished builds: adopt the fresh ones (→ remesh around them), requeue the stale ones.
    let mut i = 0;
    while i < world.builds.len() {
        let done = block_on(future::poll_once(&mut world.builds[i].task));
        let Some((out, run_ms)) = done else {
            i += 1;
            continue;
        };
        let t = world.builds.swap_remove(i);
        let n = out.len();
        let adopted = world.finish_build(&t.keys, &t.gens, t.hot, out);
        if world.timing_logs > 0 && (t.hot || !adopted) {
            world.timing_logs -= 1;
            info!(
                "MONKEY (voxel): build of {n} chunk(s) {} — {run_ms:.2} ms off-thread{}",
                if adopted { "adopted" } else { "DISCARDED (stale: newer edits)" },
                if t.hot { " (hot)" } else { "" }
            );
        }
    }

    world.phase_ms[1] = t_phase.elapsed().as_secs_f32() * 1000.0;
    t_phase = Instant::now();
    // 3. Dispatch builds (hot first; bulk under the in-flight cap + dispatch budget).
    let dispatch_deadline = t_stream + std::time::Duration::from_secs_f32(DISPATCH_MS / 1000.0);
    let pool = AsyncComputeTaskPool::get();
    for d in world.take_build_jobs(dispatch_deadline) {
        let job = d.job;
        let task = pool.spawn(async move {
            let t = Instant::now();
            let out = job.run();
            (out, t.elapsed().as_secs_f32() * 1000.0)
        });
        world.builds.push(MonkeyBuildTask {
            keys: d.keys,
            gens: d.gens,
            hot: d.hot,
            task,
        });
    }

    world.phase_ms[2] = t_phase.elapsed().as_secs_f32() * 1000.0;
    t_phase = Instant::now();
    // 4. Finished meshes → the ready map (newest per chunk).
    let mut i = 0;
    while i < world.meshing.len() {
        let Some(out) = block_on(future::poll_once(&mut world.meshing[i].task)) else {
            i += 1;
            continue;
        };
        let t = world.meshing.swap_remove(i);
        world.mesh_returned(MonkeyMeshReady {
            key: t.key,
            gen: t.gen,
            hot: t.hot,
            out,
        });
    }

    // 5. Attach under the budget: hot results first; a result older than what the chunk shows is
    //    dropped (supersession).
    let t_attach = Instant::now();
    let mut order: Vec<(bool, ChunkKey)> = world
        .mesh_ready
        .values()
        .map(|r| (!r.hot, r.key))
        .collect();
    order.sort();
    let (mut attached, mut hot_attached, mut tris_n) = (0usize, 0usize, 0usize);
    let mut colliders = false;
    // MONKEY (voxel): a live edit's hot meshes land TOGETHER (neighbours meshed from different
    // density versions would show a seam for a frame): held while a sibling hot task is still on
    // the pool, at most [`HOT_HOLD_FRAMES`]. They are exempt from the triangle budget.
    let hold_hot = world.meshing.iter().any(|t| t.hot) && world.hot_hold < HOT_HOLD_FRAMES;
    world.hot_hold = if hold_hot { world.hot_hold + 1 } else { 0 };
    let mut bulk_tris = 0usize;
    for (bulk, k) in order {
        if bulk
            && attached > 0
            && (t_attach.elapsed().as_secs_f32() * 1000.0 >= ATTACH_MS
                || bulk_tris >= ATTACH_TRIS)
        {
            break;
        }
        if !bulk && hold_hot {
            continue;
        }
        let Some(r) = world.mesh_ready.remove(&k) else {
            continue;
        };
        if bulk {
            bulk_tris += r.out.tris;
        }
        if !world.mesh_is_fresh(k, r.gen) {
            world.stale_meshes += 1;
            if !world.mesh_in_flight.contains_key(&k) && !world.hot.contains(&k) {
                world.hot_wait.remove(&k);
            }
            continue;
        }
        world.shown_gen.insert(k, r.gen);
        world.meshed.insert(k);
        world.carve_dirty = true;
        *world.clutter_col_gen.entry([k[0], k[1]]).or_default() += 1;
        world.clutter_epoch += 1;
        world.hot_wait.remove(&k);
        world.meshed_total += 1;
        attached += 1;
        hot_attached += r.hot as usize;
        tris_n += r.out.tris;
        let existing = world.entities.get(&k).copied();
        let Some((mesh, aabb)) = r.out.mesh else {
            if let Some(e) = existing {
                commands.entity(e).despawn();
                world.entities.remove(&k);
            }
            continue;
        };
        colliders |= r.out.collider.is_some();
        match existing.and_then(|e| handles.get(e).ok().map(|h| (e, h.0.clone()))) {
            Some((e, handle)) => {
                let _ = meshes.insert(&handle, mesh);
                let mut ec = commands.entity(e);
                if let Some(a) = aabb {
                    ec.insert(a);
                }
                match r.out.collider {
                    Some(c) => {
                        ec.insert(c);
                    }
                    None => {
                        ec.remove::<Collider>();
                    }
                }
            }
            None => {
                let mat = tile_material(
                    &mut tile_mats,
                    &mut materials,
                    &template,
                    tile_of_column([k[0], k[1]]),
                );
                let mut ec = commands.spawn((
                    Mesh3d(meshes.add(mesh)),
                    MeshMaterial3d(mat),
                    Transform::from_translation(wow_to_bevy(window_origin(k))),
                    MonkeyVoxelChunk(k),
                    RigidBody::Static,
                    Name::new(format!("MONKEY voxel chunk {k:?}")),
                ));
                if let Some(a) = aabb {
                    ec.insert(a);
                }
                if let Some(c) = r.out.collider {
                    ec.insert(c);
                }
                world.entities.insert(k, ec.id());
            }
        }
    }
    // A collider inserted outside the streamer's attach queue must bump the epoch (cached
    // collision answers hold only at the stamp they were computed at).
    if colliders {
        if let Some(mut epoch) = epoch {
            epoch.bump();
        }
    }
    let attach_ms = t_attach.elapsed().as_secs_f32() * 1000.0;

    world.phase_ms[3] = t_phase.elapsed().as_secs_f32() * 1000.0;
    t_phase = Instant::now();
    // 6. Dispatch remeshes (hot first; bulk under the in-flight cap + dispatch budget).
    let mut dispatched = 0usize;
    for d in world.take_mesh_jobs(dispatch_deadline) {
        let (window, skirts) = (d.window, d.skirts);
        let task = pool.spawn(async move { monkey_voxel_mesh_job(&window, skirts) });
        world.meshing.push(MonkeyMeshTask {
            key: d.key,
            gen: d.gen,
            hot: d.hot,
            task,
        });
        dispatched += 1;
    }
    world.phase_ms[4] = t_phase.elapsed().as_secs_f32() * 1000.0;
    if attached > 0 {
        if world.timing_logs > 0 && hot_attached > 0 {
            world.timing_logs -= 1;
            info!(
                "MONKEY (voxel): attached {attached} chunk mesh(es) ({hot_attached} hot, {tris_n} \
                 tris) in {attach_ms:.2} ms; {dispatched} remesh(es) dispatched; {} active, {} \
                 queued, {} meshing, {} building",
                world.data.chunks.len(),
                world.dirty.len() + world.hot.len(),
                world.meshing.len(),
                world.builds.len()
            );
        } else {
            debug!("MONKEY (voxel): attached {attached} chunk mesh(es) in {attach_ms:.2} ms");
        }
    }

    // MONKEY (voxel): one perf line per live local edit (ingest + this frame's dispatch work),
    // and the hot drain — how many frames until a live edit's chunks show their new meshes, and
    // the worst stream frame meanwhile (the held-brush number).
    let frame_ms_now = t_stream.elapsed().as_secs_f32() * 1000.0;
    if !world.perf.is_empty() || world.perf_drain.is_some() {
        let (frames, worst) = world.perf_drain.unwrap_or((0, 0.0));
        world.perf_drain = Some((frames + 1, worst.max(frame_ms_now)));
        if world.hot.is_empty() && world.hot_wait.is_empty() && world.hot_build.is_empty() {
            let (frames, worst) = world.perf_drain.take().unwrap_or_default();
            info!(
                "MONKEY (voxel): perf drain — live edit shown after {frames} frame(s), worst \
                 stream frame {worst:.2} ms"
            );
        }
    }
    if !world.perf.is_empty() {
        // MONKEY (voxel): ONE line per frame (a line per edit cost ~1 ms each on a redirected
        // console — the logging itself showed up as the burst frame's hitch).
        let edits = std::mem::take(&mut world.perf);
        let radii: Vec<String> = edits.iter().map(|(r, _)| format!("{r:.1}")).collect();
        let ingest_ms: f32 = edits.iter().map(|(_, ms)| ms).sum();
        info!(
            "MONKEY (voxel): perf edit x{} r=[{}] yd: ingest {ingest_ms:.2} ms + stream \
             {frame_ms_now:.2} ms ({dispatched} remesh(es) dispatched, {attached} attached, {} \
             active)",
            edits.len(),
            radii.join(","),
            world.data.chunks.len()
        );
    }

    let t_phase = Instant::now();
    // 7. Carve = the surface columns whose surface chunks are all meshed. During a bulk replay it
    //    waits for the queues to drain, so the tile collider / render cells are re-clipped once,
    //    not once per finished column.
    if world.carve_dirty && (world.bulk_idle() || hot_attached > 0) {
        world.carve_dirty = false;
        let cols: BTreeSet<ColumnKey> = world
            .wanted
            .iter()
            .filter_map(|(col, kzs)| {
                let (lo, hi) = world.data.seeds.get(col)?.surface_chunks()?;
                let surface = (lo..=hi)
                    .all(|kz| kzs.contains(&kz) && world.meshed.contains(&[col[0], col[1], kz]));
                surface.then_some(*col)
            })
            .collect();
        if cols != world.carve_cols {
            world.carve = (!cols.is_empty()).then(|| {
                Arc::new(VoxelCarve::from_columns(
                    cols.iter().map(|c| (c[0], c[1])),
                    CHUNK_YARDS,
                    CARVE_INSET,
                ))
            });
            info!(
                "MONKEY (voxel): carve now covers {} surface column(s) ({} active chunk(s)).",
                cols.len(),
                world.data.chunks.len()
            );
            world.carve_cols = cols;
            world.clutter_epoch += 1;
            if std::env::var_os("WOW_VOXEL_COVERAGE").is_some() {
                monkey_voxel_coverage_audit(world);
            }
        }
    }
    world.phase_ms[5] = t_phase.elapsed().as_secs_f32() * 1000.0;
    world.phase_ms[6] = t_stream.elapsed().as_secs_f32() * 1000.0;
}

/// MONKEY (voxel): a ground-effect tuft re-placed onto the voxel layer grows only on a top whose
/// normal is at least this upright — where the voxel shader draws the ground layers (its top
/// weight is `smoothstep(0.5, 0.7, n.z)`), not the wall texture.
const CLUTTER_MIN_UP: f32 = 0.65;
/// How far above / below its ADT height a tuft looks for its voxel top (yd).
const CLUTTER_REACH: f32 = 24.0;
/// Frame budget (ms) for re-placing MCNK clutter sets (~1–3 ms each); at least one per frame,
/// the rest wait for the next frame.
const CLUTTER_MS: f32 = 2.0;

/// MONKEY (voxel): the signature of the voxel state an MCNK's clutter must be re-placed for — 0
/// when the carve does not reach its XY box (the ADT scatter stands as is), else the rectangles
/// reaching it + the mesh generations of the columns under it.
fn clutter_sig(world: &MonkeyVoxelWorld, lo: [f32; 2], hi: [f32; 2]) -> u64 {
    let Some(carve) = world.carve.as_ref() else {
        return 0;
    };
    let base = carve.sig_for_box(lo, hi);
    if base == 0 {
        return 0;
    }
    let mut h = base;
    let c = |v: f32| (v / CHUNK_YARDS).floor() as i32;
    for kx in c(lo[0])..=c(hi[0]) {
        for ky in c(lo[1])..=c(hi[1]) {
            let g = world.clutter_col_gen.get(&[kx, ky]).copied().unwrap_or(0);
            h = (h ^ g.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((kx as u64) << 32 | ky as u32 as u64))
                .wrapping_mul(0x100000001b3);
        }
    }
    h | 1
}

/// MONKEY (voxel): one MCNK's ADT scatter re-placed onto the voxel layer. A tuft outside the carve
/// keeps its place; inside it, it moves onto the voxel TOP nearest its old height (a dug pit's
/// floor, a mound's crown, the untouched surface 0.15 yd above the ADT) and takes that top's
/// normal — Blizzlike: grass grows on the ground layer's tops, the same MCNK layer the voxel
/// shader draws there — and is DROPPED where the column has no top (a pit wall, inside a mound's
/// flank, air).
fn clutter_replace(
    data: &VoxelWorldData,
    carve: &VoxelCarve,
    raw: &[(String, Vec<crate::clutter::ShadedPlacement>)],
) -> Vec<(String, Vec<crate::clutter::ShadedPlacement>)> {
    raw.iter()
        .map(|(model, ps)| {
            let kept = ps
                .iter()
                .filter_map(|sp| {
                    let [x, y, z] = sp.placement.position;
                    if !carve.contains_strict(x, y) {
                        return Some(sp.clone());
                    }
                    let (top, n) = data.top_near(x, y, z, CLUTTER_REACH, CLUTTER_MIN_UP)?;
                    let mut sp = sp.clone();
                    sp.placement.position[2] = top;
                    sp.ground_normal = wow_to_bevy(n).to_array();
                    Some(sp)
                })
                .collect::<Vec<_>>();
            (model.clone(), kept)
        })
        .filter(|(_, ps)| !ps.is_empty())
        .collect()
}

/// MONKEY (voxel): keep the terrain's ground effects (grass / clutter tufts) on the voxel layer.
/// The ADT scatter stays at the OLD ground height, so over a dug pit it floats and inside a raised
/// mound it is buried. Each MCNK's clutter whose box the carve reaches is re-placed
/// ([`clutter_replace`]) when the carve or a column's voxel meshes under it change, its built
/// meshes dropped (`stream_chunk_clutter` rebuilds them); it is restored when the carve leaves.
fn monkey_voxel_clutter(
    mut commands: Commands,
    world: Res<MonkeyVoxelWorld>,
    mut chunks: Query<&mut crate::clutter::ClutterChunk>,
    mut seen_epoch: Local<u64>,
    mut backlog: Local<bool>,
    mut logs: Local<u32>,
) {
    // A freshly streamed tile's chunks need a look too (the carve may already reach them).
    if *seen_epoch == world.clutter_epoch
        && !*backlog
        && (world.carve.is_none() || !chunks.iter_mut().any(|c| c.is_added()))
    {
        return;
    }
    *seen_epoch = world.clutter_epoch;
    *backlog = false;
    if *logs == 0 && world.carve.is_some() && std::env::var_os("WOW_VOXEL_COVERAGE").is_some() {
        info!(
            "MONKEY (voxel): ground effects — {} MCNK clutter set(s) resident",
            chunks.iter().count()
        );
    }
    let t0 = Instant::now();
    let mut done = 0;
    for mut cc in &mut chunks {
        // Bevy → WoW: wow_x = -bevy_z, wow_y = -bevy_x.
        let (blo, bhi) = cc.bounds;
        let lo = [-bhi.z, -bhi.x];
        let hi = [-blo.z, -blo.x];
        let sig = clutter_sig(&world, lo, hi);
        if sig == cc.voxel_sig {
            continue;
        }
        if done > 0 && t0.elapsed().as_secs_f32() * 1000.0 >= CLUTTER_MS {
            *backlog = true;
            break;
        }
        done += 1;
        let raw = match cc.raw_models.take() {
            Some(r) => r,
            None => std::mem::take(&mut cc.models),
        };
        match world.carve.as_ref().filter(|_| sig != 0) {
            Some(carve) => {
                cc.models = clutter_replace(&world.data, carve, &raw);
                if *logs < 24 {
                    *logs += 1;
                    let n = |m: &[(String, Vec<crate::clutter::ShadedPlacement>)]| {
                        m.iter().map(|(_, p)| p.len()).sum::<usize>()
                    };
                    info!(
                        "MONKEY (voxel): ground effects re-placed on the voxel tops — {} of {} \
                         tuft(s) kept",
                        n(&cc.models),
                        n(&raw)
                    );
                }
                cc.raw_models = Some(raw);
            }
            None => {
                cc.models = raw;
            }
        }
        cc.voxel_sig = sig;
        for e in cc.built.drain(..) {
            commands.entity(e).try_despawn();
        }
    }
}

/// MONKEY (voxel): `WOW_VOXEL_COVERAGE=1` dev audit — every point (0.1-yd grid) the carve removes
/// the real ground from must be covered (XY) by a voxel triangle, or the sky shows through. Logs
/// the gap count and a sample of the gap points with their column's surface chunks.
fn monkey_voxel_coverage_audit(world: &MonkeyVoxelWorld) {
    let Some(carve) = world.carve.as_ref() else {
        return;
    };
    let meshes: Vec<(ChunkKey, benilla_voxel::VoxelMesh)> = world
        .data
        .chunks
        .keys()
        .map(|&k| (k, world.data.mesh(k)))
        .collect();
    let cols: Vec<ColumnKey> = world.carve_cols.iter().copied().collect();
    let gaps = benilla_voxel::world::xy_coverage_gaps(&meshes, &cols, 0.1, |x, y| {
        carve.contains_strict(x, y)
    });
    info!(
        "MONKEY (voxel): coverage audit — {} carved column(s), {} gap point(s) (0.1 yd grid).",
        cols.len(),
        gaps.len()
    );
    for (col, kzs) in &world.wanted {
        if world.carve_cols.contains(col) {
            continue;
        }
        let surf = world.data.seeds.get(col).and_then(|s| s.surface_chunks());
        let meshed: Vec<i32> = kzs
            .iter()
            .copied()
            .filter(|&kz| world.meshed.contains(&[col[0], col[1], kz]))
            .collect();
        info!(
            "MONKEY (voxel):   NOT carved: col {col:?} surface {surf:?} wanted kz {kzs:?} meshed {meshed:?}"
        );
    }
    for g in gaps.iter().step_by((gaps.len() / 40).max(1)) {
        let col = [
            (g[0] / CHUNK_YARDS).floor() as i32,
            (g[1] / CHUNK_YARDS).floor() as i32,
        ];
        let surf = world.data.seeds.get(&col).and_then(|s| s.surface_chunks());
        let kzs: Vec<i32> = world
            .wanted
            .get(&col)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        let d = |z: f32| world.data.density_at([g[0], g[1], z]);
        let z_hit = (0..400)
            .map(|i| 140.0 - i as f32 * 0.25)
            .find(|&z| d(z) < 0.0);
        info!(
            "MONKEY (voxel):   gap at ({:.2}, {:.2}) col {col:?} surface {surf:?} active kz {kzs:?} \
             first solid z {z_hit:?}",
            g[0], g[1]
        );
    }
}

/// MONKEY (voxel): `WOW_VOXEL_PROBE_RAY=1` — every 2 s (10 times) after a local dig, cast a
/// PHYSICS ray straight down at the dig's centre and log what it hits (a voxel chunk, the tile's
/// terrain trimesh, or other) against the raw ADT height: PASS = a voxel below the old ground and
/// no ADT height inside the carve.
fn monkey_voxel_probe_rays(
    time: Res<Time>,
    spatial: SpatialQuery,
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    world: Res<MonkeyVoxelWorld>,
    chunks: Query<(), With<MonkeyVoxelChunk>>,
    mut probe: Local<(f32, u8)>,
) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("WOW_VOXEL_PROBE_RAY").is_ok_and(|v| v == "1")) {
        return;
    }
    let Some(dig) = world.last_dig else {
        return;
    };
    if probe.1 >= 10 {
        return;
    }
    probe.0 += time.delta_secs();
    if probe.0 < 2.0 {
        return;
    }
    probe.0 = 0.0;
    probe.1 += 1;
    let point = wow_to_bevy(dig);
    let origin = point + Vec3::Y * 60.0;
    let tile_root = streamer.tile_entity(tile_of(dig[0], dig[1]));
    let kind = |e: Entity| {
        if chunks.contains(e) {
            "voxel"
        } else if tile_root == Some(e) {
            "terrain-tile"
        } else {
            "other"
        }
    };
    let height = crate::terrain_stream::terrain_height_under(&streamer, &adts, point);
    let raw = crate::terrain_stream::terrain_height_under_raw(&streamer, &adts, point);
    let filter = SpatialQueryFilter::default();
    let ground = spatial.cast_ray_predicate(origin, Dir3::NEG_Y, 200.0, true, &filter, &|e| {
        kind(e) != "other"
    });
    let hit = ground.map(|h| (kind(h.entity), origin.y - h.distance));
    let pass = matches!((hit, raw), (Some(("voxel", y)), Some(r)) if y < r - 0.5) && height.is_none();
    info!(
        "MONKEY (voxel): probe {} under the dig at WoW ({:.1}, {:.1}): ground = {:?}; \
         terrain_height_under={:?} (raw ADT {:?}); {} active chunk(s)",
        if pass { "PASS" } else { "FAIL" },
        dig[0],
        dig[1],
        hit,
        height,
        raw,
        world.data.chunks.len()
    );
}

//// MONKEY (voxel): the voxel layer's own per-frame clock (its whole system chain, start → end) and
/// the busy-period tracker behind the `perf settle` line: from the first frame with work queued /
/// in flight to the frame the layer is [`MonkeyVoxelWorld::settled`] again — frames, wall time,
/// worst voxel-attributable frame, worst whole frame (`Time::delta`).
#[derive(Resource, Default)]
pub struct MonkeyVoxelPerf {
    frame_t0: Option<Instant>,
    /// MONKEY (voxel): this frame's journal-save (encode + hand-off) ms.
    save_ms: f32,
    busy: Option<MonkeyVoxelBusy>,
    hitch_logs: u32,
}

struct MonkeyVoxelBusy {
    since: Instant,
    frames: u32,
    worst_ms: f32,
    worst_stream_ms: f32,
    sum_ms: f32,
    worst_dt_ms: f32,
    over8: u32,
    over16: u32,
}

fn monkey_voxel_clock_start(mut perf: ResMut<MonkeyVoxelPerf>, mut world: ResMut<MonkeyVoxelWorld>) {
    perf.frame_t0 = Some(Instant::now());
    perf.save_ms = 0.0;
    world.phase_ms = [0.0; 7];
}

fn monkey_voxel_clock_end(
    time: Res<Time>,
    world: Res<MonkeyVoxelWorld>,
    mut perf: ResMut<MonkeyVoxelPerf>,
) {
    let Some(t0) = perf.frame_t0.take() else {
        return;
    };
    let ms = t0.elapsed().as_secs_f32() * 1000.0;
    let dt = time.delta_secs() * 1000.0;
    let busy = !world.settled();
    if ms > 8.0 && perf.hitch_logs < 30 {
        perf.hitch_logs += 1;
        info!(
            "MONKEY (voxel): perf hitch — voxel frame {ms:.2} ms (whole frame {dt:.1} ms), {} \
             active; stream {:.2} ms = seeds {:.2} + build poll {:.2} + build dispatch {:.2} + \
             mesh attach {:.2} + mesh dispatch {:.2} + carve {:.2}; journal save {:.2} ms",
            world.data.chunks.len(),
            world.phase_ms[6],
            world.phase_ms[0],
            world.phase_ms[1],
            world.phase_ms[2],
            world.phase_ms[3],
            world.phase_ms[4],
            world.phase_ms[5],
            perf.save_ms
        );
    }
    if busy && perf.busy.is_none() {
        perf.busy = Some(MonkeyVoxelBusy {
            since: t0,
            frames: 0,
            worst_ms: 0.0,
            worst_stream_ms: 0.0,
            sum_ms: 0.0,
            worst_dt_ms: 0.0,
            over8: 0,
            over16: 0,
        });
    }
    let Some(b) = perf.busy.as_mut() else {
        return;
    };
    b.frames += 1;
    b.worst_ms = b.worst_ms.max(ms);
    b.worst_stream_ms = b.worst_stream_ms.max(world.phase_ms[6]);
    b.sum_ms += ms;
    b.worst_dt_ms = b.worst_dt_ms.max(dt);
    b.over8 += (ms > 8.0) as u32;
    b.over16 += (ms > 16.0) as u32;
    if !busy {
        let b = perf.busy.take().unwrap();
        info!(
            "MONKEY (voxel): perf settle — {} frame(s), {:.0} ms to settle; voxel frame worst \
             {:.2} ms / mean {:.2} ms ({} > 8 ms, {} > 16 ms), worst stream pass {:.2} ms; worst \
             whole frame {:.1} ms; {} active chunk(s)",
            b.frames,
            b.since.elapsed().as_secs_f32() * 1000.0,
            b.worst_ms,
            b.sum_ms / b.frames.max(1) as f32,
            b.over8,
            b.over16,
            b.worst_stream_ms,
            b.worst_dt_ms,
            world.data.chunks.len()
        );
    }
}

/// MONKEY (voxel): bench hook `WOW_VOXEL_SYNTH_EDITS=<n>[,<span yd>]` — ~6 s into the world, once
/// the ground under the anchor (`WOW_VOXEL_SYNTH_AT=x,y`, else the ground under the camera's view
/// ray) and the square's corners is resident, file `n` deterministic confirmed edits
/// ([`benilla_voxel::world::synth_edits`], default span 100 yd) into the journal and activate their
/// chunks exactly like entering an edited area (the bulk replay path). With
/// `WOW_VOXEL_SYNTH_RESYNC=1`, ~4 s after that settles, every region of the square is RESET and its
/// edits re-arrive as server `E` events through the inbox (the resync path).
#[allow(clippy::too_many_arguments)]
fn monkey_voxel_synth(
    time: Res<Time>,
    mut state: Local<(f32, u8, Vec<VoxelEditOp>)>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    mut world: ResMut<MonkeyVoxelWorld>,
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    mut net: ResMut<MonkeyVoxelNet>,
) {
    // state.1: 0 = waiting, 1 = filed (resync pending), 2 = done.
    if state.1 == 2 {
        return;
    }
    let Ok(spec) = std::env::var("WOW_VOXEL_SYNTH_EDITS") else {
        state.1 = 2;
        return;
    };
    state.0 += time.delta_secs();
    let Some(map) = world.current_map_u16() else {
        return;
    };
    if state.1 == 1 {
        if !std::env::var("WOW_VOXEL_SYNTH_RESYNC").is_ok_and(|v| v == "1") {
            state.1 = 2;
            return;
        }
        if !world.settled() {
            state.0 = 0.0;
            return;
        }
        if state.0 < 4.0 {
            return;
        }
        state.1 = 2;
        let edits = std::mem::take(&mut state.2);
        let regions: BTreeSet<(i16, i16)> = edits.iter().map(|e| e.region()).collect();
        for &(rx, ry) in &regions {
            net.events.push(MonkeyVoxelNetEvent::Reset {
                map,
                rx,
                ry,
                epoch: 2,
            });
        }
        for e in &edits {
            net.events.push(MonkeyVoxelNetEvent::Edit(*e));
        }
        info!(
            "MONKEY (voxel): SYNTH resync — {} region(s) reset, {} edit(s) re-arriving as server E",
            regions.len(),
            edits.len()
        );
        return;
    }
    if state.0 < 6.0 {
        return;
    }
    let v: Vec<f32> = spec.split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let n = v.first().copied().unwrap_or(0.0) as usize;
    let span = v.get(1).copied().unwrap_or(100.0);
    let ground = |x: f32, y: f32| {
        crate::terrain_stream::terrain_height_under_raw(&streamer, &adts, wow_to_bevy([x, y, 0.0]))
    };
    let at: Vec<f32> = std::env::var("WOW_VOXEL_SYNTH_AT")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let anchor = if at.len() == 2 {
        [at[0], at[1]]
    } else {
        let Ok(cam) = camera.single() else {
            return;
        };
        let o = bevy_to_wow(cam.translation());
        let d = bevy_to_wow(*cam.forward());
        match raymarch(o, d, 250.0, 0.25, |p| {
            monkey_voxel_aim_density(&world, &streamer, &adts, p)
        }) {
            Some(h) => [h[0], h[1]],
            None => [o[0], o[1]],
        }
    };
    let h = span * 0.5;
    let mut zc = None;
    for (dx, dy) in [(0.0, 0.0), (-h, -h), (h, -h), (-h, h), (h, h)] {
        match ground(anchor[0] + dx, anchor[1] + dy) {
            Some(z) => {
                zc.get_or_insert(z);
            }
            None => return, // not resident yet: retry next frame
        }
    }
    let edits = benilla_voxel::world::synth_edits(n, map, anchor, zc.unwrap_or(0.0), span, ground);
    let t0 = Instant::now();
    let j = world.journal.maps.entry(map).or_default();
    for e in &edits {
        j.insert(*e);
    }
    let keys: BTreeSet<ChunkKey> = edits.iter().flat_map(|e| e.touched_chunks()).collect();
    for &k in &keys {
        world.want(k);
    }
    info!(
        "MONKEY (voxel): SYNTH {} edit(s) over {span} yd at WoW ({:.1}, {:.1}) — {} chunk(s) \
         touched, {} wanted; filed in {:.2} ms",
        edits.len(),
        anchor[0],
        anchor[1],
        keys.len(),
        world.wanted_count,
        t0.elapsed().as_secs_f32() * 1000.0
    );
    state.0 = 0.0;
    state.1 = 1;
    state.2 = edits;
}

/// Persist the journal at most every 2 s while it has unsaved changes.
fn monkey_voxel_journal_autosave(
    time: Res<Time>,
    mut world: ResMut<MonkeyVoxelWorld>,
    mut perf: ResMut<MonkeyVoxelPerf>,
    mut since: Local<f32>,
) {
    if !world.journal_dirty {
        *since = 0.0;
        return;
    }
    *since += time.delta_secs();
    if *since < 2.0 {
        return;
    }
    *since = 0.0;
    world.journal_dirty = false;
    let t = Instant::now();
    monkey_voxel_journal_save(&world.journal);
    perf.save_ms = t.elapsed().as_secs_f32() * 1000.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voxel_mesh_converts_with_matching_counts_and_axes() {
        let mut w = VoxelWorldData::default();
        w.seeds.insert(
            [0, 0],
            Arc::new(ColumnSeed::from_fn([0, 0], |_, _| Some(8.0))),
        );
        w.build(&[[0, 0, 0]], std::iter::empty());
        let vm = w.mesh([0, 0, 0]);
        assert!(!vm.is_empty(), "the seeded chunk produced no surface");
        let mesh = monkey_voxel_mesh_to_bevy(&vm);
        assert_eq!(mesh.primitive_topology(), PrimitiveTopology::TriangleList);
        assert_eq!(mesh.count_vertices(), vm.positions.len());
        match mesh.indices() {
            Some(Indices::U32(ix)) => assert_eq!(ix.len(), vm.indices.len()),
            other => panic!("expected U32 indices, got {other:?}"),
        }
        // The flat seed (8 + lift yd) lands at Bevy y = 8.15 after the window-origin translation.
        let o = wow_to_bevy(window_origin([0, 0, 0]));
        let p = vm.positions[vm.positions.len() / 2];
        let y = o.y + wow_vec_to_bevy(p)[1];
        assert!((y - (8.0 + SEED_LIFT)).abs() < 0.3, "surface y {y}");
        // Up normal in WoW (+z) is +Y in Bevy.
        let n = wow_vec_to_bevy(vm.normals[vm.normals.len() / 2]);
        assert!(n[1] > 0.0, "normal {n:?}");
    }

    fn far() -> Instant {
        Instant::now() + std::time::Duration::from_secs(3600)
    }

    /// MONKEY (voxel fix): ground effects follow the voxel layer — a tuft outside the carve keeps
    /// its place, one over a dug pit drops onto the pit floor, one inside the carve on untouched
    /// ground rides the seeded surface, one on a pit wall is dropped, one under a mound moves to
    /// its crown.
    #[test]
    fn clutter_is_replaced_onto_voxel_tops() {
        use crate::clutter::ShadedPlacement;
        use benilla_formats::GroundDoodadPlacement;
        let mut data = VoxelWorldData::default();
        let col = [0, 0];
        data.seeds.insert(col, Arc::new(ColumnSeed::from_fn(col, |_, _| Some(20.0))));
        let keys = [[0, 0, 1]];
        let pit = dig([6.0, 6.0, 20.0], 3.0);
        let mound = VoxelEditOp { mode: 1, ..dig([11.0, 11.0, 20.0], 2.5) };
        data.build(&keys, [pit, mound].iter());
        let carve = VoxelCarve::from_columns([(0, 0)], CHUNK_YARDS, CARVE_INSET);
        let tuft = |x: f32, y: f32| ShadedPlacement {
            placement: GroundDoodadPlacement {
                model: "grass.m2".into(),
                position: [x, y, 20.0],
                yaw: 0.0,
                scale: 1.0,
            },
            tint: 1.0,
            ground_normal: [0.0, 1.0, 0.0],
        };
        let raw = vec![(
            "grass.m2".to_string(),
            vec![tuft(-3.0, 4.0), tuft(6.0, 6.0), tuft(2.0, 13.0), tuft(8.9, 6.0), tuft(11.0, 11.0)],
        )];
        let out = clutter_replace(&data, &carve, &raw);
        let ps: Vec<[f32; 3]> = out[0].1.iter().map(|s| s.placement.position).collect();
        assert_eq!(ps.len(), 4, "the pit-wall tuft is dropped: {ps:?}");
        assert_eq!(ps[0], [-3.0, 4.0, 20.0], "outside the carve: untouched");
        assert!((ps[1][2] - 17.0).abs() < 0.3, "pit floor {:?}", ps[1]);
        assert!((ps[2][2] - 20.15).abs() < 0.1, "seeded surface {:?}", ps[2]);
        assert!((ps[3][2] - 22.5).abs() < 0.3, "mound crown {:?}", ps[3]);
        assert!(out[0].1[1].ground_normal[1] > 0.9, "Bevy +Y up normal");
    }

    /// Ray / triangle (Möller–Trumbore): the hit distance along `d` (unit) or `None`.
    fn ray_tri(o: [f32; 3], d: [f32; 3], t: [[f32; 3]; 3]) -> Option<f32> {
        let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let cross = |a: [f32; 3], b: [f32; 3]| {
            [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
        };
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let (e1, e2) = (sub(t[1], t[0]), sub(t[2], t[0]));
        let p = cross(d, e2);
        let det = dot(e1, p);
        if det.abs() < 1.0e-9 {
            return None;
        }
        let s = sub(o, t[0]);
        let u = dot(s, p) / det;
        let q = cross(s, e1);
        let v = dot(d, q) / det;
        let tt = dot(e2, q) / det;
        (u >= -1.0e-5 && v >= -1.0e-5 && u + v <= 1.0 + 1.0e-5 && tt > 0.0).then_some(tt)
    }

    /// MONKEY (voxel fix, owner's "wounds"): wherever the carve removes the real ground, the voxel
    /// layer must cover it — straight down (the concave-corner notch) AND for grazing views across
    /// the union's outer edge (the skirt under the SEED_LIFT lip). Sloped, bumpy ground that
    /// crosses the 16-yd z-chunk boundary, an L-shaped union (one inner corner) and a dig. Without
    /// the notch the inner corner leaves sky holes; without the skirt, rays from
    /// outside slip under the lip past the terrain's carved edge.
    #[test]
    fn carved_ground_is_always_covered_and_never_shows_the_sky() {
        let ground = |x: f32, y: f32| 15.0 + 0.06 * x - 0.04 * y + (x * 0.4).sin() * 0.5;
        let cols: [ColumnKey; 3] = [[0, 0], [1, 0], [0, 1]];
        let mut data = VoxelWorldData::default();
        let mut keys = Vec::new();
        for c in cols {
            let s = ColumnSeed::from_fn(c, |x, y| Some(ground(x, y)));
            let (lo, hi) = s.surface_chunks().unwrap();
            for kz in lo..=hi {
                keys.push([c[0], c[1], kz]);
            }
            data.seeds.insert(c, Arc::new(s));
        }
        assert!(keys.iter().any(|k| k[2] == 0) && keys.iter().any(|k| k[2] == 1));
        let pit = dig([6.0, 24.0, ground(6.0, 24.0)], 2.0);
        data.build(&keys, [pit].iter());
        let skirts = |k: ChunkKey| {
            [(-1, 0), (1, 0), (0, -1), (0, 1)]
                .map(|(dx, dy)| !data.is_active([k[0] + dx, k[1] + dy, k[2]]))
        };
        let meshes: Vec<(ChunkKey, benilla_voxel::VoxelMesh)> = keys
            .iter()
            .map(|&k| {
                let mut m = data.mesh(k);
                benilla_voxel::world::add_skirts(&mut m, skirts(k), SKIRT_DEPTH);
                (k, m)
            })
            .collect();
        let carve = VoxelCarve::from_columns(
            cols.iter().map(|c| (c[0], c[1])),
            CHUNK_YARDS,
            CARVE_INSET,
        );
        // 1. Straight down: every carved point is under a voxel triangle.
        let gaps = benilla_voxel::world::xy_coverage_gaps(&meshes, &cols, 0.05, |x, y| {
            carve.contains_strict(x, y)
        });
        assert!(
            gaps.is_empty(),
            "{} uncovered carved point(s), e.g. {:?}",
            gaps.len(),
            &gaps[..gaps.len().min(4)]
        );
        // 2. Grazing rays across the outer edge, from the open sides into the union.
        let tris: Vec<[[f32; 3]; 3]> = meshes
            .iter()
            .flat_map(|(k, m)| {
                let o = window_origin(*k);
                m.indices.chunks_exact(3).map(move |t| {
                    [t[0], t[1], t[2]].map(|i| {
                        let p = m.positions[i as usize];
                        [p[0] + o[0], p[1] + o[1], p[2] + o[2]]
                    })
                })
            })
            .collect();
        let mut leaks = Vec::new();
        let mut rays = 0;
        // Eyes outside the −x, −y and +x edges (and past the inner corner), looking at the ground
        // inside at shallow angles.
        let mut eyes = Vec::new();
        for i in 0..24 {
            let s = 1.0 + i as f32 * 1.3;
            eyes.push(([-6.0, s], [6.0, s]));
            eyes.push(([s, -6.0], [s, 6.0]));
            eyes.push(([38.0, s * 0.5], [26.0, s * 0.5]));
            eyes.push(([22.0 + s * 0.2, 22.0 + s * 0.2], [12.0, 12.0 + s * 0.3]));
        }
        for (e, t) in eyes {
            for (hi, tz) in [(0.6, -0.3), (1.0, -0.2), (2.0, 0.0), (0.4, -0.05)] {
                let o = [e[0], e[1], ground(e[0], e[1]) + hi];
                let tgt = [t[0], t[1], ground(t[0], t[1]) + tz];
                let d = [tgt[0] - o[0], tgt[1] - o[1], tgt[2] - o[2]];
                let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                let d = [d[0] / len, d[1] / len, d[2] / len];
                rays += 1;
                let t_vox = tris
                    .iter()
                    .filter_map(|&t| ray_tri(o, d, t))
                    .fold(f32::INFINITY, f32::min);
                // March: the first terrain hit (ground kept outside the carve) or a point inside
                // the carve below the ground — the void the sky shows through.
                let mut s = 0.0;
                while s < t_vox.min(60.0) {
                    let p = [o[0] + d[0] * s, o[1] + d[1] * s, o[2] + d[2] * s];
                    let g = ground(p[0], p[1]);
                    if !carve.contains_strict(p[0], p[1]) {
                        if p[2] <= g {
                            break; // the real ground
                        }
                    } else if p[2] < g - 0.05 {
                        leaks.push((o, tgt, p));
                        break;
                    }
                    s += 0.01;
                }
            }
        }
        assert!(
            leaks.is_empty(),
            "{} of {rays} grazing ray(s) see under the voxel lip, e.g. {:?}",
            leaks.len(),
            &leaks[..leaks.len().min(3)]
        );
    }

    fn dig(pos: [f32; 3], radius: f32) -> VoxelEditOp {
        VoxelEditOp {
            map: 0,
            pos,
            radius,
            mode: 0,
            strength: 255,
            nonce: 0,
            seq: None,
            local: true,
        }
    }

    /// MONKEY (voxel): run every queued build (on another thread) until none is left.
    fn drain_builds(world: &mut MonkeyVoxelWorld) {
        for _ in 0..1000 {
            let jobs = world.take_build_jobs(far());
            if jobs.is_empty() && world.building.is_empty() {
                return;
            }
            for d in jobs {
                let job = d.job;
                let out = std::thread::spawn(move || job.run()).join().unwrap();
                world.finish_build(&d.keys, &d.gens, d.hot, out);
            }
        }
        panic!("builds never drained");
    }

    #[test]
    fn stale_build_result_is_discarded_and_requeued() {
        let mut world = MonkeyVoxelWorld {
            map: Some(0),
            ..Default::default()
        };
        let mut net = MonkeyVoxelNet::default();
        world.data.seeds.insert(
            [2, 3],
            Arc::new(ColumnSeed::from_fn([2, 3], |_, _| Some(40.0))),
        );
        world.local_edit(&mut net, dig([40.0, 56.0, 40.0], 2.0));
        let k = [2, 3, 2];
        assert!(world.hot_build.contains(&k), "a live edit's fresh chunk builds hot");
        let first = world.take_build_jobs(far());
        assert!(first.iter().any(|d| d.hot && d.keys.contains(&k)));
        assert!(world.building.contains(&k));
        // A newer edit to the chunk while its build is in flight.
        world.local_edit(&mut net, dig([41.0, 56.0, 39.0], 2.0));
        for d in first {
            let out = d.job.run();
            let had = d.keys.contains(&k);
            let adopted = world.finish_build(&d.keys, &d.gens, d.hot, out);
            if had {
                assert!(!adopted, "the stale result must be discarded");
            }
        }
        assert!(!world.data.is_active(k), "nothing stale landed");
        assert!(world.hot_build.contains(&k), "requeued, still hot");
        assert!(world.stale_builds >= 1);
        drain_builds(&mut world);
        assert!(world.data.is_active(k));
        // == the canonical sync build of the whole journal.
        let mut sync = VoxelWorldData::default();
        sync.seeds = world.data.seeds.clone();
        sync.build(&[k], world.journal.maps[&0].edits().iter());
        assert_eq!(sync.chunks[&k], world.data.chunks[&k]);
        // A rebuild in flight on an ACTIVE chunk + a new live edit → discarded, back in rebuild.
        world.rebuild.insert(k);
        let jobs = world.take_build_jobs(far());
        assert!(!jobs.is_empty());
        world.local_edit(&mut net, dig([42.0, 57.0, 39.0], 1.5));
        for d in jobs {
            let out = d.job.run();
            assert!(!world.finish_build(&d.keys, &d.gens, d.hot, out));
        }
        assert!(world.rebuild.contains(&k), "an active chunk's stale rebuild is requeued");
    }

    #[test]
    fn older_mesh_never_overwrites_newer() {
        let mut world = MonkeyVoxelWorld {
            map: Some(0),
            ..Default::default()
        };
        world.data.seeds.insert(
            [0, 0],
            Arc::new(ColumnSeed::from_fn([0, 0], |_, _| Some(8.0))),
        );
        let k = [0, 0, 0];
        world.data.build(&[k], std::iter::empty());
        world.hot.insert(k);
        let a = world.take_mesh_jobs(far());
        world.hot.insert(k);
        let b = world.take_mesh_jobs(far());
        assert_eq!((a.len(), b.len()), (1, 1), "two hot snapshots in flight");
        world.hot.insert(k);
        assert!(world.take_mesh_jobs(far()).is_empty(), "at most 2 in flight per chunk");
        let (ga, gb) = (a[0].gen, b[0].gen);
        assert!(gb > ga, "generations increase with each dispatch");
        let ready = |d: &MonkeyMeshDispatch| MonkeyMeshReady {
            key: d.key,
            gen: d.gen,
            hot: true,
            out: monkey_voxel_mesh_job(&d.window, d.skirts),
        };
        // The newer finishes first and is shown …
        world.mesh_returned(ready(&b[0]));
        assert!(world.mesh_is_fresh(k, gb));
        world.shown_gen.insert(k, gb);
        // … then the older one lands: it must be dropped, both in the ready map and at attach.
        world.mesh_returned(ready(&a[0]));
        assert_eq!(world.mesh_ready[&k].gen, gb, "the ready map keeps the newest");
        assert!(!world.mesh_is_fresh(k, ga), "an older snapshot never overwrites a newer one");
        assert!(world.mesh_in_flight.is_empty());
        // A chunk no longer active (map change, …) takes nothing.
        world.data.chunks.clear();
        assert!(!world.mesh_is_fresh(k, gb + 1));
    }

    #[test]
    fn async_pipeline_equals_sync_replay() {
        let ground = |x: f32, y: f32| Some(20.0 + 0.05 * x - 0.03 * y + (x * 0.2).sin());
        let mut world = MonkeyVoxelWorld {
            map: Some(0),
            ..Default::default()
        };
        // Dig / build only: those are per-sample, so the grouping of the off-thread builds cannot
        // matter and the result must equal a per-chunk replay of the whole journal.
        let edits: Vec<VoxelEditOp> =
            benilla_voxel::world::synth_edits(300, 0, [24.0, 24.0], 20.0, 48.0, ground)
                .into_iter()
                .filter(|e| e.mode < 2)
                .collect();
        for x in -2..=5 {
            for y in -2..=5 {
                world
                    .data
                    .seeds
                    .insert([x, y], Arc::new(ColumnSeed::from_fn([x, y], ground)));
            }
        }
        for e in &edits {
            world.ingest(*e, false);
        }
        drain_builds(&mut world);
        assert!(world.data.chunks.len() > 10);
        let mut sync = VoxelWorldData::default();
        sync.seeds = world.data.seeds.clone();
        let all = world.journal.maps[&0].edits().to_vec();
        let keys: Vec<ChunkKey> = world.data.chunks.keys().copied().collect();
        for &k in &keys {
            sync.build(&[k], all.iter());
        }
        for &k in &keys {
            assert_eq!(sync.chunks[&k], world.data.chunks[&k], "chunk {k:?}");
            // The off-thread mesh job == the sync mesh.
            let out = monkey_voxel_mesh_job(&world.data.window_i8(k), [false; 4]);
            assert_eq!(out.tris, sync.mesh(k).tri_count(), "chunk {k:?} mesh");
            assert_eq!(out.collider.is_some(), out.tris > 0);
        }
        // A bulk resync: reset every region, re-file the edits as bulk → same densities again.
        let regions: BTreeSet<(i16, i16)> = edits.iter().map(|e| e.region()).collect();
        for (rx, ry) in regions {
            world.reset_region(0, rx, ry, 9);
        }
        for e in &edits {
            world.ingest(*e, false);
        }
        assert!(!world.rebuild.is_empty(), "bulk edits are rebuilt, not applied inline");
        drain_builds(&mut world);
        for &k in &keys {
            assert_eq!(sync.chunks[&k], world.data.chunks[&k], "chunk {k:?} after resync");
        }
    }

    #[test]
    fn world_layer_activates_surface_chunks_and_journals_local_edits() {
        let mut world = MonkeyVoxelWorld {
            map: Some(0),
            ..Default::default()
        };
        let mut net = MonkeyVoxelNet::default();
        // A floating build one chunk above a known surface column activates the surface chunk too.
        world.data.seeds.insert(
            [2, 3],
            Arc::new(ColumnSeed::from_fn([2, 3], |_, _| Some(40.0))),
        );
        world.local_edit(
            &mut net,
            VoxelEditOp {
                map: 9,
                pos: [40.0, 56.0, 60.0],
                radius: 2.0,
                mode: 1,
                strength: 255,
                nonce: 0,
                seq: None,
                local: true,
            },
        );
        assert_eq!(net.outbox.len(), 1);
        assert_eq!(net.outbox[0].map, 0, "local edits are stamped with the current map");
        let kzs = world.wanted.get(&[2, 3]).expect("column wanted");
        assert!(kzs.contains(&3), "the edit's own chunk (z 48..64)");
        assert!(kzs.contains(&2), "the surface chunk (z 32..48, ground 40) joined it");
        assert_eq!(world.pending_local(0).len(), 1);
        let nonce = net.outbox[0].nonce;
        world.confirm(0, nonce, 77);
        assert!(world.pending_local(0).is_empty());
    }
}

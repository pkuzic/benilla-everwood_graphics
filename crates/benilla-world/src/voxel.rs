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
use bevy::camera::primitives::MeshAabb;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;

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
pub use benilla_voxel::world::{RegionSync, VoxelEditOp};
use benilla_voxel::world::{
    chunk_key_at, raymarch, window_origin, ChunkKey, ColumnKey, ColumnSeed, JournalInsert,
    VoxelJournal, VoxelWorldData, CHUNK_YARDS, SEED_LIFT,
};

/// One edit as it travels the cross-crate mailbox (the v2 EDIT body + optional server seq).
pub type MonkeyVoxelEdit = VoxelEditOp;

/// Hard cap on active chunks (32 KB of density each → 128 MB at the cap).
pub const MONKEY_VOXEL_MAX_CHUNKS: usize = 4096;
/// Wall-clock per frame for BULK meshing (journal replay, stream-in); the brush's own chunks are
/// always meshed the same frame.
const MESH_BUDGET_MS: f32 = 4.0;
/// Column seeds sampled per frame at most (each = 36² raw height queries).
const SEEDS_PER_FRAME: usize = 16;
/// Chunks (re)built per frame at most.
const BUILDS_PER_FRAME: usize = 64;

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
#[derive(Resource, Default)]
pub struct MonkeyVoxelNet {
    pub outbox: Vec<VoxelEditOp>,
    pub inbox: Vec<VoxelEditOp>,
    pub confirms: Vec<(u16, u32)>,
    pub resets: Vec<(u16, i16, i16, u16)>,
    pub region_done: Vec<(u16, i16, i16, u16, u32)>,
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
    pub fn pending_local(&self, map: u16) -> &[VoxelEditOp] {
        self.journal
            .maps
            .get(&map)
            .map_or(&[][..], |j| j.pending_local())
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
    /// incremental apply when it is in order (else a rebuild of the chunks it touches). `hot` =
    /// remesh this frame (a live local / remote edit).
    fn ingest(&mut self, e: VoxelEditOp, hot: bool) -> JournalInsert {
        let how = self.journal.maps.entry(e.map).or_default().insert(e);
        self.journal_dirty |= how != JournalInsert::Duplicate;
        if self.current_map_u16() != Some(e.map) || how == JournalInsert::Duplicate {
            return how; // another map's edit: journalled for when we go there
        }
        for k in e.touched_chunks() {
            self.want(k);
        }
        match how {
            JournalInsert::Appended => {
                self.data.apply_incremental(&e);
                // The meshes seeing the bbox (+ apron) — only chunks already active.
                let remesh: Vec<ChunkKey> = e
                    .remesh_chunks()
                    .into_iter()
                    .filter(|k| self.data.is_active(*k))
                    .collect();
                for k in remesh {
                    if hot {
                        self.hot.insert(k);
                    } else {
                        self.dirty.insert(k);
                    }
                }
            }
            JournalInsert::OutOfOrder => {
                for k in e.touched_chunks() {
                    if self.data.is_active(k) {
                        self.rebuild.insert(k);
                    }
                }
            }
            JournalInsert::Duplicate => {}
        }
        how
    }

    /// A LOCAL edit (brush / AUTOEDIT): stamps map + nonce, ingests it, queues it for the uplink.
    fn local_edit(&mut self, net: &mut MonkeyVoxelNet, mut e: VoxelEditOp) {
        let Some(map) = self.current_map_u16() else {
            return;
        };
        e.map = map;
        e.seq = None;
        e.nonce = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1);
        if !e.is_valid() {
            return;
        }
        if e.mode == 0 {
            self.last_dig = Some(e.pos);
        }
        self.ingest(e, true);
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
                for k in e.touched_chunks() {
                    if self.data.is_active(k) {
                        self.rebuild.insert(k);
                    }
                }
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
            for e in &dropped {
                for k in e.touched_chunks() {
                    if self.data.is_active(k) {
                        self.rebuild.insert(k);
                    }
                }
            }
        }
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
    let path = monkey_voxel_journal_path();
    let tmp = path.with_extension("bin.tmp");
    if std::fs::write(&tmp, j.encode()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
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
        app.add_systems(
            Update,
            (
                monkey_voxel_map_watch,
                monkey_voxel_brush_edit,
                monkey_voxel_autoedit,
                monkey_voxel_apply_inbox,
                monkey_voxel_stream,
                monkey_voxel_tile_materials,
                // Carve the real ADT ground under the surface columns after this frame's meshes.
                monkey_voxel_hide_terrain,
                monkey_voxel_probe_rays,
                monkey_voxel_journal_autosave,
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
    let edits: Vec<VoxelEditOp> = world
        .journal
        .maps
        .get(&(map.0 as u16))
        .map(|j| j.edits().to_vec())
        .unwrap_or_default();
    for e in &edits {
        for k in e.touched_chunks() {
            world.want(k);
        }
    }
    info!(
        "MONKEY (voxel): world layer on map {} — {} journalled edit(s), {} chunk(s) to activate.",
        map.0,
        edits.len(),
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
    };
    world.local_edit(&mut net, e);
}

/// Test/dev hook `WOW_VOXEL_AUTOEDIT=1` (anchor = the ground under the camera's view ray, else
/// straight below it) or `WOW_VOXEL_AUTOEDIT=x,y` (a WoW anchor): ~8 s into the world, once every
/// scripted spot's ground is resident, run a scripted sequence through the SAME path as the brush
/// (journal + uplink): a 2-step crater at the anchor, a second crater 30 yd north, a dig on a
/// 16-yd chunk CORNER (spanning 2×2×2 chunks), a build mound with an overhanging lip, an 8-yd and
/// a 2-yd dig (the perf references). Bypasses the GM gate.
#[allow(clippy::too_many_arguments)]
fn monkey_voxel_autoedit(
    time: Res<Time>,
    mut state: Local<(f32, bool)>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    mut world: ResMut<MonkeyVoxelWorld>,
    streamer: Res<TerrainStreamer>,
    adts: Res<Assets<AdtTile>>,
    mut net: ResMut<MonkeyVoxelNet>,
) {
    if state.1 {
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
}

/// Drain the network inboxes into the canonical path (remote edits are remeshed this frame).
fn monkey_voxel_apply_inbox(mut net: ResMut<MonkeyVoxelNet>, mut world: ResMut<MonkeyVoxelWorld>) {
    if world.map.is_none() {
        return; // keep buffering until the first world entry
    }
    let edits: Vec<VoxelEditOp> = net.inbox.drain(..).collect();
    let n = edits.len();
    for e in edits {
        if e.is_valid() {
            world.ingest(e, true);
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
}

/// The stream: load column seeds once their tiles are resident (→ surface-column activation),
/// build wanted chunks (seed + their journal edits in canonical order), rebuild out-of-order ones,
/// mesh the hot chunks now and the dirty ones within [`MESH_BUDGET_MS`], spawn / update the chunk
/// entities (mesh + per-tile material + collider), and republish the carve when the set of
/// fully-meshed surface columns changes.
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
) {
    let world = &mut *world;
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

    // 2. Build wanted chunks that have a seed but no density yet (canonical: seed + journal), and
    //    rebuild the out-of-order ones.
    let map = world.current_map_u16();
    let mut to_build: Vec<ChunkKey> = Vec::new();
    'outer: for (col, kzs) in &world.wanted {
        if !world.data.seeds.contains_key(col) {
            continue;
        }
        for &kz in kzs {
            let k = [col[0], col[1], kz];
            if !world.data.is_active(k) {
                to_build.push(k);
                if to_build.len() >= BUILDS_PER_FRAME {
                    break 'outer;
                }
            }
        }
    }
    let mut rebuilt: Vec<ChunkKey> = world.rebuild.drain().collect();
    rebuilt.retain(|k| world.data.is_active(*k));
    for set in [to_build, rebuilt] {
        if set.is_empty() {
            continue;
        }
        let t0 = Instant::now();
        let edits: Vec<VoxelEditOp> = map
            .and_then(|m| world.journal.maps.get(&m))
            .map(|j| j.edits().to_vec())
            .unwrap_or_default();
        let built = world.data.build(&set, edits.iter());
        // A handful (a live brush stroke activating fresh ground) is meshed this frame; a bulk
        // journal replay goes through the frame budget.
        let hot = built.len() <= 8;
        world.dirty_around(&built, hot);
        info!(
            "MONKEY (voxel): built {} chunk(s) (seed + {} journal edit(s) scanned) in {:.2} ms",
            built.len(),
            edits.len(),
            t0.elapsed().as_secs_f32() * 1000.0
        );
        world.carve_dirty = true;
    }

    // 3. Mesh: every hot chunk now (the brush must stay live), then dirty ones within budget.
    let t0 = Instant::now();
    let mut queue: Vec<ChunkKey> = world.hot.drain().collect();
    let hot_n = queue.len();
    for k in &queue {
        world.dirty.remove(k);
    }
    let (mut meshed_n, mut tris_n, mut i) = (0usize, 0usize, 0usize);
    loop {
        if i >= queue.len() {
            if t0.elapsed().as_secs_f32() * 1000.0 >= MESH_BUDGET_MS {
                break;
            }
            let Some(&k) = world.dirty.iter().next() else {
                break;
            };
            world.dirty.remove(&k);
            queue.push(k);
        }
        let k = queue[i];
        i += 1;
        if !world.data.is_active(k) {
            continue;
        }
        let vm = world.data.mesh(k);
        meshed_n += 1;
        tris_n += vm.tri_count();
        world.meshed.insert(k);
        world.carve_dirty = true;
        let existing = world.entities.get(&k).copied();
        if vm.indices.len() < 3 {
            if let Some(e) = existing {
                commands.entity(e).despawn();
                world.entities.remove(&k);
            }
            continue;
        }
        let mesh = monkey_voxel_mesh_to_bevy(&vm);
        let aabb = mesh.compute_aabb();
        let collider = monkey_voxel_collider(&vm);
        match existing.and_then(|e| handles.get(e).ok().map(|h| (e, h.0.clone()))) {
            Some((e, handle)) => {
                let _ = meshes.insert(&handle, mesh);
                let mut ec = commands.entity(e);
                if let Some(a) = aabb {
                    ec.insert(a);
                }
                match collider {
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
                if let Some(c) = collider {
                    ec.insert(c);
                }
                world.entities.insert(k, ec.id());
            }
        }
    }
    if meshed_n > 0 {
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        if world.timing_logs > 0 {
            world.timing_logs -= 1;
            info!(
                "MONKEY (voxel): remeshed {meshed_n} chunk(s) ({hot_n} hot, {tris_n} tris) + \
                 colliders in {ms:.2} ms; {} active, {} still queued",
                world.data.chunks.len(),
                world.dirty.len()
            );
        } else {
            debug!("MONKEY (voxel): remeshed {meshed_n} chunk(s) in {ms:.2} ms");
        }
    }

    // 4. Carve = the surface columns whose surface chunks are all meshed.
    if world.carve_dirty {
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
        }
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

/// Persist the journal at most every 2 s while it has unsaved changes.
fn monkey_voxel_journal_autosave(
    time: Res<Time>,
    mut world: ResMut<MonkeyVoxelWorld>,
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
    monkey_voxel_journal_save(&world.journal);
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

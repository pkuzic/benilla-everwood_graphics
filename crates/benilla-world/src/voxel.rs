//! MONKEY (voxel): P0 Lane B — render a single code-generated voxel volume in-engine.
//!
//! This is the render half of the voxel-terrain programme (`DESIGN-VoxelTerrain.md`, option C). The
//! data + mesher live engine-free in `benilla-voxel`; here we turn one of its [`benilla_voxel::VoxelMesh`]
//! surfaces into a Bevy [`Mesh`] and spawn it as a visible entity, proving the renderer consumes the
//! mesher's output. No edits, no collision, no server — those are later lanes.
//!
//! Everything in this module is gated behind the crate's `voxel` cargo feature (OFF by default), so
//! the shipped player build is byte-unchanged. The volume is the P0 overhang proof: a genuinely
//! non-heightmap surface (two surfaces over one column) that the WoW heightmap can never express.

use avian3d::prelude::{Collider, RigidBody};
use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;

mod material;
pub use material::MonkeyVoxelMaterial;
use material::MonkeyVoxelMaterialHandle;

use crate::view::WorldCamera;
use benilla_assets::coords::{bevy_to_wow, wow_to_bevy};
use benilla_voxel::BrushMode;

/// Converts the engine-neutral [`benilla_voxel::VoxelMesh`] (triangle list; `[f32; 3]`
/// positions/normals already scaled to world yards local to the chunk origin) into a Bevy [`Mesh`].
///
/// `PrimitiveTopology::TriangleList`, `ATTRIBUTE_POSITION` + `ATTRIBUTE_NORMAL` from the mesher's
/// vectors, `Indices::U32`. `RenderAssetUsages::RENDER_WORLD` only — P0 never reads the voxel mesh
/// back on the CPU (collision/navmesh is the server's job, a later lane).
pub fn monkey_voxel_mesh_to_bevy(vm: &benilla_voxel::VoxelMesh) -> Mesh {
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, vm.positions.clone());
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, vm.normals.clone());
    mesh.insert_indices(Indices::U32(vm.indices.clone()));
    mesh
}

/// An avian trimesh collider from the same mesher output the render mesh uses, so the player capsule
/// (which sweeps avian colliders, NOT the heightmap) can stand and walk on the voxel surface. No
/// `CollisionLayers` → it sits on `CollisionLayer::Default`, which the player body filter includes.
/// `None` for an empty (fully carved) volume so no phantom floor remains. Walkable needs the SERVER
/// to accept the heights too; the local server runs with anticheat off, live is the documented P3.
fn monkey_voxel_collider(vm: &benilla_voxel::VoxelMesh) -> Option<Collider> {
    if vm.indices.len() < 3 {
        return None;
    }
    let verts: Vec<Vec3> = vm
        .positions
        .iter()
        .map(|p| Vec3::new(p[0], p[1], p[2]))
        .collect();
    let tris: Vec<[u32; 3]> = vm
        .indices
        .chunks_exact(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect();
    Some(Collider::trimesh(verts, tris))
}

/// Keep the walk-collider in step with the density: whenever any edit mutates `MonkeyVoxelData`
/// (brush, remote edit, placement/relocate, auto hooks, or the first spawn), rebuild the entity's
/// trimesh collider from the new surface. One change-detection system covers every edit path; it is
/// ordered last in the chain so a same-frame edit is caught. (This re-meshes once more per edit
/// frame just for the collider — fine for the small sandbox; throttle or go async if a big volume
/// hitches.)
fn monkey_voxel_sync_collider(
    mut commands: Commands,
    mut announced: Local<bool>,
    changed: Query<(Entity, &MonkeyVoxelData), Changed<MonkeyVoxelData>>,
) {
    for (e, data) in &changed {
        let vm = data.0.mesh();
        match monkey_voxel_collider(&vm) {
            Some(c) => {
                if !*announced {
                    info!(
                        "MONKEY (voxel): attached a walk collider ({} tris) — you can stand on the terrain.",
                        vm.indices.len() / 3,
                    );
                    *announced = true;
                }
                commands.entity(e).insert(c); // RigidBody::Static is already on the entity
            }
            None => {
                commands.entity(e).remove::<Collider>();
            }
        }
    }
}

/// Marks the P0 debug voxel volume entity.
#[derive(Component)]
pub struct MonkeyVoxelVolume;

/// Keeps the volume's editable multi-chunk density field on the entity so the P1 brush can mutate it
/// and re-mesh live. The volume origin is the entity's `Transform.translation` (world yards).
#[derive(Component)]
pub struct MonkeyVoxelData(pub benilla_voxel::VoxelVolume);

/// One terrain edit, in the volume's LOCAL voxel coords, for multiplayer relay. Every client starts
/// from the same base slab, so applying the same local-coord edit reproduces the same shape — the
/// deformation syncs even though each client's patch sits at its own world position.
#[derive(Clone, Copy, Debug)]
pub struct MonkeyVoxelEdit {
    pub center: [f32; 3],
    pub radius: f32,
    /// 0 = carve, 1 = build, 2 = smooth.
    pub mode: u8,
}

/// A shared build-site placement: the volume's world anchor (WoW coords of its CENTRE) plus its
/// dimensions. A GM drops one with Ctrl+P; every client that receives it re-spawns the single shared
/// volume at this exact anchor, so the LOCAL-coord edits that ride [`MonkeyVoxelEdit`] land on the
/// same world spot for everyone (they only lined up before when players happened to stand together).
#[derive(Clone, Copy, Debug)]
pub struct MonkeyVoxelSpawn {
    /// WoW coords [x, y, z] of the volume CENTRE — the shared world anchor.
    pub anchor_wow: [f32; 3],
    /// Chunk count per axis (u8 is plenty; a volume is a handful of chunks).
    pub chunks: [u8; 3],
    /// Flat-top height within the volume, in voxels.
    pub surface: f32,
}

/// Cross-crate mailbox between the voxel brush (benilla-world) and the guild-addon relay
/// (benilla-app, behind the `voxel` feature): local edits/placements to broadcast go in the
/// `*_outbox`; those received from other players land in the `*_inbox`. benilla-app drains the
/// outboxes and sends each as a `MV:`/`VS:` guild addon message, and pushes decoded inbound ones in.
#[derive(Resource, Default)]
pub struct MonkeyVoxelNet {
    pub outbox: Vec<MonkeyVoxelEdit>,
    pub inbox: Vec<MonkeyVoxelEdit>,
    pub spawn_outbox: Vec<MonkeyVoxelSpawn>,
    pub spawn_inbox: Vec<MonkeyVoxelSpawn>,
}

impl MonkeyVoxelSpawn {
    /// Encode to hex (anchor 3×f32 LE, chunks 3×u8, surface f32 LE → 19 bytes / 38 hex).
    pub fn to_hex(&self) -> String {
        let mut b = Vec::with_capacity(19);
        for v in self.anchor_wow {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.chunks);
        b.extend_from_slice(&self.surface.to_le_bytes());
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Decode from the hex payload; `None` if malformed.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 38 {
            return None;
        }
        let b: Vec<u8> = (0..19)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Some(Self {
            anchor_wow: [f(0), f(4), f(8)],
            chunks: [b[12], b[13], b[14]],
            surface: f(15),
        })
    }
}

impl MonkeyVoxelEdit {
    /// Encode to hex for a chat-channel payload (center 3×f32 LE, radius f32 LE, mode u8 → 34 hex).
    pub fn to_hex(&self) -> String {
        let mut b = Vec::with_capacity(17);
        for v in self.center {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.radius.to_le_bytes());
        b.push(self.mode);
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Decode from the hex payload; `None` if malformed.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 34 {
            return None;
        }
        let b: Vec<u8> = (0..17)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Some(Self {
            center: [f(0), f(4), f(8)],
            radius: f(12),
            mode: b[16],
        })
    }

    fn as_bytes(&self) -> [u8; 17] {
        let mut b = [0u8; 17];
        b[0..4].copy_from_slice(&self.center[0].to_le_bytes());
        b[4..8].copy_from_slice(&self.center[1].to_le_bytes());
        b[8..12].copy_from_slice(&self.center[2].to_le_bytes());
        b[12..16].copy_from_slice(&self.radius.to_le_bytes());
        b[16] = self.mode;
        b
    }
}

/// Persistence: every applied edit is appended to a journal file so terraforming survives relog.
/// Lives under `$BENILLA_HOME` (the profile dir) so it travels with the account; falls back to CWD.
fn monkey_voxel_journal_path() -> std::path::PathBuf {
    std::env::var_os("BENILLA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("monkey_voxel_journal.bin")
}

/// Append one edit (17 bytes) to the journal. Cheap; the brush can call it per stroke-frame.
fn monkey_voxel_journal_append(edit: &MonkeyVoxelEdit) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(monkey_voxel_journal_path())
    {
        let _ = f.write_all(&edit.as_bytes());
    }
}

/// Load all journalled edits to replay onto the base slab at spawn (so prior terraforming returns).
fn monkey_voxel_journal_load() -> Vec<MonkeyVoxelEdit> {
    let Ok(bytes) = std::fs::read(monkey_voxel_journal_path()) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(17)
        .filter_map(|c| {
            MonkeyVoxelEdit::from_hex(&c.iter().map(|x| format!("{x:02x}")).collect::<String>())
        })
        .collect()
}

/// Truncate the journal — a fresh build site starts with no edits (so stale edits from a previous
/// site don't replay onto a newly placed/adopted one).
fn monkey_voxel_journal_clear() {
    let _ = std::fs::remove_file(monkey_voxel_journal_path());
}

/// The shared build site's anchor file (beside the journal). Present only once a GM has placed a
/// site (or this client adopted one); its presence makes the volume re-spawn at that fixed world
/// anchor on relog instead of ahead of the camera (the solo-sandbox default).
fn monkey_voxel_site_path() -> std::path::PathBuf {
    std::env::var_os("BENILLA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("monkey_voxel_site.bin")
}

/// Persist the current shared build site (19-byte [`MonkeyVoxelSpawn`] payload).
fn monkey_voxel_site_save(site: &MonkeyVoxelSpawn) {
    let hex = site.to_hex();
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect();
    let _ = std::fs::write(monkey_voxel_site_path(), &bytes);
}

/// Load the persisted shared build site, if any.
fn monkey_voxel_site_load() -> Option<MonkeyVoxelSpawn> {
    let bytes = std::fs::read(monkey_voxel_site_path()).ok()?;
    let hex: String = bytes.iter().map(|x| format!("{x:02x}")).collect();
    MonkeyVoxelSpawn::from_hex(&hex)
}

/// The real ground sampled under a volume's footprint: a per-column world-height grid plus the
/// world origin-y chosen so the terrain sits vertically centred in the volume (room above to build,
/// below to dig). This is what makes a placed region START as the real terrain under it instead of a
/// flat pad — the GM then sculpts it into overhangs/caves. `centre` is the volume's world centre
/// (its y is the mean ground height).
pub struct TerrainSeed {
    pub origin_y: f32,
    pub heights: Vec<f32>,
    pub centre: Vec3,
}

/// Sample the real ground height across the volume's footprint (centred at `centre_xz`), into the
/// per-column grid `VoxelVolume::from_heightmap` wants. `None` if the ground under the centre is not
/// resident yet (the caller falls back to a flat pad / retries). Unresident or hole columns are
/// filled with the mean so the seed has no gaps.
fn monkey_voxel_sample_terrain(
    world: &crate::world_point::WorldPoint,
    centre_xz: Vec2,
    chunks: [u32; 3],
) -> Option<TerrainSeed> {
    let centre_h = world.terrain_height_under(Vec3::new(centre_xz.x, 0.0, centre_xz.y))?;
    let half = half_extent_for(chunks);
    let gx = (chunks[0] * benilla_voxel::CHUNK + 2) as usize;
    let gz = (chunks[2] * benilla_voxel::CHUNK + 2) as usize;
    let (origin_x, origin_z) = (centre_xz.x - half.x, centre_xz.y - half.z);
    let mut heights = vec![f32::NAN; gx * gz];
    let (mut sum, mut n) = (0.0f32, 0u32);
    for iz in 0..gz {
        for ix in 0..gx {
            let wx = origin_x + ix as f32 * benilla_voxel::VOXEL_SIZE;
            let wz = origin_z + iz as f32 * benilla_voxel::VOXEL_SIZE;
            if let Some(h) = world.terrain_height_under(Vec3::new(wx, 0.0, wz)) {
                heights[ix + iz * gx] = h;
                sum += h;
                n += 1;
            }
        }
    }
    // Lift the seeded surface a hair above the real ground so the voxel OCCLUDES it (no z-fighting
    // between the surface-nets mesh and the heightmap triangulation at identical heights) — until the
    // proper hole-remesh hiding lands. A ~0.15 yd lip at the region edge is barely visible; a z-fight
    // is not. Holes dug below still reveal the real ground (the accepted "no hiding yet" state).
    const SEED_LIFT: f32 = 0.15;
    let mean = if n > 0 { sum / n as f32 } else { centre_h } + SEED_LIFT;
    for h in heights.iter_mut() {
        *h = if h.is_nan() { mean } else { *h + SEED_LIFT };
    }
    let origin_y = mean - half.y;
    Some(TerrainSeed { origin_y, heights, centre: Vec3::new(centre_xz.x, mean, centre_xz.y) })
}

/// A fresh volume seeded from the real heightmap (or a flat slab when `seed` is `None` — unresident
/// ground / solo sandbox), with the current journal replayed so prior sculpting returns.
fn monkey_voxel_fresh_seeded(
    chunks: [u32; 3],
    surface: f32,
    seed: Option<&TerrainSeed>,
) -> benilla_voxel::VoxelVolume {
    let mut volume = match seed {
        Some(s) => benilla_voxel::VoxelVolume::from_heightmap(chunks, s.origin_y, &s.heights),
        None => benilla_voxel::VoxelVolume::slab(chunks, surface),
    };
    let saved = monkey_voxel_journal_load();
    for e in &saved {
        monkey_voxel_apply_one(&mut volume, e);
    }
    if !saved.is_empty() {
        info!("MONKEY (voxel): replayed {} journalled edit(s)", saved.len());
    }
    volume
}

/// Spawn the single voxel-volume entity with its centre at `centre_bevy`. Used when no volume exists
/// yet (first auto-spawn, or a placement/adoption that beats the auto-spawn). The caller flips
/// [`MonkeyVoxelSiteSpawned`] so exactly one volume is ever created.
fn monkey_voxel_build_entity(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    material: &Handle<MonkeyVoxelMaterial>,
    chunks: [u32; 3],
    surface: f32,
    seed: Option<&TerrainSeed>,
    centre_bevy: Vec3,
) {
    let volume = monkey_voxel_fresh_seeded(chunks, surface, seed);
    let voxel_mesh = volume.mesh();
    let (tri_count, vert_count) = (voxel_mesh.tri_count(), voxel_mesh.positions.len());
    let mesh = meshes.add(monkey_voxel_mesh_to_bevy(&voxel_mesh));
    let origin = centre_bevy - half_extent_for(chunks);
    commands.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material.clone()),
        Transform::from_translation(origin),
        MonkeyVoxelVolume,
        MonkeyVoxelData(volume),
        // Static body once; `monkey_voxel_sync_collider` attaches/updates the trimesh Collider from
        // the density (it fires on this freshly-Added data next frame) and on every later edit.
        RigidBody::Static,
        Name::new("MONKEY voxel sandbox"),
    ));
    let wow = bevy_to_wow(centre_bevy);
    info!(
        "MONKEY (voxel): spawned terraform patch {:?} chunks — {vert_count} verts, {tri_count} tris \
         at WoW centre [{:.1}, {:.1}, {:.1}] (x,y,z). Brush [ dig / ] build / \\ smooth, -/= size; \
         Ctrl+P places a shared build site here for the whole guild.",
        chunks, wow[0], wow[1], wow[2],
    );
}

/// Relocate/reset the EXISTING volume to a fresh slab at `centre_bevy` (a placement or adoption when
/// the volume already exists), rather than despawning and respawning — which would momentarily leave
/// zero or (with deferred commands) two volumes and break the `single` queries.
fn monkey_voxel_relocate(
    data: &mut MonkeyVoxelData,
    transform: &mut Transform,
    mesh3d: &Mesh3d,
    meshes: &mut Assets<Mesh>,
    chunks: [u32; 3],
    surface: f32,
    seed: Option<&TerrainSeed>,
    centre_bevy: Vec3,
) {
    data.0 = monkey_voxel_fresh_seeded(chunks, surface, seed);
    transform.translation = centre_bevy - half_extent_for(chunks);
    let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&data.0.mesh()));
}

/// Apply an edit to a volume by its `mode` code. Returns whether anything changed.
fn monkey_voxel_apply_one(v: &mut benilla_voxel::VoxelVolume, e: &MonkeyVoxelEdit) -> bool {
    match e.mode {
        2 => v.smooth_sphere(e.center, e.radius, 0.5),
        1 => v.edit_sphere(e.center, e.radius, BrushMode::Add),
        _ => v.edit_sphere(e.center, e.radius, BrushMode::Subtract),
    }
}

/// Synchronous "a volume has been created this session" latch — shared by the auto-spawn, a GM
/// placement and an adoption so exactly ONE volume ever exists (a per-system `Local` latch let the
/// auto-spawn and a placement each create one when settle was slow → two volumes → broken queries).
#[derive(Resource, Default)]
pub struct MonkeyVoxelSiteSpawned(pub bool);

/// Whether the local player is a GM (`PLAYER_FLAGS & 0x8`, i.e. `.gm on`). benilla-world can't read
/// the network descriptor, so benilla-app fills this each frame from the self player's flags (the
/// `FfxDeathFade` bridge pattern). Gates the KEY-driven brush + build-site placement so only a GM
/// edits (owner decision 2); viewing/adopting a site and applying others' edits are NOT gated, and
/// the headless `WOW_VOXEL_AUTO*` test hooks bypass it. Defaults false until benilla-app sets it.
#[derive(Resource, Default)]
pub struct MonkeyLocalGm(pub bool);

/// P0 render: spawn one static voxel volume in front of the player, once a world is live.
pub struct MonkeyVoxelPlugin;

impl Plugin for MonkeyVoxelPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<MonkeyVoxelMaterial>::default());
        app.add_systems(Startup, material::monkey_voxel_create_material);
        app.init_resource::<MonkeyVoxelBrushRadius>();
        app.init_resource::<MonkeyVoxelNet>();
        app.init_resource::<MonkeyVoxelSiteSpawned>();
        app.init_resource::<MonkeyLocalGm>();
        // Spawn once, in front of the camera, so it is guaranteed on-screen on any login — gated on
        // a live world (a `WorldCamera` with a real pose) and a `Local` one-shot latch.
        // Chained for a deterministic order: volume management (create / place / adopt) runs BEFORE
        // edits apply that frame, so a freshly placed or adopted site is in place first.
        app.add_systems(
            Update,
            (
                monkey_voxel_manage_volume,
                monkey_voxel_capture_follow,
                monkey_voxel_brush_edit,
                monkey_voxel_apply_remote,
                monkey_voxel_autoedit,
                monkey_voxel_capture_demo_edit,
                monkey_voxel_sync_collider,
            )
                .chain()
                .run_if(crate::schedule::world_is_live),
        );
    }
}

/// Terraform sandbox size: chunks per axis. `[2,1,2]` = a 32 × 16 × 32 yd ground patch (4 chunks).
const VOL_CHUNKS: [u32; 3] = [2, 2, 2];
/// Flat-top height within the volume, in voxels (solid below, air above).
const VOL_SURFACE: f32 = 16.0;

/// Half the world size (yards) of a volume with the given chunk dims — offsets the entity origin so
/// the volume centre lands on a target point.
fn half_extent_for(chunks: [u32; 3]) -> Vec3 {
    Vec3::new(chunks[0] as f32, chunks[1] as f32, chunks[2] as f32)
        * (benilla_voxel::CHUNK as f32 * benilla_voxel::VOXEL_SIZE * 0.5)
}

/// Half the sandbox volume's world size — the aim reference for the brush.
fn volume_half_extent() -> Vec3 {
    half_extent_for(VOL_CHUNKS)
}

/// Yards in front of the camera to place the volume centre (close, so it's right there to sculpt).
const SPAWN_AHEAD_YARDS: f32 = 12.0;
/// Approx. eye height above the feet (yards), to drop the patch's flat top to ground level.
const EYE_HEIGHT_YARDS: f32 = 3.0;

/// Current brush radius in voxels (adjustable live with `-`/`=`). Default ~2 yd.
#[derive(Resource)]
pub struct MonkeyVoxelBrushRadius(pub f32);

impl Default for MonkeyVoxelBrushRadius {
    fn default() -> Self {
        Self(4.0)
    }
}

/// The ONE system that owns the single voxel volume: it creates it, relocates it on a shared-site
/// placement (local Ctrl+P / `$WOW_VOXEL_AUTOPLACE`, or one adopted from another player), and reads
/// the persisted site on relog. Keeping all of this in a single system avoids the cross-system
/// deferred-command race that made two volumes (auto-spawn queues a volume the place/adopt system
/// can't yet see via `single`, so it made a second one).
#[allow(clippy::too_many_arguments)]
fn monkey_voxel_manage_volume(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut auto_place_done: Local<bool>,
    mut place_elapsed: Local<f32>,
    mut settle_frames: Local<u32>,
    mut last_cam: Local<Option<Vec3>>,
    mut spawned: ResMut<MonkeyVoxelSiteSpawned>,
    mut net: ResMut<MonkeyVoxelNet>,
    gm: Res<MonkeyLocalGm>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Res<MonkeyVoxelMaterialHandle>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    world: crate::world_point::WorldPoint,
    mut volume: Query<(&mut MonkeyVoxelData, &mut Transform, &Mesh3d), With<MonkeyVoxelVolume>>,
) {
    // Wait for a world camera with a real pose (the player has entered the world).
    let Ok(cam) = camera.single() else {
        return;
    };
    let cam_pos = cam.translation();
    if cam_pos == Vec3::ZERO {
        return;
    }

    // A new shared build site this frame: (centre_bevy, chunks, surface). Adoption (from another
    // player) wins over a local placement if both land in one frame.
    let mut new_site: Option<(Vec3, [u32; 3], f32)> = None;

    if let Some(site) = net.spawn_inbox.drain(..).last() {
        monkey_voxel_site_save(&site);
        monkey_voxel_journal_clear();
        let chunks = [
            site.chunks[0] as u32,
            site.chunks[1] as u32,
            site.chunks[2] as u32,
        ];
        new_site = Some((wow_to_bevy(site.anchor_wow), chunks, site.surface));
        let a = site.anchor_wow;
        info!(
            "MONKEY (voxel): adopted a shared build site from another player at WoW [{:.1}, {:.1}, {:.1}].",
            a[0], a[1], a[2],
        );
    }

    // Local placement (Ctrl+P, GM only; or the headless `$WOW_VOXEL_AUTOPLACE` hook ~4 s in, which
    // bypasses the gate for tests).
    let key_place = gm.0
        && (keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight))
        && keys.just_pressed(KeyCode::KeyP);
    let auto_place = !*auto_place_done && std::env::var_os("WOW_VOXEL_AUTOPLACE").is_some() && {
        *place_elapsed += time.delta_secs();
        *place_elapsed >= 4.0
    };
    if auto_place {
        *auto_place_done = true;
    }
    if new_site.is_none() && (key_place || auto_place) {
        let centre = site_centre_at_player(cam, &world, VOL_CHUNKS, VOL_SURFACE);
        let site = MonkeyVoxelSpawn {
            anchor_wow: bevy_to_wow(centre),
            chunks: [
                VOL_CHUNKS[0] as u8,
                VOL_CHUNKS[1] as u8,
                VOL_CHUNKS[2] as u8,
            ],
            surface: VOL_SURFACE,
        };
        monkey_voxel_site_save(&site);
        monkey_voxel_journal_clear();
        net.spawn_outbox.push(site);
        new_site = Some((centre, VOL_CHUNKS, VOL_SURFACE));
        let a = site.anchor_wow;
        info!(
            "MONKEY (voxel): placed a shared build site at WoW [{:.1}, {:.1}, {:.1}] and broadcast it.",
            a[0], a[1], a[2],
        );
    }

    // Apply. If the volume already exists (from an earlier frame — commands are flushed by now),
    // relocate it on a new site. If it does not exist yet, create exactly one: at a new site, else a
    // persisted site, else the solo-sandbox placement ahead of the settled view.
    if let Ok((mut data, mut tf, mesh3d)) = volume.single_mut() {
        if let Some((mut centre, chunks, surface)) = new_site {
            // Seed the volume from the REAL ground under its footprint (so it starts as the terrain
            // there); re-centre vertically on the mean ground height. Falls back to a flat pad if the
            // ground isn't resident.
            let seed = monkey_voxel_sample_terrain(&world, Vec2::new(centre.x, centre.z), chunks);
            if let Some(s) = &seed {
                centre.y = s.centre.y;
            }
            monkey_voxel_relocate(
                &mut data,
                &mut tf,
                mesh3d,
                &mut meshes,
                chunks,
                surface,
                seed.as_ref(),
                centre,
            );
        }
        return;
    }
    if spawned.0 {
        return; // volume creation already queued this/last frame; nothing to do until it exists
    }
    if let Some((mut centre, chunks, surface)) = new_site {
        let seed = monkey_voxel_sample_terrain(&world, Vec2::new(centre.x, centre.z), chunks);
        if let Some(s) = &seed {
            centre.y = s.centre.y;
        }
        monkey_voxel_build_entity(
            &mut commands,
            &mut meshes,
            &material.0,
            chunks,
            surface,
            seed.as_ref(),
            centre,
        );
        spawned.0 = true;
    } else if let Some(site) = monkey_voxel_site_load() {
        let chunks = [
            site.chunks[0] as u32,
            site.chunks[1] as u32,
            site.chunks[2] as u32,
        ];
        let mut c = wow_to_bevy(site.anchor_wow);
        let seed = monkey_voxel_sample_terrain(&world, Vec2::new(c.x, c.z), chunks);
        if let Some(s) = &seed {
            c.y = s.centre.y;
        }
        monkey_voxel_build_entity(
            &mut commands,
            &mut meshes,
            &material.0,
            chunks,
            site.surface,
            seed.as_ref(),
            c,
        );
        spawned.0 = true;
    } else {
        // Solo sandbox: wait for the camera to settle (world-enter keeps nudging it), then seed a
        // region from the real ground a short way ahead along the settled view.
        let settled = last_cam.is_some_and(|p| p.distance(cam_pos) < 0.5);
        *last_cam = Some(cam_pos);
        *settle_frames = if settled { *settle_frames + 1 } else { 0 };
        if *settle_frames < 30 {
            return; // ~0.5 s of a still camera
        }
        let ahead = cam_pos + *cam.forward() * SPAWN_AHEAD_YARDS;
        let seed = monkey_voxel_sample_terrain(&world, Vec2::new(ahead.x, ahead.z), VOL_CHUNKS);
        let half = volume_half_extent();
        let centre = match &seed {
            Some(s) => s.centre,
            None => {
                let ground = world
                    .terrain_height_under(ahead)
                    .unwrap_or(cam_pos.y - EYE_HEIGHT_YARDS);
                Vec3::new(ahead.x, (ground + 1.5) - VOL_SURFACE * benilla_voxel::VOXEL_SIZE + half.y, ahead.z)
            }
        };
        monkey_voxel_build_entity(
            &mut commands,
            &mut meshes,
            &material.0,
            VOL_CHUNKS,
            VOL_SURFACE,
            seed.as_ref(),
            centre,
        );
        spawned.0 = true;
    }
}

/// Compute the build-site centre (Bevy) for a volume of `chunks`/`surface` placed at the player's
/// feet: horizontally under the camera, its flat top ~1.5 yd above the ground there (foot level if
/// the terrain isn't loaded).
fn site_centre_at_player(
    cam: &GlobalTransform,
    world: &crate::world_point::WorldPoint,
    chunks: [u32; 3],
    surface: f32,
) -> Vec3 {
    let cam_pos = cam.translation();
    let half = half_extent_for(chunks);
    let ground = world
        .terrain_height_under(cam_pos)
        .unwrap_or(cam_pos.y - EYE_HEIGHT_YARDS);
    let top_y = ground + 1.5;
    Vec3::new(
        cam_pos.x,
        top_y - surface * benilla_voxel::VOXEL_SIZE + half.y,
        cam_pos.z,
    )
}

/// Capture-only framing aid. In `$WOW_CAPTURE` mode the scenario camera settles to its framing a
/// few frames AFTER the one-shot spawn, which would leave the volume off-screen (both baselines log
/// the identical spawn centre yet frame differently). While capturing, keep the volume glued ~22 yd
/// in front of the CURRENT camera so the shot always frames it. In a live session the env is unset,
/// so this is a no-op and the spawned volume stays fixed in the world as intended.
fn monkey_voxel_capture_follow(
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    mut volume: Query<&mut Transform, With<MonkeyVoxelVolume>>,
) {
    if std::env::var_os("WOW_CAPTURE").is_none() {
        return;
    }
    let (Ok(cam), Ok(mut tf)) = (camera.single(), volume.single_mut()) else {
        return;
    };
    let centre = cam.translation() + *cam.forward() * SPAWN_AHEAD_YARDS - Vec3::Y * 3.0;
    tf.translation = centre - volume_half_extent();
}

/// P1 local voxel brush (GM only). Hold **`[`** carve · **`]`** build · **`\`** smooth, aimed at the
/// volume along your view; **`-`/`=`** change brush size. Mutates the chunk's density field and
/// re-meshes live, broadcasting each edit to the guild. GM-gated (owner decision 2) — a non-GM sees
/// edits but can't make them. `.gm on` to enable.
fn monkey_voxel_brush_edit(
    keys: Res<ButtonInput<KeyCode>>,
    gm: Res<MonkeyLocalGm>,
    mut radius: ResMut<MonkeyVoxelBrushRadius>,
    mut meshes: ResMut<Assets<Mesh>>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    mut volume: Query<(&mut MonkeyVoxelData, &Mesh3d, &Transform), With<MonkeyVoxelVolume>>,
    mut net: ResMut<MonkeyVoxelNet>,
) {
    if !gm.0 {
        return; // GM-only; a non-GM's brush keys do nothing (they still see others' edits).
    }
    // Brush size: `-` shrinks, `=` grows (clamped 1..14 voxels).
    if keys.just_pressed(KeyCode::Minus) {
        radius.0 = (radius.0 - 1.0).max(1.0);
    }
    if keys.just_pressed(KeyCode::Equal) {
        radius.0 = (radius.0 + 1.0).min(14.0);
    }

    // Brush mode (held = continuous): [ carve, ] build, \ smooth.
    let mode = if keys.pressed(KeyCode::BracketLeft) {
        BrushMode::Subtract
    } else if keys.pressed(KeyCode::BracketRight) {
        BrushMode::Add
    } else if keys.pressed(KeyCode::Backslash) {
        BrushMode::Smooth
    } else {
        return;
    };
    let (Ok(cam), Ok((mut data, mesh3d, tf))) = (camera.single(), volume.single_mut()) else {
        return;
    };

    // Aim the brush at the volume centre's depth along the view ray, so looking at the patch always
    // lands on it and turning your view sweeps the brush across it.
    let vol_centre = tf.translation + volume_half_extent();
    let dist = (vol_centre - cam.translation()).length();
    let world = cam.translation() + *cam.forward() * dist;
    let local = ((world - tf.translation) / benilla_voxel::VOXEL_SIZE).to_array();

    let r = radius.0;
    let changed = match mode {
        BrushMode::Smooth => data.0.smooth_sphere(local, r, 0.5),
        m => data.0.edit_sphere(local, r, m),
    };
    if changed {
        let mode_code = match mode {
            BrushMode::Subtract => 0,
            BrushMode::Add => 1,
            BrushMode::Smooth => 2,
        };
        let edit = MonkeyVoxelEdit {
            center: local,
            radius: r,
            mode: mode_code,
        };
        monkey_voxel_journal_append(&edit); // persist (survives relog)
        net.outbox.push(edit); // broadcast to other players (net relay drains the outbox)
                               // insert returns a prior-asset Result we don't need; the handle stays the same.
        let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&data.0.mesh()));
    }
}

/// Apply edits received from other players (the net relay fills the inbox) to the local volume and
/// re-mesh. Because every client's base slab is identical and edits are in local voxel coords, this
/// reproduces the same deformation the sender made.
fn monkey_voxel_apply_remote(
    mut net: ResMut<MonkeyVoxelNet>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut volume: Query<(&mut MonkeyVoxelData, &Mesh3d), With<MonkeyVoxelVolume>>,
) {
    if net.inbox.is_empty() {
        return;
    }
    let Ok((mut data, mesh3d)) = volume.single_mut() else {
        // The volume hasn't spawned yet (a remote edit can arrive before our own patch settles).
        // KEEP the inbox buffered so those edits apply the moment the volume exists, rather than
        // dropping them — the race the 2-client test caught. Cap it so an idle pre-world client
        // can't grow it without bound.
        const MAX_BUFFERED: usize = 8192;
        if net.inbox.len() > MAX_BUFFERED {
            let overflow = net.inbox.len() - MAX_BUFFERED;
            net.inbox.drain(0..overflow);
        }
        return;
    };
    let edits: Vec<MonkeyVoxelEdit> = net.inbox.drain(..).collect();
    let n = edits.len();
    let mut changed = false;
    for e in edits {
        if monkey_voxel_apply_one(&mut data.0, &e) {
            monkey_voxel_journal_append(&e); // persist remote edits too
            changed = true;
        }
    }
    if changed {
        info!("MONKEY (voxel): applied {n} remote edit(s) from other players");
        let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&data.0.mesh()));
    }
}

/// Test/dev hook: when `$WOW_VOXEL_AUTOEDIT` is set, make one build edit ~8 s after the patch
/// spawns, through the SAME path as the brush (apply locally + journal + push to the outbox so it
/// broadcasts). Lets a headless client exercise the multiplayer relay without keyboard input. No-op
/// unless the env var is present.
fn monkey_voxel_autoedit(
    mut done: Local<bool>,
    mut elapsed: Local<f32>,
    time: Res<Time>,
    mut net: ResMut<MonkeyVoxelNet>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut volume: Query<(&mut MonkeyVoxelData, &Mesh3d), With<MonkeyVoxelVolume>>,
) {
    if *done || std::env::var_os("WOW_VOXEL_AUTOEDIT").is_none() {
        return;
    }
    *elapsed += time.delta_secs();
    if *elapsed < 8.0 {
        return; // let the world settle and the channel join land first
    }
    let Ok((mut data, mesh3d)) = volume.single_mut() else {
        return;
    };
    let edit = MonkeyVoxelEdit {
        center: [33.0, 20.0, 33.0],
        radius: 8.0,
        mode: 1,
    };
    if monkey_voxel_apply_one(&mut data.0, &edit) {
        monkey_voxel_journal_append(&edit);
        net.outbox.push(edit); // broadcast to other players
        let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&data.0.mesh()));
        info!("MONKEY (voxel): AUTOEDIT fired — broadcast a build mound");
    }
    *done = true;
}

/// Capture-only proof that the edit → re-mesh → render chain works headlessly: once the volume
/// exists, carve two bites out of the volume so the baseline screenshot shows a *deformed* volume.
/// No-op in live play (env unset) — there the brush drives edits.
fn monkey_voxel_capture_demo_edit(
    mut done: Local<bool>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut volume: Query<(&mut MonkeyVoxelData, &Mesh3d), With<MonkeyVoxelVolume>>,
) {
    if *done || std::env::var_os("WOW_CAPTURE").is_none() {
        return;
    }
    let Ok((mut data, mesh3d)) = volume.single_mut() else {
        return;
    };
    // Terraform the patch for the baseline shot (global voxel coords; top surface at y=VOL_SURFACE):
    // two craters, a trench across the chunk seam, a raised mound, then a smooth pass.
    let v = &mut data.0;
    v.edit_sphere([20.0, 16.0, 22.0], 7.0, BrushMode::Subtract);
    v.edit_sphere([46.0, 16.0, 42.0], 6.0, BrushMode::Subtract);
    v.edit_sphere([32.0, 14.0, 12.0], 5.0, BrushMode::Subtract);
    v.edit_sphere([32.0, 14.0, 30.0], 5.0, BrushMode::Subtract);
    v.edit_sphere([34.0, 22.0, 46.0], 8.0, BrushMode::Add);
    v.smooth_sphere([30.0, 18.0, 30.0], 12.0, 0.5);
    let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&v.mesh()));
    *done = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monkey_voxel_edit_hex_roundtrips() {
        let e = MonkeyVoxelEdit {
            center: [13.5, 16.0, 22.25],
            radius: 4.0,
            mode: 1,
        };
        let hex = e.to_hex();
        assert_eq!(hex.len(), 34, "17 bytes = 34 hex chars");
        let d = MonkeyVoxelEdit::from_hex(&hex).expect("decode");
        assert_eq!(d.center, e.center);
        assert_eq!(d.radius, e.radius);
        assert_eq!(d.mode, e.mode);
        assert!(
            MonkeyVoxelEdit::from_hex("short").is_none(),
            "malformed rejected"
        );
    }

    #[test]
    fn voxel_mesh_converts_with_matching_counts() {
        let vm = benilla_voxel::VoxelVolume::slab([1, 1, 1], 16.0).mesh();
        assert!(!vm.is_empty(), "volume produced no geometry");

        let mesh = monkey_voxel_mesh_to_bevy(&vm);

        assert_eq!(mesh.primitive_topology(), PrimitiveTopology::TriangleList);
        // One Bevy vertex per mesher position, one normal per vertex.
        assert_eq!(mesh.count_vertices(), vm.positions.len());
        assert_eq!(vm.normals.len(), vm.positions.len());
        // Indices carried over intact as U32.
        match mesh.indices() {
            Some(Indices::U32(ix)) => assert_eq!(ix.len(), vm.indices.len()),
            other => panic!("expected U32 indices, got {other:?}"),
        }
    }
}

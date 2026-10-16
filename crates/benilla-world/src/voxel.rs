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

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;

use crate::view::WorldCamera;
use benilla_assets::coords::bevy_to_wow;
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
    // Bake a simple top-down shade (from each normal's up-component) into an earthy vertex colour,
    // so the sculpted form reads without the world's custom terrain lighting — a plain lit
    // StandardMaterial renders black against benilla's bespoke light. The real triplanar terrain
    // material replaces this in a later lane.
    const EARTH: [f32; 3] = [0.62, 0.52, 0.38];
    let colors: Vec<[f32; 4]> = vm
        .normals
        .iter()
        .map(|n| {
            let up = (n[1] * 0.5 + 0.5).clamp(0.0, 1.0); // Bevy +Y up: top faces brighter
            let b = 0.35 + 0.65 * up;
            [EARTH[0] * b, EARTH[1] * b, EARTH[2] * b, 1.0]
        })
        .collect();
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(vm.indices.clone()));
    mesh
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

/// Cross-crate mailbox between the voxel brush (benilla-world) and the chat-channel relay
/// (benilla-app, behind the `voxel` feature): local edits to broadcast go in `outbox`; edits
/// received from other players land in `inbox`. benilla-app drains the outbox and sends each as a
/// `MonkeyVoxel` channel message, and pushes decoded remote edits into the inbox.
#[derive(Resource, Default)]
pub struct MonkeyVoxelNet {
    pub outbox: Vec<MonkeyVoxelEdit>,
    pub inbox: Vec<MonkeyVoxelEdit>,
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
        .filter_map(|c| MonkeyVoxelEdit::from_hex(&c.iter().map(|x| format!("{x:02x}")).collect::<String>()))
        .collect()
}

/// Apply an edit to a volume by its `mode` code. Returns whether anything changed.
fn monkey_voxel_apply_one(v: &mut benilla_voxel::VoxelVolume, e: &MonkeyVoxelEdit) -> bool {
    match e.mode {
        2 => v.smooth_sphere(e.center, e.radius, 0.5),
        1 => v.edit_sphere(e.center, e.radius, BrushMode::Add),
        _ => v.edit_sphere(e.center, e.radius, BrushMode::Subtract),
    }
}

/// P0 render: spawn one static voxel volume in front of the player, once a world is live.
pub struct MonkeyVoxelPlugin;

impl Plugin for MonkeyVoxelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MonkeyVoxelBrushRadius>();
        app.init_resource::<MonkeyVoxelNet>();
        // Spawn once, in front of the camera, so it is guaranteed on-screen on any login — gated on
        // a live world (a `WorldCamera` with a real pose) and a `Local` one-shot latch.
        app.add_systems(
            Update,
            (
                spawn_monkey_voxel_volume,
                monkey_voxel_capture_follow,
                monkey_voxel_brush_edit,
                monkey_voxel_apply_remote,
                monkey_voxel_autoedit,
                monkey_voxel_capture_demo_edit,
            )
                .run_if(crate::schedule::world_is_live),
        );
    }
}

/// Terraform sandbox size: chunks per axis. `[2,1,2]` = a 32 × 16 × 32 yd ground patch (4 chunks).
const VOL_CHUNKS: [u32; 3] = [2, 1, 2];
/// Flat-top height within the volume, in voxels (solid below, air above).
const VOL_SURFACE: f32 = 16.0;

/// Half the volume's world size (yards) — offsets the entity origin so the volume centre lands on a
/// target point, and is the aim reference for the brush.
fn volume_half_extent() -> Vec3 {
    Vec3::new(VOL_CHUNKS[0] as f32, VOL_CHUNKS[1] as f32, VOL_CHUNKS[2] as f32)
        * (benilla_voxel::CHUNK as f32 * benilla_voxel::VOXEL_SIZE * 0.5)
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

fn spawn_monkey_voxel_volume(
    mut done: Local<bool>,
    mut settle_frames: Local<u32>,
    mut last_cam: Local<Option<Vec3>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    world: crate::world_point::WorldPoint,
) {
    if *done {
        return;
    }
    // Wait for a world camera with a non-origin pose (the player has entered the world).
    let Ok(cam) = camera.single() else {
        return;
    };
    let cam_pos = cam.translation();
    if cam_pos == Vec3::ZERO {
        return; // camera not yet placed in the world
    }
    // World-enter keeps moving the camera for a second or two as it settles behind the player. Spawn
    // only once it has held still, so the one-shot patch lands in front of the SETTLED view rather
    // than the transient load-in pose (the same settle trap capture mode hit).
    let settled = last_cam.is_some_and(|p| p.distance(cam_pos) < 0.5);
    *last_cam = Some(cam_pos);
    *settle_frames = if settled { *settle_frames + 1 } else { 0 };
    if *settle_frames < 30 {
        return; // ~0.5 s of a still camera
    }

    // Build the terraform sandbox: a multi-chunk flat-topped ground patch (slab) → combined mesh,
    // then replay the edit journal so prior terraforming returns after a relog (persistence).
    let mut volume = benilla_voxel::VoxelVolume::slab(VOL_CHUNKS, VOL_SURFACE);
    let saved = monkey_voxel_journal_load();
    for e in &saved {
        monkey_voxel_apply_one(&mut volume, e);
    }
    if !saved.is_empty() {
        info!("MONKEY (voxel): replayed {} journalled edit(s)", saved.len());
    }
    let voxel_mesh = volume.mesh();
    let tri_count = voxel_mesh.tri_count();
    let vert_count = voxel_mesh.positions.len();
    let mesh = meshes.add(monkey_voxel_mesh_to_bevy(&voxel_mesh));

    // Unlit + double-sided; the earthy shading rides in the mesh's vertex colours (see
    // monkey_voxel_mesh_to_bevy). Unlit dodges benilla's custom light (a lit material = black).
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        cull_mode: None,
        ..default()
    });

    // Place the patch a short way ahead along the SETTLED view (in front of the player, in the
    // look direction), with its flat top a touch above the real ground so it's a clearly visible
    // low platform rather than flush (which z-fights and hides under the terrain). Fall back to
    // foot level if the ground there isn't loaded.
    let ahead = cam_pos + *cam.forward() * SPAWN_AHEAD_YARDS;
    let half = volume_half_extent();
    let ground = world
        .terrain_height_under(ahead)
        .unwrap_or(cam_pos.y - EYE_HEIGHT_YARDS);
    let top_y = ground + 1.5;
    let origin = Vec3::new(
        ahead.x - half.x,
        top_y - VOL_SURFACE * benilla_voxel::VOXEL_SIZE,
        ahead.z - half.z,
    );
    let centre = origin + half;

    commands.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_translation(origin),
        MonkeyVoxelVolume,
        MonkeyVoxelData(volume),
        Name::new("MONKEY voxel sandbox"),
    ));

    let wow = bevy_to_wow(centre);
    info!(
        "MONKEY (voxel): spawned terraform patch {:?} chunks — {vert_count} verts, {tri_count} tris. \
         Bevy centre {:.1?}, WoW centre [{:.1}, {:.1}, {:.1}] (x,y,z). Look ~{SPAWN_AHEAD_YARDS} yd \
         ahead (and down); brush [ dig / ] build / \\ smooth, -/= size.",
        VOL_CHUNKS,
        centre.to_array(),
        wow[0],
        wow[1],
        wow[2],
    );
    *done = true;
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

/// P1 local voxel brush (dev/GM). Hold **`[`** carve · **`]`** build · **`\`** smooth, aimed at the
/// volume along your view; **`-`/`=`** change brush size. Mutates the chunk's density field and
/// re-meshes live. Client-only for now — server authority, GM-gating and multiplayer sync arrive in
/// P1b (`DESIGN-VoxelTerrain.md`).
fn monkey_voxel_brush_edit(
    keys: Res<ButtonInput<KeyCode>>,
    mut radius: ResMut<MonkeyVoxelBrushRadius>,
    mut meshes: ResMut<Assets<Mesh>>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    mut volume: Query<(&mut MonkeyVoxelData, &Mesh3d, &Transform), With<MonkeyVoxelVolume>>,
    mut net: ResMut<MonkeyVoxelNet>,
) {
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
        let edit = MonkeyVoxelEdit { center: local, radius: r, mode: mode_code };
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
    let edit = MonkeyVoxelEdit { center: [33.0, 20.0, 33.0], radius: 8.0, mode: 1 };
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
        let e = MonkeyVoxelEdit { center: [13.5, 16.0, 22.25], radius: 4.0, mode: 1 };
        let hex = e.to_hex();
        assert_eq!(hex.len(), 34, "17 bytes = 34 hex chars");
        let d = MonkeyVoxelEdit::from_hex(&hex).expect("decode");
        assert_eq!(d.center, e.center);
        assert_eq!(d.radius, e.radius);
        assert_eq!(d.mode, e.mode);
        assert!(MonkeyVoxelEdit::from_hex("short").is_none(), "malformed rejected");
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

//! MONKEY (voxel): P0 Lane B — render a single code-generated voxel volume in-engine.
//!
//! This is the render half of the voxel-terrain programme (`DESIGN-VoxelTerrain.md`, option C). The
//! data + mesher live engine-free in `benilla-voxel`; here we turn one of its [`benilla_voxel::VoxelMesh`]
//! surfaces into a Bevy [`Mesh`] and spawn it as a visible entity, proving the renderer consumes the
//! mesher's output. No edits, no collision, no server — those are later lanes.
//!
//! Everything in this module is gated behind the crate's `voxel` cargo feature (OFF by default), so
//! the shipped player build is byte-unchanged. The torus is the P0 overhang proof: a genuinely
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
    mesh.insert_indices(Indices::U32(vm.indices.clone()));
    mesh
}

/// Marks the P0 debug voxel volume entity.
#[derive(Component)]
pub struct MonkeyVoxelVolume;

/// Keeps the volume's editable density field on the entity so the P1 brush can mutate it and
/// re-mesh live. The chunk origin is the entity's `Transform.translation` (world yards).
#[derive(Component)]
pub struct MonkeyVoxelChunk(pub benilla_voxel::Chunk);

/// P0 render: spawn one static voxel torus in front of the player, once a world is live.
pub struct MonkeyVoxelPlugin;

impl Plugin for MonkeyVoxelPlugin {
    fn build(&self, app: &mut App) {
        // Spawn once, in front of the camera, so it is guaranteed on-screen on any login — gated on
        // a live world (a `WorldCamera` with a real pose) and a `Local` one-shot latch.
        app.add_systems(
            Update,
            (
                spawn_monkey_voxel_torus,
                monkey_voxel_capture_follow,
                monkey_voxel_brush_edit,
                monkey_voxel_capture_demo_edit,
            )
                .run_if(crate::schedule::world_is_live),
        );
    }
}

/// The torus centre in the mesher's local space: voxel coord `[17, 17, 17]` × `VOXEL_SIZE` yards.
const TORUS_LOCAL_CENTRE: f32 =
    17.0 * benilla_voxel::VOXEL_SIZE;

/// Yards in front of the camera to place the torus centre.
const SPAWN_AHEAD_YARDS: f32 = 22.0;

fn spawn_monkey_voxel_torus(
    mut done: Local<bool>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
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

    // Build the P0 overhang-proof torus and convert it to a Bevy mesh.
    let chunk = benilla_voxel::torus(10.0, 4.0, [17.0, 17.0, 17.0]);
    let voxel_mesh = chunk.mesh();
    let tri_count = voxel_mesh.tri_count();
    let vert_count = voxel_mesh.positions.len();
    let mesh = meshes.add(monkey_voxel_mesh_to_bevy(&voxel_mesh));

    // A bright, unlit, double-sided material so it is unmistakable regardless of time-of-day light.
    let material = materials.add(StandardMaterial {
        base_color: Color::srgb(1.0, 0.35, 0.0),
        unlit: true,
        cull_mode: None,
        ..default()
    });

    // Place the torus centre ~22 yd in front of the camera; the mesh is local to its chunk origin,
    // so offset the transform by the local centre to put the ring on that point.
    let centre = cam_pos + *cam.forward() * SPAWN_AHEAD_YARDS;
    let origin = centre - Vec3::splat(TORUS_LOCAL_CENTRE);

    commands.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_translation(origin),
        MonkeyVoxelVolume,
        MonkeyVoxelChunk(chunk),
        Name::new("MONKEY voxel torus (P0)"),
    ));

    let wow = bevy_to_wow(centre);
    info!(
        "MONKEY (voxel): spawned P0 torus — {vert_count} verts, {tri_count} tris. \
         Bevy centre {:.1?}, WoW centre [{:.1}, {:.1}, {:.1}] (x,y,z). Look ~{SPAWN_AHEAD_YARDS} yd ahead.",
        centre.to_array(),
        wow[0],
        wow[1],
        wow[2],
    );
    *done = true;
}

/// Capture-only framing aid. In `$WOW_CAPTURE` mode the scenario camera settles to its framing a
/// few frames AFTER the one-shot spawn, which would leave the torus off-screen (both baselines log
/// the identical spawn centre yet frame differently). While capturing, keep the torus glued ~22 yd
/// in front of the CURRENT camera so the shot always frames it. In a live session the env is unset,
/// so this is a no-op and the spawned torus stays fixed in the world as intended.
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
    let centre = cam.translation() + *cam.forward() * SPAWN_AHEAD_YARDS;
    tf.translation = centre - Vec3::splat(TORUS_LOCAL_CENTRE);
}

/// P1 local voxel brush (dev/GM): hold **`[`** to carve, **`]`** to build, aimed at the voxel volume
/// along your view. Mutates the chunk's density field and re-meshes live. Client-only for now —
/// server authority, GM-gating and multiplayer sync arrive in P1b (`DESIGN-VoxelTerrain.md`).
fn monkey_voxel_brush_edit(
    keys: Res<ButtonInput<KeyCode>>,
    mut meshes: ResMut<Assets<Mesh>>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    mut volume: Query<(&mut MonkeyVoxelChunk, &Mesh3d, &Transform), With<MonkeyVoxelVolume>>,
) {
    let mode = if keys.pressed(KeyCode::BracketLeft) {
        BrushMode::Subtract
    } else if keys.pressed(KeyCode::BracketRight) {
        BrushMode::Add
    } else {
        return;
    };
    let (Ok(cam), Ok((mut chunk, mesh3d, tf))) = (camera.single(), volume.single_mut()) else {
        return;
    };

    // Aim the brush at the volume's depth along the view ray, so looking at the volume always lands
    // on it and turning your view sweeps the brush across its face.
    let vol_centre = tf.translation + Vec3::splat(TORUS_LOCAL_CENTRE);
    let dist = (vol_centre - cam.translation()).length();
    let world = cam.translation() + *cam.forward() * dist;
    let local = (world - tf.translation) / benilla_voxel::VOXEL_SIZE;

    const BRUSH_RADIUS_VOXELS: f32 = 4.0; // ~2 yd
    if chunk.0.edit_sphere(local.to_array(), BRUSH_RADIUS_VOXELS, mode) {
        // insert returns a prior-asset Result we don't need; the handle stays the same.
        let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&chunk.0.mesh()));
    }
}

/// Capture-only proof that the edit → re-mesh → render chain works headlessly: once the volume
/// exists, carve two bites out of the torus so the baseline screenshot shows a *deformed* volume.
/// No-op in live play (env unset) — there the brush drives edits.
fn monkey_voxel_capture_demo_edit(
    mut done: Local<bool>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut volume: Query<(&mut MonkeyVoxelChunk, &Mesh3d), With<MonkeyVoxelVolume>>,
) {
    if *done || std::env::var_os("WOW_CAPTURE").is_none() {
        return;
    }
    let Ok((mut chunk, mesh3d)) = volume.single_mut() else {
        return;
    };
    // Two holes on the ring (local voxel coords on the tube: centre [17,17,17], major radius 10).
    chunk.0.edit_sphere([27.0, 17.0, 17.0], 5.0, BrushMode::Subtract);
    chunk.0.edit_sphere([17.0, 27.0, 17.0], 4.0, BrushMode::Subtract);
    let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&chunk.0.mesh()));
    *done = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voxel_mesh_converts_with_matching_counts() {
        let chunk = benilla_voxel::torus(10.0, 4.0, [17.0, 17.0, 17.0]);
        let vm = chunk.mesh();
        assert!(!vm.is_empty(), "torus produced no geometry");

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

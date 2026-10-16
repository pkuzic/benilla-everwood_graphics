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

/// P0 render: spawn one static voxel torus in front of the player, once a world is live.
pub struct MonkeyVoxelPlugin;

impl Plugin for MonkeyVoxelPlugin {
    fn build(&self, app: &mut App) {
        // Spawn once, in front of the camera, so it is guaranteed on-screen on any login — gated on
        // a live world (a `WorldCamera` with a real pose) and a `Local` one-shot latch.
        app.add_systems(
            Update,
            spawn_monkey_voxel_torus.run_if(crate::schedule::world_is_live),
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

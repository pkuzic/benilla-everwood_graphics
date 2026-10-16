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

/// Keeps the volume's editable density field on the entity so the P1 brush can mutate it and
/// re-mesh live. The chunk origin is the entity's `Transform.translation` (world yards).
#[derive(Component)]
pub struct MonkeyVoxelChunk(pub benilla_voxel::Chunk);

/// P0 render: spawn one static voxel volume in front of the player, once a world is live.
pub struct MonkeyVoxelPlugin;

impl Plugin for MonkeyVoxelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MonkeyVoxelBrushRadius>();
        // Spawn once, in front of the camera, so it is guaranteed on-screen on any login — gated on
        // a live world (a `WorldCamera` with a real pose) and a `Local` one-shot latch.
        app.add_systems(
            Update,
            (
                spawn_monkey_voxel_volume,
                monkey_voxel_capture_follow,
                monkey_voxel_brush_edit,
                monkey_voxel_capture_demo_edit,
            )
                .run_if(crate::schedule::world_is_live),
        );
    }
}

/// The volume centre in the mesher's local space: voxel coord `[17, 17, 17]` × `VOXEL_SIZE` yards.
const VOLUME_LOCAL_CENTRE: f32 = 17.0 * benilla_voxel::VOXEL_SIZE;

/// Yards in front of the camera to place the volume centre.
const SPAWN_AHEAD_YARDS: f32 = 22.0;

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

    // Build the terraform sandbox: a flat-topped ground block (slab) → Bevy mesh. One 32³ chunk at
    // 0.5 yd = a 16 yd cube; solid in the lower half, flat top at the chunk mid-height.
    let chunk = benilla_voxel::slab(17.0);
    let voxel_mesh = chunk.mesh();
    let tri_count = voxel_mesh.tri_count();
    let vert_count = voxel_mesh.positions.len();
    let mesh = meshes.add(monkey_voxel_mesh_to_bevy(&voxel_mesh));

    // Unlit + double-sided; the earthy shading rides in the mesh's vertex colours (see
    // monkey_voxel_mesh_to_bevy), which unlit renders as base_color * vertex_colour. White base =
    // the baked colour shows through. Unlit dodges benilla's custom light (a lit material = black).
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        cull_mode: None,
        ..default()
    });

    // Place the volume centre ~22 yd in front of the camera; the mesh is local to its chunk origin,
    // so offset the transform by the local centre to put the ring on that point.
    let centre = cam_pos + *cam.forward() * SPAWN_AHEAD_YARDS;
    let origin = centre - Vec3::splat(VOLUME_LOCAL_CENTRE);

    commands.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_translation(origin),
        MonkeyVoxelVolume,
        MonkeyVoxelChunk(chunk),
        Name::new("MONKEY voxel sandbox"),
    ));

    let wow = bevy_to_wow(centre);
    info!(
        "MONKEY (voxel): spawned terraform slab — {vert_count} verts, {tri_count} tris. \
         Bevy centre {:.1?}, WoW centre [{:.1}, {:.1}, {:.1}] (x,y,z). Look ~{SPAWN_AHEAD_YARDS} yd ahead; \
         brush [ dig / ] build / \\ smooth, -/= size.",
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
    let centre = cam.translation() + *cam.forward() * SPAWN_AHEAD_YARDS;
    tf.translation = centre - Vec3::splat(VOLUME_LOCAL_CENTRE);
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
    mut volume: Query<(&mut MonkeyVoxelChunk, &Mesh3d, &Transform), With<MonkeyVoxelVolume>>,
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
    let (Ok(cam), Ok((mut chunk, mesh3d, tf))) = (camera.single(), volume.single_mut()) else {
        return;
    };

    // Aim the brush at the volume's depth along the view ray, so looking at the volume always lands
    // on it and turning your view sweeps the brush across its face.
    let vol_centre = tf.translation + Vec3::splat(VOLUME_LOCAL_CENTRE);
    let dist = (vol_centre - cam.translation()).length();
    let world = cam.translation() + *cam.forward() * dist;
    let local = ((world - tf.translation) / benilla_voxel::VOXEL_SIZE).to_array();

    let r = radius.0;
    let changed = match mode {
        BrushMode::Smooth => chunk.0.smooth_sphere(local, r, 0.5),
        m => chunk.0.edit_sphere(local, r, m),
    };
    if changed {
        // insert returns a prior-asset Result we don't need; the handle stays the same.
        let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&chunk.0.mesh()));
    }
}

/// Capture-only proof that the edit → re-mesh → render chain works headlessly: once the volume
/// exists, carve two bites out of the volume so the baseline screenshot shows a *deformed* volume.
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
    // Terraform the slab for the baseline shot: carve a pit + a tunnel, raise a mound, then smooth.
    chunk.0.edit_sphere([13.0, 17.0, 13.0], 6.0, BrushMode::Subtract);
    chunk.0.edit_sphere([22.0, 17.0, 20.0], 5.0, BrushMode::Subtract);
    chunk.0.edit_sphere([24.0, 22.0, 12.0], 6.0, BrushMode::Add);
    chunk.0.smooth_sphere([18.0, 18.0, 16.0], 8.0, 0.5);
    let _ = meshes.insert(&mesh3d.0, monkey_voxel_mesh_to_bevy(&chunk.0.mesh()));
    *done = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voxel_mesh_converts_with_matching_counts() {
        let chunk = benilla_voxel::volume(10.0, 4.0, [17.0, 17.0, 17.0]);
        let vm = chunk.mesh();
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

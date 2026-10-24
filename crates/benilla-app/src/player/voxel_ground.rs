//! MONKEY (voxel, live bugs 2026-10-24): keep the local body ON the terrain when the voxel ground
//! under it changes.
//!
//! Two live failures: undoing a pit with a character standing in it made the client fall and
//! bounce (the body ends up inside the refilled solid, under its new surface), and a character
//! saved inside a pit that no longer exists (reverted while it was offline) fell through the world
//! at login (it spawns below the restored ground, with nothing under it).
//!
//! After the terrain under the local player may have changed — an `X` region reset (undo, revert,
//! resync) or a live edit near the body, or the end of the post-login / teleport settle hold — the
//! body is checked for a few seconds: if its torso is INSIDE the ground (the voxel / ADT density)
//! and no walkable collider lies under its feet (so it is not standing in a WMO cave below the
//! terrain), it is lifted onto the topmost ground surface above it (a ground query, not physics).
//! The same check runs, unarmed, for a body that has been falling for a while: falling inside the
//! ground with nothing below is always the fall-through-the-world case.

use bevy::prelude::*;

use benilla_assets::coords::{bevy_to_wow, wow_to_bevy};
use benilla_world::collision::WorldCollision;
use benilla_world::voxel::{MonkeyVoxelApi, MonkeyVoxelEvent, REGION_YARDS};

use super::Player;

/// How long a terrain change near the body keeps the check armed, seconds (the region's collider
/// is rebuilt a few frames to a second after its journal changes).
const ARM_CHANGE_S: f32 = 4.0;
/// …and the end of a settle hold (login, teleport: the voxel layer syncs right after it).
const ARM_SETTLE_S: f32 = 8.0;
/// Checks are spaced this far apart, seconds.
const CHECK_GAP_S: f32 = 0.2;
/// A live edit counts as "near" within its radius plus this, yards.
const EDIT_MARGIN: f32 = 4.0;
/// An `X` reset counts as near when the body is within this of the region's square, yards.
const RESET_MARGIN: f32 = 8.0;
/// Height above the feet that must be inside the ground for "buried", yards (mid-torso).
const BURIED_PROBE: f32 = 0.9;
/// How far above the feet the new surface is searched, yards.
const LIFT_REACH: f32 = 96.0;
/// A walkable collider within this below the feet means the body stands on something real.
const FLOOR_PROBE: f32 = 120.0;
/// The unarmed check: airborne at least this long, seconds.
const FALL_S: f32 = 1.0;
/// Lifted bodies are put this far above the surface, yards.
const LIFT_CLEAR: f32 = 0.15;

/// The check's state.
#[derive(Default)]
pub(super) struct GroundGuard {
    armed_until: f32,
    last_check: f32,
    was_settling: bool,
}

/// Arms on terrain changes near the body and lifts a buried body onto the new surface.
pub(super) fn keep_body_on_voxel_ground(
    time: Res<Time>,
    mut player: ResMut<Player>,
    mut events: MessageReader<MonkeyVoxelEvent>,
    api: MonkeyVoxelApi,
    world: WorldCollision,
    mut st: Local<GroundGuard>,
) {
    let now = time.elapsed_secs();
    let feet = bevy_to_wow(player.pos);
    let my_map = api.map();
    for ev in events.read() {
        let near = match *ev {
            MonkeyVoxelEvent::RegionReset { map, rx, ry, .. } => {
                let (x0, y0) = (rx as f32 * REGION_YARDS, ry as f32 * REGION_YARDS);
                Some(map) == my_map
                    && feet[0] >= x0 - RESET_MARGIN
                    && feet[0] <= x0 + REGION_YARDS + RESET_MARGIN
                    && feet[1] >= y0 - RESET_MARGIN
                    && feet[1] <= y0 + REGION_YARDS + RESET_MARGIN
            }
            MonkeyVoxelEvent::Remote { map, pos, radius, .. } => {
                Some(map) == my_map && dist2d(feet, pos) <= radius + EDIT_MARGIN
            }
            MonkeyVoxelEvent::Confirmed { .. } | MonkeyVoxelEvent::Dropped { .. } => false,
        };
        if near {
            st.armed_until = st.armed_until.max(now + ARM_CHANGE_S);
        }
    }
    if st.was_settling && !player.settling {
        st.armed_until = st.armed_until.max(now + ARM_SETTLE_S);
    }
    st.was_settling = player.settling;
    // Only our own body, under our control, in the world.
    if !player.active || player.detached || player.settling || player.foreign_mover.is_some() {
        return;
    }
    let falling = player.airborne_since.is_some_and(|t| now - t >= FALL_S);
    if (now > st.armed_until && !falling) || now - st.last_check < CHECK_GAP_S {
        return;
    }
    st.last_check = now;
    let [x, y, z] = feet;
    if !(api.density([x, y, z + BURIED_PROBE]) < 0.0) {
        return; // not inside the ground
    }
    // Standing (or about to land) on a WMO floor / doodad under the terrain: not ours to fix.
    if world
        .ray_body(player.pos + Vec3::Y * 0.5, Dir3::NEG_Y, FLOOR_PROBE)
        .is_some()
    {
        return;
    }
    let Some(top) = api.surface_z(x, y, z + LIFT_REACH * 0.5, LIFT_REACH * 0.5) else {
        return;
    };
    // No surface within reach (the march started inside the ground): leave it to the next check.
    if top <= z + 0.2 || top >= z + LIFT_REACH - 0.5 {
        return;
    }
    info!(
        "MONKEY (voxel): body inside the ground at ({x:.1}, {y:.1}, {z:.1}) with nothing below — \
         lifted onto the surface at z {top:.2}{}",
        if falling { " (falling)" } else { "" }
    );
    player.pos = wow_to_bevy([x, y, top + LIFT_CLEAR]);
    player.vel_y = 0.0;
    player.horiz_vel = Vec3::ZERO;
}

fn dist2d(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

//! The EME free camera: while [`EmeActive`] is on, EME owns the world camera's pose.
//!
//! - **WASD** move in the view basis, **Space / E** up, **C / Q** down;
//! - **RMB held** = mouse look;
//! - **Shift** ×4 fast, **Ctrl** ×¼ slow, **wheel** scales the base speed (Alt+wheel goes to the
//!   tool: brush radius);
//! - **Tab** = back to the character (EME closes).
//!
//! The pose is seeded from the camera when EME opens and written in `PostUpdate` (before
//! transform propagation), after anything in `Update` that seats the camera. The host holds the
//! character still and streams the world around the camera (benilla-app: `player.detached`).

use bevy::input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseScrollUnit};
use bevy::prelude::*;

use crate::{EmeActive, EmeHostInput, EmeSet, EmeToggleRequested};
use benilla_world::view::WorldCamera;

/// Base speed bounds, yards per second.
const SPEED_MIN: f32 = 2.0;
const SPEED_MAX: f32 = 250.0;
/// Mouse-look sensitivity, radians per pixel.
const LOOK_SENS: f32 = 0.004;

/// The free camera's state (Bevy axes, like the camera transform).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct EmeFreeCam {
    /// Eye position (Bevy axes).
    pub pos: Vec3,
    /// Yaw around +Y, radians.
    pub yaw: f32,
    /// Pitch, radians (clamped to ±89°).
    pub pitch: f32,
    /// Base speed, yards per second.
    pub speed: f32,
    /// The pose was seeded since EME last opened.
    pub seeded: bool,
}

impl Default for EmeFreeCam {
    fn default() -> Self {
        Self {
            pos: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            speed: 20.0,
            seeded: false,
        }
    }
}

impl EmeFreeCam {
    /// The camera rotation.
    pub fn rotation(&self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0)
    }

    /// Seed from a camera transform.
    pub fn seed_from(&mut self, t: &Transform) {
        let (yaw, pitch, _) = t.rotation.to_euler(EulerRot::YXZ);
        self.pos = t.translation;
        self.yaw = yaw;
        self.pitch = pitch.clamp(-1.553, 1.553);
        self.seeded = true;
    }

    /// Place the eye at `eye` looking at `look` (both WoW coordinates).
    pub fn look_at_wow(&mut self, eye: [f32; 3], look: [f32; 3]) {
        let e = benilla_assets::coords::wow_to_bevy(eye);
        let l = benilla_assets::coords::wow_to_bevy(look);
        let t = Transform::from_translation(e).looking_at(l, Vec3::Y);
        self.seed_from(&t);
    }

    /// One frame of movement: `axes` = (right, up, forward) in -1..=1, `fast` / `slow` modifiers.
    pub fn fly(&mut self, axes: Vec3, fast: bool, slow: bool, dt: f32) {
        if axes == Vec3::ZERO {
            return;
        }
        let rot = self.rotation();
        let dir = rot * Vec3::X * axes.x + Vec3::Y * axes.y + rot * Vec3::NEG_Z * axes.z;
        let mul = if fast { 4.0 } else { 1.0 } * if slow { 0.25 } else { 1.0 };
        self.pos += dir.normalize_or_zero() * self.speed * mul * dt;
    }

    /// Mouse look by a pixel delta.
    pub fn look(&mut self, delta: Vec2) {
        self.yaw -= delta.x * LOOK_SENS;
        self.pitch = (self.pitch - delta.y * LOOK_SENS).clamp(-1.553, 1.553);
    }

    /// Wheel steps scale the base speed (×1.25 per step).
    pub fn scale_speed(&mut self, steps: f32) {
        self.speed = (self.speed * 1.25f32.powf(steps)).clamp(SPEED_MIN, SPEED_MAX);
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<EmeFreeCam>()
        .add_systems(Update, free_cam_input.in_set(EmeSet::Input))
        .add_systems(PostUpdate, free_cam_apply.in_set(EmeSet::Camera));
}

/// Keys / mouse → the free camera's pose; Tab closes EME.
#[allow(clippy::too_many_arguments)]
fn free_cam_input(
    time: Res<Time<Real>>,
    active: Res<EmeActive>,
    host_input: Res<EmeHostInput>,
    keys: Option<Res<ButtonInput<KeyCode>>>,
    buttons: Option<Res<ButtonInput<MouseButton>>>,
    motion: Option<Res<AccumulatedMouseMotion>>,
    scroll: Option<Res<AccumulatedMouseScroll>>,
    camera: Query<&Transform, With<WorldCamera>>,
    mut cam: ResMut<EmeFreeCam>,
    mut toggle: MessageWriter<EmeToggleRequested>,
    mut was_active: Local<bool>,
) {
    if !active.0 {
        // Closing forgets the pose (the next open starts at the character's camera); a pose set
        // while closed (a script's `camera` step) is kept for the next open.
        if *was_active {
            cam.seeded = false;
        }
        *was_active = false;
        return;
    }
    *was_active = true;
    if !cam.seeded {
        if let Ok(t) = camera.single() {
            cam.seed_from(t);
        }
    }
    let typing = host_input.typing;
    let Some(keys) = keys else { return };
    let down = |k: KeyCode| !typing && keys.pressed(k);
    if !typing && keys.just_pressed(KeyCode::Tab) {
        toggle.write(EmeToggleRequested { to: Some(false) });
        return;
    }
    let axis = |pos: &[KeyCode], neg: &[KeyCode]| {
        f32::from(pos.iter().any(|k| down(*k))) - f32::from(neg.iter().any(|k| down(*k)))
    };
    let axes = Vec3::new(
        axis(&[KeyCode::KeyD], &[KeyCode::KeyA]),
        axis(&[KeyCode::Space, KeyCode::KeyE], &[KeyCode::KeyC, KeyCode::KeyQ]),
        axis(&[KeyCode::KeyW], &[KeyCode::KeyS]),
    );
    let fast = down(KeyCode::ShiftLeft) || down(KeyCode::ShiftRight);
    let slow = down(KeyCode::ControlLeft) || down(KeyCode::ControlRight);
    // Real time: the camera keeps flying while the game clock is paused / frozen.
    cam.fly(axes, fast, slow, time.delta_secs().min(0.1));
    if buttons.is_some_and(|b| b.pressed(MouseButton::Right)) {
        if let Some(m) = motion {
            cam.look(m.delta);
        }
    }
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if let (Some(s), false) = (scroll, alt || host_input.pointer_over_ui) {
        let steps = match s.unit {
            MouseScrollUnit::Line => s.delta.y,
            MouseScrollUnit::Pixel => s.delta.y / 100.0,
        };
        if steps != 0.0 {
            cam.scale_speed(steps);
        }
    }
}

/// Write the pose onto the world camera (after every `Update` seat, before propagation).
fn free_cam_apply(
    active: Res<EmeActive>,
    cam: Res<EmeFreeCam>,
    mut camera: Query<&mut Transform, With<WorldCamera>>,
) {
    if !active.0 || !cam.seeded {
        return;
    }
    for mut t in &mut camera {
        t.translation = cam.pos;
        t.rotation = cam.rotation();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_cam_flies_in_its_view_basis_with_modifiers() {
        let mut c = EmeFreeCam::default();
        c.look_at_wow([0.0, 0.0, 10.0], [10.0, 0.0, 10.0]); // looking north (+x WoW)
        let start = c.pos;
        c.fly(Vec3::new(0.0, 0.0, 1.0), false, false, 1.0);
        let moved = benilla_assets::coords::bevy_to_wow(c.pos);
        let s = benilla_assets::coords::bevy_to_wow(start);
        assert!((moved[0] - s[0] - 20.0).abs() < 1e-3, "{moved:?}");
        c.fly(Vec3::new(0.0, 1.0, 0.0), true, false, 1.0);
        assert!((c.pos.y - start.y - 80.0).abs() < 1e-3);
        c.fly(Vec3::new(0.0, 1.0, 0.0), false, true, 1.0);
        assert!((c.pos.y - start.y - 85.0).abs() < 1e-3);
        c.scale_speed(100.0);
        assert_eq!(c.speed, SPEED_MAX);
        c.look(Vec2::new(0.0, 10_000.0));
        assert!(c.pitch >= -1.554);
    }
}

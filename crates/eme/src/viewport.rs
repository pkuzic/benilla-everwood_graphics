//! Viewport picking and the brush preview.
//!
//! - [`EmeCursor`]: where the pointer is (the window cursor, or a script's virtual cursor).
//! - [`EmeAim`]: the cursor ray through the world camera, marched through the voxel seam
//!   ([`MonkeyVoxelApi::aim`]): the hit on a voxel surface or the raw ADT ground, with its normal.
//! - [`EmeBrushPreviewState`]: the active tool's [`EmePreview`] resolved against the world — the
//!   brush rim projected onto the surface and the chunks it would touch — drawn as gizmo rings
//!   coloured by mode (dig orange, build green, smooth blue, flatten yellow).

use bevy::gizmos::config::{GizmoConfigGroup, GizmoConfigStore};
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

use crate::{EmeActive, EmeHost, EmeHostInput, EmePointerOverUi, EmeSet};
use benilla_assets::coords::{bevy_to_wow, wow_to_bevy};
use benilla_world::view::WorldCamera;
use benilla_world::voxel::{MonkeyVoxelApi, MonkeyVoxelHit};
use eme_core::{EmeOptionValue, EmePreview};

/// How far the cursor ray reaches, yards.
pub const AIM_REACH: f32 = 400.0;
/// Rim points of the preview ring.
const RING_SEGMENTS: usize = 64;

/// EME's gizmo group: thick lines biased toward the camera, so the ring reads on any ground.
#[derive(Default, Reflect, GizmoConfigGroup)]
pub struct EmeGizmos;

/// The pointer.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct EmeCursor {
    /// Logical window position this frame (`None` = outside the window).
    pub screen: Option<Vec2>,
    /// A script's virtual cursor, as a fraction of the viewport (0..1, origin top-left); wins
    /// over the window cursor while set.
    pub virtual_norm: Option<Vec2>,
    /// A script holds the primary button.
    pub virtual_primary: bool,
}

/// The cursor ray's world hit this frame.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct EmeAim {
    /// Ray origin and direction, WoW axes.
    pub ray: Option<([f32; 3], [f32; 3])>,
    /// The first surface hit.
    pub hit: Option<MonkeyVoxelHit>,
}

/// The resolved brush preview (what the gizmos draw; readable by tests and scripts).
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct EmeBrushPreviewState {
    /// The preview is shown this frame.
    pub visible: bool,
    /// Rim points on the surface, WoW yards.
    pub ring: Vec<[f32; 3]>,
    /// Centre, WoW yards.
    pub center: [f32; 3],
    /// Radius, yards.
    pub radius: f32,
    /// Surface normal at the centre.
    pub normal: [f32; 3],
    /// A flatten plane (draw a level disc at `center.z`).
    pub plane: bool,
    /// Chunks the edit would touch / of which active.
    pub chunks: usize,
    /// Of `chunks`, active now.
    pub active_chunks: usize,
    /// Ring colour (by mode).
    pub color: [f32; 4],
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<EmeCursor>()
        .init_resource::<EmeAim>()
        .init_resource::<EmeBrushPreviewState>()
        .init_gizmo_group::<EmeGizmos>()
        .add_systems(Startup, configure_gizmos)
        .add_systems(Update, update_cursor_and_aim.in_set(EmeSet::Input))
        .add_systems(Update, draw_preview.after(EmeSet::Frame));
}

fn configure_gizmos(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<EmeGizmos>();
    config.line.width = 3.0;
    config.depth_bias = -0.02;
}

/// The colour of a voxel mode's preview.
pub fn mode_color(mode: u8) -> [f32; 4] {
    match mode {
        0 => [1.0, 0.45, 0.1, 1.0],
        1 => [0.25, 0.95, 0.3, 1.0],
        2 => [0.3, 0.6, 1.0, 1.0],
        3 => [1.0, 0.9, 0.2, 1.0],
        _ => [0.9, 0.9, 0.9, 1.0],
    }
}

/// Cursor → ray → hit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_cursor_and_aim(
    active: Res<EmeActive>,
    window: Query<&Window, With<PrimaryWindow>>,
    camera: Query<(&Camera, &GlobalTransform), With<WorldCamera>>,
    mut cursor: ResMut<EmeCursor>,
    mut aim: ResMut<EmeAim>,
    api: MonkeyVoxelApi,
) {
    let win = window.single().ok();
    cursor.screen = win.and_then(|w| w.cursor_position());
    if !active.0 {
        *aim = EmeAim::default();
        return;
    }
    let Ok((cam, gt)) = camera.single() else {
        *aim = EmeAim::default();
        return;
    };
    let point = match cursor.virtual_norm {
        Some(n) => cam
            .logical_viewport_size()
            .or_else(|| win.map(|w| Vec2::new(w.width(), w.height())))
            .map(|size| n * size),
        None => cursor.screen,
    };
    let Some(ray) = point.and_then(|p| cam.viewport_to_world(gt, p).ok()) else {
        *aim = EmeAim::default();
        return;
    };
    let origin = bevy_to_wow(ray.origin);
    let dir = bevy_to_wow(*ray.direction);
    aim.ray = Some((origin, dir));
    aim.hit = api.aim(origin, dir, AIM_REACH);
}

/// Resolve the active tool's preview against the world and draw it.
pub(crate) fn resolve_preview(
    preview: Option<EmePreview>,
    mode: u8,
    hit: Option<&MonkeyVoxelHit>,
    api: &MonkeyVoxelApi,
    out: &mut EmeBrushPreviewState,
) {
    *out = EmeBrushPreviewState::default();
    let (center, radius, plane) = match preview {
        Some(EmePreview::Ring { center, radius, .. }) => (center, radius, false),
        Some(EmePreview::Plane { center, radius }) => (center, radius, true),
        _ => return,
    };
    let p = api.brush_preview(center, radius, RING_SEGMENTS);
    out.visible = true;
    out.ring = if plane {
        // A level disc at the plane height.
        (0..RING_SEGMENTS)
            .map(|i| {
                let a = i as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
                [
                    center[0] + radius * a.cos(),
                    center[1] + radius * a.sin(),
                    center[2],
                ]
            })
            .collect()
    } else {
        p.ring
    };
    out.center = center;
    out.radius = radius;
    out.normal = hit.map_or([0.0, 0.0, 1.0], |h| h.normal);
    out.plane = plane;
    out.chunks = p.chunks.len();
    out.active_chunks = p.active;
    out.color = mode_color(mode);
}

/// The active tool's `mode` option (the ring colour), if it has one.
pub(crate) fn active_mode(host: &EmeHost) -> u8 {
    let Some(id) = host.registry.active_id() else {
        return 255;
    };
    host.registry
        .get(id.as_str())
        .and_then(|t| {
            t.options_ui_model().into_iter().find_map(|f| match (f.key, f.value) {
                ("mode", EmeOptionValue::Choice(i)) => Some(i as u8),
                _ => None,
            })
        })
        .unwrap_or(255)
}

fn draw_preview(
    active: Res<EmeActive>,
    over_ui: Res<EmePointerOverUi>,
    host_input: Res<EmeHostInput>,
    cursor: Res<EmeCursor>,
    state: Res<EmeBrushPreviewState>,
    mut gizmos: Gizmos<EmeGizmos>,
) {
    // A script's virtual cursor is never over a panel (the shell follows the OS pointer).
    let over = (over_ui.0 || host_input.pointer_over_ui) && cursor.virtual_norm.is_none();
    if !active.0 || !state.visible || over {
        return;
    }
    let c = state.color;
    let color = Color::srgba(c[0], c[1], c[2], c[3]);
    let dim = Color::srgba(c[0], c[1], c[2], 0.55);
    let lift = |p: [f32; 3], up: f32| wow_to_bevy([p[0], p[1], p[2] + up]);
    // The rim on the surface, a second ring a hair above it (reads on bumpy ground), and an
    // inner falloff ring + cross at the centre (the "disc").
    gizmos.linestrip(
        state
            .ring
            .iter()
            .chain(state.ring.first())
            .map(|p| lift(*p, 0.08)),
        color,
    );
    gizmos.linestrip(
        state
            .ring
            .iter()
            .chain(state.ring.first())
            .map(|p| lift(*p, 0.35)),
        dim,
    );
    let inner: Vec<Vec3> = state
        .ring
        .iter()
        .chain(state.ring.first())
        .map(|p| {
            let q = [
                state.center[0] + (p[0] - state.center[0]) * 0.75,
                state.center[1] + (p[1] - state.center[1]) * 0.75,
                state.center[2] + (p[2] - state.center[2]) * 0.75,
            ];
            lift(q, 0.08)
        })
        .collect();
    gizmos.linestrip(inner, dim);
    let n = state.normal;
    let c0 = lift(state.center, 0.05);
    let tip = wow_to_bevy([
        state.center[0] + n[0] * state.radius * 0.5,
        state.center[1] + n[1] * state.radius * 0.5,
        state.center[2] + n[2] * state.radius * 0.5,
    ]);
    gizmos.line(c0, tip, color);
    let r = state.radius * 0.2;
    gizmos.line(
        lift([state.center[0] - r, state.center[1], state.center[2]], 0.08),
        lift([state.center[0] + r, state.center[1], state.center[2]], 0.08),
        color,
    );
    gizmos.line(
        lift([state.center[0], state.center[1] - r, state.center[2]], 0.08),
        lift([state.center[0], state.center[1] + r, state.center[2]], 0.08),
        color,
    );
}

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
    /// MONKEY (EME v3.6): extra line segments (WoW yards): a bridge / tunnel path, a push / pull
    /// offset, water-fill cells.
    pub lines: Vec<[[f32; 3]; 2]>,
    /// MONKEY (EME v3.6): the ring is the active tool's cube shape (a square rim).
    pub square: bool,
}

/// The ground outline of the selected restore point's radius (cached; refreshed twice a second).
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct EmeRestoreOutline {
    /// The snapshot the ring belongs to.
    pub id: Option<u32>,
    /// Centre, WoW yards.
    pub center: [f32; 3],
    /// Radius, yards.
    pub radius: f32,
    /// Rim points following the ground, WoW yards.
    pub ring: Vec<[f32; 3]>,
    /// Seconds since the ring was computed.
    age: f32,
}

/// A "show in world" marker (a beam and a pulsing ring), drawn until `until_ms`.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct EmeMarker {
    /// Where, WoW yards.
    pub pos: [f32; 3],
    /// Game-clock ms it fades at (0 = none).
    pub until_ms: u64,
}

/// How long a marker stays, ms.
pub const MARKER_MS: u64 = 8_000;
/// Rim points of a restore-point outline.
const OUTLINE_SEGMENTS: usize = 128;

pub(crate) fn register(app: &mut App) {
    app.init_resource::<EmeCursor>()
        .init_resource::<EmeAim>()
        .init_resource::<EmeBrushPreviewState>()
        .init_resource::<EmeRestoreOutline>()
        .init_resource::<EmeMarker>()
        .init_gizmo_group::<EmeGizmos>()
        .add_systems(Startup, configure_gizmos)
        .add_systems(Update, update_cursor_and_aim.in_set(EmeSet::Input))
        .add_systems(Update, draw_preview.after(EmeSet::Frame))
        .add_systems(
            Update,
            (update_restore_outline, draw_outline_and_marker)
                .chain()
                .after(EmeSet::Frame),
        );
}

/// Refresh the selected restore point's rim (ground-following) a couple of times a second.
fn update_restore_outline(
    time: Res<Time<Real>>,
    active: Res<EmeActive>,
    restore: Res<eme_ui::EmeRestorePoints>,
    cam: Res<crate::camera::EmeFreeCam>,
    api: MonkeyVoxelApi,
    mut out: ResMut<EmeRestoreOutline>,
) {
    let sel = restore
        .selected
        .filter(|_| active.0)
        .and_then(|id| restore.rows.iter().find(|r| r.snapshot == id));
    let Some(row) = sel else {
        if out.id.is_some() {
            *out = EmeRestoreOutline::default();
        }
        return;
    };
    out.age += time.delta_secs();
    if out.id == Some(row.snapshot) && out.radius == row.radius && out.age < 0.5 {
        return;
    }
    let eye = bevy_to_wow(cam.pos);
    let near_z = api
        .surface_z(row.x, row.y, eye[2], 400.0)
        .unwrap_or(eye[2] - 10.0);
    let n = OUTLINE_SEGMENTS;
    let ring = (0..n)
        .map(|i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            let (x, y) = (row.x + row.radius * a.cos(), row.y + row.radius * a.sin());
            let z = api.surface_z(x, y, near_z, 80.0).unwrap_or(near_z);
            [x, y, z]
        })
        .collect();
    *out = EmeRestoreOutline {
        id: Some(row.snapshot),
        center: [row.x, row.y, near_z],
        radius: row.radius,
        ring,
        age: 0.0,
    };
}

/// The selected restore point's outline (cyan, with posts every 16th point so a far rim reads)
/// and the show-in-world marker.
fn draw_outline_and_marker(
    time: Res<Time>,
    active: Res<EmeActive>,
    outline: Res<EmeRestoreOutline>,
    marker: Res<EmeMarker>,
    mut gizmos: Gizmos<EmeGizmos>,
) {
    if !active.0 {
        return;
    }
    let cyan = Color::srgba(0.2, 0.9, 1.0, 1.0);
    if outline.id.is_some() && outline.ring.len() > 2 {
        let lift = |p: [f32; 3], up: f32| wow_to_bevy([p[0], p[1], p[2] + up]);
        gizmos.linestrip(
            outline.ring.iter().chain(outline.ring.first()).map(|p| lift(*p, 0.3)),
            cyan,
        );
        for p in outline.ring.iter().step_by(16) {
            gizmos.line(lift(*p, 0.0), lift(*p, 6.0), cyan);
        }
    }
    let now = crate::now_ms(&time);
    if marker.until_ms > now {
        let left = (marker.until_ms - now) as f32 / MARKER_MS as f32;
        let pulse = ((now as f32 / 1000.0) * 3.0).sin() * 0.5 + 0.5;
        let c = Color::srgba(1.0, 0.2, 0.9, left.clamp(0.2, 1.0));
        let p = marker.pos;
        gizmos.line(wow_to_bevy(p), wow_to_bevy([p[0], p[1], p[2] + 40.0]), c);
        let r = 1.5 + 2.5 * pulse;
        let ring: Vec<Vec3> = (0..=32)
            .map(|i| {
                let a = i as f32 / 32.0 * std::f32::consts::TAU;
                wow_to_bevy([p[0] + r * a.cos(), p[1] + r * a.sin(), p[2] + 0.4])
            })
            .collect();
        gizmos.linestrip(ring, c);
    }
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

/// A horizontal circle (or the cube's square) at `c`, as closed line segments.
fn rim_lines(c: [f32; 3], radius: f32, square: bool, out: &mut Vec<[[f32; 3]; 2]>) {
    let pts: Vec<[f32; 3]> = if square {
        [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
            .iter()
            .map(|(a, b)| [c[0] + a * radius, c[1] + b * radius, c[2]])
            .collect()
    } else {
        (0..32)
            .map(|i| {
                let a = i as f32 / 32.0 * std::f32::consts::TAU;
                [c[0] + radius * a.cos(), c[1] + radius * a.sin(), c[2]]
            })
            .collect()
    };
    for i in 0..pts.len() {
        out.push([pts[i], pts[(i + 1) % pts.len()]]);
    }
}

/// Resolve the active tool's preview against the world and draw it.
pub(crate) fn resolve_preview(
    preview: Option<EmePreview>,
    mode: u8,
    shape: u8,
    hit: Option<&MonkeyVoxelHit>,
    api: &MonkeyVoxelApi,
    out: &mut EmeBrushPreviewState,
) {
    *out = EmeBrushPreviewState::default();
    let square = shape == eme_core::transport::mode::SHAPE_CUBE;
    let (center, radius, plane) = match preview {
        Some(EmePreview::Ring { center, radius, .. }) => (center, radius, false),
        Some(EmePreview::Plane { center, radius }) => (center, radius, true),
        // MONKEY (EME v3.6): a path (bridge / tunnel / push-pull offset): the centre line, the
        // cross-section at both ends, a ring on the surface at the first point.
        Some(EmePreview::Path { points, radius, mode }) => {
            let Some(&first) = points.first() else { return };
            let last = *points.last().expect("non-empty");
            for w in points.windows(2) {
                out.lines.push([w[0], w[1]]);
            }
            rim_lines(first, radius, square, &mut out.lines);
            if points.len() > 1 {
                rim_lines(last, radius, square, &mut out.lines);
                // Posts at the ends (how high the deck / tunnel sits).
                for p in [first, last] {
                    out.lines.push([[p[0], p[1], p[2] - radius], [p[0], p[1], p[2] + radius]]);
                }
            }
            out.visible = true;
            out.center = first;
            out.radius = radius;
            out.normal = hit.map_or([0.0, 0.0, 1.0], |h| h.normal);
            out.color = mode_color(mode);
            out.square = square;
            return;
        }
        // MONKEY (EME v3.6): water-fill cells as squares at the level.
        Some(EmePreview::Cells { cells, level, .. }) => {
            let cell = crate::tools::water::LIQ_CELL;
            let z = level.unwrap_or(hit.map_or(0.0, |h| h.pos[2])) + 0.05;
            for (cx, cy) in cells.iter().take(4096) {
                let (x0, y0) = (*cx as f32 * cell, *cy as f32 * cell);
                let (x1, y1) = (x0 + cell, y0 + cell);
                out.lines.push([[x0, y0, z], [x1, y0, z]]);
                out.lines.push([[x1, y0, z], [x1, y1, z]]);
                out.lines.push([[x1, y1, z], [x0, y1, z]]);
                out.lines.push([[x0, y1, z], [x0, y0, z]]);
            }
            out.visible = !out.lines.is_empty();
            if let Some(&(cx, cy)) = cells.first() {
                out.center = [(cx as f32 + 0.5) * cell, (cy as f32 + 0.5) * cell, z];
            }
            out.color = [0.2, 0.55, 1.0, 1.0];
            return;
        }
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
    } else if square {
        // MONKEY (EME v3.6): the cube's square rim, projected onto the surface.
        let reach = radius.max(2.0) + 2.0;
        let per_side = RING_SEGMENTS / 4;
        let corners = [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
        (0..4)
            .flat_map(|s| {
                let (a, b) = (corners[s], corners[(s + 1) % 4]);
                (0..per_side).map(move |i| {
                    let t = i as f32 / per_side as f32;
                    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
                })
            })
            .map(|(u, v)| {
                let (x, y) = (center[0] + u * radius, center[1] + v * radius);
                let z = api.surface_z(x, y, center[2], reach).unwrap_or(center[2]);
                [x, y, z]
            })
            .collect()
    } else {
        p.ring
    };
    out.square = square;
    out.center = center;
    out.radius = radius;
    out.normal = hit.map_or([0.0, 0.0, 1.0], |h| h.normal);
    out.plane = plane;
    out.chunks = p.chunks.len();
    out.active_chunks = p.active;
    out.color = mode_color(mode);
}

/// MONKEY (EME v3.6): the active tool's `shape` option (`mode::SHAPE_*`; 0 = sphere / none).
pub(crate) fn active_shape(host: &EmeHost) -> u8 {
    host.registry
        .active_id()
        .and_then(|id| host.registry.get(id.as_str()))
        .and_then(|t| {
            t.options_ui_model().into_iter().find_map(|f| match (f.key, f.value, f.disabled) {
                ("shape", EmeOptionValue::Choice(i), false) => Some(i as u8),
                _ => None,
            })
        })
        .unwrap_or(0)
}

/// The active tool's `mode` option (the ring colour), if it has one.
pub(crate) fn active_mode(host: &EmeHost) -> u8 {
    let Some(id) = host.registry.active_id() else {
        return 255;
    };
    if id == crate::tools::flatten::FLATTEN {
        return eme_core::transport::mode::FLATTEN;
    }
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
    // MONKEY (EME v3.6): a path / cells preview is just its lines.
    for l in &state.lines {
        gizmos.line(lift(l[0], 0.1), lift(l[1], 0.1), color);
    }
    if state.ring.is_empty() {
        return;
    }
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

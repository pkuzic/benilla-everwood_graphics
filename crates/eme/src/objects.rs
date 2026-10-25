//! The Bevy side of the Objects tool (CONTRACT v3.7).
//!
//! The tool's brain is [`eme_core::EmeObjectsEditor`], shared with the registry's
//! [`crate::tools::objects::EmeObjectsTool`] through [`EmeObjectsHandle`]. This module wires it to
//! the rest of the plugin:
//!
//! - [`EmeWorldObjects`]: what the host client has streamed in (guid, entry, origin, oriented box).
//!   The host writes it; the editor picks against it. EME never reads the client's entities.
//! - [`route`]: the dispatcher's object replies and pushes, dispatch refusals, undo / redo events
//!   and link loss → the editor.
//! - [`ui_commands`] / [`hotkeys`]: the Objects tab's clicks and the keys (T move, R rotate,
//!   G scale, X axes, F frame, Del delete, Ctrl+D duplicate) → editor commands.
//! - [`view_out`]: the editor's snapshot → the shell's [`eme_ui::EmeObjects`], the camera frame
//!   request → the free camera, selection changes → [`eme_core::EmeEvent::SelectionChanged`].
//! - [`draw_objects`]: selection boxes, the gizmo, ghost previews and the placement marker.

use bevy::prelude::*;

use crate::camera::EmeFreeCam;
use crate::tools::objects::{lock, EmeObjectsShared, OBJECTS};
use crate::viewport::EmeGizmos;
use crate::{EmeActive, EmeEventMsg, EmeHost, EmeHostInput, EmeSet, EmeUiWantsKeyboard, EmeWire};
use benilla_assets::coords::wow_to_bevy;
use eme_core::gizmo::{EmeGizmoMode, EmeGizmoSpace};
use eme_core::objects::EmeGizmoDraw;
use eme_core::{
    EmeEvent, EmeGizmoHandle, EmeObb, EmeObjectsCmd, EmeServicePush, EmeUndoBackend, EmeWorldObject,
};
use eme_ui::EmeUiCommand;

/// The shared editor, as a resource.
#[derive(Resource, Clone)]
pub struct EmeObjectsHandle(pub EmeObjectsShared);

/// The objects the host has streamed in, pickable by the Objects tool. The host rewrites `objects`
/// when it changes and bumps `stamp`; EME copies it into the editor when `stamp` moves.
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct EmeWorldObjects {
    /// Pickable objects (game objects only).
    pub objects: Vec<EmeWorldObject>,
    /// Changes whenever `objects` does.
    pub stamp: u64,
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<EmeWorldObjects>();
    // `route` runs right after the dispatcher's pump (its replies are overwritten every frame),
    // `hotkeys` / `ui_commands` before the active tool is fed.
    app.add_systems(
        Update,
        (
            world_in.in_set(EmeSet::Input),
            ui_commands.in_set(EmeSet::Input),
        ),
    )
    .add_systems(
        Update,
        (view_out, draw_objects).chain().after(EmeSet::Frame),
    );
}

/// The host's streamed objects → the editor.
pub(crate) fn world_in(
    world: Res<EmeWorldObjects>,
    handle: Option<Res<EmeObjectsHandle>>,
    mut last: Local<u64>,
) {
    let Some(handle) = handle else { return };
    if world.stamp == *last {
        return;
    }
    *last = world.stamp;
    lock(&handle.0).set_world(world.objects.clone());
}

/// The Objects tab's clicks → editor commands.
pub(crate) fn ui_commands(
    mut cmds: MessageReader<EmeUiCommand>,
    handle: Option<Res<EmeObjectsHandle>>,
) {
    let Some(handle) = handle else {
        cmds.clear();
        return;
    };
    for c in cmds.read() {
        if let EmeUiCommand::Objects(cmd) = c {
            lock(&handle.0).command(cmd.clone());
        }
    }
}

/// Replies, pushes, refusals, undo events and link loss → the editor (right after the pump).
pub(crate) fn route(
    host: Res<EmeHost>,
    wire: Res<EmeWire>,
    handle: Option<Res<EmeObjectsHandle>>,
    mut events: MessageReader<EmeEventMsg>,
    mut seen_errors: Local<Vec<eme_core::EmeDispatchError>>,
    mut was_usable: Local<bool>,
) {
    let Some(handle) = handle else {
        events.clear();
        return;
    };
    let mut ed = lock(&handle.0);
    for r in &host.replies {
        if r.tool == Some(OBJECTS) && r.op.starts_with("obj_") {
            ed.on_reply(r.op, &r.reply);
        }
    }
    for p in &host.pushes {
        if matches!(p, EmeServicePush::Obj { .. }) {
            ed.on_push(p);
        }
    }
    if host.errors != *seen_errors {
        *seen_errors = host.errors.clone();
        if seen_errors.iter().any(|e| e.origin == Some(OBJECTS)) {
            ed.on_dispatch_refused();
        }
    }
    for e in events.read() {
        // An undo / redo of an object entry changed the world behind the editor's back.
        let meta = match &e.0 {
            EmeEvent::EntryUndone(m) | EmeEvent::EntryRedone(m) => Some(m),
            EmeEvent::RequestFailed { meta, .. } | EmeEvent::EntryDiscarded { meta, .. } => Some(meta),
            _ => None,
        };
        if meta.is_some_and(|m| matches!(m.backend, EmeUndoBackend::Object { .. })) {
            ed.mark_dirty();
        }
    }
    let usable = wire.link.is_usable();
    if *was_usable && !usable {
        ed.on_link_lost();
    }
    *was_usable = usable;
}

/// T / R / G pick the gizmo, X flips the axes, F frames, Del deletes, Ctrl+D duplicates.
pub(crate) fn hotkeys(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    active: Res<EmeActive>,
    host: Res<EmeHost>,
    host_input: Res<EmeHostInput>,
    ui_keyboard: Res<EmeUiWantsKeyboard>,
    handle: Option<Res<EmeObjectsHandle>>,
) {
    let (Some(keys), Some(handle)) = (keys, handle) else {
        return;
    };
    if !active.0 || host_input.typing || ui_keyboard.0 || host.registry.active_id() != Some(OBJECTS) {
        return;
    }
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let mut ed = lock(&handle.0);
    if ctrl {
        if keys.just_pressed(KeyCode::KeyD) {
            ed.command(EmeObjectsCmd::Duplicate);
        }
        return;
    }
    if keys.just_pressed(KeyCode::KeyT) {
        ed.command(EmeObjectsCmd::SetMode(EmeGizmoMode::Translate));
    }
    if keys.just_pressed(KeyCode::KeyR) {
        ed.command(EmeObjectsCmd::SetMode(EmeGizmoMode::Rotate));
    }
    if keys.just_pressed(KeyCode::KeyG) {
        ed.command(EmeObjectsCmd::SetMode(EmeGizmoMode::Scale));
    }
    if keys.just_pressed(KeyCode::KeyX) {
        let next = match ed.space() {
            EmeGizmoSpace::World => EmeGizmoSpace::Local,
            EmeGizmoSpace::Local => EmeGizmoSpace::World,
        };
        ed.command(EmeObjectsCmd::SetSpace(next));
    }
    if keys.just_pressed(KeyCode::KeyF) {
        ed.command(EmeObjectsCmd::Frame);
    }
    if keys.just_pressed(KeyCode::Delete) {
        ed.command(EmeObjectsCmd::Delete);
    }
}

/// How far back the camera stands from a framed object, in multiples of its radius.
const FRAME_STANDOFF: f32 = 2.8;

/// The editor's snapshot → the shell; its camera request → the free camera.
pub(crate) fn view_out(
    handle: Option<Res<EmeObjectsHandle>>,
    mut view: ResMut<eme_ui::EmeObjects>,
    mut cam: ResMut<EmeFreeCam>,
    mut host: ResMut<EmeHost>,
    mut last_rev: Local<u64>,
    mut last_sel: Local<Vec<u32>>,
) {
    let Some(handle) = handle else { return };
    let (snapshot, frame, sel) = {
        let mut ed = lock(&handle.0);
        let snapshot = (ed.revision() != *last_rev).then(|| {
            *last_rev = ed.revision();
            ed.view()
        });
        (snapshot, ed.take_frame_request(), ed.selection().to_vec())
    };
    if let Some(v) = snapshot {
        if view.0 != v {
            view.0 = v;
        }
    }
    if let Some(f) = frame {
        // Keep the camera's heading, back off along it until the object fills the view.
        let fwd = cam.rotation() * Vec3::NEG_Z;
        let dist = (f.radius * FRAME_STANDOFF).max(5.0);
        cam.pos = wow_to_bevy(f.center) - fwd * dist;
        cam.seeded = true;
    }
    if *last_sel != sel {
        host.dispatcher
            .push_event(EmeEvent::SelectionChanged(sel.iter().map(|g| u64::from(*g)).collect()));
        *last_sel = sel;
    }
}

// ---- drawing ---------------------------------------------------------------------------------

const SELECT_COLOR: Color = Color::srgba(0.2, 0.9, 1.0, 1.0);
const HOVER_COLOR: Color = Color::srgba(1.0, 0.9, 0.3, 0.9);
const GHOST_COLOR: Color = Color::srgba(1.0, 0.35, 0.95, 1.0);
const PLACE_COLOR: Color = Color::srgba(0.35, 1.0, 0.45, 1.0);
const ACTIVE_COLOR: Color = Color::srgba(1.0, 0.95, 0.2, 1.0);
const HOT_COLOR: Color = Color::srgba(1.0, 1.0, 1.0, 1.0);

/// Axis colours: the first axis red, the second green, the vertical blue.
fn axis_color(i: usize) -> Color {
    match i {
        0 => Color::srgba(1.0, 0.3, 0.3, 1.0),
        1 => Color::srgba(0.35, 0.95, 0.35, 1.0),
        _ => Color::srgba(0.4, 0.6, 1.0, 1.0),
    }
}

fn draw_obb(g: &mut Gizmos<EmeGizmos>, b: &EmeObb, color: Color) {
    let c = b.corners();
    for (i, j) in EmeObb::EDGES {
        g.line(wow_to_bevy(c[i]), wow_to_bevy(c[j]), color);
    }
}

fn draw_gizmo(g: &mut Gizmos<EmeGizmos>, d: &EmeGizmoDraw) {
    let f = &d.frame;
    let colour = |h: EmeGizmoHandle, base: Color| {
        if d.active == Some(h) {
            ACTIVE_COLOR
        } else if d.active.is_none() && d.hover == Some(h) {
            HOT_COLOR
        } else {
            base
        }
    };
    match d.mode {
        EmeGizmoMode::Translate => {
            for (i, h) in [EmeGizmoHandle::AxisX, EmeGizmoHandle::AxisY, EmeGizmoHandle::AxisZ]
                .into_iter()
                .enumerate()
            {
                let c = colour(h, axis_color(i));
                let tip = f.tip(i);
                g.line(wow_to_bevy(f.origin), wow_to_bevy(tip), c);
                // Arrow head: four barbs back from the tip.
                let a = f.axes[i];
                let other = f.axes[(i + 1) % 3];
                let third = f.axes[(i + 2) % 3];
                let head = f.size * 0.14;
                for s in [other, third, neg(other), neg(third)] {
                    let b = [
                        tip[0] - a[0] * head + s[0] * head * 0.4,
                        tip[1] - a[1] * head + s[1] * head * 0.4,
                        tip[2] - a[2] * head + s[2] * head * 0.4,
                    ];
                    g.line(wow_to_bevy(tip), wow_to_bevy(b), c);
                }
            }
            for (h, i, j, k) in [
                (EmeGizmoHandle::PlaneXY, 0, 1, 2),
                (EmeGizmoHandle::PlaneXZ, 0, 2, 1),
                (EmeGizmoHandle::PlaneYZ, 1, 2, 0),
            ] {
                let c = colour(h, axis_color(k));
                let sq = f.plane_square(i, j);
                for n in 0..4 {
                    g.line(wow_to_bevy(sq[n]), wow_to_bevy(sq[(n + 1) % 4]), c);
                }
                g.line(wow_to_bevy(sq[0]), wow_to_bevy(sq[2]), c.with_alpha(0.4));
            }
        }
        EmeGizmoMode::Rotate => {
            let c = colour(EmeGizmoHandle::Ring, axis_color(2));
            let pts = f.ring_points(64);
            g.linestrip(
                pts.iter().chain(pts.first()).map(|p| wow_to_bevy(*p)),
                c,
            );
            // A notch where the ring starts (the zero of the swept angle).
            let a = f.axes[0];
            let inner = [
                f.origin[0] + a[0] * f.size * 0.85,
                f.origin[1] + a[1] * f.size * 0.85,
                f.origin[2] + a[2] * f.size * 0.85,
            ];
            g.line(wow_to_bevy(inner), wow_to_bevy(f.tip(0)), c);
        }
        EmeGizmoMode::Scale => {
            let c = colour(EmeGizmoHandle::Uniform, Color::srgba(0.95, 0.8, 0.3, 1.0));
            for i in 0..3 {
                g.line(wow_to_bevy(f.origin), wow_to_bevy(f.tip(i)), axis_color(i));
                let b = EmeObb::axis_aligned(f.tip(i), [f.size * 0.06; 3]);
                draw_obb(g, &b, c);
            }
            draw_obb(g, &EmeObb::axis_aligned(f.origin, [f.size * 0.08; 3]), c);
        }
    }
}

fn neg(v: [f32; 3]) -> [f32; 3] {
    [-v[0], -v[1], -v[2]]
}

/// Selection boxes, the gizmo, ghost previews and the placement marker.
pub(crate) fn draw_objects(
    active: Res<EmeActive>,
    handle: Option<Res<EmeObjectsHandle>>,
    host: Res<EmeHost>,
    mut gizmos: Gizmos<EmeGizmos>,
) {
    let Some(handle) = handle else { return };
    if !active.0 || host.registry.active_id() != Some(OBJECTS) {
        return;
    }
    let d = lock(&handle.0).draw();
    for b in &d.selected {
        draw_obb(&mut gizmos, b, SELECT_COLOR);
    }
    if let Some(b) = &d.hover {
        draw_obb(&mut gizmos, b, HOVER_COLOR);
    }
    for b in &d.ghosts {
        draw_obb(&mut gizmos, b, GHOST_COLOR);
    }
    if let Some(g) = &d.gizmo {
        draw_gizmo(&mut gizmos, g);
    }
    if let Some(p) = d.placing {
        let (x, y, z) = (p[0], p[1], p[2] + 0.05);
        let ring: Vec<Vec3> = (0..=32)
            .map(|i| {
                let a = i as f32 / 32.0 * std::f32::consts::TAU;
                wow_to_bevy([x + 0.8 * a.cos(), y + 0.8 * a.sin(), z])
            })
            .collect();
        gizmos.linestrip(ring, PLACE_COLOR);
        gizmos.line(wow_to_bevy([x - 1.2, y, z]), wow_to_bevy([x + 1.2, y, z]), PLACE_COLOR);
        gizmos.line(wow_to_bevy([x, y - 1.2, z]), wow_to_bevy([x, y + 1.2, z]), PLACE_COLOR);
        gizmos.line(wow_to_bevy([x, y, z]), wow_to_bevy([x, y, z + 2.5]), PLACE_COLOR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::gizmo::EmeObb;

    /// A tiny app with the objects systems and no window: world_in / ui_commands / view_out.
    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<EmeHost>()
            .init_resource::<EmeFreeCam>()
            .init_resource::<EmeWorldObjects>()
            .init_resource::<eme_ui::EmeObjects>()
            .add_message::<EmeUiCommand>()
            .add_message::<EmeEventMsg>();
        let shared: EmeObjectsShared = Default::default();
        app.insert_resource(EmeObjectsHandle(shared.clone()));
        app.add_systems(Update, (world_in, ui_commands, view_out).chain());
        app
    }

    #[test]
    fn the_hosts_world_objects_reach_the_editor_and_the_view_follows() {
        let mut app = app();
        let handle = app.world().resource::<EmeObjectsHandle>().clone();
        lock(&handle.0).activate();
        app.world_mut().resource_mut::<EmeWorldObjects>().objects = vec![EmeWorldObject {
            guid: 4,
            entry: 99,
            name: Some("Barrel".into()),
            pos: [1.0, 2.0, 3.0],
            obb: EmeObb::point([1.0, 2.0, 3.0], 1.0),
        }];
        app.world_mut().resource_mut::<EmeWorldObjects>().stamp = 1;
        app.update();
        // The name became a spawnable template and the shell's snapshot knows it.
        let v = &app.world().resource::<eme_ui::EmeObjects>().0;
        assert!(v.active);
        assert_eq!(v.templates, vec![(99, "Barrel".to_owned())]);
    }

    #[test]
    fn a_selection_change_becomes_an_event_and_a_frame_moves_the_camera() {
        let mut app = app();
        let handle = app.world().resource::<EmeObjectsHandle>().clone();
        {
            let mut ed = lock(&handle.0);
            ed.activate();
            ed.set_world(vec![EmeWorldObject {
                guid: 4,
                entry: 99,
                name: None,
                pos: [100.0, 0.0, 0.0],
                obb: EmeObb::point([100.0, 0.0, 1.0], 1.0),
            }]);
            ed.select_only(4);
        }
        app.world_mut().write_message(EmeUiCommand::Objects(EmeObjectsCmd::Frame));
        // The command waits in the editor's inbox for its next step; drive it.
        app.update();
        let perms = eme_core::EmePermissions::from_rank(eme_core::EmeRank(3));
        let ctx = eme_core::EmeInputCtx::idle(1_000, 0, perms);
        let mut sink = eme_core::EmeCommandSink::new();
        lock(&handle.0).step(&ctx, None, &mut sink);
        app.update();
        let cam = app.world().resource::<EmeFreeCam>();
        let at = benilla_assets::coords::bevy_to_wow(cam.pos);
        assert!((at[0] - 100.0).abs() < 20.0, "stands near the object: {at:?}");
        let ev = app.world_mut().resource_mut::<EmeHost>().dispatcher.drain_events();
        assert!(ev.contains(&EmeEvent::SelectionChanged(vec![4])));
    }
}

#[cfg(test)]
mod hotkey_tests {
    use super::*;
    use crate::tools::objects::EmeObjectsTool;
    use eme_core::{EmeCommandSink, EmeInputCtx, EmePermissions, EmeRank};

    fn app_with_tool() -> (App, EmeObjectsShared) {
        let mut app = App::new();
        app.init_resource::<EmeHost>()
            .init_resource::<EmeActive>()
            .init_resource::<EmeHostInput>()
            .init_resource::<EmeUiWantsKeyboard>()
            .init_resource::<ButtonInput<KeyCode>>();
        let shared: EmeObjectsShared = Default::default();
        app.insert_resource(EmeObjectsHandle(shared.clone()));
        app.world_mut().resource_mut::<EmeActive>().0 = true;
        {
            let mut host = app.world_mut().resource_mut::<EmeHost>();
            host.registry
                .register(Box::new(EmeObjectsTool::new(shared.clone())))
                .unwrap();
            let perms = EmePermissions::from_rank(EmeRank(3));
            let mut sink = EmeCommandSink::new();
            host.registry.activate("objects", &perms, None, &mut sink).unwrap();
        }
        app.add_systems(Update, hotkeys);
        (app, shared)
    }

    /// Run the editor one step so queued commands apply.
    fn step(shared: &EmeObjectsShared) {
        let ctx = EmeInputCtx::idle(1_000, 0, EmePermissions::from_rank(EmeRank(3)));
        lock(shared).step(&ctx, None, &mut EmeCommandSink::new());
    }

    fn press(app: &mut App, key: KeyCode) {
        let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        keys.release_all();
        keys.clear();
        keys.press(key);
        app.update();
    }

    #[test]
    fn t_r_g_pick_the_gizmo_and_x_flips_the_axes() {
        let (mut app, shared) = app_with_tool();
        for (key, mode) in [
            (KeyCode::KeyR, EmeGizmoMode::Rotate),
            (KeyCode::KeyG, EmeGizmoMode::Scale),
            (KeyCode::KeyT, EmeGizmoMode::Translate),
        ] {
            press(&mut app, key);
            step(&shared);
            assert_eq!(lock(&shared).mode(), mode, "{key:?}");
        }
        press(&mut app, KeyCode::KeyX);
        step(&shared);
        assert_eq!(lock(&shared).space(), EmeGizmoSpace::Local);
        press(&mut app, KeyCode::KeyX);
        step(&shared);
        assert_eq!(lock(&shared).space(), EmeGizmoSpace::World);
    }

    #[test]
    fn typing_in_a_field_or_another_tool_blocks_the_hotkeys() {
        let (mut app, shared) = app_with_tool();
        app.world_mut().resource_mut::<EmeUiWantsKeyboard>().0 = true;
        press(&mut app, KeyCode::KeyR);
        step(&shared);
        assert_eq!(lock(&shared).mode(), EmeGizmoMode::Translate, "an EME text field has the keyboard");
        app.world_mut().resource_mut::<EmeUiWantsKeyboard>().0 = false;
        app.world_mut().resource_mut::<EmeHostInput>().typing = true;
        press(&mut app, KeyCode::KeyR);
        step(&shared);
        assert_eq!(lock(&shared).mode(), EmeGizmoMode::Translate, "the chat box has the keyboard");
        app.world_mut().resource_mut::<EmeHostInput>().typing = false;
        // Another active tool: the keys are not ours.
        {
            let mut host = app.world_mut().resource_mut::<EmeHost>();
            let mut sink = EmeCommandSink::new();
            host.registry.deactivate(&mut sink);
        }
        press(&mut app, KeyCode::KeyR);
        step(&shared);
        assert_eq!(lock(&shared).mode(), EmeGizmoMode::Translate);
    }

    #[test]
    fn ctrl_d_duplicates_and_ctrl_blocks_the_letter_hotkeys() {
        let (mut app, shared) = app_with_tool();
        lock(&shared).activate();
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::ControlLeft);
            keys.press(KeyCode::KeyR);
        }
        app.update();
        step(&shared);
        assert_eq!(lock(&shared).mode(), EmeGizmoMode::Translate, "Ctrl+R is not Rotate");
    }
}

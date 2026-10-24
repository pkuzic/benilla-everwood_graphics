//! `objects` (CONTRACT v3 §13 / v3.7): select, move, rotate, scale, place, duplicate and delete
//! game objects. The tool is a thin shell around [`EmeObjectsEditor`] (eme-core): the editor does
//! the picking, the gizmo maths, the ghost preview and the request pacing; this file lends it to
//! the registry and shares it with the Bevy side ([`crate::objects`]) through an
//! `Arc<Mutex<…>>`.
//!
//! Rank 2 sees the tool as read-only (the server lists it as `view_objects`): the outliner,
//! selection and camera framing work; every mutation needs rank 3 and an active project.
//!
//! Options (left panel): the gizmo mode and axes, snapping (grid / angle), ground snap, the list
//! radius and the scale / heading of a newly placed object. The outliner, inspector and spawn list
//! live in the right panel's Objects tab (`eme-ui`).

use std::sync::{Arc, Mutex, MutexGuard};

use eme_core::command::EmeCommandSink;
use eme_core::gizmo::{EmeGizmoMode, EmeGizmoSpace, SNAP_ANGLES, SNAP_GRIDS};
use eme_core::objects::EmeObjectsEditor;
use eme_core::tool::{
    EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview,
    EmeTerrain, EmeTool, EmeToolId, EmeToolNotice,
};

/// The tool id (the server's name for it in `caps.tools`; rank 2 gets `view_objects`).
pub const OBJECTS: EmeToolId = EmeToolId("objects");
/// What the server lists for a read-only rank.
pub const VIEW_OBJECTS: EmeToolId = EmeToolId("view_objects");

/// The editor, shared between the registry's tool and the Bevy systems.
pub type EmeObjectsShared = Arc<Mutex<EmeObjectsEditor>>;

/// Lock the shared editor (a panic elsewhere must not take the editor down with it).
pub fn lock(shared: &EmeObjectsShared) -> MutexGuard<'_, EmeObjectsEditor> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// The objects tool.
pub struct EmeObjectsTool {
    editor: EmeObjectsShared,
    /// Commands produced by [`EmeTool::on_terrain`] (which has no sink) until `on_input` runs.
    queued: EmeCommandSink,
}

impl EmeObjectsTool {
    /// A tool over the shared `editor`.
    pub fn new(editor: EmeObjectsShared) -> Self {
        Self {
            editor,
            queued: EmeCommandSink::new(),
        }
    }
}

impl std::fmt::Debug for EmeObjectsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EmeObjectsTool")
    }
}

fn choice_index(values: &[f32], v: f32) -> usize {
    values
        .iter()
        .position(|g| (g - v).abs() < 1.0e-4)
        .unwrap_or(0)
}

impl EmeTool for EmeObjectsTool {
    fn id(&self) -> EmeToolId {
        OBJECTS
    }

    fn server_tool_alt(&self) -> Option<EmeToolId> {
        Some(VIEW_OBJECTS)
    }

    /// Rank 2 may use the tool read-only; the editor itself refuses every mutation below rank 3.
    fn min_rank(&self) -> u8 {
        2
    }

    fn on_terrain(&mut self, ctx: &EmeInputCtx, terrain: &dyn EmeTerrain) {
        lock(&self.editor).step(ctx, Some(terrain), &mut self.queued);
    }

    fn take_notice(&mut self) -> Option<EmeToolNotice> {
        lock(&self.editor).take_notice()
    }

    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        {
            let mut ed = lock(&self.editor);
            if !ed.stepped() {
                // A host without terrain queries (tests) steps here.
                ed.step(ctx, None, &mut self.queued);
            }
            ed.end_frame();
        }
        for issued in self.queued.drain() {
            out.push(issued.command);
        }
    }

    fn options_ui_model(&self) -> EmeOptions {
        let ed = lock(&self.editor);
        let p = self.id().locale_prefix();
        let key = |k: &str| format!("{p}{k}");
        let snap = ed.snap();
        vec![
            EmeOptionField {
                key: "mode",
                label: key("MODE"),
                kind: EmeOptionKind::Choice(vec![
                    key("MODE_MOVE"),
                    key("MODE_ROTATE"),
                    key("MODE_SCALE"),
                ]),
                value: EmeOptionValue::Choice(match ed.mode() {
                    EmeGizmoMode::Translate => 0,
                    EmeGizmoMode::Rotate => 1,
                    EmeGizmoMode::Scale => 2,
                }),
                disabled: false,
            },
            EmeOptionField {
                key: "space",
                label: key("SPACE"),
                kind: EmeOptionKind::Choice(vec![key("SPACE_WORLD"), key("SPACE_LOCAL")]),
                value: EmeOptionValue::Choice(match ed.space() {
                    EmeGizmoSpace::World => 0,
                    EmeGizmoSpace::Local => 1,
                }),
                disabled: false,
            },
            EmeOptionField {
                key: "snap",
                label: key("SNAP"),
                kind: EmeOptionKind::Toggle,
                value: EmeOptionValue::Bool(snap.enabled),
                disabled: false,
            },
            EmeOptionField {
                key: "grid",
                label: key("GRID"),
                kind: EmeOptionKind::Choice(vec![key("GRID_01"), key("GRID_05"), key("GRID_1")]),
                value: EmeOptionValue::Choice(choice_index(&SNAP_GRIDS, snap.grid)),
                disabled: !snap.enabled,
            },
            EmeOptionField {
                key: "angle",
                label: key("ANGLE"),
                kind: EmeOptionKind::Choice(vec![key("ANGLE_5"), key("ANGLE_15")]),
                value: EmeOptionValue::Choice(choice_index(&SNAP_ANGLES, snap.angle_deg)),
                disabled: !snap.enabled,
            },
            EmeOptionField {
                key: "ground",
                label: key("GROUND"),
                kind: EmeOptionKind::Toggle,
                value: EmeOptionValue::Bool(ed.ground_snap()),
                disabled: false,
            },
            EmeOptionField {
                key: "radius",
                label: key("RADIUS"),
                kind: EmeOptionKind::Slider {
                    min: 10.0,
                    max: 200.0,
                    step: 5.0,
                },
                value: EmeOptionValue::Number(ed.radius()),
                disabled: false,
            },
            EmeOptionField {
                key: "spawn_scale",
                label: key("SPAWN_SCALE"),
                kind: EmeOptionKind::Slider {
                    min: 0.1,
                    max: 5.0,
                    step: 0.1,
                },
                value: EmeOptionValue::Number(ed.spawn_scale()),
                disabled: false,
            },
            EmeOptionField {
                key: "spawn_yaw",
                label: key("SPAWN_YAW"),
                kind: EmeOptionKind::Slider {
                    min: 0.0,
                    max: 355.0,
                    step: 5.0,
                },
                value: EmeOptionValue::Number(ed.spawn_yaw_deg()),
                disabled: false,
            },
        ]
    }

    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        let mut ed = lock(&self.editor);
        match (key, value) {
            ("mode", EmeOptionValue::Choice(i)) => ed.set_mode(match i {
                1 => EmeGizmoMode::Rotate,
                2 => EmeGizmoMode::Scale,
                _ => EmeGizmoMode::Translate,
            }),
            ("space", EmeOptionValue::Choice(i)) => ed.set_space(if i == 1 {
                EmeGizmoSpace::Local
            } else {
                EmeGizmoSpace::World
            }),
            ("snap", EmeOptionValue::Bool(b)) => ed.set_snap_enabled(b),
            ("grid", EmeOptionValue::Choice(i)) => {
                ed.set_grid(SNAP_GRIDS.get(i).copied().unwrap_or(SNAP_GRIDS[1]));
            }
            ("angle", EmeOptionValue::Choice(i)) => {
                ed.set_angle_step(SNAP_ANGLES.get(i).copied().unwrap_or(SNAP_ANGLES[0]));
            }
            ("ground", EmeOptionValue::Bool(b)) => ed.set_ground_snap(b),
            ("radius", EmeOptionValue::Number(r)) => ed.set_radius(r),
            ("spawn_scale", EmeOptionValue::Number(s)) => ed.set_spawn_scale(s),
            ("spawn_yaw", EmeOptionValue::Number(d)) => ed.set_spawn_yaw_deg(d),
            _ => {}
        }
    }

    fn preview(&self, _ctx: &EmeInputCtx) -> Option<EmePreview> {
        // The gizmo, boxes and ghosts are drawn by `objects::draw_objects` from the editor.
        None
    }

    fn on_activate(&mut self) {
        lock(&self.editor).activate();
    }

    fn on_deactivate(&mut self, _out: &mut EmeCommandSink) {
        lock(&self.editor).deactivate();
        self.queued.drain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::perm::{EmePermissions, EmeRank};

    fn tool() -> (EmeObjectsTool, EmeObjectsShared) {
        let shared: EmeObjectsShared = Arc::new(Mutex::new(EmeObjectsEditor::new()));
        (EmeObjectsTool::new(shared.clone()), shared)
    }

    #[test]
    fn options_map_onto_the_editor() {
        let (mut t, shared) = tool();
        let keys: Vec<_> = t.options_ui_model().iter().map(|f| f.key).collect();
        assert_eq!(
            keys,
            ["mode", "space", "snap", "grid", "angle", "ground", "radius", "spawn_scale", "spawn_yaw"]
        );
        t.set_option("snap", EmeOptionValue::Bool(true));
        t.set_option("grid", EmeOptionValue::Choice(2));
        t.set_option("angle", EmeOptionValue::Choice(1));
        t.set_option("ground", EmeOptionValue::Bool(false));
        t.set_option("radius", EmeOptionValue::Number(120.0));
        {
            let ed = lock(&shared);
            assert!(ed.snap().enabled && ed.snap().grid == 1.0 && ed.snap().angle_deg == 15.0);
            assert!(!ed.ground_snap() && ed.radius() == 120.0);
        }
        let f = t.options_ui_model();
        let grid = f.iter().find(|f| f.key == "grid").unwrap();
        assert_eq!(grid.value, EmeOptionValue::Choice(2));
        assert!(!grid.disabled);
        t.set_option("snap", EmeOptionValue::Bool(false));
        assert!(t.options_ui_model().iter().find(|f| f.key == "grid").unwrap().disabled);
        // Every label key is a locale key of this tool.
        for f in t.options_ui_model() {
            assert!(f.label.starts_with("MONKEY_EME_OBJECTS_"), "{}", f.label);
        }
    }

    #[test]
    fn rank_two_sees_it_as_view_objects_and_the_server_name_is_accepted() {
        let (t, _) = tool();
        assert_eq!(t.min_rank(), 2);
        let perms2 = EmePermissions::from_rank(EmeRank(2));
        let perms1 = EmePermissions::from_rank(EmeRank(1));
        let caps = |tools: &[&str]| {
            let r = eme_core::EmeServiceReply::ok(eme_core::EmeRequestId(1))
                .with("rank", 2)
                .with("tools", tools);
            eme_core::EmeCaps::from_reply(&r).unwrap()
        };
        assert!(eme_core::EmeToolRegistry::is_permitted(&t, &perms2, Some(&caps(&["view_objects"]))));
        assert!(eme_core::EmeToolRegistry::is_permitted(&t, &perms2, Some(&caps(&["objects"]))));
        assert!(!eme_core::EmeToolRegistry::is_permitted(&t, &perms2, Some(&caps(&["history"]))));
        assert!(!eme_core::EmeToolRegistry::is_permitted(&t, &perms1, Some(&caps(&["objects"]))));
    }

    #[test]
    fn the_tool_steps_the_editor_once_per_frame_and_hands_its_commands_to_the_sink() {
        let (mut t, shared) = tool();
        t.on_activate();
        let perms = EmePermissions::from_rank(EmeRank(3));
        let ctx = EmeInputCtx::idle(1_000, 0, perms);
        let mut sink = EmeCommandSink::new();
        // No terrain queries: on_input steps. The first frame asks for the list.
        t.on_input(&ctx, &mut sink);
        let cmds = sink.drain();
        assert_eq!(cmds.len(), 1);
        assert!(!lock(&shared).stepped());
        // With terrain, on_terrain steps and on_input only drains.
        struct Flat;
        impl EmeTerrain for Flat {
            fn ground_z(&self, _: f32, _: f32, _: f32, _: f32) -> Option<f32> {
                Some(0.0)
            }
        }
        let ctx2 = EmeInputCtx::idle(1_100, 0, perms);
        t.on_terrain(&ctx2, &Flat);
        assert!(lock(&shared).stepped());
        t.on_input(&ctx2, &mut sink);
        assert!(!lock(&shared).stepped());
        // Deactivating clears the editor's active flag.
        t.on_deactivate(&mut sink);
        assert!(!lock(&shared).view().active);
    }
}

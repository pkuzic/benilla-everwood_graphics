//! `WOW_EME_SCRIPT=<json file>`: drive EME headlessly through the SAME code paths as a GM at the
//! keyboard (toggle → free camera → tool → strokes → undo), for offline captures and tests.
//!
//! A script switches the transport to the offline emulated server ([`crate::offline`]) unless it
//! says `"online": true`. Steps run one after another, on game time (a capture's frozen clock
//! makes a run deterministic: the steps start once the capture releases its clock).
//!
//! ```json
//! {"steps": [
//!   {"op": "wait", "s": 2},
//!   {"op": "camera", "eye": [-9560, -300, 70], "look": [-9540, -306, 56]},
//!   {"op": "open"},                                   // = F9 (waits for caps, ≤ max_s)
//!   {"op": "cursor", "x": 0.5, "y": 0.55},            // virtual cursor, viewport fractions
//!   {"op": "wait_hit"},                               // the cursor ray hits ground / voxels
//!   {"op": "screenshot", "path": "G:/mangos/disposable/eme_d_ring.png"},
//!   {"op": "stroke", "x": 0.48, "y": 0.55, "to": [0.52, 0.55], "s": 0.6, "mode": "dig", "radius": 5},
//!   {"op": "wait_settled"},
//!   {"op": "undo"}, {"op": "wait_settled"},
//!   {"op": "close"},                                   // = Tab
//!   {"op": "exit"}
//! ]}
//! ```
//! [EME objects] `obj_row {guid}` (outliner click), `obj_pick {guid}` (click the object in the
//! world), `obj_drag {handle: x|y|z|xy|xz|yz|ring|scale, delta, s}` (grab a gizmo handle and drag),
//! `obj_field {field: x|y|z|o|s, value}`, `obj_arm {entry}` (then `click` the ground),
//! `obj_cmd {name: delete|duplicate|frame|refresh|more|none|disarm}`, `obj_wait {idle|rows|has|gone}`,
//! `obj_report`.
//! Other ops: `tool {id}`, `option {key, value}`, `redo`, `log {msg}`, `wait_active`,
//! [v3.6] `click {x, y}` (one LMB click), `key {name: esc|enter}`; `cursor` / `click` / `stroke`
//! also take `at: [x, y, z]` (a WoW point projected through the camera; `stroke` + `to_at`),
//! `tab {name: history|restore|log|projects}` / `panels {left, right}` (the shell's side panels),
//! [EME projects] `wait_project {none?}`, `project_open_named {name}`, `project_revert {name?}`,
//! `wait_conflict`, `project_force {cancel?}` (create / leave / close via `svc`),
//! `svc {req}` (any service request as the host, e.g. `{"op":"snapshot_create","label":"x"}`),
//! `revert_last` (revert the newest listed restore point, as the confirmed UI button would),
//! `select_restore {idx}`, `show {x, y}` (camera jump + marker), `filter {player, radius,
//! minutes, mode, layer}` (the log tab's Apply), `report` (log the restore list, log table,
//! other-GM rows and notice) and `wait_caps` (the server's `caps` arrived). Every wait
//! takes `max_s` (default 20) and logs a timeout instead of hanging. Each step logs an
//! `EME script:` line with the voxel journal / chunk counts, so a run is checkable from its log.

use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use serde_json::Value as Json;

use crate::camera::EmeFreeCam;
use crate::viewport::{EmeAim, EmeBrushPreviewState, EmeCursor};
use crate::{EmeActive, EmeHost, EmeRequest, EmeSet, EmeToggleRequested, EmeWire};
use benilla_world::voxel::MonkeyVoxelApi;
use eme_core::EmeOptionValue;

/// A loaded script and its progress.
#[derive(Resource, Debug)]
pub struct EmeScript {
    steps: Vec<Json>,
    idx: usize,
    /// Game seconds the current step has run.
    t: f32,
    started: bool,
}

impl EmeScript {
    /// Parse a script (`{"steps":[…]}` or a bare array). `None` = malformed.
    pub fn parse(text: &str) -> Option<(Self, bool)> {
        let v: Json = serde_json::from_str(text).ok()?;
        let online = v.get("online").and_then(Json::as_bool).unwrap_or(false);
        let steps = match v {
            Json::Array(a) => a,
            Json::Object(ref o) => o.get("steps")?.as_array()?.clone(),
            _ => return None,
        };
        Some((
            Self {
                steps,
                idx: 0,
                t: 0.0,
                started: false,
            },
            online,
        ))
    }

    /// Whether every step ran.
    pub fn done(&self) -> bool {
        self.idx >= self.steps.len()
    }
}

pub(crate) fn register(app: &mut App) {
    let Ok(path) = std::env::var("WOW_EME_SCRIPT") else {
        return;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            error!("EME script: cannot read {path}: {e}");
            return;
        }
    };
    let Some((script, online)) = EmeScript::parse(&text) else {
        error!("EME script: {path} is not a JSON step list");
        return;
    };
    info!(
        "EME script: {} step(s) from {path} ({})",
        script.steps.len(),
        if online { "online" } else { "offline emulated server" }
    );
    if !online {
        app.world_mut().resource_mut::<EmeWire>().offline = true;
    }
    app.insert_resource(script)
        .add_systems(Update, run_script.in_set(EmeSet::Input).before(crate::viewport::update_cursor_and_aim));
}

fn num(v: &Json, k: &str) -> Option<f32> {
    v.get(k).and_then(Json::as_f64).map(|f| f as f32)
}

fn vec3(v: &Json, k: &str) -> Option<[f32; 3]> {
    let a = v.get(k)?.as_array()?;
    (a.len() == 3).then(|| {
        [
            a[0].as_f64().unwrap_or(0.0) as f32,
            a[1].as_f64().unwrap_or(0.0) as f32,
            a[2].as_f64().unwrap_or(0.0) as f32,
        ]
    })
}

fn mode_value(v: &Json) -> Option<EmeOptionValue> {
    let m = match v {
        Json::Number(n) => n.as_u64()? as usize,
        Json::String(s) => match s.as_str() {
            "dig" | "lower" => 0,
            "build" | "raise" => 1,
            "smooth" => 2,
            "push" => 3,
            "pull" => 4,
            _ => return None,
        },
        _ => return None,
    };
    Some(EmeOptionValue::Choice(m))
}

fn option_value(v: &Json) -> Option<EmeOptionValue> {
    match v {
        Json::Number(n) => Some(EmeOptionValue::Number(n.as_f64()? as f32)),
        Json::Bool(b) => Some(EmeOptionValue::Bool(*b)),
        Json::String(s) => Some(EmeOptionValue::Text(s.clone())),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_script(
    time: Res<Time>,
    mut script: ResMut<EmeScript>,
    active: Res<EmeActive>,
    host: Res<EmeHost>,
    aim: Res<EmeAim>,
    preview: Res<EmeBrushPreviewState>,
    mut cursor: ResMut<EmeCursor>,
    mut cam: ResMut<EmeFreeCam>,
    mut toggle: MessageWriter<EmeToggleRequested>,
    mut requests: MessageWriter<EmeRequest>,
    mut commands: Commands,
    mut exit: MessageWriter<AppExit>,
    api: MonkeyVoxelApi,
    (mut shell, mut ui_cmds, mut vkeys): (
        ResMut<eme_ui::EmeShell>,
        MessageWriter<eme_ui::EmeUiCommand>,
        ResMut<crate::dispatch::EmeVirtualKeys>,
    ),
    camera_q: Query<(&Camera, &GlobalTransform), With<benilla_world::view::WorldCamera>>,
    (restore, log_table, others, notice, wire, projects, objects, world_objs): (
        Res<eme_ui::EmeRestorePoints>,
        Res<eme_ui::EmeLogTable>,
        Res<eme_ui::EmeOthersHistory>,
        Res<eme_ui::EmeNotice>,
        Res<EmeWire>,
        Res<eme_ui::EmeProjects>,
        Res<crate::objects::EmeObjectsHandle>,
        Res<crate::objects::EmeWorldObjects>,
    ),
) {
    if script.done() {
        return;
    }
    let dt = time.delta_secs();
    let Some(step) = script.steps.get(script.idx).cloned() else {
        return;
    };
    let op = step.get("op").and_then(Json::as_str).unwrap_or("");
    let first = !script.started;
    script.started = true;
    script.t += dt;
    let t = script.t;
    let max_s = num(&step, "max_s").unwrap_or(20.0);
    let map = api.map().unwrap_or(0);
    let status = || {
        // An FNV hash over the canonical journal: two clients that converged print the same one.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for e in api.journal(map).iter().filter(|e| e.seq.is_some()) {
            for v in [
                u64::from(e.seq.unwrap_or(0)),
                u64::from(e.mode),
                (e.pos[0] * 100.0).round() as i64 as u64,
                (e.pos[1] * 100.0).round() as i64 as u64,
                (e.pos[2] * 100.0).round() as i64 as u64,
            ] {
                h = (h ^ v).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        format!(
            "map {map}, journal {} edit(s) ({} pending), confirmed-hash {h:016x}, {} active chunk(s), settled {}, hit {:?}",
            api.journal(map).len(),
            api.pending_local(map).len(),
            api.active_chunks(),
            api.settled(),
            aim.hit.map(|h| [h.pos[0].round(), h.pos[1].round(), (h.pos[2] * 10.0).round() / 10.0]),
        )
    };
    // MONKEY (EME v3.6): a WoW point → the virtual cursor's viewport fraction (`at` / `to_at`).
    let project = |p: [f32; 3]| -> Option<Vec2> {
        let (cam, gt) = camera_q.single().ok()?;
        let size = cam.logical_viewport_size()?;
        let v = cam
            .world_to_viewport(gt, benilla_assets::coords::wow_to_bevy(p))
            .ok()?;
        Some(v / size)
    };
    let mut done = true;
    match op {
        "wait" => done = t >= num(&step, "s").unwrap_or(1.0),
        "camera" => {
            if let (Some(eye), Some(look)) = (vec3(&step, "eye"), vec3(&step, "look")) {
                cam.look_at_wow(eye, look);
            }
        }
        "open" | "close" => {
            let on = op == "open";
            if active.0 != on {
                if t > max_s {
                    warn!("EME script: {op} timed out (rank allows open: {})", host.dispatcher.permissions().can_open());
                } else {
                    toggle.write(EmeToggleRequested { to: Some(on) });
                    done = false;
                }
            }
        }
        "wait_active" => done = active.0 || t > max_s,
        "cursor" => {
            cursor.virtual_norm = Some(vec3(&step, "at").and_then(|p| project(p)).unwrap_or_else(|| {
                Vec2::new(num(&step, "x").unwrap_or(0.5), num(&step, "y").unwrap_or(0.5))
            }));
        }
        "wait_hit" => done = aim.hit.is_some() && preview.visible || t > max_s,
        "wait_settled" => done = (api.settled() && t > 0.25) || t > max_s,
        "tool" => {
            if let Some(id) = step.get("id").and_then(Json::as_str) {
                requests.write(EmeRequest::SelectTool(id.to_owned()));
            }
        }
        "option" => {
            if let (Some(k), Some(v)) = (step.get("key").and_then(Json::as_str), step.get("value")) {
                // `mode` takes dig / build / smooth (sculpt) or a tool's own word / number.
                let value = if k == "mode" {
                    mode_value(v).or_else(|| option_value(v))
                } else {
                    option_value(v)
                };
                if let Some(value) = value {
                    requests.write(EmeRequest::SetOption {
                        key: k.to_owned(),
                        value,
                    });
                }
            }
        }
        "stroke" => {
            let from = vec3(&step, "at")
                .and_then(|p| project(p))
                .unwrap_or_else(|| Vec2::new(num(&step, "x").unwrap_or(0.5), num(&step, "y").unwrap_or(0.5)));
            let to = step
                .get("to")
                .and_then(Json::as_array)
                .filter(|a| a.len() == 2)
                .map_or(from, |a| {
                    Vec2::new(
                        a[0].as_f64().unwrap_or(0.5) as f32,
                        a[1].as_f64().unwrap_or(0.5) as f32,
                    )
                });
            let to = vec3(&step, "to_at").and_then(|p| project(p)).unwrap_or(to);
            let dur = num(&step, "s").unwrap_or(0.5).max(0.05);
            if first {
                // "flatten" is its own tool; dig / build / smooth are the sculpt tool's modes.
                match step.get("mode") {
                    Some(Json::String(m)) if m == "flatten" => {
                        requests.write(EmeRequest::SelectTool("flatten".into()));
                    }
                    Some(m) => {
                        if let Some(m) = mode_value(m) {
                            requests.write(EmeRequest::SelectTool("sculpt".into()));
                            requests.write(EmeRequest::SetOption {
                                key: "mode".into(),
                                value: m,
                            });
                        }
                    }
                    None => {}
                }
                if let Some(r) = num(&step, "radius") {
                    requests.write(EmeRequest::SetOption {
                        key: "radius".into(),
                        value: EmeOptionValue::Number(r),
                    });
                }
                // Hover first: the press lands on the next frame, with the options applied.
                cursor.virtual_norm = Some(from);
                cursor.virtual_primary = false;
                done = false;
            } else if t < dur || step.get("_held").and_then(Json::as_u64).unwrap_or(0) < 2 {
                // At least two held frames, however slow the frames run (a 0.3-s stroke on a
                // 4-fps frame must still press).
                let held = step.get("_held").and_then(Json::as_u64).unwrap_or(0);
                let i = script.idx;
                script.steps[i]["_held"] = Json::from(held + 1);
                cursor.virtual_norm = Some(from.lerp(to, (t / dur).clamp(0.0, 1.0)));
                cursor.virtual_primary = true;
                done = false;
            } else {
                cursor.virtual_norm = Some(to);
                cursor.virtual_primary = false;
            }
        }
        "undo" => {
            requests.write(EmeRequest::Undo);
        }
        // MONKEY (EME v3.6): one click (hover a frame, press ~0.1 s, release) at viewport fractions.
        "click" => {
            cursor.virtual_norm = Some(vec3(&step, "at").and_then(|p| project(p)).unwrap_or_else(|| {
                Vec2::new(num(&step, "x").unwrap_or(0.5), num(&step, "y").unwrap_or(0.5))
            }));
            // Frame-counted (a slow frame must not skip the press): hover, press, release.
            let i = script.idx;
            let phase = if first { 0 } else { step.get("_phase").and_then(Json::as_u64).unwrap_or(0) };
            match phase {
                0 => cursor.virtual_primary = false,
                1 => cursor.virtual_primary = true,
                _ => cursor.virtual_primary = false,
            }
            if phase < 2 {
                done = false;
                script.steps[i]["_phase"] = Json::from(phase + 1);
            }
        }
        // MONKEY (EME v3.6): `{"op":"key","name":"esc"|"enter"}` = Esc / Enter for the tools.
        "key" => match step.get("name").and_then(Json::as_str).unwrap_or("") {
            "esc" | "escape" => vkeys.cancel = true,
            "enter" | "return" => vkeys.confirm = true,
            other => warn!("EME script: key {other:?} unknown (esc / enter)"),
        },
        "wait_caps" => {
            done = host.dispatcher.caps().is_some() || t > max_s;
            if t > max_s {
                warn!("EME script: wait_caps timed out (link {:?})", wire.link);
            }
        }
        "svc" => match step
            .get("req")
            .cloned()
            .and_then(|v| serde_json::from_value::<eme_core::EmeServiceRequest>(v).ok())
        {
            Some(req) => {
                requests.write(EmeRequest::Service(req));
            }
            None => warn!("EME script: svc needs a valid service request in `req`"),
        },
        "revert_last" => match restore.rows.first() {
            Some(row) => {
                info!("EME script: reverting restore point {} ({:?})", row.snapshot, row.label);
                ui_cmds.write(eme_ui::EmeUiCommand::Service(
                    eme_core::EmeServiceRequest::Revert {
                        snapshot: row.snapshot,
                    },
                ));
            }
            None => warn!("EME script: revert_last: no restore point listed"),
        },
        "select_restore" => {
            let idx = num(&step, "idx").unwrap_or(0.0) as usize;
            let id = restore.rows.get(idx).map(|r| r.snapshot);
            ui_cmds.write(eme_ui::EmeUiCommand::SelectRestore(id));
        }
        "show" => {
            ui_cmds.write(eme_ui::EmeUiCommand::ShowInWorld {
                x: num(&step, "x").unwrap_or(0.0),
                y: num(&step, "y").unwrap_or(0.0),
                z: num(&step, "z"),
            });
        }
        "filter" => {
            let text = |k: &str| {
                step.get(k).map_or(String::new(), |v| match v {
                    Json::String(s) => s.clone(),
                    other => other.to_string(),
                })
            };
            shell.filter = eme_ui::EmeLogFilterUi {
                player: text("player"),
                radius: text("radius"),
                minutes: text("minutes"),
                mode: num(&step, "mode").unwrap_or(0.0) as usize,
                layer: num(&step, "layer").unwrap_or(0.0) as usize,
                project_only: step.get("project_only").and_then(Json::as_bool).unwrap_or(false),
            };
            shell.layout.right_open = true;
            shell.layout.right_tab = eme_ui::EmeRightTab::Log;
            let active = host.dispatcher.project().map(|p| p.id);
            ui_cmds.write(eme_ui::EmeUiCommand::LogQuery(shell.filter.query_for(active)));
        }
        "report" => {
            info!(
                "EME script: REPORT restore={:?} selected={:?}",
                restore
                    .rows
                    .iter()
                    .map(|r| (r.snapshot, r.label.clone(), r.radius))
                    .collect::<Vec<_>>(),
                restore.selected
            );
            info!(
                "EME script: REPORT log count={} more={} page={} rows={:?}",
                log_table.count,
                log_table.more,
                log_table.page,
                log_table
                    .rows
                    .iter()
                    .take(8)
                    .map(|r| (r.seq, r.name.clone(), r.mode, r.stroke, r.reverted))
                    .collect::<Vec<_>>()
            );
            info!(
                "EME script: REPORT others={:?}",
                others
                    .rows
                    .iter()
                    .map(|r| (r.actor, r.name.clone(), r.stroke, r.edits))
                    .collect::<Vec<_>>()
            );
            info!("EME script: REPORT notice={:?} args={:?} error={}", notice.key, notice.args, notice.error);
            info!(
                "EME script: REPORT project={:?} required={} rows={:?} conflict={:?}",
                host.dispatcher.project(),
                host.dispatcher.projects_required(),
                projects.rows.iter().map(|r| (r.id, r.name.clone(), r.status.clone(), r.edits)).collect::<Vec<_>>(),
                projects.conflict
            );
            let undo = host.dispatcher.undo_stack();
            info!(
                "EME script: REPORT caps={:?} undo entries={}",
                host.dispatcher.caps().map(|c| (c.rank.0, c.tools.clone())),
                undo.entries().len()
            );
        }
        "panels" => {
            if let Some(b) = step.get("left").and_then(Json::as_bool) {
                shell.layout.left_open = b;
            }
            if let Some(b) = step.get("right").and_then(Json::as_bool) {
                shell.layout.right_open = b;
            }
        }
        "tab" => {
            let tab = match step.get("name").and_then(Json::as_str).unwrap_or("") {
                "restore" => Some(eme_ui::EmeRightTab::Restore),
                "log" => Some(eme_ui::EmeRightTab::Log),
                "history" => Some(eme_ui::EmeRightTab::History),
                "projects" => Some(eme_ui::EmeRightTab::Projects),
                "objects" => Some(eme_ui::EmeRightTab::Objects),
                _ => None,
            };
            if let Some(tab) = tab {
                shell.layout.right_open = true;
                shell.layout.right_tab = tab;
                ui_cmds.write(eme_ui::EmeUiCommand::TabOpened(tab));
            }
        }
        "redo" => {
            requests.write(EmeRequest::Redo);
        }
        // MONKEY (EME projects): wait until a project is active (`"none": true` = until none is).
        "wait_project" => {
            let want_none = step.get("none").and_then(Json::as_bool).unwrap_or(false);
            done = host.dispatcher.project().is_some() != want_none || t > max_s;
            if t > max_s {
                warn!("EME script: wait_project timed out (active {:?})", host.dispatcher.project());
            }
        }
        // MONKEY (EME projects): wait for the `others_edited` modal of a project revert.
        "wait_conflict" => {
            done = projects.conflict.is_some() || t > max_s;
            if t > max_s {
                warn!("EME script: wait_conflict timed out");
            }
        }
        // MONKEY (EME projects): the modal's "Revert everyone's edits" (or `"cancel": true`).
        "project_force" => match projects.conflict.clone() {
            Some(c) => {
                if !step.get("cancel").and_then(Json::as_bool).unwrap_or(false) {
                    info!("EME script: forcing the revert of project {} ({})", c.project, c.name);
                    ui_cmds.write(eme_ui::EmeUiCommand::Service(
                        eme_core::EmeServiceRequest::ProjectRevert { project: c.project, force: true },
                    ));
                }
                ui_cmds.write(eme_ui::EmeUiCommand::DismissProjectConflict);
            }
            None => warn!("EME script: project_force: no conflict shown"),
        },
        // MONKEY (EME projects): open the newest listed project named `name` (the Projects tab's Open).
        "project_open_named" => {
            let name = step.get("name").and_then(Json::as_str).unwrap_or("");
            match projects.rows.iter().find(|r| r.name == name) {
                Some(r) => {
                    ui_cmds.write(eme_ui::EmeUiCommand::Service(
                        eme_core::EmeServiceRequest::ProjectOpen { project: r.id },
                    ));
                }
                None if t <= max_s => done = false,
                None => warn!("EME script: project_open_named: no listed project {name:?}"),
            }
        }
        // MONKEY (EME projects): revert the active (or the named) project, as the confirmed button.
        "project_revert" => {
            let name = step.get("name").and_then(Json::as_str);
            let id = match name {
                Some(n) => projects.rows.iter().find(|r| r.name == n).map(|r| r.id),
                None => host.dispatcher.project().map(|p| p.id),
            };
            match id {
                Some(project) => {
                    ui_cmds.write(eme_ui::EmeUiCommand::Service(
                        eme_core::EmeServiceRequest::ProjectRevert { project, force: false },
                    ));
                }
                None if t <= max_s => done = false,
                None => warn!("EME script: project_revert: no project"),
            }
        }
        // MONKEY (EME objects): drive the Objects tool through the same paths as a GM -----------
        // `obj_row {guid}`: click the outliner row (select + frame).
        "obj_row" => {
            let guid = num(&step, "guid").unwrap_or(0.0) as u32;
            ui_cmds.write(eme_ui::EmeUiCommand::Objects(eme_core::EmeObjectsCmd::Select(guid)));
            ui_cmds.write(eme_ui::EmeUiCommand::Objects(eme_core::EmeObjectsCmd::Frame));
            // The camera needs the editor to step once (it applies the command).
            done = t >= 0.3;
        }
        // `obj_cmd {name: delete|duplicate|frame|refresh|more|none|disarm}`.
        "obj_cmd" => {
            use eme_core::EmeObjectsCmd as C;
            let cmd = match step.get("name").and_then(Json::as_str).unwrap_or("") {
                "delete" => Some(C::Delete),
                "duplicate" => Some(C::Duplicate),
                "frame" => Some(C::Frame),
                "refresh" => Some(C::Refresh),
                "more" => Some(C::LoadMore),
                "none" => Some(C::SelectNone),
                "disarm" => Some(C::DisarmSpawn),
                other => {
                    warn!("EME script: obj_cmd {other:?} unknown");
                    None
                }
            };
            if let Some(c) = cmd {
                ui_cmds.write(eme_ui::EmeUiCommand::Objects(c));
            }
            done = t >= 0.2;
        }
        // `obj_field {field: x|y|z|o|s, value}`: the inspector's Enter (heading in degrees).
        "obj_field" => {
            use eme_core::EmeObjField as F;
            let field = match step.get("field").and_then(Json::as_str).unwrap_or("") {
                "x" => Some(F::X),
                "y" => Some(F::Y),
                "z" => Some(F::Z),
                "o" => Some(F::Orientation),
                "s" => Some(F::Scale),
                _ => None,
            };
            if let (Some(field), Some(value)) = (field, num(&step, "value")) {
                ui_cmds.write(eme_ui::EmeUiCommand::Objects(eme_core::EmeObjectsCmd::SetField { field, value }));
            }
            done = t >= 0.2;
        }
        // `obj_arm {entry}`: arm a template for placement (then `click` the ground).
        "obj_arm" => {
            ui_cmds.write(eme_ui::EmeUiCommand::Objects(eme_core::EmeObjectsCmd::ArmSpawn {
                entry: num(&step, "entry").unwrap_or(0.0) as u32,
                name: None,
            }));
            done = t >= 0.2;
        }
        // `obj_pick {guid}`: click the object itself in the world (its box centre, through the
        // camera): the real picking path.
        "obj_pick" => {
            // By guid, or by template entry (the newest one: the highest guid).
            let guid = num(&step, "guid").unwrap_or(0.0) as u32;
            let entry = num(&step, "entry").map(|e| e as u32);
            let at = world_objs
                .objects
                .iter()
                .filter(|o| if let Some(e) = entry { o.entry == e } else { o.guid == guid })
                .max_by_key(|o| o.guid)
                .map(|o| o.obb.center);
            match at {
                Some(p) => {
                    let i = script.idx;
                    script.steps[i] = serde_json::json!({"op": "click", "at": [p[0], p[1], p[2]]});
                    done = false;
                }
                None if t <= max_s => done = false,
                None => warn!("EME script: obj_pick: object {guid} not streamed in"),
            }
        }
        // `obj_drag {handle: x|y|z|xy|xz|yz|ring|scale, delta, s}`: grab a gizmo handle of the
        // selection and drag it: yards (x y z), [dx, dy] yards (planes), degrees (ring), a factor
        // (scale). Frame-counted so a slow frame cannot skip the press or the release.
        "obj_drag" => 'drag: {
            let draw = objects.0.lock().map(|e| e.draw()).ok();
            let Some(g) = draw.and_then(|d| d.gizmo) else {
                if t > max_s {
                    warn!("EME script: obj_drag: no gizmo (nothing selected, or the rank cannot edit)");
                } else {
                    done = false;
                }
                break 'drag;
            };
            let f = g.frame;
            let handle = step.get("handle").and_then(Json::as_str).unwrap_or("x").to_owned();
            let d1 = num(&step, "delta").unwrap_or(1.0);
            let d2 = step
                .get("delta")
                .and_then(Json::as_array)
                .map(|a| [a.first().and_then(Json::as_f64).unwrap_or(0.0) as f32, a.get(1).and_then(Json::as_f64).unwrap_or(0.0) as f32]);
            let mix = |a: [f32; 3], k: f32, b: [f32; 3]| [a[0] + b[0] * k, a[1] + b[1] * k, a[2] + b[2] * k];
            let at = |u: f32, v: f32, i: usize, j: usize| mix(mix(f.origin, u * f.size, f.axes[i]), v * f.size, f.axes[j]);
            let (grab, target) = match handle.as_str() {
                "x" | "y" | "z" => {
                    let i = match handle.as_str() { "x" => 0, "y" => 1, _ => 2 };
                    let grab = mix(f.origin, 0.6 * f.size, f.axes[i]);
                    (grab, mix(grab, d1, f.axes[i]))
                }
                "xy" | "xz" | "yz" => {
                    let (i, j) = match handle.as_str() { "xy" => (0, 1), "xz" => (0, 2), _ => (1, 2) };
                    let grab = at(0.4, 0.4, i, j);
                    let [dx, dy] = d2.unwrap_or([d1, 0.0]);
                    (grab, mix(mix(grab, dx, f.axes[i]), dy, f.axes[j]))
                }
                "ring" => {
                    let a = d1.to_radians();
                    let r = eme_core::gizmo::rot_z([f.size, 0.0, 0.0], a);
                    let ax = f.axes[0];
                    let ay = f.axes[1];
                    let grab = mix(f.origin, f.size, ax);
                    let target = mix(mix(f.origin, r[0], ax), r[1], ay);
                    (grab, target)
                }
                _ => {
                    let grab = f.tip(0);
                    (grab, mix(f.origin, d1 * f.size, f.axes[0]))
                }
            };
            let (gp, tp) = (project(grab), project(target));
            let (Some(gp), Some(tp)) = (gp, tp) else {
                warn!("EME script: obj_drag: the handle is off screen — aim the camera first");
                done = t > max_s;
                break 'drag;
            };
            let dur = num(&step, "s").unwrap_or(0.8).max(0.1);
            let i = script.idx;
            let phase = if first { 0 } else { step.get("_phase").and_then(Json::as_u64).unwrap_or(0) };
            script.steps[i]["_phase"] = Json::from(phase + 1);
            done = false;
            // The gizmo follows the ghost while dragging, so the grab and target points are
            // fixed ONCE (phase 0) in viewport fractions: recomputing them each frame would chase
            // the object it moves.
            let (gp, tp) = if phase == 0 {
                script.steps[i]["_gp"] = serde_json::json!([gp.x, gp.y]);
                script.steps[i]["_tp"] = serde_json::json!([tp.x, tp.y]);
                (gp, tp)
            } else {
                let read = |k: &str| {
                    step.get(k).and_then(Json::as_array).map_or(Vec2::ZERO, |a| {
                        Vec2::new(
                            a.first().and_then(Json::as_f64).unwrap_or(0.0) as f32,
                            a.get(1).and_then(Json::as_f64).unwrap_or(0.0) as f32,
                        )
                    })
                };
                (read("_gp"), read("_tp"))
            };
            match phase {
                // Hover the handle, press on it, then give the tool two frames to grab it.
                0 => {
                    cursor.virtual_norm = Some(gp);
                    cursor.virtual_primary = false;
                }
                1..=3 => {
                    cursor.virtual_norm = Some(gp);
                    cursor.virtual_primary = true;
                    if phase == 3 {
                        script.steps[i]["_t0"] = Json::from(t);
                    }
                }
                _ => {
                    let t0 = step.get("_t0").and_then(Json::as_f64).unwrap_or(0.0) as f32;
                    let k = ((t - t0) / dur).clamp(0.0, 1.0);
                    cursor.virtual_norm = Some(gp.lerp(tp, k));
                    cursor.virtual_primary = true;
                    if k >= 1.0 {
                        // Two held frames at the target, then release.
                        let hold = step.get("_hold").and_then(Json::as_u64).unwrap_or(0);
                        script.steps[i]["_hold"] = Json::from(hold + 1);
                        if hold >= 2 {
                            cursor.virtual_primary = false;
                            if hold >= 4 {
                                done = true;
                            }
                        }
                    }
                }
            }
        }
        // `obj_wait {idle: true | rows: n | gone: guid | has: guid}`: wait for the editor.
        "obj_wait" => {
            let v = objects.0.lock().map(|e| e.view()).unwrap_or_default();
            let ok = if step.get("idle").and_then(Json::as_bool).unwrap_or(false) {
                v.pending == 0 && t > 0.3
            } else if let Some(n) = num(&step, "rows") {
                v.rows.len() >= n as usize
            } else if let Some(g) = num(&step, "gone") {
                !v.rows.iter().any(|r| r.guid == g as u32)
            } else if let Some(g) = num(&step, "has") {
                v.rows.iter().any(|r| r.guid == g as u32)
            } else {
                true
            };
            done = ok || t > max_s;
            if t > max_s {
                warn!("EME script: obj_wait timed out ({} rows, {} pending)", v.rows.len(), v.pending);
            }
        }
        // `obj_report`: log the editor's state (rows, selection, inspector, world pick list).
        "obj_report" => {
            let v = objects.0.lock().map(|e| e.view()).unwrap_or_default();
            info!(
                "EME script: OBJ rows={:?} selected={:?} pending={} world={} armed={:?} truncated={}",
                v.rows.iter().map(|r| (r.guid, r.entry, (r.x * 10.0).round() / 10.0, (r.y * 10.0).round() / 10.0, (r.z * 10.0).round() / 10.0, (r.o * 100.0).round() / 100.0, (r.scale * 100.0).round() / 100.0)).collect::<Vec<_>>(),
                v.selected,
                v.pending,
                world_objs.objects.len(),
                v.armed,
                v.truncated
            );
            info!(
                "EME script: OBJ primary={:?} notice={:?} {:?}",
                v.primary.as_ref().map(|r| (r.guid, r.x, r.y, r.z, r.o, r.scale)),
                notice.key,
                notice.args
            );
        }
        "screenshot" => {
            let path = step
                .get("path")
                .and_then(Json::as_str)
                .unwrap_or("eme_script_shot.png")
                .to_owned();
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path.clone()));
            info!("EME script: screenshot -> {path}");
            // Give the async save a few frames before the next step changes the scene.
            done = t >= 0.2;
            if !done {
                // Only spawn once.
                let i = script.idx;
                script.steps[i]["op"] = Json::from("wait");
                script.steps[i]["s"] = Json::from(0.2);
            }
        }
        "log" => {
            info!(
                "EME script: {} — {}",
                step.get("msg").and_then(Json::as_str).unwrap_or(""),
                status()
            );
        }
        "exit" => {
            exit.write(AppExit::Success);
        }
        other => warn!("EME script: unknown op {other:?} — skipped"),
    }
    if done {
        if op != "log" {
            let undo = host.dispatcher.undo_stack();
            info!(
                "EME script: step {} {op} done at {t:.2} s — {}; undo entries {} (can undo {}, \
                 redo {}); ring {} pts r={:.1} chunks {}/{} active",
                script.idx,
                status(),
                undo.entries().len(),
                undo.can_undo(),
                undo.can_redo(),
                preview.ring.len(),
                preview.radius,
                preview.active_chunks,
                preview.chunks,
            );
        }
        script.idx += 1;
        script.t = 0.0;
        script.started = false;
        if script.done() {
            info!("EME script: finished");
        }
    }
}

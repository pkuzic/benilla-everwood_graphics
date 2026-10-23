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
//! Other ops: `tool {id}`, `option {key, value}`, `redo`, `log {msg}`, `wait_active`. Every wait
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
            "dig" => 0,
            "build" => 1,
            "smooth" => 2,
            "flatten" => 3,
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
        format!(
            "map {map}, journal {} edit(s) ({} pending), {} active chunk(s), settled {}, hit {:?}",
            api.journal(map).len(),
            api.pending_local(map).len(),
            api.active_chunks(),
            api.settled(),
            aim.hit.map(|h| [h.pos[0].round(), h.pos[1].round(), (h.pos[2] * 10.0).round() / 10.0]),
        )
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
            cursor.virtual_norm = Some(Vec2::new(
                num(&step, "x").unwrap_or(0.5),
                num(&step, "y").unwrap_or(0.5),
            ));
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
                let value = if k == "mode" { mode_value(v) } else { option_value(v) };
                if let Some(value) = value {
                    requests.write(EmeRequest::SetOption {
                        key: k.to_owned(),
                        value,
                    });
                }
            }
        }
        "stroke" => {
            let from = Vec2::new(num(&step, "x").unwrap_or(0.5), num(&step, "y").unwrap_or(0.5));
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
            let dur = num(&step, "s").unwrap_or(0.5).max(0.05);
            if first {
                if let Some(m) = step.get("mode").and_then(mode_value) {
                    requests.write(EmeRequest::SetOption {
                        key: "mode".into(),
                        value: m,
                    });
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
            } else if t < dur {
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
        "redo" => {
            requests.write(EmeRequest::Redo);
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

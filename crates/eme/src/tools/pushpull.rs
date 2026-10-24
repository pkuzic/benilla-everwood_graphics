//! `pushpull` (MONKEY, CONTRACT v3.6, owner 2026-10-24 "Planet-Zoo-style terraforming"): grow
//! material SIDEWAYS out of a surface (push) or carve INTO it (pull) — overhangs out of a cliff,
//! caves into a hill.
//!
//! Every edit is a HARD dig / build of the chosen shape whose centre is offset from the aimed point
//! along the surface normal (default) or toward the camera (the `direction` option): push centres
//! the brush `radius − step` BEHIND the surface, so it sticks out by `step`; pull centres it
//! `radius − step` IN FRONT, so it bites `step` deep. `step = radius × strength × 0.5`
//! (≥ 0.25 yd). Holding the button keeps going: the cursor ray hits the surface just grown /
//! carved, so the next edit starts from there — an overhang grows outward, a cave inward, ~8
//! edits/s (the core stroke builder; one stroke = one Ctrl+Z).
//!
//! `falloff` ≠ hard edge alternates every CSG edit with a SOFT smooth (mode 2, v3.3) over the same
//! spot, which rounds the new rim — the soft lower / raise maths are vertical only, so a sideways
//! falloff is done by smoothing.
//!
//! No wire change: these are ordinary mode 0 / 1 (+ 2) EDIT3 records, so the server offers this tool
//! whenever it offers `sculpt` ([`EmeTool::server_tool`]).

use eme_core::command::{EmeCommand, EmeCommandSink, EmeStrokeMark};
use eme_core::stroke::{EmeBrushSample, EmeStrokeBuilder, EmeStrokeConfig, EmeStrokeIds};
use eme_core::tool::{
    EmeHit, EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview,
    EmeTool, EmeToolId,
};
use eme_core::transport::mode;

use super::sculpt::{shape_field, shape_value, strength_u8, SCULPT};

/// The tool id.
pub const PUSHPULL: EmeToolId = EmeToolId("pushpull");

/// Choice labels of the `mode` option: 0 push (grow out), 1 pull (carve in).
pub const MODE_LABELS: [&str; 2] = ["MONKEY_EME_PUSHPULL_MODE_PUSH", "MONKEY_EME_PUSHPULL_MODE_PULL"];
/// Choice labels of the `direction` option: 0 surface normal, 1 view direction.
pub const DIRECTION_LABELS: [&str; 2] =
    ["MONKEY_EME_PUSHPULL_DIR_NORMAL", "MONKEY_EME_PUSHPULL_DIR_VIEW"];
/// Choice labels of the `falloff` option: 0 hard edge, 1 smooth, 2 linear (the softening pass).
pub const FALLOFF_LABELS: [&str; 3] = [
    "MONKEY_EME_PUSHPULL_FALLOFF_HARD",
    "MONKEY_EME_SCULPT_FALLOFF_SMOOTH",
    "MONKEY_EME_SCULPT_FALLOFF_LINEAR",
];

/// The growth per edit at the centre, yards: `radius × strength% / 100 × 0.5`, at least 0.25 yd,
/// at most the radius.
pub fn step_yd(radius: f32, strength_pct: f32) -> f32 {
    (radius * strength_pct.clamp(1.0, 100.0) / 100.0 * 0.5).clamp(0.25, radius.max(0.25))
}

/// The edit centre for a hit at `hit` with outward direction `dir` (unit): push = behind the
/// surface by `radius − step` (sticks out `step`), pull = in front by `radius − step` (bites `step`).
pub fn edit_centre(hit: [f32; 3], dir: [f32; 3], radius: f32, step: f32, pull: bool) -> [f32; 3] {
    let k = if pull { radius - step } else { step - radius };
    [hit[0] + dir[0] * k, hit[1] + dir[1] * k, hit[2] + dir[2] * k]
}

fn unit(v: [f32; 3]) -> Option<[f32; 3]> {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    (l > 1.0e-4 && l.is_finite()).then(|| [v[0] / l, v[1] / l, v[2] / l])
}

/// The push / pull tool.
#[derive(Debug)]
pub struct EmePushPullTool {
    builder: EmeStrokeBuilder,
    /// false push (grow), true pull (carve).
    pub pull: bool,
    /// Radius, yards.
    pub radius: f32,
    /// Strength, percent (the growth per edit, [`step_yd`]).
    pub strength: f32,
    /// `mode::SHAPE_*`.
    pub shape: u8,
    /// 0 hard edge, 1 smooth, 2 linear (alternating soft-smooth pass).
    pub falloff: u8,
    /// Offset along the view direction instead of the surface normal.
    pub along_view: bool,
    /// The last CSG centre sent, and whether the softening pass is due next.
    last_centre: Option<[f32; 3]>,
    soften_next: bool,
}

impl EmePushPullTool {
    /// A push / pull tool numbering its strokes from the shared `ids` counter.
    pub fn new(ids: EmeStrokeIds) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x5050, |d| (d.subsec_micros() as u16).rotate_left(7) ^ 0x5050);
        let config = EmeStrokeConfig {
            // The centre advances along the normal (also vertically) and a held brush keeps
            // growing: no ground-plane rule, and a generous depth cap.
            plane_moves: false,
            hard_depth_radii: 8.0,
            ..Default::default()
        };
        Self {
            builder: EmeStrokeBuilder::new(config)
                .with_start(ids.peek(), seed)
                .with_ids(ids),
            pull: false,
            radius: 3.0,
            strength: 30.0,
            shape: mode::SHAPE_SPHERE,
            falloff: 0,
            along_view: false,
            last_centre: None,
            soften_next: false,
        }
    }

    /// The stroke being held, if any.
    pub fn active_stroke(&self) -> Option<u16> {
        self.builder.active_stroke()
    }

    /// The outward direction for this hit (surface normal, or toward the camera).
    fn direction(&self, ctx: &EmeInputCtx, hit: &EmeHit) -> [f32; 3] {
        let v = if self.along_view {
            ctx.view_dir.map(|d| [-d[0], -d[1], -d[2]])
        } else {
            None
        };
        v.and_then(unit)
            .or_else(|| unit(hit.normal))
            .unwrap_or([0.0, 0.0, 1.0])
    }

    /// The CSG sample for this hit.
    fn csg(&self, ctx: &EmeInputCtx, hit: &EmeHit) -> EmeBrushSample {
        let dir = self.direction(ctx, hit);
        let step = step_yd(self.radius, self.strength);
        EmeBrushSample {
            map: ctx.map,
            pos: edit_centre(hit.pos, dir, self.radius, step, self.pull),
            radius: self.radius,
            mode: if self.pull { mode::DIG } else { mode::BUILD },
            strength: 255,
            param: mode::with_shape(0, self.shape),
        }
    }

    /// The next sample: the CSG edit, or (falloff on, after a CSG edit) the softening smooth.
    fn sample(&self, ctx: &EmeInputCtx, hit: &EmeHit) -> EmeBrushSample {
        match (self.falloff, self.soften_next, self.last_centre) {
            (1 | 2, true, Some(c)) => EmeBrushSample {
                map: ctx.map,
                pos: c,
                radius: (self.radius * 1.25).clamp(0.5, 16.0),
                mode: mode::SMOOTH,
                strength: strength_u8(60.0),
                param: mode::soft_param(
                    if self.falloff == 2 {
                        mode::FALLOFF_LINEAR
                    } else {
                        mode::FALLOFF_SMOOTH
                    },
                    0,
                ),
            },
            _ => self.csg(ctx, hit),
        }
    }

    fn sent(&mut self, e: &eme_core::EmeVoxelEdit) {
        if e.mode == mode::SMOOTH {
            self.soften_next = false;
        } else {
            self.last_centre = Some(e.pos);
            self.soften_next = self.falloff != 0;
        }
    }

    fn emit(out: &mut EmeCommandSink, stroke: u16, ops: Vec<eme_core::EmeVoxelEdit>, mark: EmeStrokeMark) {
        if ops.is_empty() && mark == EmeStrokeMark::Continue {
            return;
        }
        out.push(EmeCommand::VoxelStroke { stroke, ops, mark });
    }
}

impl EmeTool for EmePushPullTool {
    fn id(&self) -> EmeToolId {
        PUSHPULL
    }

    fn server_tool(&self) -> EmeToolId {
        SCULPT
    }

    fn min_rank(&self) -> u8 {
        3
    }

    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        let now = ctx.now_ms;
        if ctx.modifiers.alt && ctx.wheel != 0.0 {
            self.radius = (self.radius + 0.5 * ctx.wheel.signum()).clamp(0.5, 16.0);
        }
        let held = self.builder.active_stroke().is_some();
        if let Some(hit) = ctx.hit {
            if ctx.primary.pressed && !ctx.pointer_over_ui {
                self.last_centre = None;
                self.soften_next = false;
                let s = self.csg(ctx, &hit);
                let (stroke, e) = self.builder.begin(now, s);
                if let Some(e) = &e {
                    self.sent(e);
                }
                Self::emit(out, stroke, e.into_iter().collect(), EmeStrokeMark::Continue);
                return;
            }
            if ctx.primary.held && held {
                let s = self.sample(ctx, &hit);
                if let Some(e) = self.builder.update(now, s) {
                    self.sent(&e);
                    Self::emit(out, e.stroke, vec![e], EmeStrokeMark::Continue);
                }
                return;
            }
        }
        if held && (ctx.primary.released || !ctx.primary.held) {
            // The release adds nothing new (the last edit is where the brush was).
            if let Some(end) = self.builder.end(now, None) {
                Self::emit(
                    out,
                    end.stroke,
                    end.edit.into_iter().collect(),
                    EmeStrokeMark::End {
                        trailing: end.trailing,
                    },
                );
            }
            self.last_centre = None;
            self.soften_next = false;
            return;
        }
        if let Some(e) = self.builder.tick(now) {
            Self::emit(out, e.stroke, vec![e], EmeStrokeMark::Continue);
        }
    }

    fn options_ui_model(&self) -> EmeOptions {
        let p = PUSHPULL.locale_prefix();
        let choice = |key: &'static str, labels: &[&str], v: usize| EmeOptionField {
            key,
            label: format!("{p}{}", key.to_ascii_uppercase()),
            kind: EmeOptionKind::Choice(labels.iter().map(|s| s.to_string()).collect()),
            value: EmeOptionValue::Choice(v),
            disabled: false,
        };
        let slider = |key: &'static str, min: f32, max: f32, step: f32, v: f32| EmeOptionField {
            key,
            label: format!("{p}{}", key.to_ascii_uppercase()),
            kind: EmeOptionKind::Slider { min, max, step },
            value: EmeOptionValue::Number(v),
            disabled: false,
        };
        vec![
            choice("mode", &MODE_LABELS, self.pull as usize),
            slider("radius", 0.5, 16.0, 0.5, self.radius),
            shape_field(format!("{p}SHAPE"), self.shape, false),
            slider("strength", 1.0, 100.0, 1.0, self.strength),
            choice("falloff", &FALLOFF_LABELS, self.falloff as usize),
            choice("direction", &DIRECTION_LABELS, self.along_view as usize),
        ]
    }

    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        let idx = |v: &EmeOptionValue, max: usize| match *v {
            EmeOptionValue::Choice(i) if i <= max => Some(i),
            EmeOptionValue::Number(n) if n >= 0.0 && n <= max as f32 => Some(n as usize),
            EmeOptionValue::Bool(b) => Some(b as usize),
            _ => None,
        };
        match key {
            "mode" => {
                if let Some(i) = idx(&value, 1) {
                    self.pull = i == 1;
                } else if let EmeOptionValue::Text(t) = &value {
                    self.pull = matches!(t.as_str(), "pull" | "dig");
                }
            }
            "radius" => {
                if let EmeOptionValue::Number(r) = value {
                    self.radius = r.clamp(0.5, 16.0);
                }
            }
            "strength" => {
                if let EmeOptionValue::Number(s) = value {
                    self.strength = s.clamp(1.0, 100.0).round();
                }
            }
            "shape" => {
                if let Some(s) = shape_value(&value) {
                    self.shape = s;
                }
            }
            "falloff" => {
                if let Some(i) = idx(&value, 2) {
                    self.falloff = i as u8;
                }
            }
            "direction" => {
                if let Some(i) = idx(&value, 1) {
                    self.along_view = i == 1;
                } else if let EmeOptionValue::Text(t) = &value {
                    self.along_view = t == "view";
                }
            }
            _ => {}
        }
    }

    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview> {
        let hit = ctx.hit?;
        let c = self.csg(ctx, &hit);
        Some(EmePreview::Path {
            points: vec![hit.pos, c.pos],
            radius: self.radius,
            mode: c.mode,
        })
    }

    fn on_deactivate(&mut self, out: &mut EmeCommandSink) {
        if let Some(stroke) = self.builder.active_stroke() {
            self.builder.cancel();
            out.push(EmeCommand::VoxelStroke {
                stroke,
                ops: Vec::new(),
                mark: EmeStrokeMark::End { trailing: false },
            });
        }
        self.last_centre = None;
        self.soften_next = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::perm::{EmePermissions, EmeRank};
    use eme_core::tool::EmeButton;

    fn ctx(now: u64, pos: [f32; 3], normal: [f32; 3], b: EmeButton) -> EmeInputCtx {
        EmeInputCtx {
            hit: Some(EmeHit {
                pos,
                normal,
                object: None,
            }),
            primary: b,
            view_dir: Some([0.0, 1.0, 0.0]),
            ..EmeInputCtx::idle(now, 0, EmePermissions::from_rank(EmeRank::EDIT))
        }
    }

    fn edits(sink: &mut EmeCommandSink) -> Vec<eme_core::EmeVoxelEdit> {
        sink.drain()
            .into_iter()
            .flat_map(|i| match i.command {
                EmeCommand::VoxelStroke { ops, .. } => ops,
                _ => Vec::new(),
            })
            .collect()
    }

    const PRESS: EmeButton = EmeButton { pressed: true, held: true, released: false };
    const HOLD: EmeButton = EmeButton { pressed: false, held: true, released: false };

    #[test]
    fn centre_offsets_along_the_normal() {
        let n = [1.0, 0.0, 0.0];
        // Push: behind the surface by r - step (sticks out `step`).
        assert_eq!(edit_centre([10.0, 0.0, 5.0], n, 3.0, 0.5, false), [7.5, 0.0, 5.0]);
        // Pull: in front by r - step (bites `step`).
        assert_eq!(edit_centre([10.0, 0.0, 5.0], n, 3.0, 0.5, true), [12.5, 0.0, 5.0]);
        assert_eq!(step_yd(3.0, 30.0), 0.45);
        assert_eq!(step_yd(1.0, 1.0), 0.25);
    }

    /// Holding on a cliff face (normal +x) where every edit's hit moves outward by the growth: a
    /// push grows outward edit after edit (an overhang), all one stroke of hard builds.
    #[test]
    fn held_push_on_a_cliff_grows_outward_in_one_stroke() {
        let mut t = EmePushPullTool::new(EmeStrokeIds::starting_at(4));
        t.set_option("shape", EmeOptionValue::Choice(1));
        let mut sink = EmeCommandSink::new();
        let n = [1.0, 0.0, 0.0];
        let mut x = 10.0;
        t.on_input(&ctx(0, [x, 0.0, 50.0], n, PRESS), &mut sink);
        let mut all = edits(&mut sink);
        for f in 1..=40u64 {
            if let Some(last) = all.last() {
                // The cursor now hits the grown surface.
                x = last.pos[0] + last.radius;
            }
            t.on_input(&ctx(f * 25, [x, 0.0, 50.0], n, HOLD), &mut sink);
            all.extend(edits(&mut sink));
        }
        assert!((6..=10).contains(&all.len()), "{} edits in 1 s", all.len());
        assert!(all.iter().all(|e| e.stroke == 4 && e.mode == mode::BUILD && e.param == 0x0200));
        assert!(all.windows(2).all(|w| w[1].pos[0] > w[0].pos[0]), "grows outward");
        assert!((all[0].pos[2] - 50.0).abs() < 1e-4, "sideways, not up");
    }

    #[test]
    fn pull_along_the_view_and_softening_pass() {
        let mut t = EmePushPullTool::new(EmeStrokeIds::starting_at(1));
        t.set_option("mode", EmeOptionValue::Choice(1));
        t.set_option("direction", EmeOptionValue::Choice(1));
        t.set_option("falloff", EmeOptionValue::Choice(1));
        let mut sink = EmeCommandSink::new();
        // The camera looks along +y; the surface normal is ignored.
        t.on_input(&ctx(0, [0.0, 10.0, 5.0], [1.0, 0.0, 0.0], PRESS), &mut sink);
        let first = edits(&mut sink)[0];
        assert_eq!(first.mode, mode::DIG);
        // Pull centre is in front of the surface, toward the camera (-y).
        assert!(first.pos[1] < 10.0 && (first.pos[0]).abs() < 1e-4);
        t.on_input(&ctx(200, [0.0, 10.4, 5.0], [1.0, 0.0, 0.0], HOLD), &mut sink);
        let second = edits(&mut sink)[0];
        assert_eq!(second.mode, mode::SMOOTH, "the softening pass follows a CSG edit");
        assert!(mode::is_soft(second.mode, second.param));
        assert_eq!(second.pos, first.pos);
        t.on_input(&ctx(400, [0.0, 10.4, 5.0], [1.0, 0.0, 0.0], HOLD), &mut sink);
        assert_eq!(edits(&mut sink)[0].mode, mode::DIG);
        let keys: Vec<_> = t.options_ui_model().iter().map(|f| f.key).collect();
        assert_eq!(keys, ["mode", "radius", "shape", "strength", "falloff", "direction"]);
        assert_eq!(t.server_tool(), SCULPT);
    }
}

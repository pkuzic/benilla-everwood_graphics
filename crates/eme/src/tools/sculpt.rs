//! `sculpt` (T1 + T2): the terrain brush — lower / raise / smooth on the real ground or an
//! existing voxel surface.
//!
//! MONKEY (EME, owner 2026-10-23 "tools way too strong, should feel like Noggit"): by default the
//! brush is SOFT (CONTRACT v3.3, `param` bit 15): lower / raise move the ground by up to
//! `strength` × 0.5 yd per edit at the centre with a radial falloff (smooth / linear / flat, inner
//! `hardness`), smooth blends toward the local average by `strength` × falloff. The uplink cadence is
//! 8 edits/s while held (still or moving), so a held brush GROWS a hill gradually: the default 20 %
//! is ~0.8 yd/s at the centre. `hard` = the old CSG dig / build spheres (+ the uniform v2 smooth). The same machinery with the mode locked to flatten is the separate
//! `flatten` tool ([`super::flatten`]). LMB held = one stroke; the core [`EmeStrokeBuilder`] coalesces the held
//! brush into ≤ 8 EDIT3/s (move ≥ r/2 or 125 ms), every edit tagged with the stroke id, so one
//! Ctrl+Z undoes the whole stroke.
//!
//! Flatten (mode 3, the `flatten` tool) samples its plane at the press: every edit of the stroke carries
//! `param = (plane − z) · 100` (cm, i16), so the whole stroke levels toward the clicked height.
//!
//! MONKEY (EME, CONTRACT v3.6): `shape` = sphere / cube / vertical cylinder (lower / raise, hard
//! and soft: `param` bits 9-10; smooth is always a sphere).
//!
//! Options: `mode` (choice lower / raise / smooth; sculpt only), `radius` (0.5–16 yd), `shape`, `strength`
//! (1–100 %), `falloff` (smooth / linear / flat) + `hardness` (0–100 %) + `hard` (sculpt only),
//! `offset` (flatten plane offset, yards; flatten only).

use eme_core::command::{EmeCommand, EmeCommandSink, EmeStrokeMark};
use eme_core::stroke::{EmeBrushSample, EmeStrokeBuilder, EmeStrokeConfig, EmeStrokeIds};
use eme_core::tool::{
    EmeHit, EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview,
    EmeTool, EmeToolId,
};
use eme_core::transport::mode;

/// The tool id (also the server's name for it in `caps.tools`).
pub const SCULPT: EmeToolId = EmeToolId("sculpt");

/// Choice labels of the `mode` option (locale keys), in mode order 0..=2 (flatten is its own tool).
pub const MODE_LABELS: [&str; 3] = [
    "MONKEY_EME_SCULPT_MODE_DIG",
    "MONKEY_EME_SCULPT_MODE_BUILD",
    "MONKEY_EME_SCULPT_MODE_SMOOTH",
];

/// Choice labels of the `falloff` option, in `mode::FALLOFF_*` order.
pub const FALLOFF_LABELS: [&str; 3] = [
    "MONKEY_EME_SCULPT_FALLOFF_SMOOTH",
    "MONKEY_EME_SCULPT_FALLOFF_LINEAR",
    "MONKEY_EME_SCULPT_FALLOFF_FLAT",
];

/// MONKEY (EME, CONTRACT v3.6): choice labels of every tool's `shape` option, in
/// `mode::SHAPE_*` order (sphere, cube, vertical cylinder).
pub const SHAPE_LABELS: [&str; 3] = [
    "MONKEY_EME_SHAPE_SPHERE",
    "MONKEY_EME_SHAPE_CUBE",
    "MONKEY_EME_SHAPE_CYLINDER",
];

/// The `shape` option field of a tool (`label` = its locale key).
pub fn shape_field(label: String, shape: u8, disabled: bool) -> EmeOptionField {
    EmeOptionField {
        key: "shape",
        label,
        kind: EmeOptionKind::Choice(SHAPE_LABELS.iter().map(|s| s.to_string()).collect()),
        value: EmeOptionValue::Choice(shape as usize),
        disabled,
    }
}

/// A `shape` option value (choice index or a script's number) → `mode::SHAPE_*`.
pub fn shape_value(value: &EmeOptionValue) -> Option<u8> {
    match *value {
        EmeOptionValue::Choice(i) if i <= 2 => Some(i as u8),
        EmeOptionValue::Number(n) if (0.0..=2.0).contains(&n) => Some(n as u8),
        EmeOptionValue::Text(ref s) => match s.as_str() {
            "sphere" => Some(mode::SHAPE_SPHERE),
            "cube" => Some(mode::SHAPE_CUBE),
            "cylinder" => Some(mode::SHAPE_CYLINDER),
            _ => None,
        },
        _ => None,
    }
}

/// Default strength, percent, of the sculpt brush (soft lower / raise ~0.1 yd per edit) …
pub const DEFAULT_STRENGTH: f32 = 20.0;
/// … and of the flatten tool (a quarter of the way to the plane per edit).
pub const DEFAULT_FLATTEN_STRENGTH: f32 = 25.0;

/// Percent (1–100) → the wire strength /255.
pub fn strength_u8(pct: f32) -> u8 {
    (pct.clamp(1.0, 100.0) * 2.55).round().clamp(1.0, 255.0) as u8
}

/// The terrain brush.
#[derive(Debug)]
pub struct EmeSculptTool {
    builder: EmeStrokeBuilder,
    /// The tool's id (`sculpt`, or `flatten` for the locked variant).
    tool: EmeToolId,
    /// The mode is fixed (the `flatten` tool): no mode option.
    locked: bool,
    /// Voxel mode 0..=3.
    pub mode: u8,
    /// Radius, yards.
    pub radius: f32,
    /// Strength, percent (1–100).
    pub strength: f32,
    /// Soft falloff curve (`mode::FALLOFF_*`).
    pub falloff: u8,
    /// Soft inner radius, percent of the radius at full strength (0–100).
    pub hardness: f32,
    /// The old hard brush: CSG dig / build spheres, uniform smooth (`param` 0).
    pub hard: bool,
    /// MONKEY (EME, CONTRACT v3.6): brush shape of lower / raise (`mode::SHAPE_*`).
    pub shape: u8,
    /// Flatten plane offset from the sampled height, yards.
    pub offset: f32,
    /// The flatten plane of the held stroke (sampled at the press).
    plane: Option<f32>,
    /// A wheel step with Alt resizes the brush.
    wheel_resizes: bool,
}

impl EmeSculptTool {
    /// A sculpt tool numbering its strokes from the shared `ids` counter. Nonces start at a
    /// time-seeded value, so they do not collide with an earlier session's pending predictions.
    pub fn new(ids: EmeStrokeIds) -> Self {
        Self::with_kind(ids, SCULPT, mode::DIG, false)
    }

    /// The brush with its mode locked to flatten (see [`super::flatten::EmeFlattenTool`]).
    pub(crate) fn new_flatten(ids: EmeStrokeIds, id: EmeToolId) -> Self {
        Self::with_kind(ids, id, mode::FLATTEN, true)
    }

    fn with_kind(ids: EmeStrokeIds, tool: EmeToolId, mode: u8, locked: bool) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x4d45, |d| (d.subsec_micros() as u16).rotate_left(5) ^ 0x4d45);
        Self {
            builder: EmeStrokeBuilder::new(EmeStrokeConfig::default())
                .with_start(ids.peek(), seed)
                .with_ids(ids),
            tool,
            locked,
            mode,
            radius: 4.0,
            strength: if mode == mode::FLATTEN {
                DEFAULT_FLATTEN_STRENGTH
            } else {
                DEFAULT_STRENGTH
            },
            falloff: mode::FALLOFF_SMOOTH,
            hardness: 0.0,
            hard: false,
            shape: mode::SHAPE_SPHERE,
            offset: 0.0,
            plane: None,
            wheel_resizes: true,
        }
    }

    /// The edit strength /255 of the current mode (the hard dig / build ignore it; the hard
    /// smooth stays at most half, as before).
    fn effective_strength(&self) -> u8 {
        let s = strength_u8(self.strength);
        match self.mode {
            mode::DIG | mode::BUILD if self.hard => 255,
            mode::SMOOTH if self.hard => s.min(128),
            _ => s,
        }
    }

    /// Whether the current brush is a soft one (CONTRACT v3.3).
    fn soft(&self) -> bool {
        !self.hard && matches!(self.mode, mode::DIG | mode::BUILD | mode::SMOOTH)
    }

    fn sample(&self, ctx: &EmeInputCtx, hit: &EmeHit) -> EmeBrushSample {
        let param = if self.mode == mode::FLATTEN {
            let plane = self.plane.unwrap_or(hit.pos[2] + self.offset);
            (((plane - hit.pos[2]) * 100.0).round().clamp(-32768.0, 32767.0) as i16) as u16
        } else if self.soft() {
            mode::soft_param(self.falloff, self.hardness.clamp(0.0, 100.0).round() as u8)
        } else {
            0
        };
        // MONKEY (EME, CONTRACT v3.6): the shape rides on lower / raise, hard and soft.
        let param = if matches!(self.mode, mode::DIG | mode::BUILD) {
            mode::with_shape(param, self.shape)
        } else {
            param
        };
        EmeBrushSample {
            map: ctx.map,
            pos: hit.pos,
            radius: self.radius,
            mode: self.mode,
            strength: self.effective_strength(),
            param,
        }
    }

    fn emit(out: &mut EmeCommandSink, stroke: u16, ops: Vec<eme_core::EmeVoxelEdit>, mark: EmeStrokeMark) {
        if ops.is_empty() && mark == EmeStrokeMark::Continue {
            return;
        }
        out.push(EmeCommand::VoxelStroke { stroke, ops, mark });
    }

    /// The stroke being held, if any.
    pub fn active_stroke(&self) -> Option<u16> {
        self.builder.active_stroke()
    }
}

impl EmeTool for EmeSculptTool {
    fn id(&self) -> EmeToolId {
        self.tool
    }

    fn min_rank(&self) -> u8 {
        3
    }

    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        let now = ctx.now_ms;
        if self.wheel_resizes && ctx.modifiers.alt && ctx.wheel != 0.0 {
            self.radius = (self.radius + 0.5 * ctx.wheel.signum()).clamp(0.5, 16.0);
        }
        let held = self.builder.active_stroke().is_some();
        if let Some(hit) = ctx.hit {
            if ctx.primary.pressed && !ctx.pointer_over_ui {
                self.plane = Some(hit.pos[2] + self.offset);
                let s = self.sample(ctx, &hit);
                let (stroke, e) = self.builder.begin(now, s);
                Self::emit(out, stroke, e.into_iter().collect(), EmeStrokeMark::Continue);
                return;
            }
            if ctx.primary.held && held {
                let s = self.sample(ctx, &hit);
                if let Some(e) = self.builder.update(now, s) {
                    Self::emit(out, e.stroke, vec![e], EmeStrokeMark::Continue);
                }
                return;
            }
        }
        if held && (ctx.primary.released || !ctx.primary.held) {
            let sample = ctx.hit.map(|h| self.sample(ctx, &h));
            if let Some(end) = self.builder.end(now, sample) {
                Self::emit(
                    out,
                    end.stroke,
                    end.edit.into_iter().collect(),
                    EmeStrokeMark::End {
                        trailing: end.trailing,
                    },
                );
            }
            self.plane = None;
            return;
        }
        if let Some(e) = self.builder.tick(now) {
            Self::emit(out, e.stroke, vec![e], EmeStrokeMark::Continue);
        }
    }

    fn options_ui_model(&self) -> EmeOptions {
        let p = self.tool.locale_prefix();
        let mut fields = vec![
            EmeOptionField {
                key: "mode",
                label: format!("{p}MODE"),
                kind: EmeOptionKind::Choice(MODE_LABELS.iter().map(|s| s.to_string()).collect()),
                value: EmeOptionValue::Choice(self.mode as usize),
                disabled: false,
            },
            EmeOptionField {
                key: "radius",
                label: format!("{p}RADIUS"),
                kind: EmeOptionKind::Slider {
                    min: 0.5,
                    max: 16.0,
                    step: 0.5,
                },
                value: EmeOptionValue::Number(self.radius),
                disabled: false,
            },
            shape_field(
                format!("{p}SHAPE"),
                self.shape,
                !matches!(self.mode, mode::DIG | mode::BUILD),
            ),
            EmeOptionField {
                key: "strength",
                label: format!("{p}STRENGTH"),
                kind: EmeOptionKind::Slider {
                    min: 1.0,
                    max: 100.0,
                    step: 1.0,
                },
                value: EmeOptionValue::Number(self.strength),
                disabled: self.hard && matches!(self.mode, mode::DIG | mode::BUILD),
            },
            EmeOptionField {
                key: "falloff",
                label: format!("{p}FALLOFF"),
                kind: EmeOptionKind::Choice(FALLOFF_LABELS.iter().map(|s| s.to_string()).collect()),
                value: EmeOptionValue::Choice(self.falloff as usize),
                disabled: !self.soft(),
            },
            EmeOptionField {
                key: "hardness",
                label: format!("{p}HARDNESS"),
                kind: EmeOptionKind::Slider {
                    min: 0.0,
                    max: 100.0,
                    step: 5.0,
                },
                value: EmeOptionValue::Number(self.hardness),
                disabled: !self.soft(),
            },
            EmeOptionField {
                key: "hard",
                label: format!("{p}HARD"),
                kind: EmeOptionKind::Toggle,
                value: EmeOptionValue::Bool(self.hard),
                disabled: false,
            },
            EmeOptionField {
                key: "offset",
                label: format!("{p}OFFSET"),
                kind: EmeOptionKind::Slider {
                    min: -16.0,
                    max: 16.0,
                    step: 0.25,
                },
                value: EmeOptionValue::Number(self.offset),
                disabled: self.mode != mode::FLATTEN,
            },
        ];
        if self.locked {
            fields.retain(|f| !matches!(f.key, "mode" | "falloff" | "hardness" | "hard" | "shape"));
        } else {
            fields.retain(|f| f.key != "offset");
        }
        fields
    }

    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        match (key, value) {
            ("mode", _) if self.locked => {}
            ("mode", EmeOptionValue::Choice(i)) if i <= 2 => self.mode = i as u8,
            ("mode", EmeOptionValue::Number(n)) if (0.0..=2.0).contains(&n) => self.mode = n as u8,
            ("radius", EmeOptionValue::Number(r)) => self.radius = r.clamp(0.5, 16.0),
            ("strength", EmeOptionValue::Number(s)) => self.strength = s.clamp(1.0, 100.0).round(),
            ("falloff", _) | ("hardness", _) | ("hard", _) if self.locked => {}
            ("falloff", EmeOptionValue::Choice(i)) if i <= 2 => self.falloff = i as u8,
            ("falloff", EmeOptionValue::Number(n)) if (0.0..=2.0).contains(&n) => {
                self.falloff = n as u8
            }
            ("hardness", EmeOptionValue::Number(h)) => self.hardness = h.clamp(0.0, 100.0),
            ("hard", EmeOptionValue::Bool(b)) => self.hard = b,
            ("shape", _) if self.locked => {}
            ("shape", v) => {
                if let Some(s) = shape_value(&v) {
                    self.shape = s;
                }
            }
            ("offset", EmeOptionValue::Number(o)) => self.offset = o.clamp(-16.0, 16.0),
            _ => {}
        }
    }

    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview> {
        let hit = ctx.hit?;
        if self.mode == mode::FLATTEN {
            let z = self.plane.unwrap_or(hit.pos[2] + self.offset);
            return Some(EmePreview::Plane {
                center: [hit.pos[0], hit.pos[1], z],
                radius: self.radius,
            });
        }
        Some(EmePreview::Ring {
            center: hit.pos,
            radius: self.radius,
            // The soft brush's falloff band (outside the inner `hardness` disc); hard = none.
            falloff: if self.soft() {
                1.0 - self.hardness.clamp(0.0, 100.0) / 100.0
            } else {
                0.0
            },
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
        self.plane = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::perm::{EmePermissions, EmeRank};
    use eme_core::tool::EmeButton;

    fn ctx(now: u64, x: f32, primary: EmeButton) -> EmeInputCtx {
        EmeInputCtx {
            hit: Some(EmeHit {
                pos: [x, 0.0, 10.0],
                normal: [0.0, 0.0, 1.0],
                object: None,
            }),
            primary,
            ..EmeInputCtx::idle(now, 0, EmePermissions::from_rank(EmeRank::EDIT))
        }
    }

    fn edits(sink: &mut EmeCommandSink) -> Vec<(u16, eme_core::EmeVoxelEdit, EmeStrokeMark)> {
        sink.drain()
            .into_iter()
            .flat_map(|i| match i.command {
                EmeCommand::VoxelStroke { stroke, ops, mark } => {
                    ops.into_iter().map(|o| (stroke, o, mark)).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn held_brush_is_one_stroke_rate_limited_and_closed_on_release() {
        let mut t = EmeSculptTool::new(EmeStrokeIds::starting_at(7));
        let mut sink = EmeCommandSink::new();
        let press = EmeButton {
            pressed: true,
            held: true,
            released: false,
        };
        let hold = EmeButton {
            pressed: false,
            held: true,
            released: false,
        };
        t.on_input(&ctx(0, 0.0, press), &mut sink);
        // Moving 3 yd every 16 ms: the rate limit (125 ms) lets one edit through per 8 frames.
        for f in 1..=16u64 {
            t.on_input(&ctx(f * 16, f as f32 * 3.0, hold), &mut sink);
        }
        let release = EmeButton {
            pressed: false,
            held: false,
            released: true,
        };
        t.on_input(&ctx(17 * 16 + 200, 60.0, release), &mut sink);
        let all = edits(&mut sink);
        assert!(all.iter().all(|(s, e, _)| *s == 7 && e.stroke == 7 && e.mode == mode::DIG));
        // The default brush is SOFT (v3.3): param bit 15, the gentle default strength.
        assert!(all.iter().all(|(_, e, _)| mode::is_soft(e.mode, e.param) && e.strength == 51));
        assert!((3..=5).contains(&all.len()), "{} edits", all.len());
        // Nonces strictly increase.
        assert!(all.windows(2).all(|w| w[1].1.nonce == w[0].1.nonce.wrapping_add(1)));
        assert!(t.active_stroke().is_none());
        // The next press opens stroke 8.
        t.on_input(&ctx(2000, 0.0, press), &mut sink);
        assert_eq!(t.active_stroke(), Some(8));
    }

    #[test]
    fn flatten_samples_its_plane_at_the_press() {
        let mut t = EmeSculptTool::new_flatten(EmeStrokeIds::default(), super::super::flatten::FLATTEN);
        t.set_option("offset", EmeOptionValue::Number(0.5));
        let mut sink = EmeCommandSink::new();
        let press = EmeButton {
            pressed: true,
            held: true,
            released: false,
        };
        t.on_input(&ctx(0, 0.0, press), &mut sink);
        let mut c = ctx(200, 5.0, EmeButton {
            pressed: false,
            held: true,
            released: false,
        });
        c.hit.as_mut().unwrap().pos[2] = 12.0; // higher ground under the second sample
        t.on_input(&c, &mut sink);
        let all = edits(&mut sink);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].1.param as i16, 50); // plane 10.5 over z 10
        assert_eq!(all[1].1.param as i16, -150); // plane 10.5 under z 12
        assert!(matches!(t.preview(&c), Some(EmePreview::Plane { center, .. }) if (center[2] - 10.5).abs() < 1e-4));
    }

    /// MONKEY (EME v3.3): the soft brush keeps working while held still (time trigger, ~8/s), the
    /// hard brush does not resend the same sphere; options map to the wire param + strength.
    #[test]
    fn soft_brush_held_still_keeps_raising_and_hard_is_the_old_sphere() {
        let mut t = EmeSculptTool::new(EmeStrokeIds::starting_at(1));
        t.set_option("mode", EmeOptionValue::Choice(1));
        t.set_option("strength", EmeOptionValue::Number(40.0));
        t.set_option("falloff", EmeOptionValue::Choice(1));
        t.set_option("hardness", EmeOptionValue::Number(30.0));
        let mut sink = EmeCommandSink::new();
        let press = EmeButton { pressed: true, held: true, released: false };
        let hold = EmeButton { pressed: false, held: true, released: false };
        let release = EmeButton { pressed: false, held: false, released: true };
        t.on_input(&ctx(0, 0.0, press), &mut sink);
        for f in 1..=63u64 {
            t.on_input(&ctx(f * 16, 0.0, hold), &mut sink);
        }
        let soft = edits(&mut sink);
        assert!((7..=9).contains(&soft.len()), "held still ~1 s = ~8 edits, got {}", soft.len());
        let e = soft[0].1;
        assert_eq!((e.mode, e.strength), (mode::BUILD, 102));
        assert_eq!(e.param, mode::soft_param(mode::FALLOFF_LINEAR, 30));
        t.on_input(&ctx(2000, 0.0, release), &mut sink);
        sink.drain();
        t.set_option("hard", EmeOptionValue::Bool(true));
        t.on_input(&ctx(3000, 0.0, press), &mut sink);
        for f in 1..=63u64 {
            t.on_input(&ctx(3000 + f * 16, 0.0, hold), &mut sink);
        }
        let hard = edits(&mut sink);
        assert_eq!(hard.len(), 1, "a hard sphere held still is sent once");
        assert_eq!((hard[0].1.param, hard[0].1.strength), (0, 255));
        let keys: Vec<_> = t.options_ui_model().iter().map(|f| f.key).collect();
        assert_eq!(keys, ["mode", "radius", "shape", "strength", "falloff", "hardness", "hard"]);
    }

    /// MONKEY (EME, CONTRACT v3.6): the shape rides on lower / raise (hard and soft), never on smooth.
    #[test]
    fn shape_option_sets_param_bits_9_10() {
        let mut t = EmeSculptTool::new(EmeStrokeIds::starting_at(1));
        let press = EmeButton { pressed: true, held: true, released: false };
        let release = EmeButton { pressed: false, held: false, released: true };
        let mut sink = EmeCommandSink::new();
        let mut first = |t: &mut EmeSculptTool, now: u64| {
            t.on_input(&ctx(now, 0.0, press), &mut sink);
            let e = edits(&mut sink)[0].1;
            t.on_input(&ctx(now + 500, 0.0, release), &mut sink);
            sink.drain();
            e
        };
        t.set_option("shape", EmeOptionValue::Choice(1));
        let e = first(&mut t, 0);
        assert_eq!(e.param, mode::soft_param(mode::FALLOFF_SMOOTH, 0) | 0x0200);
        assert_eq!(mode::shape_of(e.mode, e.param), mode::SHAPE_CUBE);
        t.set_option("hard", EmeOptionValue::Bool(true));
        t.set_option("shape", EmeOptionValue::Text("cylinder".into()));
        let e = first(&mut t, 2000);
        assert_eq!((e.param, e.strength), (0x0400, 255));
        t.set_option("mode", EmeOptionValue::Choice(2));
        let e = first(&mut t, 4000);
        assert_eq!(e.param, 0, "smooth carries no shape");
        let shape = t.options_ui_model().into_iter().find(|f| f.key == "shape").unwrap();
        assert!(shape.disabled);
    }
}

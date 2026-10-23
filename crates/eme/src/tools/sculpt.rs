//! `sculpt` (T1 + T2): the terrain brush — dig / build / smooth / flatten on the real ground or an
//! existing voxel surface. LMB held = one stroke; the core [`EmeStrokeBuilder`] coalesces the held
//! brush into ≤ 8 EDIT3/s (move ≥ r/2 or 125 ms), every edit tagged with the stroke id, so one
//! Ctrl+Z undoes the whole stroke.
//!
//! Flatten (mode 3) samples its plane at the press: every edit of the stroke carries
//! `param = (plane − z) · 100` (cm, i16), so the whole stroke levels toward the clicked height.
//!
//! Options: `mode` (choice dig / build / smooth / flatten), `radius` (0.5–16 yd), `strength`
//! (smooth / flatten blend, 1–255), `offset` (flatten plane offset, yards).

use eme_core::command::{EmeCommand, EmeCommandSink, EmeStrokeMark};
use eme_core::stroke::{EmeBrushSample, EmeStrokeBuilder, EmeStrokeConfig, EmeStrokeIds};
use eme_core::tool::{
    EmeHit, EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview,
    EmeTool, EmeToolId,
};
use eme_core::transport::mode;

/// The tool id (also the server's name for it in `caps.tools`).
pub const SCULPT: EmeToolId = EmeToolId("sculpt");

/// Choice labels of the `mode` option (locale keys), in mode order 0..=3.
pub const MODE_LABELS: [&str; 4] = [
    "MONKEY_EME_SCULPT_MODE_DIG",
    "MONKEY_EME_SCULPT_MODE_BUILD",
    "MONKEY_EME_SCULPT_MODE_SMOOTH",
    "MONKEY_EME_SCULPT_MODE_FLATTEN",
];

/// The terrain brush.
#[derive(Debug)]
pub struct EmeSculptTool {
    builder: EmeStrokeBuilder,
    /// Voxel mode 0..=3.
    pub mode: u8,
    /// Radius, yards.
    pub radius: f32,
    /// Smooth / flatten blend /255.
    pub strength: u8,
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
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x4d45, |d| (d.subsec_micros() as u16).rotate_left(5) ^ 0x4d45);
        Self {
            builder: EmeStrokeBuilder::new(EmeStrokeConfig::default())
                .with_start(ids.peek(), seed)
                .with_ids(ids),
            mode: mode::DIG,
            radius: 4.0,
            strength: 255,
            offset: 0.0,
            plane: None,
            wheel_resizes: true,
        }
    }

    /// The edit strength of the current mode (dig / build ignore it; smooth defaults firm-half).
    fn effective_strength(&self) -> u8 {
        match self.mode {
            mode::DIG | mode::BUILD => 255,
            mode::SMOOTH => self.strength.min(128).max(1),
            _ => self.strength.max(1),
        }
    }

    fn sample(&self, ctx: &EmeInputCtx, hit: &EmeHit) -> EmeBrushSample {
        let param = if self.mode == mode::FLATTEN {
            let plane = self.plane.unwrap_or(hit.pos[2] + self.offset);
            (((plane - hit.pos[2]) * 100.0).round().clamp(-32768.0, 32767.0) as i16) as u16
        } else {
            0
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
        SCULPT
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
        let p = SCULPT.locale_prefix();
        vec![
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
            EmeOptionField {
                key: "strength",
                label: format!("{p}STRENGTH"),
                kind: EmeOptionKind::Slider {
                    min: 1.0,
                    max: 255.0,
                    step: 1.0,
                },
                value: EmeOptionValue::Number(self.strength as f32),
                disabled: matches!(self.mode, mode::DIG | mode::BUILD),
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
        ]
    }

    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        match (key, value) {
            ("mode", EmeOptionValue::Choice(i)) if i <= 3 => self.mode = i as u8,
            ("mode", EmeOptionValue::Number(n)) if (0.0..=3.0).contains(&n) => self.mode = n as u8,
            ("radius", EmeOptionValue::Number(r)) => self.radius = r.clamp(0.5, 16.0),
            ("strength", EmeOptionValue::Number(s)) => self.strength = s.clamp(1.0, 255.0) as u8,
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
            falloff: 0.25,
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
        let mut t = EmeSculptTool::new(EmeStrokeIds::default());
        t.set_option("mode", EmeOptionValue::Choice(3));
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
}

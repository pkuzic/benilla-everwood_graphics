//! `bridge` (MONKEY, CONTRACT v3.6, owner 2026-10-24 "Planet-Zoo-style terraforming"): a straight
//! BRIDGE (build) or TUNNEL (dig) between two picked points.
//!
//! Click point A, click point B: the path is previewed (centre line + the cross-section at both
//! ends, the arch curve); click again (or Enter, or the Confirm button) to build it, Esc / Cancel
//! drops the picks. The whole path goes out as ONE stroke (one Ctrl+Z) of hard edits of the chosen
//! shape spaced ≤ radius / 2 along the segment, ~8 edits/s:
//! - **bridge**: build edits whose TOP is on the path (centre `radius` below it), so the deck meets
//!   the ground at both ends. Default shape cube = a flat deck `2 × radius` wide. `thickness` < the
//!   full `2 × radius` carves the deck's underside afterwards, but only over the span (where the
//!   real ground lies below the carve), never into the banks.
//! - **tunnel**: dig edits whose FLOOR is on the path (centre `radius` above it). Default shape sphere
//!   = a round tube; a vertical cylinder gives a flat floor + ceiling with straight walls.
//! - `arch` raises the middle of the path by up to N yd (a parabola, 0 at the ends).
//!
//! No wire change: ordinary mode 0 / 1 EDIT3 records, offered whenever `sculpt` is.

use eme_core::command::{EmeCommand, EmeCommandSink, EmeStrokeMark};
use eme_core::stroke::{EmeBrushSample, EmeStrokeBuilder, EmeStrokeConfig, EmeStrokeIds};
use eme_core::tool::{
    EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview,
    EmeTerrain, EmeTool, EmeToolId, EmeToolNotice,
};
use eme_core::transport::mode;

use super::sculpt::{shape_field, shape_value, SCULPT};

/// The tool id.
pub const BRIDGE: EmeToolId = EmeToolId("bridge");
/// Choice labels of the `mode` option: 0 bridge (build), 1 tunnel (dig).
pub const MODE_LABELS: [&str; 2] = ["MONKEY_EME_BRIDGE_MODE_BRIDGE", "MONKEY_EME_BRIDGE_MODE_TUNNEL"];
/// The longest path, yards (every edit must stay in the server's edit range of the camera).
pub const MAX_LENGTH: f32 = 160.0;
/// The most edits one path may send.
pub const MAX_EDITS: usize = 400;

/// What a path is made of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeBridgeSpec {
    /// Point A, WoW yards.
    pub a: [f32; 3],
    /// Point B, WoW yards.
    pub b: [f32; 3],
    /// Brush radius (half-width), yards.
    pub radius: f32,
    /// `mode::SHAPE_*`.
    pub shape: u8,
    /// true = tunnel (dig), false = bridge (build).
    pub tunnel: bool,
    /// Raise the middle by this much, yards (parabola).
    pub arch: f32,
    /// Bridge deck thickness, yards (≥ `2 × radius` = the full shape, no underside carve).
    pub thickness: f32,
}

impl EmeBridgeSpec {
    /// The number of path points: spacing `<= radius / 2` in 3D (the arch included: a step is at
    /// most `(len + 4 * arch) / (n - 1)` long), both ends included.
    pub fn points(&self) -> usize {
        let len = dist(self.a, self.b) + 4.0 * self.arch.abs();
        ((len / (self.radius * 0.5)).ceil() as usize).max(1) + 1
    }

    /// Path point `i` of `n` (the arch included): on the straight segment A→B, raised by
    /// `arch × 4t(1 − t)`.
    pub fn point(&self, i: usize, n: usize) -> [f32; 3] {
        let t = if n <= 1 { 0.0 } else { i as f32 / (n - 1) as f32 };
        let lerp = |a: f32, b: f32| a + (b - a) * t;
        [
            lerp(self.a[0], self.b[0]),
            lerp(self.a[1], self.b[1]),
            lerp(self.a[2], self.b[2]) + self.arch * 4.0 * t * (1.0 - t),
        ]
    }

    /// Whether the bridge carves its underside (a deck thinner than the shape).
    pub fn carves_underside(&self) -> bool {
        !self.tunnel && self.thickness < 2.0 * self.radius - 0.01
    }

    /// The edits of the path, in send order. `ground(x, y, near_z)` (the real ground before the
    /// path) decides where a bridge's underside may be carved: only where the whole carve lies
    /// above it (over the span, never into the banks). `None` from it = no carve there.
    pub fn edits(&self, map: u16, ground: &dyn Fn(f32, f32, f32) -> Option<f32>) -> Vec<EmeBrushSample> {
        let n = self.points();
        let r = self.radius;
        let param = mode::with_shape(0, self.shape);
        let sample = |pos: [f32; 3], m: u8| EmeBrushSample {
            map,
            pos,
            radius: r,
            mode: m,
            strength: 255,
            param,
        };
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let p = self.point(i, n);
            if self.tunnel {
                out.push(sample([p[0], p[1], p[2] + r], mode::DIG));
            } else {
                out.push(sample([p[0], p[1], p[2] - r], mode::BUILD));
            }
        }
        if self.carves_underside() {
            for i in 0..n {
                let p = self.point(i, n);
                let zc = p[2] - self.thickness - r;
                // The carve spans zc ± r: keep it clear of the real ground (+ a margin).
                if ground(p[0], p[1], p[2]).is_some_and(|g| g < zc - r - 0.5) {
                    out.push(sample([p[0], p[1], zc], mode::DIG));
                }
            }
        }
        out
    }
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// The bridge / tunnel tool.
#[derive(Debug)]
pub struct EmeBridgeTool {
    builder: EmeStrokeBuilder,
    /// true tunnel, false bridge.
    pub tunnel: bool,
    /// Radius (half-width), yards.
    pub radius: f32,
    /// `mode::SHAPE_*`.
    pub shape: u8,
    /// Arch height, yards.
    pub arch: f32,
    /// Deck thickness, yards.
    pub thickness: f32,
    a: Option<[f32; 3]>,
    b: Option<[f32; 3]>,
    /// Ground heights under the path points (from the host's terrain), for the underside carve.
    ground: Option<(EmeBridgeSpec, Vec<Option<f32>>)>,
    /// The path being sent: its samples and the next index.
    sending: Option<(Vec<EmeBrushSample>, usize)>,
    confirm: bool,
    notice: Option<EmeToolNotice>,
}

impl EmeBridgeTool {
    /// A bridge tool numbering its strokes from the shared `ids` counter.
    pub fn new(ids: EmeStrokeIds) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x4252, |d| (d.subsec_micros() as u16).rotate_left(3) ^ 0x4252);
        let config = EmeStrokeConfig {
            plane_moves: false,
            hard_depth_radii: 1.0e6,
            ..Default::default()
        };
        Self {
            builder: EmeStrokeBuilder::new(config)
                .with_start(ids.peek(), seed)
                .with_ids(ids),
            tunnel: false,
            radius: 2.5,
            shape: mode::SHAPE_CUBE,
            arch: 0.0,
            thickness: 1.5,
            a: None,
            b: None,
            ground: None,
            sending: None,
            confirm: false,
            notice: None,
        }
    }

    /// The current spec (both points picked).
    pub fn spec(&self) -> Option<EmeBridgeSpec> {
        Some(EmeBridgeSpec {
            a: self.a?,
            b: self.b?,
            radius: self.radius,
            shape: self.shape,
            tunnel: self.tunnel,
            arch: self.arch,
            thickness: self.thickness.clamp(0.5, 2.0 * self.radius),
        })
    }

    /// Whether a path is being sent.
    pub fn is_sending(&self) -> bool {
        self.sending.is_some()
    }

    fn say(&mut self, key: &str, args: Vec<String>, error: bool) {
        self.notice = Some(EmeToolNotice {
            key: key.into(),
            args,
            error,
        });
    }

    fn cancel(&mut self) {
        self.a = None;
        self.b = None;
        self.ground = None;
        self.confirm = false;
    }

    /// Turn the picked path into a stroke (refused with a notice when too long / too many edits).
    fn start(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        let Some(spec) = self.spec() else { return };
        let len = dist(spec.a, spec.b);
        if !(1.0..=MAX_LENGTH).contains(&len) {
            self.say(
                "MONKEY_EME_BRIDGE_TOO_LONG",
                vec![format!("{len:.0}"), format!("{MAX_LENGTH:.0}")],
                true,
            );
            return;
        }
        let heights = self.ground.as_ref().filter(|(s, _)| *s == spec).map(|(_, h)| h.clone());
        let n = spec.points();
        let ground = |x: f32, y: f32, _z: f32| -> Option<f32> {
            let h = heights.as_ref()?;
            (0..n)
                .find(|&i| {
                    let p = spec.point(i, n);
                    (p[0] - x).abs() < 1e-3 && (p[1] - y).abs() < 1e-3
                })
                .and_then(|i| h[i])
        };
        let edits = spec.edits(ctx.map, &ground);
        if edits.len() > MAX_EDITS {
            self.say(
                "MONKEY_EME_BRIDGE_TOO_MANY",
                vec![edits.len().to_string(), MAX_EDITS.to_string()],
                true,
            );
            return;
        }
        let (stroke, first) = self.builder.begin(ctx.now_ms, edits[0]);
        let next = if first.is_some() { 1 } else { 0 };
        if let Some(e) = first {
            out.push(EmeCommand::VoxelStroke {
                stroke,
                ops: vec![e],
                mark: EmeStrokeMark::Continue,
            });
        }
        let kind = if spec.tunnel {
            "MONKEY_EME_BRIDGE_MODE_TUNNEL"
        } else {
            "MONKEY_EME_BRIDGE_MODE_BRIDGE"
        };
        self.say(
            "MONKEY_EME_BRIDGE_SENDING",
            vec![kind.into(), edits.len().to_string(), format!("{len:.0}")],
            false,
        );
        self.sending = Some((edits, next));
        self.cancel();
    }

    /// Send the next edit(s) of the path being sent; close the stroke after the last.
    fn pump(&mut self, now: u64, out: &mut EmeCommandSink) {
        let Some((edits, next)) = self.sending.as_mut() else {
            return;
        };
        let Some(stroke) = self.builder.active_stroke() else {
            self.sending = None;
            return;
        };
        if *next < edits.len() {
            if let Some(e) = self.builder.update(now, edits[*next]) {
                *next += 1;
                out.push(EmeCommand::VoxelStroke {
                    stroke: e.stroke,
                    ops: vec![e],
                    mark: EmeStrokeMark::Continue,
                });
            }
            return;
        }
        if let Some(end) = self.builder.end(now, None) {
            out.push(EmeCommand::VoxelStroke {
                stroke: end.stroke,
                ops: end.edit.into_iter().collect(),
                mark: EmeStrokeMark::End {
                    trailing: end.trailing,
                },
            });
        }
        let _ = stroke;
        self.sending = None;
    }
}

impl EmeTool for EmeBridgeTool {
    fn id(&self) -> EmeToolId {
        BRIDGE
    }

    fn server_tool(&self) -> EmeToolId {
        SCULPT
    }

    fn min_rank(&self) -> u8 {
        3
    }

    fn on_terrain(&mut self, _ctx: &EmeInputCtx, terrain: &dyn EmeTerrain) {
        let Some(spec) = self.spec() else { return };
        if !spec.carves_underside() || self.ground.as_ref().is_some_and(|(s, _)| *s == spec) {
            return;
        }
        let n = spec.points().min(MAX_EDITS);
        let h = (0..n)
            .map(|i| {
                let p = spec.point(i, n);
                terrain.ground_z(p[0], p[1], p[2], 80.0)
            })
            .collect();
        self.ground = Some((spec, h));
    }

    fn take_notice(&mut self) -> Option<EmeToolNotice> {
        self.notice.take()
    }

    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        if self.sending.is_some() {
            self.pump(ctx.now_ms, out);
            return;
        }
        if ctx.cancel {
            if self.a.is_some() {
                self.say("MONKEY_EME_BRIDGE_CANCELLED", Vec::new(), false);
            }
            self.cancel();
            return;
        }
        if ctx.modifiers.alt && ctx.wheel != 0.0 {
            self.radius = (self.radius + 0.5 * ctx.wheel.signum()).clamp(0.5, 16.0);
        }
        let confirm = std::mem::take(&mut self.confirm) || ctx.confirm;
        if confirm && self.b.is_some() {
            self.start(ctx, out);
            return;
        }
        if !ctx.primary.pressed || ctx.pointer_over_ui {
            return;
        }
        let Some(hit) = ctx.hit else { return };
        match (self.a, self.b) {
            (None, _) => {
                self.a = Some(hit.pos);
                self.say("MONKEY_EME_BRIDGE_PICK_B", Vec::new(), false);
            }
            (Some(_), None) => {
                self.b = Some(hit.pos);
                self.say("MONKEY_EME_BRIDGE_READY", Vec::new(), false);
            }
            (Some(_), Some(_)) => self.start(ctx, out),
        }
    }

    fn options_ui_model(&self) -> EmeOptions {
        let p = BRIDGE.locale_prefix();
        let slider = |key: &'static str, min: f32, max: f32, step: f32, v: f32, disabled: bool| {
            EmeOptionField {
                key,
                label: format!("{p}{}", key.to_ascii_uppercase()),
                kind: EmeOptionKind::Slider { min, max, step },
                value: EmeOptionValue::Number(v),
                disabled,
            }
        };
        let button = |key: &'static str, disabled: bool| EmeOptionField {
            key,
            label: format!("{p}{}", key.to_ascii_uppercase()),
            kind: EmeOptionKind::Button,
            value: EmeOptionValue::Click,
            disabled,
        };
        let busy = self.sending.is_some();
        vec![
            EmeOptionField {
                key: "mode",
                label: format!("{p}MODE"),
                kind: EmeOptionKind::Choice(MODE_LABELS.iter().map(|s| s.to_string()).collect()),
                value: EmeOptionValue::Choice(self.tunnel as usize),
                disabled: busy,
            },
            slider("radius", 0.5, 16.0, 0.5, self.radius, busy),
            shape_field(format!("{p}SHAPE"), self.shape, busy),
            slider("thickness", 0.5, 32.0, 0.25, self.thickness, busy || self.tunnel),
            slider("arch", 0.0, 20.0, 0.5, self.arch, busy),
            button("confirm", busy || self.b.is_none()),
            button("cancel", busy || self.a.is_none()),
        ]
    }

    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        if self.sending.is_some() {
            return;
        }
        match (key, value) {
            ("mode", v) => {
                let tunnel = match v {
                    EmeOptionValue::Choice(i) => i == 1,
                    EmeOptionValue::Number(n) => n >= 0.5,
                    EmeOptionValue::Bool(b) => b,
                    EmeOptionValue::Text(t) => t == "tunnel",
                    _ => self.tunnel,
                };
                if tunnel != self.tunnel {
                    self.tunnel = tunnel;
                    // Each mode's natural shape: a flat deck, a round tube.
                    self.shape = if tunnel {
                        mode::SHAPE_SPHERE
                    } else {
                        mode::SHAPE_CUBE
                    };
                }
            }
            ("radius", EmeOptionValue::Number(r)) => self.radius = r.clamp(0.5, 16.0),
            ("shape", v) => {
                if let Some(s) = shape_value(&v) {
                    self.shape = s;
                }
            }
            ("thickness", EmeOptionValue::Number(t)) => self.thickness = t.clamp(0.5, 32.0),
            ("arch", EmeOptionValue::Number(a)) => self.arch = a.clamp(0.0, 20.0),
            ("confirm", _) => self.confirm = true,
            ("cancel", _) => self.cancel(),
            _ => {}
        }
    }

    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview> {
        let m = if self.tunnel { mode::DIG } else { mode::BUILD };
        let (a, b) = match (self.a, self.b, ctx.hit) {
            (Some(a), Some(b), _) => (a, b),
            (Some(a), None, Some(h)) => (a, h.pos),
            (None, _, Some(h)) => {
                return Some(EmePreview::Path {
                    points: vec![h.pos],
                    radius: self.radius,
                    mode: m,
                })
            }
            _ => return None,
        };
        let spec = EmeBridgeSpec {
            a,
            b,
            radius: self.radius,
            shape: self.shape,
            tunnel: self.tunnel,
            arch: self.arch,
            thickness: self.thickness,
        };
        let n = spec.points().min(64);
        Some(EmePreview::Path {
            points: (0..n).map(|i| spec.point(i, n)).collect(),
            radius: self.radius,
            mode: m,
        })
    }

    fn on_deactivate(&mut self, out: &mut EmeCommandSink) {
        // A path being sent is closed where it stands (what went out stays one undoable stroke).
        if let Some(stroke) = self.builder.active_stroke() {
            self.builder.cancel();
            out.push(EmeCommand::VoxelStroke {
                stroke,
                ops: Vec::new(),
                mark: EmeStrokeMark::End { trailing: false },
            });
        }
        self.sending = None;
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::perm::{EmePermissions, EmeRank};
    use eme_core::tool::{EmeButton, EmeHit};

    fn spec(tunnel: bool) -> EmeBridgeSpec {
        EmeBridgeSpec {
            a: [0.0, 0.0, 10.0],
            b: [20.0, 0.0, 10.0],
            radius: 2.0,
            shape: mode::SHAPE_CUBE,
            tunnel,
            arch: 4.0,
            thickness: 4.0,
        }
    }

    #[test]
    fn bridge_edits_are_spaced_at_most_half_a_radius_with_the_arch() {
        let s = spec(false);
        let e = s.edits(0, &|_, _, _| None);
        assert_eq!(s.points(), 37);
        assert_eq!(e.len(), 37);
        for w in e.windows(2) {
            assert!(dist(w[0].pos, w[1].pos) <= s.radius * 0.5 + 1e-4, "{:?}", w);
            let flat = ((w[0].pos[0] - w[1].pos[0]).powi(2) + (w[0].pos[1] - w[1].pos[1]).powi(2)).sqrt();
            assert!(flat <= s.radius * 0.5 + 1e-4);
        }
        // Deck top on the path: the ends sit radius below A / B, the middle is raised by the arch.
        assert_eq!(e[0].pos, [0.0, 0.0, 8.0]);
        assert_eq!(e[36].pos, [20.0, 0.0, 8.0]);
        assert!((e[18].pos[2] - (10.0 + 4.0 - 2.0)).abs() < 1e-4);
        assert!(e.iter().all(|x| x.mode == mode::BUILD && x.param == 0x0200 && x.strength == 255));
        // A tunnel's floor is on the path (centre radius above it).
        let t = spec(true).edits(0, &|_, _, _| None);
        assert_eq!(t[0].pos, [0.0, 0.0, 12.0]);
        assert!(t.iter().all(|x| x.mode == mode::DIG));
    }

    #[test]
    fn a_thin_deck_carves_its_underside_only_over_the_span() {
        let mut s = spec(false);
        s.arch = 0.0;
        s.thickness = 1.0;
        // Banks: ground at the path for x < 4 and x > 16; a 20-yd-deep pit in between.
        let ground = |x: f32, _y: f32, z: f32| Some(if (4.0..=16.0).contains(&x) { z - 20.0 } else { z });
        let e = s.edits(0, &ground);
        let builds = e.iter().filter(|x| x.mode == mode::BUILD).count();
        let carves: Vec<_> = e.iter().filter(|x| x.mode == mode::DIG).collect();
        assert_eq!(builds, 21, "no arch: 20 yd / 1 yd + 1");
        assert!(!carves.is_empty());
        assert!(carves.iter().all(|c| (4.0..=16.0).contains(&c.pos[0])), "never into the banks");
        // Carves come after every build (the deck exists before its underside is cut).
        assert!(e[..21].iter().all(|x| x.mode == mode::BUILD));
        // Deck [top - 1, top]: carve centre = top - 1 - r.
        let top = 10.0;
        assert!((carves[0].pos[2] - (top - 1.0 - 2.0)).abs() < 1e-3);
    }

    fn ctx(now: u64, pos: Option<[f32; 3]>, pressed: bool) -> EmeInputCtx {
        let mut c = EmeInputCtx::idle(now, 0, EmePermissions::from_rank(EmeRank::EDIT));
        c.hit = pos.map(|pos| EmeHit {
            pos,
            normal: [0.0, 0.0, 1.0],
            object: None,
        });
        c.primary = EmeButton {
            pressed,
            held: pressed,
            released: false,
        };
        c
    }

    #[test]
    fn pick_a_pick_b_confirm_sends_one_rate_limited_stroke_and_esc_cancels() {
        let mut t = EmeBridgeTool::new(EmeStrokeIds::starting_at(9));
        let mut sink = EmeCommandSink::new();
        t.on_input(&ctx(0, Some([0.0, 0.0, 10.0]), true), &mut sink);
        t.on_input(&ctx(10, Some([10.0, 0.0, 10.0]), true), &mut sink);
        assert!(sink.is_empty(), "two picks send nothing");
        assert!(matches!(t.preview(&ctx(20, None, false)), Some(EmePreview::Path { ref points, .. }) if points.len() == 9));
        // Esc drops the picks.
        let mut esc = ctx(30, None, false);
        esc.cancel = true;
        t.on_input(&esc, &mut sink);
        assert!(t.spec().is_none());
        // Pick again and confirm with a third click.
        t.on_input(&ctx(40, Some([0.0, 0.0, 10.0]), true), &mut sink);
        t.on_input(&ctx(50, Some([10.0, 0.0, 10.0]), true), &mut sink);
        t.on_input(&ctx(60, Some([5.0, 0.0, 10.0]), true), &mut sink);
        assert!(t.is_sending());
        let mut now = 60;
        while t.is_sending() && now < 10_000 {
            now += 16;
            t.on_input(&ctx(now, None, false), &mut sink);
        }
        let items = sink.drain();
        let mut ops = Vec::new();
        let mut ended = 0;
        for i in items {
            if let EmeCommand::VoxelStroke { stroke, ops: o, mark } = i.command {
                assert_eq!(stroke, 9);
                ops.extend(o);
                if matches!(mark, EmeStrokeMark::End { .. }) {
                    ended += 1;
                }
            }
        }
        assert_eq!(ops.len(), 9, "10 yd at r 2.5 = 9 points");
        assert_eq!(ended, 1, "one stroke, closed once");
        // ≤ 8 edits/s.
        assert!(now >= 60 + 8 * 125, "rate limited: done at {now}");
        assert!(ops.windows(2).all(|w| w[1].nonce == w[0].nonce.wrapping_add(1)));
    }
}

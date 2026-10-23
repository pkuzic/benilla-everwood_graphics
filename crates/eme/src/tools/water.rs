//! `water` (T16, lane K): paint flat liquid cells (CONTRACT v3 §11: the 25/6-yd ADT liquid grid).
//!
//! LMB held = one overlay stroke: the level is sampled at the press (the aimed surface's height +
//! the `offset` option, like flatten's plane), and every liquid cell whose centre the brush disc
//! covers while held is stamped at that level — `kind` water, or "dry" (kind 0: no liquid here, the
//! baseline's included). Cells go out as `ov_edit` batches (≤ [`BATCH_CELLS`] per request, one
//! request per [`BATCH_GAP_MS`], so a held brush stays inside the server's rate bucket); every
//! batch carries the stroke id, so one Ctrl+Z (`undo` of the stroke) removes the whole stroke.
//! The server fans the accepted cells back out as `L` records to every client, this one included:
//! the water appears from the server's answer, never from a local guess.
//!
//! Options: `radius` (yards), `offset` (yards over the aimed surface), `kind` (water / dry).

use std::collections::{BTreeSet, VecDeque};

use eme_core::command::{EmeCommand, EmeCommandSink};
use eme_core::perm::EmeCapability;
use eme_core::service::{EmeLiquidCell, EmeLiquidKind, EmeOverlayEdit};
use eme_core::stroke::EmeStrokeIds;
use eme_core::tool::{
    EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview, EmeTool,
    EmeToolId,
};

/// The tool id (also the server's name for it in `caps.tools`).
pub const WATER: EmeToolId = EmeToolId("water");
/// Liquid cell edge, yards (CONTRACT §11.1).
pub const LIQ_CELL: f32 = 25.0 / 6.0;
/// Cells per `ov_edit` at most (the contract allows 32; 16 keeps a held brush at ~10 rate tokens/s).
pub const BATCH_CELLS: usize = 16;
/// Gap between two `ov_edit` batches of a held stroke, ms.
pub const BATCH_GAP_MS: u64 = 400;
/// Choice labels of the `kind` option (locale keys): index 0 water, 1 dry.
pub const KIND_LABELS: [&str; 2] = ["MONKEY_EME_WATER_KIND_WATER", "MONKEY_EME_WATER_KIND_DRY"];

/// The liquid cell containing a WoW XY.
pub fn cell_of(x: f32, y: f32) -> (i16, i16) {
    ((x * 6.0 / 25.0).floor() as i16, (y * 6.0 / 25.0).floor() as i16)
}

/// Every liquid cell whose centre lies within `radius` of `(x, y)` (at least the cell under it).
pub fn cells_in_disc(x: f32, y: f32, radius: f32) -> Vec<(i16, i16)> {
    let (c0x, c0y) = cell_of(x - radius, y - radius);
    let (c1x, c1y) = cell_of(x + radius, y + radius);
    let mut out = Vec::new();
    for cx in c0x..=c1x {
        for cy in c0y..=c1y {
            let mx = (cx as f32 + 0.5) * LIQ_CELL;
            let my = (cy as f32 + 0.5) * LIQ_CELL;
            if (mx - x).powi(2) + (my - y).powi(2) <= radius * radius {
                out.push((cx, cy));
            }
        }
    }
    if out.is_empty() {
        out.push(cell_of(x, y));
    }
    out
}

/// The water brush.
#[derive(Debug)]
pub struct EmeWaterTool {
    ids: EmeStrokeIds,
    /// Brush radius, yards.
    pub radius: f32,
    /// Level offset over the aimed surface, yards.
    pub offset: f32,
    /// 0 water, 1 dry (kind 0).
    pub kind: usize,
    /// The held stroke: id and level.
    stroke: Option<(u16, f32)>,
    /// Cells of the held stroke already queued or sent.
    seen: BTreeSet<(i16, i16)>,
    /// Cells waiting for their batch, and the stroke + level they belong to.
    queue: VecDeque<(u16, f32, (i16, i16))>,
    /// When the last batch went out (`None` = the next may go at once).
    last_batch_ms: Option<u64>,
}

impl EmeWaterTool {
    /// A water tool numbering its strokes from the shared `ids` counter.
    pub fn new(ids: EmeStrokeIds) -> Self {
        Self {
            ids,
            radius: 6.0,
            offset: 0.0,
            kind: 0,
            stroke: None,
            seen: BTreeSet::new(),
            queue: VecDeque::new(),
            last_batch_ms: None,
        }
    }

    /// The stroke being held, if any.
    pub fn active_stroke(&self) -> Option<u16> {
        self.stroke.map(|(s, _)| s)
    }

    fn wire_kind(&self) -> EmeLiquidKind {
        if self.kind == 1 {
            EmeLiquidKind::None
        } else {
            EmeLiquidKind::Water
        }
    }

    /// Send the next batch if its gap has passed (`force` = ignore the gap: the stroke's first).
    fn flush(&mut self, ctx: &EmeInputCtx, force: bool, out: &mut EmeCommandSink) {
        if self.queue.is_empty() {
            return;
        }
        if !force {
            if let Some(t) = self.last_batch_ms {
                if ctx.now_ms.saturating_sub(t) < BATCH_GAP_MS {
                    return;
                }
            }
        }
        let stroke = self.queue[0].0;
        let kind = self.wire_kind();
        let mut cells = Vec::new();
        while let Some(&(s, l, c)) = self.queue.front() {
            if s != stroke || cells.len() >= BATCH_CELLS {
                break;
            }
            self.queue.pop_front();
            cells.push(EmeLiquidCell {
                cx: c.0,
                cy: c.1,
                kind,
                level: l,
                flags: 0,
                fade: 0,
            });
        }
        self.last_batch_ms = Some(ctx.now_ms);
        out.push(EmeCommand::Overlay(EmeOverlayEdit::Liquid {
            map: ctx.map,
            stroke,
            cells,
        }));
    }
}

impl EmeTool for EmeWaterTool {
    fn id(&self) -> EmeToolId {
        WATER
    }
    fn min_rank(&self) -> u8 {
        3
    }
    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        // Alt + wheel resizes.
        if ctx.modifiers.alt && ctx.wheel != 0.0 {
            self.radius = (self.radius + ctx.wheel.signum()).clamp(1.0, 32.0);
        }
        let can = ctx.perms.has(EmeCapability::Edit);
        if ctx.primary.pressed && can && !ctx.pointer_over_ui {
            if let Some(hit) = ctx.hit {
                let level = ((hit.pos[2] + self.offset) * 100.0).round() / 100.0;
                self.stroke = Some((self.ids.next_id(), level));
                self.seen.clear();
            }
        }
        if !ctx.primary.held {
            self.stroke = None;
        }
        let mut first = false;
        if let (Some((stroke, level)), Some(hit)) = (self.stroke, ctx.hit) {
            first = self.seen.is_empty();
            for c in cells_in_disc(hit.pos[0], hit.pos[1], self.radius) {
                if self.seen.insert(c) {
                    self.queue.push_back((stroke, level, c));
                }
            }
        }
        self.flush(ctx, first, out);
    }
    fn options_ui_model(&self) -> EmeOptions {
        vec![
            EmeOptionField {
                key: "radius",
                label: "MONKEY_EME_WATER_RADIUS".into(),
                kind: EmeOptionKind::Slider {
                    min: 1.0,
                    max: 32.0,
                    step: 0.5,
                },
                value: EmeOptionValue::Number(self.radius),
                disabled: false,
            },
            EmeOptionField {
                key: "offset",
                label: "MONKEY_EME_WATER_OFFSET".into(),
                kind: EmeOptionKind::Slider {
                    min: -10.0,
                    max: 10.0,
                    step: 0.25,
                },
                value: EmeOptionValue::Number(self.offset),
                disabled: false,
            },
            EmeOptionField {
                key: "kind",
                label: "MONKEY_EME_WATER_KIND".into(),
                kind: EmeOptionKind::Choice(KIND_LABELS.iter().map(|s| s.to_string()).collect()),
                value: EmeOptionValue::Choice(self.kind),
                disabled: false,
            },
        ]
    }
    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        match (key, value) {
            ("radius", EmeOptionValue::Number(v)) => self.radius = v.clamp(1.0, 32.0),
            ("offset", EmeOptionValue::Number(v)) => self.offset = v.clamp(-10.0, 10.0),
            ("kind", EmeOptionValue::Choice(i)) => self.kind = i.min(KIND_LABELS.len() - 1),
            // A script's number (0 water, 1 dry).
            ("kind", EmeOptionValue::Number(v)) => {
                self.kind = (v.max(0.0) as usize).min(KIND_LABELS.len() - 1);
            }
            _ => {}
        }
    }
    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview> {
        let hit = ctx.hit?;
        let level = self
            .stroke
            .map_or(hit.pos[2] + self.offset, |(_, level)| level);
        Some(EmePreview::Plane {
            center: [hit.pos[0], hit.pos[1], level],
            radius: self.radius,
        })
    }
    fn on_deactivate(&mut self, _out: &mut EmeCommandSink) {
        // A switched-away tool gets no more frames: what is still queued is dropped (its stroke
        // id would be stale by the next activation).
        self.stroke = None;
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::perm::{EmePermissions, EmeRank};
    use eme_core::tool::{EmeButton, EmeHit};

    fn ctx(now: u64, pos: [f32; 3], held: bool, pressed: bool, rank: EmeRank) -> EmeInputCtx {
        let mut c = EmeInputCtx::idle(now, 0, EmePermissions::from_rank(rank));
        c.hit = Some(EmeHit {
            pos,
            normal: [0.0, 0.0, 1.0],
            object: None,
        });
        c.primary = EmeButton {
            pressed,
            held,
            released: false,
        };
        c
    }

    fn batches(sink: &mut EmeCommandSink) -> Vec<(u16, Vec<EmeLiquidCell>)> {
        sink.drain()
            .into_iter()
            .filter_map(|i| match i.command {
                EmeCommand::Overlay(EmeOverlayEdit::Liquid { stroke, cells, .. }) => {
                    Some((stroke, cells))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn disc_cells_match_the_contract_grid() {
        assert_eq!(cell_of(418.0, -80.0), (100, -20));
        let c = cells_in_disc(418.0, -80.0, 0.1);
        assert_eq!(c, vec![(100, -20)], "a tiny brush still stamps its own cell");
        // r = 6 yd ≈ π·36 / 17.36 ≈ 6.5 cells.
        let n = cells_in_disc(418.0, -80.0, 6.0).len();
        assert!((4..=10).contains(&n), "{n}");
    }

    #[test]
    fn a_held_stroke_paints_at_the_press_level_in_rate_limited_batches() {
        let ids = EmeStrokeIds::default();
        let mut t = EmeWaterTool::new(ids.clone());
        t.set_option("offset", EmeOptionValue::Number(-0.5));
        t.set_option("radius", EmeOptionValue::Number(10.0));
        let mut sink = EmeCommandSink::new();
        t.on_input(&ctx(0, [418.0, -80.0, 81.0], true, true, EmeRank::EDIT), &mut sink);
        let b = batches(&mut sink);
        assert_eq!(b.len(), 1, "the first batch goes at once");
        let stroke = b[0].0;
        assert_eq!(b[0].1.len(), BATCH_CELLS);
        assert!(b[0].1.iter().all(|c| c.level == 80.5 && c.kind == EmeLiquidKind::Water));
        // Dragging uphill keeps the press level; nothing before the gap.
        t.on_input(&ctx(100, [425.0, -80.0, 90.0], true, false, EmeRank::EDIT), &mut sink);
        assert!(batches(&mut sink).is_empty());
        let mut total = 16;
        let mut now = 400;
        loop {
            t.on_input(&ctx(now, [425.0, -80.0, 90.0], false, false, EmeRank::EDIT), &mut sink);
            let b = batches(&mut sink);
            if b.is_empty() {
                break;
            }
            assert_eq!(b[0].0, stroke, "one stroke id for the whole press");
            assert!(b[0].1.iter().all(|c| c.level == 80.5));
            total += b[0].1.len();
            now += 400;
        }
        assert!(total > 16, "the drag added cells: {total}");
        // No duplicate cells inside a stroke; a new press is a new stroke.
        t.on_input(&ctx(now, [418.0, -80.0, 81.0], true, true, EmeRank::EDIT), &mut sink);
        let b = batches(&mut sink);
        assert_ne!(b[0].0, stroke);
        // Dry kind → kind 0.
        t.set_option("kind", EmeOptionValue::Choice(1));
        t.on_input(&ctx(now + 10_000, [0.0, 0.0, 5.0], true, true, EmeRank::EDIT), &mut sink);
        let b = batches(&mut sink);
        assert!(b.last().unwrap().1.iter().all(|c| c.kind == EmeLiquidKind::None));
    }

    #[test]
    fn read_only_rank_paints_nothing_and_preview_is_the_level_disc() {
        let mut t = EmeWaterTool::new(EmeStrokeIds::default());
        let mut sink = EmeCommandSink::new();
        let c = ctx(0, [1.0, 2.0, 3.0], true, true, EmeRank::VIEW);
        t.on_input(&c, &mut sink);
        assert!(batches(&mut sink).is_empty());
        t.set_option("offset", EmeOptionValue::Number(2.0));
        assert_eq!(
            t.preview(&c),
            Some(EmePreview::Plane {
                center: [1.0, 2.0, 5.0],
                radius: 6.0
            })
        );
    }
}

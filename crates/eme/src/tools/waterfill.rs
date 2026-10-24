//! `waterfill` (MONKEY, CONTRACT v3.6, owner 2026-10-24 "Planet-Zoo-style terraforming"): click
//! inside a depression — a dug pit or a natural hollow — and fill it with water up to where it
//! would spill.
//!
//! The fill runs on the liquid cell grid (CONTRACT §11.1, 25/6-yd cells) over the ground heights
//! the host reports at the cell centres (the voxel surface where edited, else the ADT ground):
//!
//! A cell's height is the LOWEST of nine ground samples (corners, edge midpoints, centre): a cell
//! is wet when ANY part of its ground is below the level, and connectivity is 8-way, so the water
//! reaches into every wall/corner cell and the terrain (higher than the flat water plane there)
//! hides the surplus: the visible shoreline follows the real pit shape.
//! 1. walk downhill from the clicked cell to the basin's lowest cell (≤ [`MAX_DESCENT`] steps,
//!    else the ground is an open slope → refused);
//! 2. a priority flood from there (4-connected, lowest cell first, ties by cell) until it reaches a
//!    cell LOWER than the highest cell taken so far: that height is the SPILL height (the lowest
//!    rim the water escapes over). More than `max_cells` taken first = open / too big → refused;
//! 3. `level = spill − margin` (and at most `floor + max depth` when the cap is on);
//! 4. the water = the cells connected to the floor whose ground lies below the level.
//!
//! The filled cells are previewed at the level; click again (or Enter / Confirm) to send them as
//! `ov_edit` batches (≤ [`BATCH_CELLS`] cells, one per [`BATCH_GAP_MS`]: the server's service rate
//! bucket) in ONE stroke, so one Ctrl+Z removes the whole pool. Esc / Cancel drops the preview.
//! Offered whenever the server offers `water` ([`EmeTool::server_tool`]).

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap, HashMap, VecDeque};

use eme_core::command::{EmeCommand, EmeCommandSink};
use eme_core::perm::EmeCapability;
use eme_core::service::{EmeLiquidCell, EmeLiquidKind, EmeOverlayEdit, EmeOverlayLayer};
use eme_core::stroke::EmeStrokeIds;
use eme_core::tool::{
    EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview,
    EmeTerrain, EmeTool, EmeToolId, EmeToolNotice,
};

use super::water::{cell_of, LIQ_CELL, WATER};

/// The tool id.
pub const WATERFILL: EmeToolId = EmeToolId("waterfill");
/// The most cells one fill may cover.
pub const MAX_CELLS: usize = 1024;
/// Cells per `ov_edit` (the contract's maximum).
pub const BATCH_CELLS: usize = 32;
/// Gap between two batches, ms (32 cells = 8 service tokens; the bucket refills 10/s).
pub const BATCH_GAP_MS: u64 = 900;
/// Downhill steps from the clicked cell to the basin floor at most.
pub const MAX_DESCENT: usize = 96;
/// Heights closer than this count as equal, yards.
const EPS: f32 = 1.0e-3;

/// A liquid cell.
pub type Cell = (i16, i16);

/// The centre of a liquid cell, WoW yards.
pub fn cell_centre(c: Cell) -> (f32, f32) {
    ((c.0 as f32 + 0.5) * LIQ_CELL, (c.1 as f32 + 0.5) * LIQ_CELL)
}

/// What [`flood_fill`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeFill {
    /// A closed basin: its water level, the filled cells (sorted), the floor and the spill height.
    Basin {
        /// Water surface height, yards.
        level: f32,
        /// The cells under water, sorted.
        cells: Vec<Cell>,
        /// The lowest ground cell's height.
        floor: f32,
        /// The lowest rim height the water would escape over.
        spill: f32,
    },
    /// The ground keeps falling away from the click (a slope, not a hollow).
    Open,
    /// The basin is open or larger than the cell cap.
    TooBig,
    /// The hollow is shallower than the margin (nothing to fill).
    Shallow,
    /// No ground data under the click (not loaded).
    NoGround,
}

/// A totally ordered heap key: lowest height first, then the cell.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Key(f32, Cell);
impl Eq for Key {}
impl PartialOrd for Key {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Key {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0).then(self.1.cmp(&o.1))
    }
}

const N8: [(i16, i16); 8] = [
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, 1),
    (1, -1),
    (-1, 1),
    (-1, -1),
];

/// The sample offsets inside a cell (corners, edge midpoints, centre), as fractions of the cell.
pub const CELL_SAMPLES: [(f32, f32); 9] = [
    (0.0, 0.0),
    (0.5, 0.0),
    (1.0, 0.0),
    (0.0, 0.5),
    (0.5, 0.5),
    (1.0, 0.5),
    (0.0, 1.0),
    (0.5, 1.0),
    (1.0, 1.0),
];

/// Flood-fill the basin around `seed` (see the module docs). `ground(cell)` = the ground height at
/// the LOWEST of the cell's nine samples (see [`CELL_SAMPLES`]), `None` = unknown (counts as a hole the water escapes into). `margin` = how far
/// below the spill height the surface stays; `max_depth` = cap the level at `floor + max_depth`.
pub fn flood_fill(
    seed: Cell,
    ground: &mut dyn FnMut(Cell) -> Option<f32>,
    max_cells: usize,
    margin: f32,
    max_depth: Option<f32>,
) -> EmeFill {
    let mut cache: HashMap<Cell, f32> = HashMap::new();
    let mut h = |c: Cell, cache: &mut HashMap<Cell, f32>| -> f32 {
        *cache
            .entry(c)
            .or_insert_with(|| ground(c).unwrap_or(f32::NEG_INFINITY))
    };
    if h(seed, &mut cache) == f32::NEG_INFINITY {
        return EmeFill::NoGround;
    }
    // 1. Downhill to the floor (8-connected steepest descent, ties by cell order).
    let mut floor = seed;
    let mut steps = 0;
    loop {
        let hf = h(floor, &mut cache);
        let mut best: Option<Key> = None;
        for dx in -1i16..=1 {
            for dy in -1i16..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let c = (floor.0.saturating_add(dx), floor.1.saturating_add(dy));
                let k = Key(h(c, &mut cache), c);
                if best.is_none_or(|b| k < b) {
                    best = Some(k);
                }
            }
        }
        let b = best.expect("8 neighbours");
        if b.0 == f32::NEG_INFINITY {
            return EmeFill::NoGround;
        }
        if b.0 < hf - EPS {
            floor = b.1;
            steps += 1;
            if steps > MAX_DESCENT {
                return EmeFill::Open;
            }
        } else {
            break;
        }
    }
    let floor_h = h(floor, &mut cache);
    // 2. Priority flood until the first cell lower than the rim taken so far.
    let mut heap = BinaryHeap::new();
    let mut seen: BTreeSet<Cell> = BTreeSet::new();
    let mut taken: Vec<(Cell, f32)> = Vec::new();
    heap.push(Reverse(Key(floor_h, floor)));
    seen.insert(floor);
    let mut rim = floor_h;
    let spill = loop {
        let Some(Reverse(Key(hc, c))) = heap.pop() else {
            return EmeFill::TooBig;
        };
        if hc < rim - EPS {
            break rim;
        }
        if hc > rim {
            rim = hc;
        }
        taken.push((c, hc));
        if taken.len() > max_cells {
            return EmeFill::TooBig;
        }
        for (dx, dy) in N8 {
            let n = (c.0.saturating_add(dx), c.1.saturating_add(dy));
            if seen.insert(n) {
                heap.push(Reverse(Key(h(n, &mut cache), n)));
            }
        }
    };
    // 3. The level.
    let mut level = spill - margin;
    if let Some(d) = max_depth {
        level = level.min(floor_h + d);
    }
    if level <= floor_h + 0.05 {
        return EmeFill::Shallow;
    }
    // 4. The cells under the level connected to the floor (all among the taken cells).
    let under: HashMap<Cell, f32> = taken.into_iter().filter(|(_, hc)| *hc < level).collect();
    let mut cells = BTreeSet::new();
    let mut q = VecDeque::from([floor]);
    cells.insert(floor);
    while let Some(c) = q.pop_front() {
        for (dx, dy) in N8 {
            let n = (c.0.saturating_add(dx), c.1.saturating_add(dy));
            if under.contains_key(&n) && cells.insert(n) {
                q.push_back(n);
            }
        }
    }
    if cells.len() > max_cells {
        return EmeFill::TooBig;
    }
    EmeFill::Basin {
        level: (level * 100.0).round() / 100.0,
        cells: cells.into_iter().collect(),
        floor: floor_h,
        spill,
    }
}

/// The water-fill tool.
#[derive(Debug)]
pub struct EmeWaterFillTool {
    ids: EmeStrokeIds,
    /// The surface stays this far below the spill height, yards.
    pub margin: f32,
    /// Cap the depth at `max_depth` over the floor.
    pub cap: bool,
    /// The depth cap, yards.
    pub max_depth: f32,
    /// A click to analyse: where, and the clicked height.
    pending: Option<([f32; 3], u64)>,
    /// The analysed basin waiting for confirmation.
    preview: Option<(f32, Vec<Cell>)>,
    /// Batches still to send: stroke, level, cells.
    queue: VecDeque<(u16, f32, Cell)>,
    last_batch_ms: Option<u64>,
    confirm: bool,
    notice: Option<EmeToolNotice>,
}

impl EmeWaterFillTool {
    /// A water-fill tool numbering its strokes from the shared `ids` counter.
    pub fn new(ids: EmeStrokeIds) -> Self {
        Self {
            ids,
            margin: 0.25,
            cap: false,
            max_depth: 4.0,
            pending: None,
            preview: None,
            queue: VecDeque::new(),
            last_batch_ms: None,
            confirm: false,
            notice: None,
        }
    }

    /// The previewed fill (level, cells), if any.
    pub fn previewed(&self) -> Option<&(f32, Vec<Cell>)> {
        self.preview.as_ref()
    }

    /// Cells still waiting to be sent.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    fn say(&mut self, key: &str, args: Vec<String>, error: bool) {
        self.notice = Some(EmeToolNotice {
            key: key.into(),
            args,
            error,
        });
    }

    /// Analyse a click (the pure fill over the host's ground).
    fn analyse(&mut self, at: [f32; 3], terrain: &dyn EmeTerrain) {
        let near = at[2];
        let mut ground = |c: Cell| {
            // The lowest of nine samples; any unknown sample makes the cell unknown (an escape).
            let (x0, y0) = (c.0 as f32 * LIQ_CELL, c.1 as f32 * LIQ_CELL);
            let mut low = f32::INFINITY;
            for (fx, fy) in CELL_SAMPLES {
                low = low.min(terrain.ground_z(x0 + fx * LIQ_CELL, y0 + fy * LIQ_CELL, near, 40.0)?);
            }
            Some(low)
        };
        let fill = flood_fill(
            cell_of(at[0], at[1]),
            &mut ground,
            MAX_CELLS,
            self.margin,
            self.cap.then_some(self.max_depth),
        );
        match &fill {
            EmeFill::Basin { level, cells, floor, spill } => bevy::log::info!(
                "EME waterfill at {at:?}: {} cell(s), level {level:.2}, floor {floor:.2}, spill {spill:.2}",
                cells.len()
            ),
            other => bevy::log::info!("EME waterfill at {at:?}: refused {other:?}"),
        }
        match fill {
            EmeFill::Basin {
                level,
                cells,
                floor,
                spill,
            } => {
                self.say(
                    "MONKEY_EME_WATERFILL_PREVIEW",
                    vec![
                        cells.len().to_string(),
                        format!("{level:.2}"),
                        format!("{:.1}", level - floor),
                        format!("{spill:.2}"),
                    ],
                    false,
                );
                self.preview = Some((level, cells));
            }
            other => {
                let key = match other {
                    EmeFill::Open => "MONKEY_EME_WATERFILL_OPEN",
                    EmeFill::TooBig => "MONKEY_EME_WATERFILL_TOO_BIG",
                    EmeFill::Shallow => "MONKEY_EME_WATERFILL_SHALLOW",
                    _ => "MONKEY_EME_WATERFILL_NO_GROUND",
                };
                self.say(key, vec![MAX_CELLS.to_string()], true);
                self.preview = None;
            }
        }
    }

    fn flush(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        let Some(&(stroke, _, _)) = self.queue.front() else {
            return;
        };
        if self
            .last_batch_ms
            .is_some_and(|t| ctx.now_ms.saturating_sub(t) < BATCH_GAP_MS)
        {
            return;
        }
        let mut cells = Vec::new();
        while let Some(&(s, level, c)) = self.queue.front() {
            if s != stroke || cells.len() >= BATCH_CELLS {
                break;
            }
            self.queue.pop_front();
            cells.push(EmeLiquidCell {
                cx: c.0,
                cy: c.1,
                kind: EmeLiquidKind::Water,
                level,
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
        if self.queue.is_empty() {
            self.say("MONKEY_EME_WATERFILL_DONE", Vec::new(), false);
        }
    }
}

impl EmeTool for EmeWaterFillTool {
    fn id(&self) -> EmeToolId {
        WATERFILL
    }

    fn server_tool(&self) -> EmeToolId {
        WATER
    }

    fn min_rank(&self) -> u8 {
        3
    }

    fn on_terrain(&mut self, _ctx: &EmeInputCtx, terrain: &dyn EmeTerrain) {
        if let Some((at, _)) = self.pending.take() {
            self.analyse(at, terrain);
        }
    }

    fn take_notice(&mut self) -> Option<EmeToolNotice> {
        self.notice.take()
    }

    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        if !self.queue.is_empty() {
            self.flush(ctx, out);
            return;
        }
        if ctx.cancel {
            if self.preview.take().is_some() {
                self.say("MONKEY_EME_WATERFILL_CANCELLED", Vec::new(), false);
            }
            self.pending = None;
            return;
        }
        let confirm = std::mem::take(&mut self.confirm) || ctx.confirm;
        let click = ctx.primary.pressed && !ctx.pointer_over_ui && ctx.hit.is_some();
        if (confirm || click) && self.preview.is_some() && ctx.perms.has(EmeCapability::Edit) {
            let (level, cells) = self.preview.take().expect("preview");
            let stroke = self.ids.next_id();
            self.queue = cells.into_iter().map(|c| (stroke, level, c)).collect();
            self.last_batch_ms = None;
            self.flush(ctx, out);
            return;
        }
        if ctx.primary.pressed {
            bevy::log::info!(
                "EME waterfill: press (hit {:?}, over ui {}, preview {})",
                ctx.hit.map(|h| h.pos),
                ctx.pointer_over_ui,
                self.preview.is_some()
            );
        }
        if click {
            let hit = ctx.hit.expect("click has a hit");
            self.pending = Some((hit.pos, ctx.now_ms));
        }
    }

    fn options_ui_model(&self) -> EmeOptions {
        let p = WATERFILL.locale_prefix();
        let busy = !self.queue.is_empty();
        let field = |key: &'static str, kind: EmeOptionKind, value: EmeOptionValue, disabled: bool| {
            EmeOptionField {
                key,
                label: format!("{p}{}", key.to_ascii_uppercase()),
                kind,
                value,
                disabled,
            }
        };
        vec![
            field(
                "margin",
                EmeOptionKind::Slider {
                    min: 0.0,
                    max: 2.0,
                    step: 0.05,
                },
                EmeOptionValue::Number(self.margin),
                busy,
            ),
            field("cap", EmeOptionKind::Toggle, EmeOptionValue::Bool(self.cap), busy),
            field(
                "max_depth",
                EmeOptionKind::Slider {
                    min: 0.25,
                    max: 40.0,
                    step: 0.25,
                },
                EmeOptionValue::Number(self.max_depth),
                busy || !self.cap,
            ),
            field(
                "confirm",
                EmeOptionKind::Button,
                EmeOptionValue::Click,
                busy || self.preview.is_none(),
            ),
            field(
                "cancel",
                EmeOptionKind::Button,
                EmeOptionValue::Click,
                busy || self.preview.is_none(),
            ),
        ]
    }

    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        if !self.queue.is_empty() {
            return;
        }
        match (key, value) {
            ("margin", EmeOptionValue::Number(m)) => self.margin = m.clamp(0.0, 2.0),
            ("cap", EmeOptionValue::Bool(b)) => self.cap = b,
            ("max_depth", EmeOptionValue::Number(d)) => self.max_depth = d.clamp(0.25, 40.0),
            ("confirm", _) => self.confirm = true,
            ("cancel", _) => {
                self.preview = None;
                self.pending = None;
            }
            _ => {}
        }
    }

    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview> {
        if let Some((level, cells)) = &self.preview {
            return Some(EmePreview::Cells {
                layer: EmeOverlayLayer::Liquid,
                cells: cells.iter().map(|&(x, y)| (x as i32, y as i32)).collect(),
                level: Some(*level),
            });
        }
        let hit = ctx.hit?;
        // The cell under the cursor (where a click would start the fill).
        let c = cell_of(hit.pos[0], hit.pos[1]);
        Some(EmePreview::Cells {
            layer: EmeOverlayLayer::Liquid,
            cells: vec![(c.0 as i32, c.1 as i32)],
            level: Some(hit.pos[2]),
        })
    }

    fn on_deactivate(&mut self, _out: &mut EmeCommandSink) {
        // Batches not sent yet are dropped with the tool (their stroke stays what went out).
        self.queue.clear();
        self.pending = None;
        self.preview = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::perm::{EmePermissions, EmeRank};
    use eme_core::tool::{EmeButton, EmeHit};

    /// A bowl: ground = r² / 20 around (0, 0) in cell units, rim capped at `rim` beyond radius
    /// `r_rim` with a notch (the spill point) of height `notch` at +x.
    fn bowl(c: Cell, r_rim: f32, rim: f32, notch: f32) -> Option<f32> {
        let (x, y) = (c.0 as f32, c.1 as f32);
        let r = (x * x + y * y).sqrt();
        if r < r_rim {
            return Some(r * r / 20.0);
        }
        if r < r_rim + 1.5 {
            // The rim ring: high, except a notch on the +x axis.
            return Some(if c.1 == 0 && c.0 > 0 { notch } else { rim });
        }
        // Outside: falls away.
        Some(notch - 3.0 - (r - r_rim) * 0.5)
    }

    #[test]
    fn a_closed_bowl_fills_to_its_spill_height() {
        let mut g = |c: Cell| bowl(c, 6.0, 5.0, 2.0);
        match flood_fill((3, 2), &mut g, MAX_CELLS, 0.25, None) {
            EmeFill::Basin {
                level,
                cells,
                floor,
                spill,
            } => {
                assert_eq!(floor, 0.0, "walked down to the bottom");
                assert_eq!(spill, 2.0, "spills over the notch, not the high rim");
                assert_eq!(level, 1.75);
                // Every cell under water is below the level, the bottom is in, the rim is out.
                assert!(cells.contains(&(0, 0)));
                assert!(cells.iter().all(|&c| bowl(c, 6.0, 5.0, 2.0).unwrap() < level));
                let expect = (-6..=6)
                    .flat_map(|x| (-6..=6).map(move |y| (x, y)))
                    .filter(|&(x, y): &(i16, i16)| {
                        let r = ((x * x + y * y) as f32).sqrt();
                        r < 6.0 && r * r / 20.0 < 1.75
                    })
                    .count();
                assert_eq!(cells.len(), expect);
            }
            other => panic!("expected a basin, got {other:?}"),
        }
        // The depth cap.
        let mut g = |c: Cell| bowl(c, 6.0, 5.0, 2.0);
        match flood_fill((0, 0), &mut g, MAX_CELLS, 0.25, Some(0.5)) {
            EmeFill::Basin { level, .. } => assert_eq!(level, 0.5),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_open_slope_refuses_and_the_cap_is_enforced() {
        // A plane falling to +x forever: no floor within the descent budget.
        let mut slope = |c: Cell| Some(-(c.0 as f32) * 0.3 + c.1 as f32 * 0.01);
        assert_eq!(flood_fill((0, 0), &mut slope, MAX_CELLS, 0.25, None), EmeFill::Open);
        // Flat ground: a floor at once, but it never spills: open → too big at the cap.
        let mut flat = |_c: Cell| Some(10.0);
        assert_eq!(flood_fill((0, 0), &mut flat, 200, 0.25, None), EmeFill::TooBig);
        // A bowl wider than the cap.
        let mut big = |c: Cell| bowl(c, 30.0, 80.0, 60.0);
        assert_eq!(flood_fill((0, 0), &mut big, 1024, 0.25, None), EmeFill::TooBig);
        // A dimple shallower than the margin.
        let mut dimple = |c: Cell| Some(if c == (0, 0) { 9.9 } else { 10.0 + (c.0 as f32).abs() * 0.0 });
        assert_eq!(flood_fill((0, 0), &mut dimple, 64, 0.25, None), EmeFill::TooBig);
        let mut dimple2 = |c: Cell| {
            let r = ((c.0 * c.0 + c.1 * c.1) as f32).sqrt();
            Some(if r < 2.0 { 9.9 } else if r < 3.5 { 10.1 } else { 9.0 })
        };
        assert_eq!(flood_fill((0, 0), &mut dimple2, 64, 0.25, None), EmeFill::Shallow);
        // No ground under the click.
        let mut none = |_c: Cell| None;
        assert_eq!(flood_fill((0, 0), &mut none, 64, 0.25, None), EmeFill::NoGround);
    }

    /// A square pit (half-width `hw` yd, floor 0, wall to 5) on a continuous heightfield.
    fn pit_z(x: f32, y: f32, hw: f32) -> f32 {
        let d = x.abs().max(y.abs());
        if d < hw { 0.0 } else { (5.0f32).min((d - hw) * 20.0) }
    }

    /// Run the fill over a continuous field `f(x, y)`, nine samples per cell like the tool.
    fn fill_field(f: &dyn Fn(f32, f32) -> f32, seed: Cell) -> EmeFill {
        let mut g = |c: Cell| {
            let (x0, y0) = (c.0 as f32 * LIQ_CELL, c.1 as f32 * LIQ_CELL);
            Some(CELL_SAMPLES.iter().map(|(fx, fy)| f(x0 + fx * LIQ_CELL, y0 + fy * LIQ_CELL)).fold(f32::INFINITY, f32::min))
        };
        flood_fill(seed, &mut g, MAX_CELLS, 0.25, None)
    }

    fn low_of(f: &dyn Fn(f32, f32) -> f32, cx: i16, cy: i16) -> f32 {
        let (x0, y0) = (cx as f32 * LIQ_CELL, cy as f32 * LIQ_CELL);
        CELL_SAMPLES.iter().map(|(a, b)| f(x0 + a * LIQ_CELL, y0 + b * LIQ_CELL)).fold(f32::INFINITY, f32::min)
    }

    #[test]
    fn a_cube_pit_fills_every_partly_low_cell_without_holes() {
        // Pit 10.3 yd half-width, off the cell grid: edge cells are only partly low.
        let outer = |x: f32, y: f32| { let d = x.abs().max(y.abs()); if d > 14.0 { 5.0 - (d - 14.0) } else { pit_z(x, y, 10.3) } };
        match fill_field(&outer, (0, 0)) {
            EmeFill::Basin { level, cells, .. } => {
                assert!(level > 0.5, "{level}");
                let set: BTreeSet<Cell> = cells.iter().copied().collect();
                for cx in -3i16..3 {
                    for cy in -3i16..3 {
                        let low = low_of(&outer, cx, cy);
                        assert_eq!(set.contains(&(cx, cy)), low < level, "cell {cx},{cy} low {low}");
                    }
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_round_bowl_covers_the_diagonals() {
        let f = |x: f32, y: f32| {
            let r = (x * x + y * y).sqrt();
            if r < 9.4 { r * r / 90.0 } else if r < 12.0 { 0.98 + (r - 9.4) * 2.0 } else if r < 15.0 { 6.0 } else { 6.0 - (r - 15.0) }
        };
        match fill_field(&f, (0, 0)) {
            EmeFill::Basin { level, cells, .. } => {
                let set: BTreeSet<Cell> = cells.iter().copied().collect();
                for cx in -3i16..3 {
                    for cy in -3i16..3 {
                        assert_eq!(set.contains(&(cx, cy)), low_of(&f, cx, cy) < level, "cell {cx},{cy}");
                    }
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_pit_open_on_one_side_is_refused() {
        let f = |x: f32, y: f32| {
            let d = x.abs().max(y.abs());
            if x > 0.0 && y.abs() < 3.0 { -(x * 0.3) } else if d < 6.0 { 0.0 } else { 5.0 }
        };
        assert!(!matches!(fill_field(&f, (0, 0)), EmeFill::Basin { .. }));
    }

    struct Bowl;
    impl EmeTerrain for Bowl {
        fn ground_z(&self, x: f32, y: f32, _near: f32, _reach: f32) -> Option<f32> {
            bowl(cell_of(x, y), 6.0, 5.0, 2.0).map(|z| z + 100.0)
        }
    }

    fn ctx(now: u64, pressed: bool) -> EmeInputCtx {
        let mut c = EmeInputCtx::idle(now, 0, EmePermissions::from_rank(EmeRank::EDIT));
        c.hit = Some(EmeHit {
            pos: [1.0, 1.0, 100.0],
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
    fn click_previews_confirm_sends_one_stroke_in_batches() {
        let ids = EmeStrokeIds::starting_at(30);
        let mut t = EmeWaterFillTool::new(ids);
        let mut sink = EmeCommandSink::new();
        t.on_terrain(&ctx(0, true), &Bowl);
        t.on_input(&ctx(0, true), &mut sink);
        t.on_terrain(&ctx(16, false), &Bowl);
        t.on_input(&ctx(16, false), &mut sink);
        assert!(sink.is_empty(), "a click only previews");
        let (level, cells) = t.previewed().cloned().expect("a basin");
        assert!((100.0..101.75).contains(&level), "{level}");
        assert!(matches!(t.preview(&ctx(16, false)), Some(EmePreview::Cells { ref cells, .. }) if cells.len() == cells.len()));
        assert_eq!(t.take_notice().unwrap().key, "MONKEY_EME_WATERFILL_PREVIEW");
        // Esc drops it; a new click + Enter sends.
        let mut esc = ctx(20, false);
        esc.cancel = true;
        t.on_input(&esc, &mut sink);
        assert!(t.previewed().is_none());
        t.on_input(&ctx(30, true), &mut sink);
        t.on_terrain(&ctx(46, false), &Bowl);
        let mut enter = ctx(46, false);
        enter.confirm = true;
        t.on_input(&enter, &mut sink);
        let mut now = 46;
        while t.queued() > 0 && now < 60_000 {
            now += 100;
            t.on_input(&ctx(now, false), &mut sink);
        }
        let batches: Vec<_> = sink
            .drain()
            .into_iter()
            .filter_map(|i| match i.command {
                EmeCommand::Overlay(EmeOverlayEdit::Liquid { stroke, cells, .. }) => Some((stroke, cells)),
                _ => None,
            })
            .collect();
        assert!(batches.iter().all(|(s, b)| *s == 30 && b.len() <= BATCH_CELLS));
        let sent: usize = batches.iter().map(|(_, b)| b.len()).sum();
        assert_eq!(sent, cells.len());
        assert!(batches.iter().flat_map(|(_, b)| b).all(|c| c.level == level && c.kind == EmeLiquidKind::Water));
        assert_eq!(batches.len(), cells.len().div_ceil(BATCH_CELLS));
        assert_eq!(t.server_tool(), WATER);
    }
}

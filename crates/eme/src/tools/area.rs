//! `area` (T17, lane L): paint an existing `AreaTable` area id onto 16-yd area cells (CONTRACT v3
//! §11.1), or remove the override (`area_id` 0: the baseline MCNK area applies again).
//!
//! The area is picked from a searchable list of the current map's `AreaTable` names
//! ([`EmeAreaCatalog`], filled by the host from the client DBC, so names are in the client's
//! language), or typed as an id. LMB held = one overlay stroke; every area cell whose centre the
//! brush disc covers is stamped (≤ [`BATCH_CELLS`] per `ov_edit`, one batch per
//! [`BATCH_GAP_MS`]); one Ctrl+Z removes the stroke. The server validates the id against its own
//! `area_template` for the map (`bad_area`) and fans the cells out as `A` records; every client's
//! zone text / minimap name / area-keyed look follow the overlay from that answer.
//!
//! Options: `search` (text), `area` (choice from the filtered list), `area_id` (number),
//! `radius` (yards; 0 = one cell), `erase` (remove the override).

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, RwLock};

use eme_core::command::{EmeCommand, EmeCommandSink};
use eme_core::perm::EmeCapability;
use eme_core::service::{EmeAreaCell, EmeOverlayEdit};
use eme_core::stroke::EmeStrokeIds;
use eme_core::tool::{
    EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview, EmeTool,
    EmeToolId,
};

/// The tool id (also the server's name for it in `caps.tools`).
pub const AREA: EmeToolId = EmeToolId("area");
/// Area cell edge, yards (CONTRACT §11.1, MVP).
pub const AREA_CELL: f32 = 16.0;
/// Cells per `ov_edit` at most.
pub const BATCH_CELLS: usize = 16;
/// Gap between two batches of a held stroke, ms.
pub const BATCH_GAP_MS: u64 = 400;
/// Rows the picker shows at most.
pub const PICKER_ROWS: usize = 40;

/// The current map's `AreaTable` rows, `(id, name)` sorted by name; shared between the host (which
/// fills it from the DBC) and the tool.
#[derive(Debug, Clone, Default)]
pub struct EmeAreaCatalog(Arc<RwLock<Vec<(u16, String)>>>);

impl EmeAreaCatalog {
    /// Replace the rows.
    pub fn set(&self, mut rows: Vec<(u16, String)>) {
        rows.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then(a.0.cmp(&b.0)));
        if let Ok(mut g) = self.0.write() {
            *g = rows;
        }
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.0.read().map_or(0, |g| g.len())
    }

    /// No rows yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rows whose name contains `query` (case-insensitive) or whose id is `query`, ≤ `max`.
    pub fn search(&self, query: &str, max: usize) -> Vec<(u16, String)> {
        let q = query.trim().to_lowercase();
        let id: Option<u16> = q.parse().ok();
        self.0.read().map_or_else(
            |_| Vec::new(),
            |g| {
                g.iter()
                    .filter(|(i, n)| q.is_empty() || Some(*i) == id || n.to_lowercase().contains(&q))
                    .take(max)
                    .cloned()
                    .collect()
            },
        )
    }

    /// The name of `id`.
    pub fn name(&self, id: u16) -> Option<String> {
        self.0
            .read()
            .ok()?
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, n)| n.clone())
    }
}

/// The area cell containing a WoW XY.
pub fn cell_of(x: f32, y: f32) -> (i16, i16) {
    ((x / AREA_CELL).floor() as i16, (y / AREA_CELL).floor() as i16)
}

/// Every area cell whose centre lies within `radius` of `(x, y)`, at least the one under it.
pub fn cells_in_disc(x: f32, y: f32, radius: f32) -> Vec<(i16, i16)> {
    let (c0x, c0y) = cell_of(x - radius, y - radius);
    let (c1x, c1y) = cell_of(x + radius, y + radius);
    let mut out = vec![cell_of(x, y)];
    for ax in c0x..=c1x {
        for ay in c0y..=c1y {
            let mx = (ax as f32 + 0.5) * AREA_CELL;
            let my = (ay as f32 + 0.5) * AREA_CELL;
            if (mx - x).powi(2) + (my - y).powi(2) <= radius * radius && !out.contains(&(ax, ay)) {
                out.push((ax, ay));
            }
        }
    }
    out
}

/// The area brush.
#[derive(Debug)]
pub struct EmeAreaTool {
    ids: EmeStrokeIds,
    catalog: EmeAreaCatalog,
    /// The picker's search text.
    pub search: String,
    /// The area id to paint (0 = none picked yet).
    pub area_id: u16,
    /// Brush radius, yards (0 = one cell).
    pub radius: f32,
    /// Paint "remove the override" (id 0).
    pub erase: bool,
    stroke: Option<u16>,
    seen: BTreeSet<(i16, i16)>,
    queue: VecDeque<(u16, u16, (i16, i16))>,
    last_batch_ms: Option<u64>,
}

impl EmeAreaTool {
    /// An area tool numbering its strokes from the shared `ids` counter, picking from `catalog`.
    pub fn new(ids: EmeStrokeIds, catalog: EmeAreaCatalog) -> Self {
        Self {
            ids,
            catalog,
            search: String::new(),
            area_id: 0,
            radius: 0.0,
            erase: false,
            stroke: None,
            seen: BTreeSet::new(),
            queue: VecDeque::new(),
            last_batch_ms: None,
        }
    }

    /// The stroke being held, if any.
    pub fn active_stroke(&self) -> Option<u16> {
        self.stroke
    }

    /// The id a stroke would paint now (`None` = nothing picked).
    fn paint_id(&self) -> Option<u16> {
        if self.erase {
            Some(0)
        } else {
            (self.area_id != 0).then_some(self.area_id)
        }
    }

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
        let mut cells = Vec::new();
        while let Some(&(s, id, c)) = self.queue.front() {
            if s != stroke || cells.len() >= BATCH_CELLS {
                break;
            }
            self.queue.pop_front();
            cells.push(EmeAreaCell {
                ax: c.0,
                ay: c.1,
                area_id: id,
            });
        }
        self.last_batch_ms = Some(ctx.now_ms);
        out.push(EmeCommand::Overlay(EmeOverlayEdit::Area {
            map: ctx.map,
            stroke,
            cells,
        }));
    }
}

impl EmeTool for EmeAreaTool {
    fn id(&self) -> EmeToolId {
        AREA
    }
    fn min_rank(&self) -> u8 {
        3
    }
    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        if ctx.modifiers.alt && ctx.wheel != 0.0 {
            self.radius = (self.radius + 8.0 * ctx.wheel.signum()).clamp(0.0, 64.0);
        }
        let can = ctx.perms.has(EmeCapability::Edit);
        if ctx.primary.pressed && can && !ctx.pointer_over_ui && ctx.hit.is_some() {
            if self.paint_id().is_some() {
                self.stroke = Some(self.ids.next_id());
                self.seen.clear();
            }
        }
        if !ctx.primary.held {
            self.stroke = None;
        }
        let mut first = false;
        if let (Some(stroke), Some(hit), Some(id)) = (self.stroke, ctx.hit, self.paint_id()) {
            first = self.seen.is_empty();
            for c in cells_in_disc(hit.pos[0], hit.pos[1], self.radius) {
                if self.seen.insert(c) {
                    self.queue.push_back((stroke, id, c));
                }
            }
        }
        self.flush(ctx, first, out);
    }
    fn options_ui_model(&self) -> EmeOptions {
        let rows = self.catalog.search(&self.search, PICKER_ROWS);
        let mut labels: Vec<String> = rows.iter().map(|(id, n)| format!("{n} [{id}]")).collect();
        let sel = rows.iter().position(|(id, _)| *id == self.area_id);
        let sel = match sel {
            Some(i) => i,
            None => {
                // Keep the picked id visible even when the filter hides it.
                labels.insert(
                    0,
                    match self.catalog.name(self.area_id) {
                        Some(n) => format!("{n} [{}]", self.area_id),
                        None if self.area_id == 0 => "MONKEY_EME_AREA_PICK_NONE".into(),
                        None => format!("[{}]", self.area_id),
                    },
                );
                0
            }
        };
        vec![
            EmeOptionField {
                key: "search",
                label: "MONKEY_EME_AREA_SEARCH".into(),
                kind: EmeOptionKind::Text,
                value: EmeOptionValue::Text(self.search.clone()),
                disabled: false,
            },
            EmeOptionField {
                key: "area",
                label: "MONKEY_EME_AREA_AREA".into(),
                kind: EmeOptionKind::Choice(labels),
                value: EmeOptionValue::Choice(sel),
                disabled: self.erase,
            },
            EmeOptionField {
                key: "area_id",
                label: "MONKEY_EME_AREA_ID".into(),
                kind: EmeOptionKind::Number,
                value: EmeOptionValue::Number(f32::from(self.area_id)),
                disabled: self.erase,
            },
            EmeOptionField {
                key: "radius",
                label: "MONKEY_EME_AREA_RADIUS".into(),
                kind: EmeOptionKind::Slider {
                    min: 0.0,
                    max: 64.0,
                    step: 8.0,
                },
                value: EmeOptionValue::Number(self.radius),
                disabled: false,
            },
            EmeOptionField {
                key: "erase",
                label: "MONKEY_EME_AREA_ERASE".into(),
                kind: EmeOptionKind::Toggle,
                value: EmeOptionValue::Bool(self.erase),
                disabled: false,
            },
        ]
    }
    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        match (key, value) {
            ("search", EmeOptionValue::Text(t)) => self.search = t,
            ("area", EmeOptionValue::Choice(i)) => {
                let rows = self.catalog.search(&self.search, PICKER_ROWS);
                // Index 0 may be the "kept visible" row of a filtered-out pick (see the model).
                let shifted = !rows.iter().any(|(id, _)| *id == self.area_id);
                let i = if shifted { i.checked_sub(1) } else { Some(i) };
                if let Some((id, _)) = i.and_then(|i| rows.get(i)) {
                    self.area_id = *id;
                }
            }
            ("area_id", EmeOptionValue::Number(v)) => {
                self.area_id = v.round().clamp(0.0, f32::from(u16::MAX)) as u16;
            }
            ("radius", EmeOptionValue::Number(v)) => self.radius = v.clamp(0.0, 64.0),
            ("erase", EmeOptionValue::Bool(b)) => self.erase = b,
            _ => {}
        }
    }
    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview> {
        let hit = ctx.hit?;
        // MONKEY (EME, live bug 2026-10-24): an area cell is a whole column (zone ids have no
        // height), so the brush is drawn as a FLAT disc at the aim height: projecting the rim onto
        // the surface draws it down a pit's walls and up a hill, which reads as a 3D brush.
        Some(EmePreview::Plane {
            center: hit.pos,
            radius: self.radius.max(AREA_CELL * 0.5),
        })
    }
    fn on_deactivate(&mut self, _out: &mut EmeCommandSink) {
        self.stroke = None;
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::perm::{EmePermissions, EmeRank};
    use eme_core::tool::{EmeButton, EmeHit};

    fn catalog() -> EmeAreaCatalog {
        let c = EmeAreaCatalog::default();
        c.set(vec![
            (12, "Elwynn Forest".into()),
            (87, "Goldshire".into()),
            (9, "Northshire Valley".into()),
            (1519, "Stormwind City".into()),
        ]);
        c
    }

    fn press(now: u64, pos: [f32; 3], pressed: bool, held: bool) -> EmeInputCtx {
        let mut c = EmeInputCtx::idle(now, 0, EmePermissions::from_rank(EmeRank::EDIT));
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

    fn cells(sink: &mut EmeCommandSink) -> Vec<(u16, Vec<EmeAreaCell>)> {
        sink.drain()
            .into_iter()
            .filter_map(|i| match i.command {
                EmeCommand::Overlay(EmeOverlayEdit::Area { stroke, cells, .. }) => {
                    Some((stroke, cells))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn catalog_search_by_name_or_id() {
        let c = catalog();
        assert_eq!(c.search("gold", 10), vec![(87, "Goldshire".to_string())]);
        assert_eq!(c.search("87", 10)[0].0, 87);
        assert_eq!(c.search("", 2).len(), 2, "capped");
        assert_eq!(c.search("", 10)[0].1, "Elwynn Forest", "sorted by name");
    }

    #[test]
    fn picker_selects_from_the_filtered_list_and_paints_cells() {
        let mut t = EmeAreaTool::new(EmeStrokeIds::default(), catalog());
        let mut sink = EmeCommandSink::new();
        // Nothing picked: a press paints nothing.
        t.on_input(&press(0, [-8911.0, -117.0, 60.0], true, true), &mut sink);
        assert!(cells(&mut sink).is_empty());
        t.set_option("search", EmeOptionValue::Text("gold".into()));
        let model = t.options_ui_model();
        let EmeOptionKind::Choice(labels) = &model[1].kind else {
            panic!("area is a choice")
        };
        assert_eq!(labels, &["MONKEY_EME_AREA_PICK_NONE", "Goldshire [87]"]);
        t.set_option("area", EmeOptionValue::Choice(1));
        assert_eq!(t.area_id, 87);
        let model = t.options_ui_model();
        assert_eq!(model[1].value, EmeOptionValue::Choice(0), "now in the list, first row");
        t.on_input(&press(1000, [-8911.0, -117.0, 60.0], true, true), &mut sink);
        let c = cells(&mut sink);
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0].1,
            vec![EmeAreaCell {
                ax: -557,
                ay: -8,
                area_id: 87
            }]
        );
        // Erase paints id 0 in a new stroke.
        t.set_option("erase", EmeOptionValue::Bool(true));
        t.on_input(&press(5000, [-8911.0, -117.0, 60.0], true, true), &mut sink);
        let e = cells(&mut sink);
        assert_ne!(e[0].0, c[0].0);
        assert_eq!(e[0].1[0].area_id, 0);
        // A radius covers several cells (centres within 24 yd).
        t.set_option("erase", EmeOptionValue::Bool(false));
        t.set_option("area_id", EmeOptionValue::Number(12.0));
        t.set_option("radius", EmeOptionValue::Number(24.0));
        t.on_input(&press(9000, [0.0, 0.0, 0.0], true, true), &mut sink);
        let r = cells(&mut sink);
        assert!(r[0].1.len() >= 4, "{}", r[0].1.len());
        assert!(r[0].1.iter().all(|c| c.area_id == 12));
    }
}

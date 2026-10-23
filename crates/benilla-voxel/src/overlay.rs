//! MONKEY (EME): the overlay layers — liquid and area cells on top of the immutable ADT baseline
//! (CONTRACT v3 §11, `disposable/voxel-v2/CONTRACT.md`). Engine-free, unit-tested here.
//!
//! - **Liquid cell** = the ADT liquid grid cell, [`LIQ_CELL`] = 25/6 yd, `cx = floor(x·6/25)`; content
//!   [`LiquidCell`] (`kind` 0 = no liquid here, 1 water, …; a FLAT surface at `level`).
//! - **Area cell** = [`AREA_CELL`] = 16 yd, `ax = floor(x/16)`; content an `AreaTable` id (0 = the
//!   override was removed: the baseline MCNK area applies).
//! - A cell belongs to the 64-yd region of its CENTRE. Each `(layer, map, region)` has its own epoch +
//!   last_seq ([`RegionSync`], the voxel model). Cells are last-writer-wins by `seq`: the journal keeps
//!   the highest-seq row per cell, so a re-delivered (older or equal) seq never changes anything.
//! - `'Z'` (region reset) drops every cell of that layer whose centre lies in the region, adopts the
//!   epoch and restarts the region's sync point from 0; the re-OSYNC brings back the live rows.
//!
//! The journal persists to `$BENILLA_HOME/monkey_overlay.bin` ([`OverlayJournal::encode`]).

use std::collections::BTreeMap;

use crate::world::{region_of, RegionSync};

/// Liquid cell edge, yards (= the ADT MCLQ cell: 8 per 33.33-yd MCNK).
pub const LIQ_CELL: f32 = 25.0 / 6.0;
/// Area cell edge, yards (MVP resolution).
pub const AREA_CELL: f32 = 16.0;
/// Layer id: liquid.
pub const LAYER_LIQUID: u8 = 1;
/// Layer id: area.
pub const LAYER_AREA: u8 = 2;
/// Liquid kinds (wire `u8`).
pub const KIND_NONE: u8 = 0;
/// Water.
pub const KIND_WATER: u8 = 1;
/// Ocean.
pub const KIND_OCEAN: u8 = 2;
/// Magma.
pub const KIND_MAGMA: u8 = 3;
/// Slime.
pub const KIND_SLIME: u8 = 4;

/// The liquid cell containing world `(x, y)` (CONTRACT §11.1: `floor(x * 6 / 25)`).
pub fn liquid_cell_of(x: f32, y: f32) -> (i16, i16) {
    ((x * 6.0 / 25.0).floor() as i16, (y * 6.0 / 25.0).floor() as i16)
}

/// The area cell containing world `(x, y)`.
pub fn area_cell_of(x: f32, y: f32) -> (i16, i16) {
    ((x / AREA_CELL).floor() as i16, (y / AREA_CELL).floor() as i16)
}

/// The world-space centre of liquid cell `(cx, cy)`.
pub fn liquid_cell_centre(cx: i16, cy: i16) -> (f32, f32) {
    ((cx as f32 + 0.5) * LIQ_CELL, (cy as f32 + 0.5) * LIQ_CELL)
}

/// The world-space centre of area cell `(ax, ay)`.
pub fn area_cell_centre(ax: i16, ay: i16) -> (f32, f32) {
    ((ax as f32 + 0.5) * AREA_CELL, (ay as f32 + 0.5) * AREA_CELL)
}

/// The 64-yd region a cell of `layer` belongs to (the region of its centre).
pub fn cell_region(layer: u8, a: i16, b: i16) -> (i16, i16) {
    let (x, y) = if layer == LAYER_AREA {
        area_cell_centre(a, b)
    } else {
        liquid_cell_centre(a, b)
    };
    region_of(x, y)
}

/// One liquid cell's content (CONTRACT §11.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiquidCell {
    /// 0 none (no liquid here, the baseline's included), 1 water, 2 ocean, 3 magma, 4 slime.
    pub kind: u8,
    /// Flat surface height z, yards.
    pub level: f32,
    /// bit0 dark, bit1 hidden, bit2 fatigue.
    pub flags: u8,
    /// Depth fade 0..255.
    pub fade: u8,
}

impl LiquidCell {
    /// A cell that carries liquid (kind ≠ 0 and not `hidden`).
    pub fn is_wet(&self) -> bool {
        self.kind != KIND_NONE && self.flags & 0x02 == 0
    }
}

/// One cell's live value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlayValue {
    /// A liquid cell.
    Liquid(LiquidCell),
    /// An area id (0 = override removed).
    Area(u16),
}

/// The live row of one cell (the highest seq seen).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayRow {
    /// Layer seq.
    pub seq: u32,
    /// Stroke id.
    pub stroke: u16,
    /// Low GUID of the editor.
    pub actor: u32,
    /// The content.
    pub value: OverlayValue,
}

/// One `(layer, map)`: region sync points + the live cell rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayerJournal {
    /// Region → (epoch, last_seq).
    pub regions: BTreeMap<(i16, i16), RegionSync>,
    /// Cell → live row.
    pub cells: BTreeMap<(i16, i16), OverlayRow>,
}

/// All overlay layers of all maps.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlayJournal {
    /// `(layer, map)` → journal.
    pub layers: BTreeMap<(u8, u16), LayerJournal>,
}

/// Why an overlay journal file could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayJournalError {
    /// Wrong magic.
    BadMagic,
    /// Another format version.
    Version(u16),
    /// Cut short / corrupt.
    Truncated,
}

const MAGIC: &[u8; 4] = b"MVOV";
const VERSION: u16 = 1;

impl OverlayJournal {
    /// The journal of `(layer, map)`, if any.
    pub fn layer(&self, layer: u8, map: u16) -> Option<&LayerJournal> {
        self.layers.get(&(layer, map))
    }

    /// Apply one cell record. `true` = the cell's live value changed (a new higher seq).
    pub fn apply(
        &mut self,
        layer: u8,
        map: u16,
        cell: (i16, i16),
        row: OverlayRow,
    ) -> bool {
        let j = self.layers.entry((layer, map)).or_default();
        match j.cells.get(&cell) {
            Some(old) if old.seq >= row.seq => false,
            _ => {
                j.cells.insert(cell, row);
                true
            }
        }
    }

    /// `'Z'`: drop every cell of the region, adopt `epoch`, restart from seq 0. Returns the dropped
    /// cells (the caller rebuilds what they covered).
    pub fn reset(&mut self, layer: u8, map: u16, rx: i16, ry: i16, epoch: u16) -> Vec<(i16, i16)> {
        let j = self.layers.entry((layer, map)).or_default();
        let dropped: Vec<(i16, i16)> = j
            .cells
            .keys()
            .copied()
            .filter(|&(a, b)| cell_region(layer, a, b) == (rx, ry))
            .collect();
        for c in &dropped {
            j.cells.remove(c);
        }
        j.regions.insert((rx, ry), RegionSync { epoch, last_seq: 0 });
        dropped
    }

    /// Persist a region's sync point (`'O'`, or a live record in a synced region).
    pub fn set_region(&mut self, layer: u8, map: u16, rx: i16, ry: i16, sync: RegionSync) {
        self.layers
            .entry((layer, map))
            .or_default()
            .regions
            .insert((rx, ry), sync);
    }

    /// The persisted sync point of a region (default epoch 0 / seq 0).
    pub fn region_sync(&self, layer: u8, map: u16, rx: i16, ry: i16) -> RegionSync {
        self.layer(layer, map)
            .and_then(|j| j.regions.get(&(rx, ry)).copied())
            .unwrap_or_default()
    }

    /// The live liquid cell at world `(x, y)` on `map` (any kind, `kind 0` included = "no liquid").
    pub fn liquid_at(&self, map: u16, x: f32, y: f32) -> Option<LiquidCell> {
        self.liquid_cell(map, liquid_cell_of(x, y))
    }

    /// The live liquid cell `(cx, cy)`.
    pub fn liquid_cell(&self, map: u16, cell: (i16, i16)) -> Option<LiquidCell> {
        match self.layer(LAYER_LIQUID, map)?.cells.get(&cell)?.value {
            OverlayValue::Liquid(c) => Some(c),
            OverlayValue::Area(_) => None,
        }
    }

    /// The overriding area id at world `(x, y)` on `map` (`None` = no override, or it was removed).
    pub fn area_at(&self, map: u16, x: f32, y: f32) -> Option<u16> {
        match self.layer(LAYER_AREA, map)?.cells.get(&area_cell_of(x, y))?.value {
            OverlayValue::Area(id) if id != 0 => Some(id),
            _ => None,
        }
    }

    /// Every live liquid cell of `map`.
    pub fn liquid_cells(&self, map: u16) -> impl Iterator<Item = ((i16, i16), LiquidCell)> + '_ {
        self.layer(LAYER_LIQUID, map)
            .into_iter()
            .flat_map(|j| j.cells.iter())
            .filter_map(|(&k, r)| match r.value {
                OverlayValue::Liquid(c) => Some((k, c)),
                OverlayValue::Area(_) => None,
            })
    }

    /// Every live area override (id ≠ 0) of `map`.
    pub fn area_cells(&self, map: u16) -> impl Iterator<Item = ((i16, i16), u16)> + '_ {
        self.layer(LAYER_AREA, map)
            .into_iter()
            .flat_map(|j| j.cells.iter())
            .filter_map(|(&k, r)| match r.value {
                OverlayValue::Area(id) if id != 0 => Some((k, id)),
                _ => None,
            })
    }

    /// Serialise: `"MVOV", u16 version, u16 layer-maps`, per layer-map `u8 layer, u16 map, u32
    /// regions × (i16 rx, i16 ry, u16 epoch, u32 last_seq), u32 cells × (i16 a, i16 b, u32 seq, u16
    /// stroke, u32 actor, value)`; value = liquid `u8 kind, f32 level, u8 flags, u8 fade` or area
    /// `u16 id`. Little-endian.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&VERSION.to_le_bytes());
        b.extend_from_slice(&(self.layers.len() as u16).to_le_bytes());
        for (&(layer, map), j) in &self.layers {
            b.push(layer);
            b.extend_from_slice(&map.to_le_bytes());
            b.extend_from_slice(&(j.regions.len() as u32).to_le_bytes());
            for (&(rx, ry), r) in &j.regions {
                b.extend_from_slice(&rx.to_le_bytes());
                b.extend_from_slice(&ry.to_le_bytes());
                b.extend_from_slice(&r.epoch.to_le_bytes());
                b.extend_from_slice(&r.last_seq.to_le_bytes());
            }
            b.extend_from_slice(&(j.cells.len() as u32).to_le_bytes());
            for (&(a, c), r) in &j.cells {
                b.extend_from_slice(&a.to_le_bytes());
                b.extend_from_slice(&c.to_le_bytes());
                b.extend_from_slice(&r.seq.to_le_bytes());
                b.extend_from_slice(&r.stroke.to_le_bytes());
                b.extend_from_slice(&r.actor.to_le_bytes());
                match r.value {
                    OverlayValue::Liquid(l) => {
                        b.push(l.kind);
                        b.extend_from_slice(&l.level.to_le_bytes());
                        b.push(l.flags);
                        b.push(l.fade);
                    }
                    OverlayValue::Area(id) => b.extend_from_slice(&id.to_le_bytes()),
                }
            }
        }
        b
    }

    /// Read [`Self::encode`]'s format.
    pub fn decode(b: &[u8]) -> Result<Self, OverlayJournalError> {
        struct R<'a>(&'a [u8], usize);
        impl R<'_> {
            fn take(&mut self, n: usize) -> Result<&[u8], OverlayJournalError> {
                let s = self
                    .0
                    .get(self.1..self.1 + n)
                    .ok_or(OverlayJournalError::Truncated)?;
                self.1 += n;
                Ok(s)
            }
            fn u8(&mut self) -> Result<u8, OverlayJournalError> {
                Ok(self.take(1)?[0])
            }
            fn u16(&mut self) -> Result<u16, OverlayJournalError> {
                let s = self.take(2)?;
                Ok(u16::from_le_bytes([s[0], s[1]]))
            }
            fn i16(&mut self) -> Result<i16, OverlayJournalError> {
                Ok(self.u16()? as i16)
            }
            fn u32(&mut self) -> Result<u32, OverlayJournalError> {
                let s = self.take(4)?;
                Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            }
        }
        if b.len() < 4 || &b[..4] != MAGIC {
            return Err(OverlayJournalError::BadMagic);
        }
        let mut r = R(b, 4);
        let v = r.u16()?;
        if v != VERSION {
            return Err(OverlayJournalError::Version(v));
        }
        let mut out = OverlayJournal::default();
        for _ in 0..r.u16()? {
            let layer = r.u8()?;
            let map = r.u16()?;
            let mut j = LayerJournal::default();
            for _ in 0..r.u32()? {
                let (rx, ry) = (r.i16()?, r.i16()?);
                let epoch = r.u16()?;
                let last_seq = r.u32()?;
                j.regions.insert((rx, ry), RegionSync { epoch, last_seq });
            }
            for _ in 0..r.u32()? {
                let (a, c) = (r.i16()?, r.i16()?);
                let seq = r.u32()?;
                let stroke = r.u16()?;
                let actor = r.u32()?;
                let value = if layer == LAYER_AREA {
                    OverlayValue::Area(r.u16()?)
                } else {
                    let kind = r.u8()?;
                    let level = f32::from_bits(r.u32()?);
                    let flags = r.u8()?;
                    let fade = r.u8()?;
                    OverlayValue::Liquid(LiquidCell {
                        kind,
                        level,
                        flags,
                        fade,
                    })
                };
                j.cells.insert(
                    (a, c),
                    OverlayRow {
                        seq,
                        stroke,
                        actor,
                        value,
                    },
                );
            }
            out.layers.insert((layer, map), j);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn water(seq: u32, level: f32) -> OverlayRow {
        OverlayRow {
            seq,
            stroke: 3,
            actor: 42,
            value: OverlayValue::Liquid(LiquidCell {
                kind: KIND_WATER,
                level,
                flags: 0,
                fade: 40,
            }),
        }
    }

    fn area(seq: u32, id: u16) -> OverlayRow {
        OverlayRow {
            seq,
            stroke: 4,
            actor: 42,
            value: OverlayValue::Area(id),
        }
    }

    #[test]
    fn cell_maths_matches_the_contract() {
        // CONTRACT §11.1: cx = floor(x·6/25); the example cell (100, -20).
        assert_eq!(liquid_cell_of(418.0, -80.0), (100, -20));
        assert_eq!(liquid_cell_of(-0.01, 0.0), (-1, 0));
        assert_eq!(area_cell_of(-8911.0, -117.0), (-557, -8));
        // The OVERLAY.md e2e area cell (-557,-8) is in region (-140,-2).
        assert_eq!(cell_region(LAYER_AREA, -557, -8), (-140, -2));
        // A liquid cell whose span straddles a region edge goes by its centre.
        let (cx, _) = liquid_cell_of(63.9, 0.0); // cell 15: [62.5, 66.67], centre 64.58
        assert_eq!(cx, 15);
        assert_eq!(cell_region(LAYER_LIQUID, cx, 0), (1, 0));
    }

    #[test]
    fn lww_by_seq_and_dedupe() {
        let mut j = OverlayJournal::default();
        assert!(j.apply(LAYER_LIQUID, 0, (100, -20), water(5, 12.5)));
        assert!(!j.apply(LAYER_LIQUID, 0, (100, -20), water(5, 99.0)), "same seq = dup");
        assert!(!j.apply(LAYER_LIQUID, 0, (100, -20), water(4, 99.0)), "older seq loses");
        assert_eq!(j.liquid_at(0, 418.0, -80.0).unwrap().level, 12.5);
        assert!(j.apply(LAYER_LIQUID, 0, (100, -20), water(9, 13.0)));
        assert_eq!(j.liquid_at(0, 418.0, -80.0).unwrap().level, 13.0);
        assert_eq!(j.liquid_at(1, 418.0, -80.0), None, "other map");
        // Area: 0 = override removed → no answer.
        assert!(j.apply(LAYER_AREA, 0, (10, -10), area(6, 74)));
        assert_eq!(j.area_at(0, 170.0, -150.0), Some(74));
        assert!(j.apply(LAYER_AREA, 0, (10, -10), area(7, 0)));
        assert_eq!(j.area_at(0, 170.0, -150.0), None);
        assert_eq!(j.area_cells(0).count(), 0);
    }

    #[test]
    fn reset_drops_only_that_regions_cells_and_restarts_the_sync_point() {
        let mut j = OverlayJournal::default();
        // Cells in region (1, 0) (x 64..128) and (0, 0).
        let (a, b) = liquid_cell_of(70.0, 10.0);
        let (c, d) = liquid_cell_of(10.0, 10.0);
        j.apply(LAYER_LIQUID, 0, (a, b), water(1, 5.0));
        j.apply(LAYER_LIQUID, 0, (c, d), water(2, 5.0));
        j.apply(LAYER_AREA, 0, area_cell_of(70.0, 10.0), area(1, 87));
        j.set_region(LAYER_LIQUID, 0, 1, 0, RegionSync { epoch: 0, last_seq: 1 });
        let dropped = j.reset(LAYER_LIQUID, 0, 1, 0, 1);
        assert_eq!(dropped, vec![(a, b)]);
        assert_eq!(j.liquid_at(0, 70.0, 10.0), None);
        assert!(j.liquid_at(0, 10.0, 10.0).is_some(), "other region kept");
        assert_eq!(j.area_at(0, 70.0, 10.0), Some(87), "other layer kept");
        assert_eq!(
            j.region_sync(LAYER_LIQUID, 0, 1, 0),
            RegionSync { epoch: 1, last_seq: 0 }
        );
        // Resync replays the surviving row; an older dup after it is still ignored.
        assert!(j.apply(LAYER_LIQUID, 0, (a, b), water(1, 5.0)));
    }

    #[test]
    fn journal_round_trips() {
        let mut j = OverlayJournal::default();
        j.apply(LAYER_LIQUID, 0, (100, -20), water(5, 12.5));
        j.apply(LAYER_LIQUID, 1, (-3, 7), water(6, -2.25));
        j.apply(LAYER_AREA, 0, (-557, -8), area(1, 87));
        j.set_region(LAYER_AREA, 0, -140, -2, RegionSync { epoch: 3, last_seq: 1 });
        let back = OverlayJournal::decode(&j.encode()).unwrap();
        assert_eq!(back, j);
        assert_eq!(OverlayJournal::decode(b"MVXW"), Err(OverlayJournalError::BadMagic));
        let enc = j.encode();
        assert_eq!(
            OverlayJournal::decode(&enc[..enc.len() - 1]),
            Err(OverlayJournalError::Truncated)
        );
    }
}

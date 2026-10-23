//! MONKEY (EME overlay): an overlay liquid cell REPLACES the ADT liquid cell it covers (CONTRACT
//! v3 §11.5) — `kind 0` erases it, any other kind draws its own flat surface instead
//! (`crate::voxel::overlay`). This pass keeps every streamed ADT (MCLQ) surface's coverage mask =
//! its authored cells minus the covered ones, for the queries (swim, wade, eye, mirror, foam: one
//! [`WaterChunkInfo`]) and the drawn mesh (its index buffer; the LOD copy is rebuilt from it).
//! The original mask is kept on the entity, so undo / revert restores the baseline exactly.

use std::collections::HashSet;

use bevy::mesh::Indices;
use bevy::prelude::*;

use benilla_voxel::overlay::liquid_cell_of;

use super::lod::LiquidLod;
use super::query::WaterChunkInfo;
use super::surface::LiquidSurface;

/// The overlay liquid cells of the current map (any kind): the ADT cells under them yield.
#[derive(Resource, Default)]
pub struct MonkeyLiquidMask {
    covered: HashSet<(i16, i16)>,
    gen: u64,
}

impl MonkeyLiquidMask {
    /// Publish the covered cells; bumps the generation only on a real change.
    pub fn set(&mut self, covered: HashSet<(i16, i16)>) {
        if covered != self.covered {
            self.covered = covered;
            self.gen += 1;
        }
    }

    /// Is liquid cell `c` covered by an overlay cell?
    pub fn covers(&self, c: (i16, i16)) -> bool {
        self.covered.contains(&c)
    }
}

/// Marks a surface the overlay spawned (never masked by itself).
#[derive(Component)]
pub struct MonkeyOverlayLiquid;

/// An ADT surface's authored coverage, and the mask generation it was last fitted to.
#[derive(Component)]
pub(super) struct MonkeyAdtLiquidBase {
    wet: Vec<bool>,
    gen: u64,
}

/// The masked coverage: authored `base` cells whose centre is not under an overlay cell.
pub(super) fn masked(base: &[bool], centres: &[[f32; 2]], mask: &MonkeyLiquidMask) -> Vec<bool> {
    base.iter()
        .zip(centres)
        .map(|(&w, c)| w && !mask.covers(liquid_cell_of(c[0], c[1])))
        .collect()
}

/// The triangle list of a `cols × rows` grid's wet cells (the MCLQ winding,
/// `benilla_formats::LiquidMesh::retain_cells`). An all-dry grid keeps one degenerate triangle so
/// the index buffer is never empty.
fn grid_indices(cols: usize, rows: usize, wet: &[bool]) -> Vec<u32> {
    let (xt, yt) = (cols.saturating_sub(1), rows.saturating_sub(1));
    let mut out = Vec::new();
    for ty in 0..yt {
        for tx in 0..xt {
            if !wet.get(ty * xt + tx).copied().unwrap_or(false) {
                continue;
            }
            let tl = (ty * cols + tx) as u32;
            let (tr, bl) = (tl + 1, ((ty + 1) * cols + tx) as u32);
            let br = bl + 1;
            out.extend_from_slice(&[tl, bl, br, tl, br, tr]);
        }
    }
    if out.is_empty() {
        out.extend_from_slice(&[0, 0, 0]);
    }
    out
}

type AdtSurface = (
    Entity,
    &'static mut WaterChunkInfo,
    &'static mut Mesh3d,
    Option<&'static mut LiquidLod>,
    Option<&'static mut MonkeyAdtLiquidBase>,
);

/// Fit every ADT surface to the current mask (only surfaces whose fitted generation is stale; with
/// no overlay at all, untouched surfaces cost one branch).
pub(super) fn monkey_mask_adt_liquid(
    mask: Res<MonkeyLiquidMask>,
    mut commands: Commands,
    mut surfaces: Query<AdtSurface, (With<LiquidSurface>, Without<MonkeyOverlayLiquid>)>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    for (entity, mut info, mut mesh3d, lod, base) in &mut surfaces {
        if !info.is_adt() {
            continue;
        }
        let base_wet = match &base {
            Some(b) if b.gen == mask.gen => continue,
            Some(b) => b.wet.clone(),
            None if mask.covered.is_empty() => continue,
            None => info.wet_mask().to_vec(),
        };
        let new = masked(&base_wet, &info.cell_centres(), &mask);
        if new.as_slice() != info.wet_mask() {
            let [cols, rows] = info.grid_dims();
            info.set_wet_mask(new.clone());
            let coarse = match lod {
                Some(mut lod) => {
                    let coarse = lod.monkey_reset_wet(new.clone(), &mut meshes);
                    mesh3d.0 = coarse.clone();
                    coarse
                }
                None => mesh3d.0.clone(),
            };
            if let Some(m) = meshes.get_mut(&coarse) {
                m.insert_indices(Indices::U32(grid_indices(cols, rows, &new)));
            }
        }
        match base {
            Some(mut b) => b.gen = mask.gen,
            None => {
                commands.entity(entity).insert(MonkeyAdtLiquidBase {
                    wet: base_wet,
                    gen: mask.gen,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_drops_only_covered_cells_and_indices_follow() {
        let mut mask = MonkeyLiquidMask::default();
        // Two cells, centres in liquid cells (100, -20) and (101, -20).
        let centres = [[418.0, -80.0], [422.5, -80.0]];
        assert_eq!(liquid_cell_of(422.5, -80.0), (101, -20));
        let base = [true, true];
        assert_eq!(masked(&base, &centres, &mask), vec![true, true]);
        mask.set([(101, -20)].into_iter().collect());
        assert_eq!(mask.gen, 1);
        assert_eq!(masked(&base, &centres, &mask), vec![true, false]);
        mask.set([(101, -20)].into_iter().collect());
        assert_eq!(mask.gen, 1, "no change, no new generation");
        // A 3×2 vertex grid (2×1 cells): one cell → 6 indices, none → one degenerate triangle.
        assert_eq!(grid_indices(3, 2, &[true, false]), vec![0, 3, 4, 0, 4, 1]);
        assert_eq!(grid_indices(3, 2, &[false, false]), vec![0, 0, 0]);
        // Undo: the mask empties, the base comes back whole.
        mask.set(HashSet::new());
        assert_eq!(masked(&base, &centres, &mask), vec![true, true]);
    }
}

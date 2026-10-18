//! MONKEY (voxel): exact footprint clipping of MCNK chunks under a voxel build site.
//!
//! A build site REPLACES the real ground under its footprint. MCNK holes are 2×2-cell blocks
//! (8.33 yd), far coarser than the 32-yd site, so a block-hole carve leaves an un-carved rim inside
//! the footprint where a dig is covered by the real ground drawn over it. Instead every chunk the
//! footprint overlaps is CLIPPED: all triangle area strictly inside the carve rectangle is dropped
//! and everything outside is kept exactly (the original MCNK holes stay holes — the walk starts from
//! `chunk.indices`).
//!
//! The outside of the rectangle is split into four disjoint convex regions — `x ≤ x0`, `x ≥ x1`,
//! and the middle column's `y ≤ y0` / `y ≥ y1` — and each straddling triangle is Sutherland–Hodgman
//! split against them and fan-triangulated, keeping its winding. A new vertex interpolates every
//! per-vertex attribute a [`ChunkMesh`] carries (position incl. height, MCNR normal, chunk UV); the
//! rest of the mesh data the renderer bakes is per CHUNK (layer / alpha / shadow array indices in
//! `benilla_assets::chunks_to_mesh`), and the alpha / shadow / layer maps are sampled through the
//! chunk UV, so an appended vertex shades exactly like the surface it lies on. Appended vertices go
//! AFTER the original 145, so `positions[0]` (the chunk's NW corner every footprint lookup reads)
//! is untouched.

use benilla_formats::ChunkMesh;

/// A WoW-yard rectangle `[x0, x1] × [y0, y1]` (raw WoW X north, Y west) whose interior the real
/// ground is carved out of: the build site's footprint inset by its edge overlap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelCarveRect {
    pub x0: f32,
    pub x1: f32,
    pub y0: f32,
    pub y1: f32,
}

impl VoxelCarveRect {
    /// Whether the column `(x, y)` lies STRICTLY inside (the carve's boundary keeps its ground).
    pub fn contains_strict(&self, x: f32, y: f32) -> bool {
        x > self.x0 && x < self.x1 && y > self.y0 && y < self.y1
    }

    /// Whether the XY box `[lo, hi]` reaches the rectangle's open interior.
    fn box_overlaps(&self, lo: [f32; 2], hi: [f32; 2]) -> bool {
        hi[0] > self.x0 && lo[0] < self.x1 && hi[1] > self.y0 && lo[1] < self.y1
    }

    /// Whether any of `chunk`'s vertices' XY box reaches the interior — a chunk that doesn't is
    /// drawn untouched.
    pub fn overlaps_chunk(&self, chunk: &ChunkMesh) -> bool {
        let Some(first) = chunk.positions.first() else {
            return false;
        };
        let (mut lo, mut hi) = ([first[0], first[1]], [first[0], first[1]]);
        for p in &chunk.positions {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        self.box_overlaps(lo, hi)
    }

    /// A non-zero signature of the exact rectangle (FNV-1a over its bits): a moved footprint
    /// re-carves, an unchanged one doesn't.
    pub fn sig(&self) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        for v in [self.x0, self.x1, self.y0, self.y1] {
            for b in v.to_bits().to_le_bytes() {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
        }
        h | 1
    }
}

/// One clip-space vertex: everything a `ChunkMesh` stores per vertex.
#[derive(Clone, Copy)]
struct V {
    p: [f32; 3],
    n: [f32; 3],
    uv: [f32; 2],
}

/// The point where edge `a`–`b` crosses `axis = value`. The endpoints are ordered canonically
/// first, so the edge shared by two neighbouring triangles splits at the bit-identical point from
/// either side (no T-crack), and the crossing coordinate is snapped exactly onto the line.
fn cross(a: &V, b: &V, axis: usize, value: f32) -> V {
    let (a, b) = if (a.p[0], a.p[1], a.p[2]) <= (b.p[0], b.p[1], b.p[2]) {
        (a, b)
    } else {
        (b, a)
    };
    let d = b.p[axis] - a.p[axis];
    let t = if d.abs() > f32::EPSILON {
        ((value - a.p[axis]) / d).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let l = |x: f32, y: f32| x + (y - x) * t;
    let mut p = [l(a.p[0], b.p[0]), l(a.p[1], b.p[1]), l(a.p[2], b.p[2])];
    p[axis] = value;
    let n = [l(a.n[0], b.n[0]), l(a.n[1], b.n[1]), l(a.n[2], b.n[2])];
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    let n = if len > 1.0e-6 {
        [n[0] / len, n[1] / len, n[2] / len]
    } else {
        n
    };
    V {
        p,
        n,
        uv: [l(a.uv[0], b.uv[0]), l(a.uv[1], b.uv[1])],
    }
}

/// Split a convex polygon by the line `axis = value` into its `≤` and `≥` halves (both convex,
/// order — hence winding — preserved). A vertex on the line goes to both.
fn split(poly: &[V], axis: usize, value: f32) -> (Vec<V>, Vec<V>) {
    let (mut below, mut above) = (Vec::new(), Vec::new());
    let n = poly.len();
    for i in 0..n {
        let (a, b) = (&poly[i], &poly[(i + 1) % n]);
        let (da, db) = (a.p[axis] - value, b.p[axis] - value);
        if da <= 0.0 {
            below.push(*a);
        }
        if da >= 0.0 {
            above.push(*a);
        }
        if (da < 0.0 && db > 0.0) || (da > 0.0 && db < 0.0) {
            let m = cross(a, b, axis, value);
            below.push(m);
            above.push(m);
        }
    }
    (below, above)
}

/// Twice the signed XY area of a polygon.
fn area2(poly: &[V]) -> f32 {
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (a, b) = (poly[i].p, poly[(i + 1) % n].p);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum()
}

/// Below this (yd², doubled) a clipped piece is a numerical sliver and is dropped.
const SLIVER: f32 = 1.0e-6;

/// The parts of triangle `tri` OUTSIDE the rectangle's interior, as convex polygons in the
/// triangle's own winding: the four disjoint regions left / right / middle-low / middle-high.
fn outside_parts(tri: [V; 3], r: &VoxelCarveRect) -> Vec<Vec<V>> {
    let (left, rest) = split(&tri, 0, r.x0);
    let (mid, right) = split(&rest, 0, r.x1);
    let (low, rest) = split(&mid, 1, r.y0);
    let (_inside, high) = split(&rest, 1, r.y1);
    [left, right, low, high]
        .into_iter()
        .filter(|p| p.len() >= 3 && area2(p).abs() > SLIVER)
        .collect()
}

/// `chunk` with every triangle's area strictly inside `rect` removed; `None` when the chunk doesn't
/// reach the rectangle (draw / weld the original). Triangles wholly outside keep their original
/// indices; straddling ones are re-triangulated over appended vertices; ones wholly inside go.
pub fn clip_chunk_outside_rect(chunk: &ChunkMesh, rect: &VoxelCarveRect) -> Option<ChunkMesh> {
    if !rect.overlaps_chunk(chunk) {
        return None;
    }
    let np = chunk.positions.len();
    let has_n = chunk.normals.len() == np;
    let has_uv = chunk.uvs.len() == np;
    let vert = |i: usize| V {
        p: chunk.positions[i],
        n: if has_n { chunk.normals[i] } else { [0.0; 3] },
        uv: if has_uv { chunk.uvs[i] } else { [0.0; 2] },
    };
    let mut out = chunk.clone();
    out.indices = Vec::with_capacity(chunk.indices.len());
    // Appended vertices, deduplicated by exact position so a split edge is welded across the two
    // triangles that share it.
    let mut added: std::collections::HashMap<[u32; 3], u32> = std::collections::HashMap::new();
    for t in chunk.indices.as_chunks::<3>().0 {
        if t.iter().any(|&i| i as usize >= np) {
            continue;
        }
        let tri = [
            vert(t[0] as usize),
            vert(t[1] as usize),
            vert(t[2] as usize),
        ];
        let lo = [
            tri.iter().map(|v| v.p[0]).fold(f32::INFINITY, f32::min),
            tri.iter().map(|v| v.p[1]).fold(f32::INFINITY, f32::min),
        ];
        let hi = [
            tri.iter().map(|v| v.p[0]).fold(f32::NEG_INFINITY, f32::max),
            tri.iter().map(|v| v.p[1]).fold(f32::NEG_INFINITY, f32::max),
        ];
        if !rect.box_overlaps(lo, hi) {
            out.indices.extend_from_slice(t); // wholly outside: exactly the original
            continue;
        }
        if lo[0] >= rect.x0 && hi[0] <= rect.x1 && lo[1] >= rect.y0 && hi[1] <= rect.y1 {
            continue; // wholly inside the (closed) rectangle: carved
        }
        for part in outside_parts(tri, rect) {
            let mut ids: Vec<u32> = Vec::with_capacity(part.len());
            for v in &part {
                let orig = (0..3).find(|&k| tri[k].p == v.p).map(|k| t[k]);
                let id = orig.unwrap_or_else(|| {
                    let key = [v.p[0].to_bits(), v.p[1].to_bits(), v.p[2].to_bits()];
                    *added.entry(key).or_insert_with(|| {
                        out.positions.push(v.p);
                        if has_n {
                            out.normals.push(v.n);
                        }
                        if has_uv {
                            out.uvs.push(v.uv);
                        }
                        (out.positions.len() - 1) as u32
                    })
                });
                ids.push(id);
            }
            // Fan-triangulate the convex piece, skipping degenerate fan triangles.
            for k in 1..part.len() - 1 {
                if area2(&[part[0], part[k], part[k + 1]]).abs() <= SLIVER {
                    continue;
                }
                out.indices.extend_from_slice(&[ids[0], ids[k], ids[k + 1]]);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use benilla_formats::{chunk_indices_holed, CHUNK_SIZE};

    /// A flat 145-vertex chunk whose NW corner is WoW (100, 100), with chunk UVs and up normals.
    fn flat_chunk(holes: u16) -> ChunkMesh {
        let step = CHUNK_SIZE / 8.0;
        let mut positions = Vec::with_capacity(145);
        let mut uvs = Vec::with_capacity(145);
        for row in 0..9 {
            for col in 0..9 {
                positions.push([100.0 - row as f32 * step, 100.0 - col as f32 * step, 5.0]);
                uvs.push([col as f32 / 8.0, row as f32 / 8.0]);
            }
            if row < 8 {
                for col in 0..8 {
                    positions.push([
                        100.0 - (row as f32 + 0.5) * step,
                        100.0 - (col as f32 + 0.5) * step,
                        5.0,
                    ]);
                    uvs.push([(col as f32 + 0.5) / 8.0, (row as f32 + 0.5) / 8.0]);
                }
            }
        }
        ChunkMesh {
            normals: vec![[0.0, 0.0, 1.0]; positions.len()],
            positions,
            uvs,
            indices: chunk_indices_holed(holes),
            holes,
            base_texture: None,
            layer_textures: Vec::new(),
            layer_effect_ids: Vec::new(),
            alpha_map: None,
            shadow: None,
            pred_tex: [0; 64],
            no_effect_doodad: [false; 64],
            index_x: 0,
            index_y: 0,
            area_id: 0,
            impassable: false,
            liquids: Vec::new(),
        }
    }

    fn tris(c: &ChunkMesh) -> Vec<[[f32; 3]; 3]> {
        c.indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| t.map(|i| c.positions[i as usize]))
            .collect()
    }

    fn tri_area(t: &[[f32; 3]; 3]) -> f32 {
        let (a, b, c) = (t[0], t[1], t[2]);
        ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])) * 0.5
    }

    /// A rect that cuts the chunk off-grid (not on any vertex line) and spills past its south-east.
    fn rect() -> VoxelCarveRect {
        VoxelCarveRect {
            x0: 100.0 - CHUNK_SIZE - 3.0,
            x1: 100.0 - 7.3,
            y0: 100.0 - CHUNK_SIZE - 5.0,
            y1: 100.0 - 11.9,
        }
    }

    #[test]
    fn no_kept_triangle_reaches_inside_the_rect() {
        let r = rect();
        let c = clip_chunk_outside_rect(&flat_chunk(0), &r).expect("overlaps");
        for t in tris(&c) {
            // Every point of a kept triangle (corners and centroid) is outside the open interior.
            for p in t {
                assert!(!r.contains_strict(p[0], p[1]), "vertex {p:?} inside");
            }
            let cx = (t[0][0] + t[1][0] + t[2][0]) / 3.0;
            let cy = (t[0][1] + t[1][1] + t[2][1]) / 3.0;
            assert!(!r.contains_strict(cx, cy), "triangle {t:?} reaches inside");
        }
    }

    #[test]
    fn kept_area_is_chunk_minus_overlap_and_winding_is_kept() {
        let r = rect();
        let src = flat_chunk(0);
        let c = clip_chunk_outside_rect(&src, &r).expect("overlaps");
        let chunk_area: f32 = tris(&src).iter().map(|t| tri_area(t).abs()).sum();
        let kept: f32 = tris(&c).iter().map(|t| tri_area(t).abs()).sum();
        // The chunk spans X/Y [100 - CHUNK_SIZE, 100]; the overlap is the rect ∩ that square.
        let lo = 100.0 - CHUNK_SIZE;
        let ox = r.x1.min(100.0) - r.x0.max(lo);
        let oy = r.y1.min(100.0) - r.y0.max(lo);
        let expect = chunk_area - ox * oy;
        assert!(
            (kept - expect).abs() < 1.0e-3 * chunk_area.max(1.0),
            "kept {kept} expected {expect}"
        );
        // Winding: every kept triangle faces the same way as the source's.
        let sign = tri_area(&tris(&src)[0]).signum();
        for t in tris(&c) {
            assert_eq!(tri_area(&t).signum(), sign, "flipped {t:?}");
        }
        // Appended vertices carry every per-vertex attribute.
        assert_eq!(c.normals.len(), c.positions.len());
        assert_eq!(c.uvs.len(), c.positions.len());
        assert_eq!(c.positions[0], src.positions[0], "NW corner stays vertex 0");
        // A new vertex's UV is the chunk-local position (the flat chunk's uv = offset / size).
        for (p, uv) in c.positions.iter().zip(&c.uvs).skip(145) {
            let u = (100.0 - p[1]) / CHUNK_SIZE;
            let v = (100.0 - p[0]) / CHUNK_SIZE;
            assert!((uv[0] - u).abs() < 1.0e-4 && (uv[1] - v).abs() < 1.0e-4);
        }
    }

    #[test]
    fn original_holes_stay_holes() {
        // Block 0 (NW corner) lies wholly outside the rect, block 3 (north row, east end) straddles
        // its north edge, block 15 (SE) is wholly inside it.
        let holes = (1u16 << 3) | (1u16 << 15) | 1;
        let r = rect();
        let src = flat_chunk(holes);
        let c = clip_chunk_outside_rect(&src, &r).expect("overlaps");
        let b = CHUNK_SIZE / 4.0;
        for t in tris(&c) {
            let cx = (t[0][0] + t[1][0] + t[2][0]) / 3.0;
            let cy = (t[0][1] + t[1][1] + t[2][1]) / 3.0;
            let (row, col) = (((100.0 - cx) / b) as u32, ((100.0 - cy) / b) as u32);
            let bit = row.min(3) * 4 + col.min(3);
            assert_eq!(holes & (1 << bit), 0, "triangle {t:?} in hole block {bit}");
        }
        // And the kept area also loses the holes' area outside the rect (block 0 + part of 3).
        let full = clip_chunk_outside_rect(&flat_chunk(0), &r).unwrap();
        let a = |c: &ChunkMesh| tris(c).iter().map(|t| tri_area(t).abs()).sum::<f32>();
        assert!(a(&c) < a(&full) - b * b * 1.5, "holes dropped area");
    }

    #[test]
    fn a_chunk_the_rect_does_not_reach_is_left_alone() {
        let r = VoxelCarveRect {
            x0: 0.0,
            x1: 10.0,
            y0: 0.0,
            y1: 10.0,
        };
        assert!(clip_chunk_outside_rect(&flat_chunk(0), &r).is_none());
    }
}

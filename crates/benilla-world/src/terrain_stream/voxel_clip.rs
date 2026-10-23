//! MONKEY (voxel): exact clipping of MCNK chunks under the voxel world layer's carve.
//!
//! The voxel layer REPLACES the real ground under its carve. MCNK holes are 2×2-cell blocks
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
/// ground is carved out of: one carved column, inset by the edge overlap on outer sides.
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

/// MONKEY (voxel): the v2 carve — the UNION of the world voxel layer's surface columns (16 × 16 yd,
/// `(kx, ky) = floor(wow_xy / 16)`), each inset by `inset` ONLY on the sides whose neighbour column
/// is not carved too. Internal edges between two carved columns are therefore shared exactly (no
/// slit), the union's outer boundary keeps `inset` of real ground under the voxel skirt (CONTRACT
/// §1). Clipping a chunk applies every rectangle reaching it in turn.
///
/// MONKEY (voxel fix): a CONCAVE corner of the union — both side neighbours carved, the diagonal
/// one not — is notched by `inset` × `inset` too. The voxel mesh there needs the missing diagonal
/// chunk's quad around the shared corner, so without the notch the carve removed ground nobody
/// covers (a small sky hole at every inner corner of an L-shaped site). A notched column is
/// split into up to three bands along x; every side between two pieces of the union (a column
/// edge with a carved neighbour, or a band seam) is INCLUSIVE in [`Self::contains_strict`].
#[derive(Clone, Debug, Default)]
pub struct VoxelCarve {
    rects: Vec<VoxelCarveRect>,
    /// Per rect: whether its `x0, x1, y0, y1` side is internal to the union (inclusive).
    incl: Vec<[bool; 4]>,
    /// Column → its rects `start..end`.
    by_column: std::collections::HashMap<(i32, i32), (usize, usize)>,
    column_yards: f32,
    sig: u64,
    columns: usize,
}

impl VoxelCarve {
    /// The carve of a set of surface columns. `column_yards` = 16 (the chunk edge), `inset` = the
    /// outer-boundary overlap.
    pub fn from_columns(
        columns: impl IntoIterator<Item = (i32, i32)>,
        column_yards: f32,
        inset: f32,
    ) -> Self {
        let mut cols: Vec<(i32, i32)> = columns.into_iter().collect();
        cols.sort_unstable();
        cols.dedup();
        let set: std::collections::HashSet<(i32, i32)> = cols.iter().copied().collect();
        let mut rects = Vec::with_capacity(cols.len());
        let mut incl = Vec::with_capacity(cols.len());
        let mut by_column = std::collections::HashMap::with_capacity(cols.len());
        let mut h = 0xcbf29ce484222325u64;
        for &(kx, ky) in &cols {
            let has = |dx: i32, dy: i32| set.contains(&(kx + dx, ky + dy));
            let ins = |dx: i32, dy: i32| if has(dx, dy) { 0.0 } else { inset };
            let (cx0, cy0) = (kx as f32 * column_yards, ky as f32 * column_yards);
            let (cx1, cy1) = (cx0 + column_yards, cy0 + column_yards);
            let (x0, x1) = (cx0 + ins(-1, 0), cx1 - ins(1, 0));
            let (y0, y1) = (cy0 + ins(0, -1), cy1 - ins(0, 1));
            // A notch where both side neighbours are carved but the diagonal is not.
            let notch = |dx: i32, dy: i32| has(dx, 0) && has(0, dy) && !has(dx, dy);
            let (n_mm, n_mp, n_pm, n_pp) = (notch(-1, -1), notch(-1, 1), notch(1, -1), notch(1, 1));
            let start = rects.len();
            let mut push = |r: VoxelCarveRect, i: [bool; 4]| {
                if r.x1 > r.x0 && r.y1 > r.y0 {
                    h = (h ^ r.sig()).wrapping_mul(0x100000001b3);
                    rects.push(r);
                    incl.push(i);
                }
            };
            let (in_y0, in_y1) = (has(0, -1), has(0, 1));
            let mx0 = if n_mm || n_mp { x0 + inset } else { x0 };
            let mx1 = if n_pm || n_pp { x1 - inset } else { x1 };
            if mx0 > x0 {
                let (lo, hi) = (
                    if n_mm { y0 + inset } else { y0 },
                    if n_mp { y1 - inset } else { y1 },
                );
                push(
                    VoxelCarveRect {
                        x0,
                        x1: mx0,
                        y0: lo,
                        y1: hi,
                    },
                    [has(-1, 0), true, in_y0 && !n_mm, in_y1 && !n_mp],
                );
            }
            push(
                VoxelCarveRect {
                    x0: mx0,
                    x1: mx1,
                    y0,
                    y1,
                },
                // A band seam belongs to the NOTCHED band (inclusive there), so the notch's own
                // edge on the seam line stays outside.
                [
                    mx0 == x0 && has(-1, 0),
                    mx1 == x1 && has(1, 0),
                    in_y0,
                    in_y1,
                ],
            );
            if mx1 < x1 {
                let (lo, hi) = (
                    if n_pm { y0 + inset } else { y0 },
                    if n_pp { y1 - inset } else { y1 },
                );
                push(
                    VoxelCarveRect {
                        x0: mx1,
                        x1,
                        y0: lo,
                        y1: hi,
                    },
                    [true, has(1, 0), in_y0 && !n_pm, in_y1 && !n_pp],
                );
            }
            by_column.insert((kx, ky), (start, rects.len()));
        }
        Self {
            rects,
            incl,
            by_column,
            column_yards,
            sig: if cols.is_empty() { 0 } else { h | 1 },
            columns: cols.len(),
        }
    }

    /// How many surface columns the union holds.
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Whether nothing is carved.
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// The per-column rectangles.
    pub fn rects(&self) -> &[VoxelCarveRect] {
        &self.rects
    }

    /// A non-zero signature of the whole carve (0 when empty).
    pub fn sig(&self) -> u64 {
        self.sig
    }

    /// Whether the column `(x, y)` lies strictly inside the union (a rect never leaves its own
    /// column, so the column under the point decides). MONKEY (voxel): strict only on the union's
    /// OUTER boundary — a side shared with another piece of the union (a carved neighbour column,
    /// a notch band seam) is inclusive, or a point lying exactly on it would answer the old ADT
    /// height (a zero-width slit in the height query).
    pub fn contains_strict(&self, x: f32, y: f32) -> bool {
        if self.rects.is_empty() {
            return false;
        }
        let cy = self.column_yards;
        let (kx, ky) = ((x / cy).floor() as i32, (y / cy).floor() as i32);
        let Some(&(a, b)) = self.by_column.get(&(kx, ky)) else {
            return false;
        };
        let lo = |v: f32, side: f32, inc: bool| v > side || (inc && v == side);
        let hi = |v: f32, side: f32, inc: bool| v < side || (inc && v == side);
        (a..b).any(|i| {
            let (r, f) = (&self.rects[i], self.incl[i]);
            lo(x, r.x0, f[0]) && hi(x, r.x1, f[1]) && lo(y, r.y0, f[2]) && hi(y, r.y1, f[3])
        })
    }

    /// The rectangles whose interior reaches the XY box `[lo, hi]`.
    pub fn overlapping(&self, lo: [f32; 2], hi: [f32; 2]) -> impl Iterator<Item = &VoxelCarveRect> {
        self.rects.iter().filter(move |r| r.box_overlaps(lo, hi))
    }

    /// A signature of the rectangles reaching `[lo, hi]` (0 = none): a render cell / tile whose
    /// value is unchanged keeps its clip when columns elsewhere are added or removed.
    pub fn sig_for_box(&self, lo: [f32; 2], hi: [f32; 2]) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        let mut any = false;
        for r in self.overlapping(lo, hi) {
            h = (h ^ r.sig()).wrapping_mul(0x100000001b3);
            any = true;
        }
        if any {
            h | 1
        } else {
            0
        }
    }

    /// `chunk` with every triangle's area strictly inside the union removed; `None` when no
    /// rectangle reaches it (draw / weld the original).
    pub fn clip_chunk(&self, chunk: &ChunkMesh) -> Option<ChunkMesh> {
        let first = chunk.positions.first()?;
        let (mut lo, mut hi) = ([first[0], first[1]], [first[0], first[1]]);
        for p in &chunk.positions {
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
        }
        let mut out: Option<ChunkMesh> = None;
        for r in self.overlapping(lo, hi) {
            let src = out.as_ref().unwrap_or(chunk);
            if let Some(c) = clip_chunk_outside_rect(src, r) {
                out = Some(c);
            }
        }
        if let Some(c) = out.as_mut() {
            close_t_junctions(c, chunk.positions.len());
        }
        out
    }

    /// MONKEY (voxel fix): [`Self::clip_chunk`] for DRAWING — the clipped chunk plus a vertical
    /// [`CARVE_SKIRT`] curtain hung under every kept edge that borders the carve (see
    /// [`add_carve_skirts`]). The collider / height / torch paths keep the plain clip.
    pub fn clip_chunk_drawn(&self, chunk: &ChunkMesh) -> Option<ChunkMesh> {
        let mut out = self.clip_chunk(chunk)?;
        add_carve_skirts(&mut out, |x, y| self.contains_strict(x, y), CARVE_SKIRT);
        Some(out)
    }
}

/// MONKEY (voxel fix): weld the T-junctions the clip leaves inside a chunk. The half-plane splits
/// run each cut line across the WHOLE straddling triangle, so past a rectangle's corner a cut vertex
/// lands on an edge that the neighbouring piece / triangle keeps whole (e.g. a right piece's long
/// edge on `x = x1` beside the low piece's short one): a T-junction, which rasterises with
/// hairline pinholes. Every kept triangle with a vertex appended by the clip (index ≥ `first_new`)
/// lying strictly inside one of its edges is re-fanned around its centroid (a new interior vertex,
/// attributes interpolated), so every such edge is split exactly where its neighbour's is. The
/// winding is the triangle's own.
fn close_t_junctions(chunk: &mut ChunkMesh, first_new: usize) {
    let np = chunk.positions.len();
    if np <= first_new {
        return;
    }
    let has_n = chunk.normals.len() == np;
    let has_uv = chunk.uvs.len() == np;
    // Appended vertices, deduplicated by position (two passes may append the same point twice).
    let mut cand: Vec<u32> = Vec::new();
    let mut seen: std::collections::HashSet<[u32; 3]> = std::collections::HashSet::new();
    for i in first_new..np {
        let p = chunk.positions[i];
        if seen.insert([p[0].to_bits(), p[1].to_bits(), p[2].to_bits()]) {
            cand.push(i as u32);
        }
    }
    let tris: Vec<[u32; 3]> = chunk
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .copied()
        .filter(|t| t.iter().all(|&i| (i as usize) < np))
        .collect();
    let mut indices: Vec<u32> = Vec::with_capacity(chunk.indices.len());
    for t in tris {
        let pos = |i: u32| chunk.positions[i as usize];
        // Per edge k (t[k] → t[k+1]): the candidates strictly inside it, by their parameter.
        let mut on_edge: [Vec<(f32, u32)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut any = false;
        for k in 0..3 {
            let (a, b) = (pos(t[k]), pos(t[(k + 1) % 3]));
            let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
            let l2 = ex * ex + ey * ey;
            if l2 < 1.0e-10 {
                continue;
            }
            let len = l2.sqrt();
            for &v in &cand {
                if t.contains(&v) {
                    continue;
                }
                let p = pos(v);
                if p == a || p == b {
                    continue;
                }
                let (px, py) = (p[0] - a[0], p[1] - a[1]);
                let s = (px * ex + py * ey) / l2;
                if s <= 1.0e-5 || s >= 1.0 - 1.0e-5 {
                    continue;
                }
                // Off the edge's line in XY, or in height.
                if (px * ey - py * ex).abs() / len > 1.0e-3 {
                    continue;
                }
                if (a[2] + (b[2] - a[2]) * s - p[2]).abs() > 1.0e-2 {
                    continue;
                }
                on_edge[k].push((s, v));
                any = true;
            }
            on_edge[k].sort_by(|x, y| x.0.total_cmp(&y.0));
        }
        if !any {
            indices.extend_from_slice(&t);
            continue;
        }
        let mut ring: Vec<u32> = Vec::with_capacity(6);
        for k in 0..3 {
            ring.push(t[k]);
            ring.extend(on_edge[k].iter().map(|&(_, v)| v));
        }
        let (a, b, c) = (pos(t[0]), pos(t[1]), pos(t[2]));
        let centre = (0..3)
            .map(|i| (a[i] + b[i] + c[i]) / 3.0)
            .collect::<Vec<_>>();
        chunk.positions.push([centre[0], centre[1], centre[2]]);
        if has_n {
            let (na, nb, nc) = (
                chunk.normals[t[0] as usize],
                chunk.normals[t[1] as usize],
                chunk.normals[t[2] as usize],
            );
            let n = [
                na[0] + nb[0] + nc[0],
                na[1] + nb[1] + nc[1],
                na[2] + nb[2] + nc[2],
            ];
            let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1.0e-6);
            chunk.normals.push([n[0] / l, n[1] / l, n[2] / l]);
        }
        if has_uv {
            let (ua, ub, uc) = (
                chunk.uvs[t[0] as usize],
                chunk.uvs[t[1] as usize],
                chunk.uvs[t[2] as usize],
            );
            chunk.uvs.push([
                (ua[0] + ub[0] + uc[0]) / 3.0,
                (ua[1] + ub[1] + uc[1]) / 3.0,
            ]);
        }
        let ci = (chunk.positions.len() - 1) as u32;
        for i in 0..ring.len() {
            indices.extend_from_slice(&[ci, ring[i], ring[(i + 1) % ring.len()]]);
        }
    }
    chunk.indices = indices;
}

/// MONKEY (voxel fix): how far (yd) the drawn ADT curtain hangs below the carve's cut edge.
///
/// The voxel layer is drawn `RENDER_DROP` (0.17 yd) under its density surface, i.e. ~0.02 yd UNDER
/// the real ground plus the surface-nets error, and its unedited fragments stop 0.1 yd past the
/// carve region (`BAND_DISCARD`). Seen from INSIDE the carve at a grazing angle, a ray that clears
/// the voxel top reaches the cut ADT edge in the slit BETWEEN the two sheets, runs on under the
/// ADT (backface-culled from below) far beyond the 0.1-yd overlap before it has fallen the few cm
/// to the voxel plane, and finds nothing: the sky showed through as a dashed hairline along the
/// carve's outer edge (owner, Northshire 2026-10-23). The curtain closes that slit; it sits under
/// the kept ground and faces the carve, so it is only ever seen edge-on.
pub const CARVE_SKIRT: f32 = 0.3;

/// Hang a `depth`-yd vertical curtain under every BOUNDARY edge of `chunk` (an edge only one kept
/// triangle uses) whose open side lies in the carve (`carved(x, y)`, tested just past the edge's
/// midpoint) — the cut edges, not the chunk's outer border or an original MCNK hole. Each curtain
/// quad copies its edge's normals and chunk UVs (it shades like the ground it hangs from) and
/// faces the carve: its winding is the kept triangle's folded down about the edge, reversed.
pub fn add_carve_skirts(chunk: &mut ChunkMesh, carved: impl Fn(f32, f32) -> bool, depth: f32) {
    let np = chunk.positions.len();
    let key = |p: [f32; 3]| [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()];
    let ekey = |a: [f32; 3], b: [f32; 3]| {
        let (ka, kb) = (key(a), key(b));
        if ka <= kb {
            (ka, kb)
        } else {
            (kb, ka)
        }
    };
    let tris: Vec<[u32; 3]> = chunk
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .copied()
        .filter(|t| t.iter().all(|&i| (i as usize) < np))
        .collect();
    let mut uses: std::collections::HashMap<([u32; 3], [u32; 3]), u32> =
        std::collections::HashMap::new();
    for t in &tris {
        for k in 0..3 {
            let (a, b) = (
                chunk.positions[t[k] as usize],
                chunk.positions[t[(k + 1) % 3] as usize],
            );
            *uses.entry(ekey(a, b)).or_default() += 1;
        }
    }
    let has_n = chunk.normals.len() == np;
    let has_uv = chunk.uvs.len() == np;
    // One lowered copy per skirted vertex, so adjacent curtain quads share their verticals.
    let mut low: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut new_idx: Vec<u32> = Vec::new();
    for t in &tris {
        for k in 0..3 {
            let (ia, ib, ic) = (t[k], t[(k + 1) % 3], t[(k + 2) % 3]);
            let (a, b, c) = (
                chunk.positions[ia as usize],
                chunk.positions[ib as usize],
                chunk.positions[ic as usize],
            );
            if uses.get(&ekey(a, b)).copied() != Some(1) {
                continue;
            }
            // The open side: perpendicular to a→b in XY, away from the third vertex.
            let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
            let len = (ex * ex + ey * ey).sqrt();
            if len < 1.0e-4 {
                continue;
            }
            let (mut nx, mut ny) = (-ey / len, ex / len);
            let (mx, my) = ((a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5);
            if nx * (c[0] - mx) + ny * (c[1] - my) > 0.0 {
                (nx, ny) = (-nx, -ny);
            }
            const PROBE: f32 = 0.05;
            if !carved(mx + nx * PROBE, my + ny * PROBE) {
                continue;
            }
            let mut lower = |i: u32| -> u32 {
                *low.entry(i).or_insert_with(|| {
                    let p = chunk.positions[i as usize];
                    chunk.positions.push([p[0], p[1], p[2] - depth]);
                    if has_n {
                        let n = chunk.normals[i as usize];
                        chunk.normals.push(n);
                    }
                    if has_uv {
                        let uv = chunk.uvs[i as usize];
                        chunk.uvs.push(uv);
                    }
                    (chunk.positions.len() - 1) as u32
                })
            };
            let (la, lb) = (lower(ia), lower(ib));
            // The kept triangle (a, b, c) folded down about a–b would face the KEPT side; the
            // curtain faces the carve, so it runs b → a.
            new_idx.extend_from_slice(&[ib, ia, la, ib, la, lb]);
        }
    }
    chunk.indices.extend(new_idx);
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
///
/// MONKEY (voxel fix): taken RELATIVE to the first vertex. The plain shoelace over absolute WoW
/// coordinates multiplies ~10⁴-yard values (Westfall: x ≈ −10400, y ≈ 1300 → products ≈ 1.4·10⁷,
/// where one f32 ulp is 1.0), so a good clipped triangle of a few square yards could cancel to 0
/// and be dropped as a "sliver": a hole in the kept ground along the carve's outer edge, where
/// neither the ADT nor the voxel layer draws (the sky-cyan triangles in a line).
fn area2(poly: &[V]) -> f32 {
    let Some(o) = poly.first().map(|v| v.p) else {
        return 0.0;
    };
    (1..poly.len().saturating_sub(1))
        .map(|i| {
            let (a, b) = (poly[i].p, poly[i + 1].p);
            (a[0] - o[0]) * (b[1] - o[1]) - (b[0] - o[0]) * (a[1] - o[1])
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
        flat_chunk_at([100.0, 100.0], holes)
    }

    /// The same flat chunk with its NW corner at WoW `nw`.
    fn flat_chunk_at(nw: [f32; 2], holes: u16) -> ChunkMesh {
        let step = CHUNK_SIZE / 8.0;
        let mut positions = Vec::with_capacity(145);
        let mut uvs = Vec::with_capacity(145);
        for row in 0..9 {
            for col in 0..9 {
                positions.push([nw[0] - row as f32 * step, nw[1] - col as f32 * step, 5.0]);
                uvs.push([col as f32 / 8.0, row as f32 / 8.0]);
            }
            if row < 8 {
                for col in 0..8 {
                    positions.push([
                        nw[0] - (row as f32 + 0.5) * step,
                        nw[1] - (col as f32 + 0.5) * step,
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

    /// MONKEY (voxel fix): far from the map origin (Westfall, x ≈ −10400, y ≈ 1300) the clip must
    /// keep exactly the ground outside the carve too. The absolute-coordinate shoelace lost every
    /// bit of a few-yd² piece there and dropped good triangles as slivers: sky holes in a line
    /// along the carve's outer edge.
    #[test]
    fn far_from_the_origin_no_kept_ground_is_lost() {
        let cs = CHUNK_SIZE;
        // The MCNK chunk west of the y = 1300 chunk line, rows from x = −10400 south.
        let nw = [-10400.0f32, 1300.0 + cs];
        let src = flat_chunk_at(nw, 0);
        let rel_area = |t: &[[f32; 3]; 3]| {
            let o = t[0];
            tri_area(&t.map(|p| [p[0] - o[0], p[1] - o[1], p[2]])).abs()
        };
        let chunk_area: f32 = tris(&src).iter().map(rel_area).sum();
        for (y1, x0, x1) in [
            (1311.5f32, -10416.0f32, -10384.0f32), // the outer edge y = 1312 − inset
            (1311.5, -10421.3, -10403.7),
            (1327.25, -10431.9, -10405.05),
        ] {
            let r = VoxelCarveRect {
                x0,
                x1,
                y0: 1296.0,
                y1,
            };
            let c = clip_chunk_outside_rect(&src, &r).expect("overlaps");
            let kept: f32 = tris(&c).iter().map(rel_area).sum();
            let ox = r.x1.min(nw[0]) - r.x0.max(nw[0] - cs);
            let oy = r.y1.min(nw[1]) - r.y0.max(nw[1] - cs);
            let expect = chunk_area - ox.max(0.0) * oy.max(0.0);
            assert!(
                (kept - expect).abs() < 0.05,
                "rect {r:?}: kept {kept} yd², expected {expect} yd² (ground lost = sky holes)"
            );
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

    /// MONKEY (voxel): an L-shaped union of three 16-yd columns laid across the flat chunk (NW
    /// corner WoW (100, 100), spanning X/Y [66.67, 100]) — columns (4,4), (5,4), (4,5) = X/Y
    /// [64, 96] minus the (5,5) quadrant.
    fn l_carve() -> VoxelCarve {
        VoxelCarve::from_columns([(4, 4), (5, 4), (4, 5)], 16.0, 0.5)
    }

    #[test]
    fn union_carve_keeps_no_triangle_inside_the_union_minus_the_outer_inset() {
        let carve = l_carve();
        let src = flat_chunk(0);
        let c = carve.clip_chunk(&src).expect("the union reaches the chunk");
        // The union shrunk by the outer inset, as a point test (the three inset rectangles).
        let inside = |x: f32, y: f32| carve.contains_strict(x, y);
        for t in tris(&c) {
            for p in t {
                assert!(!inside(p[0], p[1]), "vertex {p:?} inside the union");
            }
            let cx = (t[0][0] + t[1][0] + t[2][0]) / 3.0;
            let cy = (t[0][1] + t[1][1] + t[2][1]) / 3.0;
            assert!(!inside(cx, cy), "triangle {t:?} reaches inside the union");
        }
        // The kept area = chunk − (union ∩ chunk) with the inset applied on the outer boundary:
        // the union rects clipped to the chunk square [100 − CHUNK_SIZE, 100].
        let lo = 100.0 - CHUNK_SIZE;
        let carved: f32 = carve
            .rects()
            .iter()
            .map(|r| {
                let w = (r.x1.min(100.0) - r.x0.max(lo)).max(0.0);
                let h = (r.y1.min(100.0) - r.y0.max(lo)).max(0.0);
                w * h
            })
            .sum();
        let full: f32 = tris(&src).iter().map(|t| tri_area(t).abs()).sum();
        let kept: f32 = tris(&c).iter().map(|t| tri_area(t).abs()).sum();
        assert!(
            (kept - (full - carved)).abs() < 1.0e-2,
            "kept {kept} expected {}",
            full - carved
        );
    }

    #[test]
    fn union_internal_column_edges_are_not_inset() {
        let carve = l_carve();
        // Internal edge between (4,4) and (5,4) at X = 80 (Y mid 72): carved on both sides.
        assert!(carve.contains_strict(79.75, 72.0) && carve.contains_strict(80.25, 72.0));
        // Internal edge between (4,4) and (4,5) at Y = 80.
        assert!(carve.contains_strict(72.0, 79.75) && carve.contains_strict(72.0, 80.25));
        // A point exactly ON an internal column edge is carved too (no zero-width height slit).
        assert!(carve.contains_strict(80.0, 72.0) && carve.contains_strict(72.0, 80.0));
        assert!(carve.contains_strict(80.0, 80.0 - 0.75) && !carve.contains_strict(96.0, 72.0));
        // Outer boundary of (5,4) at X = 96 keeps 0.5 yd of ground, and (5,4)'s +Y side borders
        // the missing (5,5): inset there too.
        assert!(carve.contains_strict(95.4, 72.0) && !carve.contains_strict(95.6, 72.0));
        assert!(!carve.contains_strict(88.0, 79.75) && carve.contains_strict(88.0, 79.4));
        // The outer -X edge of (4,4) at X = 64.
        assert!(!carve.contains_strict(64.25, 72.0) && carve.contains_strict(64.6, 72.0));
        // Signatures: a box the union does not reach is 0; one it does is non-zero and changes
        // only when a rect reaching it changes.
        assert_eq!(carve.sig_for_box([0.0, 0.0], [10.0, 10.0]), 0);
        let near = carve.sig_for_box([60.0, 60.0], [70.0, 70.0]);
        assert_ne!(near, 0);
        let grown = VoxelCarve::from_columns([(4, 4), (5, 4), (4, 5), (20, 20)], 16.0, 0.5);
        assert_eq!(grown.sig_for_box([60.0, 60.0], [70.0, 70.0]), near);
        assert_ne!(grown.sig(), carve.sig());
        assert!(VoxelCarve::from_columns([], 16.0, 0.5).is_empty());
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

    /// MONKEY (voxel fix): the L's inner corner — (4,4) has carved neighbours on +x (5,4) and +y
    /// (4,5) but not the diagonal (5,5) — keeps an inset × inset notch of real ground; the seams
    /// between the notched column's bands stay inclusive; the clip removes exactly the union.
    #[test]
    fn union_inner_corner_is_notched() {
        let carve = l_carve();
        // The notch at (96 - 16 = 80, 80): the corner square [80, 80.5]² of (5,4) / (4,5) side is
        // already inset; (4,4)'s own corner [79.5, 80]² must keep its ground too.
        assert!(!carve.contains_strict(79.75, 79.75));
        assert!(!carve.contains_strict(80.0, 80.0));
        // Right next to the notch the column is carved, also exactly on its band seam (x = 79.5).
        assert!(carve.contains_strict(79.75, 79.25) && carve.contains_strict(79.25, 79.75));
        assert!(carve.contains_strict(79.5, 72.0) && carve.contains_strict(79.5, 79.4));
        // Still inclusive on the internal column edges away from the corner.
        assert!(carve.contains_strict(80.0, 72.0) && carve.contains_strict(72.0, 80.0));
        // The clip leaves the notch's ground and nothing strictly inside the union.
        let src = flat_chunk(0);
        let c = carve.clip_chunk(&src).expect("reaches");
        let lo = 100.0 - CHUNK_SIZE;
        let carved: f32 = carve
            .rects()
            .iter()
            .map(|r| {
                let w = (r.x1.min(100.0) - r.x0.max(lo)).max(0.0);
                let h = (r.y1.min(100.0) - r.y0.max(lo)).max(0.0);
                w * h
            })
            .sum();
        let full: f32 = tris(&src).iter().map(|t| tri_area(t).abs()).sum();
        let kept: f32 = tris(&c).iter().map(|t| tri_area(t).abs()).sum();
        assert!(
            (kept - (full - carved)).abs() < 1.0e-2,
            "kept {kept} vs {}",
            full - carved
        );
        for t in tris(&c) {
            let cx = (t[0][0] + t[1][0] + t[2][0]) / 3.0;
            let cy = (t[0][1] + t[1][1] + t[2][1]) / 3.0;
            assert!(
                !carve.contains_strict(cx, cy),
                "triangle {t:?} reaches inside the union"
            );
        }
        assert_eq!(carve.columns(), 3);
    }

    /// The first FRONT-facing hit of a ray on a mesh (`up` = the sign of a top triangle's normal Z,
    /// so a top faces up), or `None`. Möller–Trumbore.
    fn ray_hits_front(c: &ChunkMesh, o: [f32; 3], d: [f32; 3], up: f32) -> Option<f32> {
        let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let crs = |a: [f32; 3], b: [f32; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let mut best: Option<f32> = None;
        for t in tris(c) {
            let (e1, e2) = (sub(t[1], t[0]), sub(t[2], t[0]));
            if dot(d, crs(e1, e2)) * up >= 0.0 {
                continue; // back-facing (culled)
            }
            let pv = crs(d, e2);
            let det = dot(e1, pv);
            if det.abs() < 1.0e-12 {
                continue;
            }
            let tv = sub(o, t[0]);
            let u = dot(tv, pv) / det;
            let qv = crs(tv, e1);
            let v = dot(d, qv) / det;
            let s = dot(e2, qv) / det;
            if u >= -1.0e-6 && v >= -1.0e-6 && u + v <= 1.0 + 1.0e-6 && s > 0.0 {
                best = Some(best.map_or(s, |b: f32| b.min(s)));
            }
        }
        best
    }

    /// MONKEY (voxel fix, owner's Northshire hairline): a GRAZING look from inside the carve
    /// across its cut edge must land on ground. The voxel layer lies ~0.02 yd UNDER the real ground
    /// (drawn `RENDER_DROP` below its seed) and its unedited fragments end 0.1 yd past the carve
    /// region, so a ray that clears the voxel top enters the slit under the cut ADT edge and —
    /// with the plain clip — met nothing for metres: sky through a dashed line. The drawn clip
    /// hangs a curtain there; the collider's plain clip keeps the slit (documented below).
    #[cfg(feature = "voxel")]
    #[test]
    fn grazing_rays_across_the_cut_edge_hit_ground() {
        use benilla_voxel::world::{BAND_DISCARD, RENDER_DROP, SEED_LIFT};
        // Column (5,5) alone: carve region X/Y [80.5, 95.5], wholly inside the flat chunk
        // ([66.67, 100], ground z = 5).
        let carve = VoxelCarve::from_columns([(5, 5)], 16.0, 0.5);
        let src = flat_chunk(0);
        let plain = carve.clip_chunk(&src).expect("reaches");
        let drawn = carve.clip_chunk_drawn(&src).expect("reaches");
        let up = {
            let t = tris(&src)[0];
            tri_area(&t).signum()
        };
        let r = carve.rects()[0];
        // The voxel's untouched top: the seed RENDER_DROP lower, kept to BAND_DISCARD past the region.
        let vz = 5.0 + SEED_LIFT - RENDER_DROP;
        assert!(vz < 5.0, "the voxel is drawn under the real ground");
        let voxel_hit = |o: [f32; 3], d: [f32; 3]| {
            let s = (vz - o[2]) / d[2];
            let (x, y) = (o[0] + d[0] * s, o[1] + d[1] * s);
            let b = BAND_DISCARD;
            (s > 0.0 && x > r.x0 - b && x < r.x1 + b && y > r.y0 - b && y < r.y1 + b).then_some(s)
        };
        let mut plain_misses = 0;
        let mut rays = 0;
        // Rays from 3 yd inside each side, outward, falling so they cross the edge midway
        // between the voxel top and the real ground (a 0.25° grazing look).
        let slope = 0.25f32.to_radians().tan();
        for side in 0..4 {
            for k in 1..8 {
                let f = k as f32 / 8.0;
                let along = |lo: f32, hi: f32| lo + (hi - lo) * f;
                let (edge, dir): ([f32; 2], [f32; 2]) = match side {
                    0 => ([r.x0, along(r.y0, r.y1)], [-1.0, 0.0]),
                    1 => ([r.x1, along(r.y0, r.y1)], [1.0, 0.0]),
                    2 => ([along(r.x0, r.x1), r.y0], [0.0, -1.0]),
                    _ => ([along(r.x0, r.x1), r.y1], [0.0, 1.0]),
                };
                let z_edge = (vz + 5.0) * 0.5;
                let o = [
                    edge[0] - dir[0] * 3.0,
                    edge[1] - dir[1] * 3.0,
                    z_edge + slope * 3.0,
                ];
                let n = (1.0 + slope * slope).sqrt();
                let d = [dir[0] / n, dir[1] / n, -slope / n];
                rays += 1;
                let hit = |m: &ChunkMesh| {
                    let a = ray_hits_front(m, o, d, up);
                    let b = voxel_hit(o, d);
                    match (a, b) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    }
                };
                // The hit must be at the edge, not metres beyond it (a 3-yd run-up + the slit).
                let h = hit(&drawn);
                assert!(
                    h.is_some_and(|s| s < 3.0 + 0.2),
                    "side {side} ray {k}: grazing look across the cut edge hits {h:?} (sky)"
                );
                if hit(&plain).is_none_or(|s| s > 3.0 + 0.2) {
                    plain_misses += 1;
                }
            }
        }
        assert_eq!(plain_misses, rays, "the plain clip's slit (the bug this guards)");
        // The curtain runs exactly along the region's perimeter, 0.3 yd deep (no curtain on the
        // chunk's own border or anywhere else).
        let vert_area: f32 = tris(&drawn)
            .iter()
            .filter(|t| tri_area(t).abs() < 1.0e-6)
            .map(|t| {
                let (e1, e2) = (
                    [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]],
                    [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]],
                );
                let c = [
                    e1[1] * e2[2] - e1[2] * e2[1],
                    e1[2] * e2[0] - e1[0] * e2[2],
                    e1[0] * e2[1] - e1[1] * e2[0],
                ];
                0.5 * (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt()
            })
            .sum();
        let perimeter = 2.0 * ((r.x1 - r.x0) + (r.y1 - r.y0));
        assert!(
            (vert_area - perimeter * CARVE_SKIRT).abs() < 1.0e-2,
            "curtain area {vert_area} vs {}",
            perimeter * CARVE_SKIRT
        );
        // Every drawn vertex keeps its attributes.
        assert_eq!(drawn.normals.len(), drawn.positions.len());
        assert_eq!(drawn.uvs.len(), drawn.positions.len());
    }

    /// MONKEY (voxel fix): the clipped chunk is watertight — no vertex lies strictly inside a kept
    /// edge (a T-junction: hairline pinholes past every rectangle corner, where a cut line ran on
    /// across the straddling triangle), for a lone rect, the L-shaped union and its notch.
    #[test]
    fn clipped_chunk_has_no_t_junctions() {
        let check = |c: &ChunkMesh, what: &str| {
            for t in tris(c) {
                for k in 0..3 {
                    let (a, b) = (t[k], t[(k + 1) % 3]);
                    let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
                    let l2 = ex * ex + ey * ey;
                    for p in &c.positions {
                        if *p == a || *p == b {
                            continue;
                        }
                        let (px, py) = (p[0] - a[0], p[1] - a[1]);
                        let s = (px * ex + py * ey) / l2;
                        let off = (px * ey - py * ex).abs() / l2.sqrt();
                        let used = tris(c).iter().any(|u| u.contains(p));
                        assert!(
                            !(used && s > 1.0e-4 && s < 1.0 - 1.0e-4 && off < 1.0e-4),
                            "{what}: vertex {p:?} splits edge {a:?}-{b:?} (T-junction)"
                        );
                    }
                }
            }
        };
        let carve = VoxelCarve::from_columns([(5, 5)], 16.0, 0.5);
        check(&carve.clip_chunk(&flat_chunk(0)).unwrap(), "lone column");
        check(&l_carve().clip_chunk(&flat_chunk(0)).unwrap(), "L union");
        let one = VoxelCarve::from_columns([(4, 4)], 16.0, 0.5);
        check(&one.clip_chunk(&flat_chunk(0)).unwrap(), "corner column");
        // Re-fanning keeps exactly the kept area and the winding.
        let src = flat_chunk(0);
        let c = carve.clip_chunk(&src).unwrap();
        let r = carve.rects()[0];
        let full: f32 = tris(&src).iter().map(|t| tri_area(t).abs()).sum();
        let kept: f32 = tris(&c).iter().map(|t| tri_area(t).abs()).sum();
        let hole = (r.x1 - r.x0) * (r.y1 - r.y0);
        assert!((kept - (full - hole)).abs() < 1.0e-2, "kept {kept} vs {}", full - hole);
        let sign = tri_area(&tris(&src)[0]).signum();
        for t in tris(&c) {
            assert_eq!(tri_area(&t).signum(), sign, "flipped {t:?}");
        }
    }

    /// The curtain also hangs on a carve that crosses the chunk's border (only the cut edges, not
    /// the chunk border inside the carve) and not along an original MCNK hole.
    #[test]
    fn curtain_only_on_cut_edges() {
        let r = rect();
        let carve = VoxelCarve::from_columns([], 16.0, 0.5);
        assert!(carve.clip_chunk_drawn(&flat_chunk(0)).is_none());
        let holes = 1u16; // block 0 (NW corner), far from the rect
        let mut c = clip_chunk_outside_rect(&flat_chunk(holes), &r).expect("overlaps");
        let n0 = c.indices.len();
        add_carve_skirts(&mut c, |x, y| r.contains_strict(x, y), CARVE_SKIRT);
        let walls: Vec<_> = tris(&c).into_iter().skip(n0 / 3).collect();
        assert!(!walls.is_empty());
        for t in &walls {
            for p in t {
                // On the rect's x1 or y1 line (its x0 / y0 sides lie past the chunk).
                let on = (p[0] - r.x1).abs() < 1.0e-4 || (p[1] - r.y1).abs() < 1.0e-4;
                assert!(on, "curtain vertex {p:?} off the cut edges");
            }
        }
    }
}

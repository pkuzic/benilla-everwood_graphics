//! MONKEY (voxel): the v2 WORLD voxel layer (CONTRACT §1–2, `disposable/voxel-v2/CONTRACT.md`).
//!
//! Engine-free like the rest of the crate. The world is a sparse set of world-aligned chunks on one
//! lattice shared by every client:
//! - lattice point `I = round(wow / VOXEL_SIZE)` per axis, in **WoW axes** (x north, y west, z up);
//! - chunk key `k = floor(wow / 16)` = `I.div_euclid(32)`; a chunk OWNS lattice `32k ..= 32k+31`;
//! - a chunk is meshed from a 34³ window (lattice `32k-1 ..= 32k+32`): its own 32³ plus a one-sample
//!   apron read from the neighbour that owns it (its density if active, else the seed function), so
//!   every sample has exactly ONE owner and neighbouring chunks can never disagree on a seam;
//! - the seed of a sample is the raw ADT height of its lattice column (+ [`SEED_LIFT`]): solid
//!   below, air above ([`ColumnSeed`]);
//! - edits ([`VoxelEditOp`]) are applied in canonical order (confirmed `seq` ascending, then local
//!   unconfirmed predictions in arrival order — [`MapJournal`]); "rebuild chunk = seed + every edit
//!   touching it, in order" is [`VoxelWorldData::build`].
//!
//! The journal's binary form (`monkey_voxel_world.bin`) and the v2 EDIT wire body live here too, so
//! the server lane can reuse the exact encoders.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use fast_surface_nets::ndshape::ConstShape;
use fast_surface_nets::{surface_nets, SurfaceNetsBuffer};

use crate::{ChunkShape, VoxelMesh, CHUNK, DENSITY_SCALE, SAMPLES, VOXEL_SIZE};

/// World size of one chunk edge, in yards (`CHUNK * VOXEL_SIZE`).
pub const CHUNK_YARDS: f32 = CHUNK as f32 * VOXEL_SIZE;
/// Owned samples per chunk (32³).
pub const OWNED: usize = (CHUNK * CHUNK * CHUNK) as usize;
/// The seeded surface sits this far above the raw ADT height (no z-fight with the heightmap).
pub const SEED_LIFT: f32 = 0.15;
/// Region (sync + revert unit) edge in yards (CONTRACT §2).
pub const REGION_YARDS: f32 = 64.0;
/// The seed window per column: lattice `32k-2 ..= 32k+33` on each horizontal axis (the mesher's
/// apron plus the smooth brush's one-sample read ring).
pub const SEED_W: usize = CHUNK as usize + 4;
/// Density of a sample nobody knows (unloaded column): air.
pub const AIR: i8 = i8::MAX;
/// The v2 EDIT op byte (CONTRACT §3).
pub const EDIT_OP: u8 = 0x01;
/// Bytes of the v2 EDIT body (`op u8, nonce u16, map u16, x f32, y f32, z f32, radius f32, mode u8,
/// strength u8`).
pub const EDIT_WIRE_LEN: usize = 23;

const C: i32 = CHUNK as i32;

/// `(kx, ky, kz)` chunk key on the current map (WoW axes).
pub type ChunkKey = [i32; 3];
/// `(kx, ky)` chunk column key.
pub type ColumnKey = [i32; 2];

/// The chunk owning lattice coordinate `i` on one axis.
#[inline]
pub fn chunk_of(i: i32) -> i32 {
    i.div_euclid(C)
}

/// The chunk key containing a WoW point.
pub fn chunk_key_at(wow: [f32; 3]) -> ChunkKey {
    [
        (wow[0] / CHUNK_YARDS).floor() as i32,
        (wow[1] / CHUNK_YARDS).floor() as i32,
        (wow[2] / CHUNK_YARDS).floor() as i32,
    ]
}

/// The region `(rx, ry)` of a WoW column (CONTRACT §2).
pub fn region_of(x: f32, y: f32) -> (i16, i16) {
    (
        (x / REGION_YARDS).floor() as i16,
        (y / REGION_YARDS).floor() as i16,
    )
}

/// WoW coordinate of a lattice point.
#[inline]
pub fn lattice_to_wow(i: i32) -> f32 {
    i as f32 * VOXEL_SIZE
}

/// Index of an owned sample `(x, y, z)` (0..32 each) in a chunk's density.
#[inline]
pub fn owned_index(x: i32, y: i32, z: i32) -> usize {
    (x + C * (y + C * z)) as usize
}

/// One terrain edit (CONTRACT §1 ops, §3 EDIT body, + the server-assigned `seq` once confirmed).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelEditOp {
    pub map: u16,
    /// Brush centre, WoW yards (x north, y west, z up).
    pub pos: [f32; 3],
    /// Brush radius, yards.
    pub radius: f32,
    /// 0 dig (Subtract), 1 build (Add), 2 smooth.
    pub mode: u8,
    /// Smooth strength /255 (dig/build ignore it; sent as 255).
    pub strength: u8,
    /// Per-sender nonce (matches the server echo to the local prediction).
    pub nonce: u16,
    /// Server sequence once confirmed; `None` = local prediction / guild relay (no server).
    pub seq: Option<u32>,
    /// MONKEY (voxel): THIS client made the edit (brush / AUTOEDIT). Only local edits are uplink
    /// candidates ([`MapJournal::pending_local`]) and nonce-confirmable; relay / server records
    /// are `false`. Not on the wire (journal flag bit 1 only).
    pub local: bool,
}

impl VoxelEditOp {
    /// Centre in lattice units.
    fn centre_lat(&self) -> [f32; 3] {
        [
            self.pos[0] / VOXEL_SIZE,
            self.pos[1] / VOXEL_SIZE,
            self.pos[2] / VOXEL_SIZE,
        ]
    }

    /// Radius in lattice units.
    fn radius_lat(&self) -> f32 {
        self.radius / VOXEL_SIZE
    }

    /// Whether the fields are sane enough to apply (finite, radius within reason).
    pub fn is_valid(&self) -> bool {
        self.pos.iter().all(|v| v.is_finite() && v.abs() < 40_000.0)
            && self.radius.is_finite()
            && self.radius > 0.0
            && self.radius <= 16.0
            && self.mode <= 2
    }

    /// The inclusive lattice box every sample this edit can change lies in: centre ± radius ± 1
    /// voxel (CONTRACT §1 bounding box).
    pub fn lattice_bbox(&self) -> ([i32; 3], [i32; 3]) {
        let c = self.centre_lat();
        let r = self.radius_lat() + 1.0;
        (
            [
                (c[0] - r).floor() as i32,
                (c[1] - r).floor() as i32,
                (c[2] - r).floor() as i32,
            ],
            [
                (c[0] + r).ceil() as i32,
                (c[1] + r).ceil() as i32,
                (c[2] + r).ceil() as i32,
            ],
        )
    }

    /// Chunks owning any sample of [`Self::lattice_bbox`] — the chunks the edit ACTIVATES.
    pub fn touched_chunks(&self) -> Vec<ChunkKey> {
        let (lo, hi) = self.lattice_bbox();
        chunks_of_box(lo, hi)
    }

    /// Chunks whose 34³ mesh window can see a changed sample (the bbox grown by the apron).
    pub fn remesh_chunks(&self) -> Vec<ChunkKey> {
        let (lo, hi) = self.lattice_bbox();
        chunks_of_box([lo[0] - 1, lo[1] - 1, lo[2] - 1], [hi[0] + 1, hi[1] + 1, hi[2] + 1])
    }

    /// Whether the edit's bbox reaches chunk `k`'s owned samples.
    pub fn touches(&self, k: ChunkKey) -> bool {
        let (lo, hi) = self.lattice_bbox();
        (0..3).all(|a| hi[a] >= k[a] * C && lo[a] <= k[a] * C + C - 1)
    }

    /// The region of the edit's centre (CONTRACT §2).
    pub fn region(&self) -> (i16, i16) {
        region_of(self.pos[0], self.pos[1])
    }

    /// The 23-byte v2 EDIT body (CONTRACT §3, little-endian).
    pub fn to_wire(&self) -> [u8; EDIT_WIRE_LEN] {
        let mut b = [0u8; EDIT_WIRE_LEN];
        b[0] = EDIT_OP;
        b[1..3].copy_from_slice(&self.nonce.to_le_bytes());
        b[3..5].copy_from_slice(&self.map.to_le_bytes());
        b[5..9].copy_from_slice(&self.pos[0].to_le_bytes());
        b[9..13].copy_from_slice(&self.pos[1].to_le_bytes());
        b[13..17].copy_from_slice(&self.pos[2].to_le_bytes());
        b[17..21].copy_from_slice(&self.radius.to_le_bytes());
        b[21] = self.mode;
        b[22] = self.strength;
        b
    }

    /// Decode a v2 EDIT body (`seq` = `None`); `None` if malformed / invalid.
    pub fn from_wire(b: &[u8]) -> Option<Self> {
        if b.len() != EDIT_WIRE_LEN || b[0] != EDIT_OP {
            return None;
        }
        let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let e = Self {
            nonce: u16::from_le_bytes([b[1], b[2]]),
            map: u16::from_le_bytes([b[3], b[4]]),
            pos: [f(5), f(9), f(13)],
            radius: f(17),
            mode: b[21],
            strength: b[22],
            seq: None,
            local: false,
        };
        e.is_valid().then_some(e)
    }

    /// Lowercase hex of [`Self::to_wire`] (46 chars).
    pub fn to_hex(&self) -> String {
        self.to_wire().iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Decode [`Self::to_hex`].
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != EDIT_WIRE_LEN * 2 || !s.is_ascii() {
            return None;
        }
        let b: Vec<u8> = (0..EDIT_WIRE_LEN)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        Self::from_wire(&b)
    }
}

/// Every chunk owning a sample of the inclusive lattice box `[lo, hi]`.
pub fn chunks_of_box(lo: [i32; 3], hi: [i32; 3]) -> Vec<ChunkKey> {
    let mut out = Vec::new();
    for z in chunk_of(lo[2])..=chunk_of(hi[2]) {
        for y in chunk_of(lo[1])..=chunk_of(hi[1]) {
            for x in chunk_of(lo[0])..=chunk_of(hi[0]) {
                out.push([x, y, z]);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Seed
// ---------------------------------------------------------------------------------------------

/// The seeded surface heights of one chunk column: the RAW ADT height (+ [`SEED_LIFT`]) of every
/// lattice column in the `SEED_W²` window `32k-2 ..= 32k+33`. A hole / missing MCNK is air
/// (`-inf`), so every client derives the identical value for a lattice column whichever column's
/// window it is read through.
#[derive(Clone, Debug)]
pub struct ColumnSeed {
    pub key: ColumnKey,
    heights: Vec<f32>,
    /// The lattice z range `[lo, hi]` the seeded surface spans over the column's OWN footprint
    /// (`None` = no ground at all: the whole column is air).
    surface_lat: Option<(i32, i32)>,
}

impl ColumnSeed {
    /// Sample the column with `height(wow_x, wow_y)` = the raw ADT height (`None` = hole). The
    /// caller guarantees the tiles are resident (else it must not build a seed yet).
    pub fn from_fn(key: ColumnKey, mut height: impl FnMut(f32, f32) -> Option<f32>) -> Self {
        let mut heights = vec![f32::NEG_INFINITY; SEED_W * SEED_W];
        let (bx, by) = (key[0] * C - 2, key[1] * C - 2);
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for j in 0..SEED_W {
            for i in 0..SEED_W {
                let (ix, iy) = (bx + i as i32, by + j as i32);
                if let Some(h) = height(lattice_to_wow(ix), lattice_to_wow(iy)) {
                    let h = h + SEED_LIFT;
                    heights[i + j * SEED_W] = h;
                    // The own footprint plus the apron ring.
                    if (1..SEED_W - 1).contains(&i) && (1..SEED_W - 1).contains(&j) {
                        lo = lo.min(h);
                        hi = hi.max(h);
                    }
                }
            }
        }
        let surface_lat = (lo <= hi).then(|| {
            (
                (lo / VOXEL_SIZE).floor() as i32,
                (hi / VOXEL_SIZE).ceil() as i32,
            )
        });
        Self {
            key,
            heights,
            surface_lat,
        }
    }

    /// Whether lattice column `(ix, iy)` lies in this seed's window.
    #[inline]
    pub fn covers(&self, ix: i32, iy: i32) -> bool {
        let (i, j) = (ix - (self.key[0] * C - 2), iy - (self.key[1] * C - 2));
        (0..SEED_W as i32).contains(&i) && (0..SEED_W as i32).contains(&j)
    }

    /// The seeded height (WoW z, lift included; `-inf` = air) of a lattice column in the window.
    #[inline]
    pub fn height_lat(&self, ix: i32, iy: i32) -> f32 {
        let (i, j) = (ix - (self.key[0] * C - 2), iy - (self.key[1] * C - 2));
        self.heights[i as usize + j as usize * SEED_W]
    }

    /// The seed density of lattice point `(ix, iy, iz)` (column inside the window).
    #[inline]
    pub fn value(&self, ix: i32, iy: i32, iz: i32) -> i8 {
        seed_density(self.height_lat(ix, iy), iz)
    }

    /// The chunk z range `[lo, hi]` that brackets the seeded surface anywhere over this column's
    /// footprint — the "surface chunks" of CONTRACT §1. Generous by one sample each side so the
    /// z-edges / cells the surface crosses always fall in an active chunk.
    pub fn surface_chunks(&self) -> Option<(i32, i32)> {
        self.surface_lat
            .map(|(lo, hi)| (chunk_of(lo - 1), chunk_of(hi + 1)))
    }
}

/// The fixed-point seed SDF of a sample at lattice height `iz` over a column of height `h` (yards):
/// `(z - h) / VOXEL_SIZE` voxel units, negative = solid.
#[inline]
pub fn seed_density(h: f32, iz: i32) -> i8 {
    if h == f32::NEG_INFINITY {
        return AIR;
    }
    let sdf = (lattice_to_wow(iz) - h) / VOXEL_SIZE;
    (sdf * DENSITY_SCALE)
        .round()
        .clamp(i8::MIN as f32, i8::MAX as f32) as i8
}

/// A freshly seeded chunk's owned density.
pub fn seed_chunk(key: ChunkKey, seed: &ColumnSeed) -> Box<[i8]> {
    let mut d = vec![AIR; OWNED].into_boxed_slice();
    let base = [key[0] * C, key[1] * C, key[2] * C];
    for z in 0..C {
        for y in 0..C {
            for x in 0..C {
                d[owned_index(x, y, z)] = seed.value(base[0] + x, base[1] + y, base[2] + z);
            }
        }
    }
    d
}

// ---------------------------------------------------------------------------------------------
// The live world data: chunk densities + column seeds
// ---------------------------------------------------------------------------------------------

/// The active chunks' owned densities and the loaded column seeds of one map.
#[derive(Default)]
pub struct VoxelWorldData {
    pub chunks: HashMap<ChunkKey, Box<[i8]>>,
    pub seeds: HashMap<ColumnKey, Arc<ColumnSeed>>,
}

/// One lattice sample through the active chunks, else the owner column's seed, else `fallback`'s
/// window, else air.
fn sample_in(
    chunks: &HashMap<ChunkKey, Box<[i8]>>,
    seeds: &HashMap<ColumnKey, Arc<ColumnSeed>>,
    fallback: Option<&ColumnSeed>,
    i: [i32; 3],
) -> i8 {
    let k = [chunk_of(i[0]), chunk_of(i[1]), chunk_of(i[2])];
    if let Some(d) = chunks.get(&k) {
        return d[owned_index(i[0] - k[0] * C, i[1] - k[1] * C, i[2] - k[2] * C)];
    }
    if let Some(s) = seeds.get(&[k[0], k[1]]) {
        return s.value(i[0], i[1], i[2]);
    }
    match fallback {
        Some(s) if s.covers(i[0], i[1]) => s.value(i[0], i[1], i[2]),
        _ => AIR,
    }
}

/// Apply one edit to the chunks in `set` (owned samples only). `outside` answers any sample whose
/// owner is NOT in `set` (the smooth brush reads one ring past the samples it writes). Smoothing
/// is Jacobi over the whole edit: every new value reads the PRE-edit field, across chunk borders,
/// so a smooth spanning several chunks of `set` is exactly the one-grid result. Returns the keys
/// whose density changed.
pub fn apply_edit_set(
    edit: &VoxelEditOp,
    set: &mut HashMap<ChunkKey, Box<[i8]>>,
    outside: &dyn Fn([i32; 3]) -> i8,
) -> Vec<ChunkKey> {
    let (lo, hi) = edit.lattice_bbox();
    let c = edit.centre_lat();
    let r = edit.radius_lat();
    let mut changed = Vec::new();
    match edit.mode {
        0 | 1 => {
            for k in chunks_of_box(lo, hi) {
                let Some(d) = set.get_mut(&k) else {
                    continue;
                };
                let base = [k[0] * C, k[1] * C, k[2] * C];
                let mut any = false;
                for z in lo[2].max(base[2])..=hi[2].min(base[2] + C - 1) {
                    for y in lo[1].max(base[1])..=hi[1].min(base[1] + C - 1) {
                        for x in lo[0].max(base[0])..=hi[0].min(base[0] + C - 1) {
                            let brush = ((x as f32 - c[0]).powi(2)
                                + (y as f32 - c[1]).powi(2)
                                + (z as f32 - c[2]).powi(2))
                            .sqrt()
                                - r;
                            let idx = owned_index(x - base[0], y - base[1], z - base[2]);
                            let old = d[idx] as f32 / DENSITY_SCALE;
                            let new = if edit.mode == 0 {
                                old.max(-brush)
                            } else {
                                old.min(brush)
                            };
                            let q = (new * DENSITY_SCALE)
                                .round()
                                .clamp(i8::MIN as f32, i8::MAX as f32)
                                as i8;
                            if q != d[idx] {
                                d[idx] = q;
                                any = true;
                            }
                        }
                    }
                }
                if any {
                    changed.push(k);
                }
            }
        }
        _ => {
            let strength = edit.strength as f32 / 255.0;
            // Gather the pre-edit field over the brush box + the read ring.
            let (glo, ghi) = (
                [lo[0] - 1, lo[1] - 1, lo[2] - 1],
                [hi[0] + 1, hi[1] + 1, hi[2] + 1],
            );
            let dim = [
                (ghi[0] - glo[0] + 1) as usize,
                (ghi[1] - glo[1] + 1) as usize,
                (ghi[2] - glo[2] + 1) as usize,
            ];
            let at = |p: [i32; 3]| -> usize {
                (p[0] - glo[0]) as usize
                    + dim[0] * ((p[1] - glo[1]) as usize + dim[1] * (p[2] - glo[2]) as usize)
            };
            let mut grid = vec![0i8; dim[0] * dim[1] * dim[2]];
            for z in glo[2]..=ghi[2] {
                for y in glo[1]..=ghi[1] {
                    for x in glo[0]..=ghi[0] {
                        let k = [chunk_of(x), chunk_of(y), chunk_of(z)];
                        let v = match set.get(&k) {
                            Some(d) => d[owned_index(x - k[0] * C, y - k[1] * C, z - k[2] * C)],
                            None => outside([x, y, z]),
                        };
                        grid[at([x, y, z])] = v;
                    }
                }
            }
            let r2 = r * r;
            let mut touched: HashSet<ChunkKey> = HashSet::new();
            for z in lo[2]..=hi[2] {
                for y in lo[1]..=hi[1] {
                    for x in lo[0]..=hi[0] {
                        let d2 = (x as f32 - c[0]).powi(2)
                            + (y as f32 - c[1]).powi(2)
                            + (z as f32 - c[2]).powi(2);
                        if d2 > r2 {
                            continue;
                        }
                        let k = [chunk_of(x), chunk_of(y), chunk_of(z)];
                        let Some(d) = set.get_mut(&k) else {
                            continue;
                        };
                        let g = |dx: i32, dy: i32, dz: i32| grid[at([x + dx, y + dy, z + dz])] as f32;
                        let avg = (g(1, 0, 0)
                            + g(-1, 0, 0)
                            + g(0, 1, 0)
                            + g(0, -1, 0)
                            + g(0, 0, 1)
                            + g(0, 0, -1))
                            / 6.0;
                        let old = g(0, 0, 0);
                        let new = (old * (1.0 - strength) + avg * strength)
                            .round()
                            .clamp(i8::MIN as f32, i8::MAX as f32) as i8;
                        let idx = owned_index(x - k[0] * C, y - k[1] * C, z - k[2] * C);
                        if new != d[idx] {
                            d[idx] = new;
                            touched.insert(k);
                        }
                    }
                }
            }
            changed.extend(touched);
        }
    }
    changed
}

impl VoxelWorldData {
    /// One lattice sample (active chunk → owner column seed → air).
    pub fn sample(&self, i: [i32; 3]) -> i8 {
        sample_in(&self.chunks, &self.seeds, None, i)
    }

    /// Whether chunk `k` is active (has a density).
    pub fn is_active(&self, k: ChunkKey) -> bool {
        self.chunks.contains_key(&k)
    }

    /// Apply an edit to the already-active chunks it touches (the in-order incremental path).
    /// Returns the chunks whose density changed.
    pub fn apply_incremental(&mut self, edit: &VoxelEditOp) -> Vec<ChunkKey> {
        let seeds = &self.seeds;
        apply_edit_set(edit, &mut self.chunks, &|i| {
            sample_in(&HashMap::new(), seeds, None, i)
        })
    }

    /// The canonical (re)build of `keys`: each is reseeded, then every edit of `edits` (already
    /// in canonical order) touching any of them is applied to the whole set in lockstep, reading
    /// other active chunks / seeds outside the set. Every key's column seed must be loaded (keys
    /// without one are skipped). The results replace the chunks' densities.
    pub fn build<'a>(
        &mut self,
        keys: &[ChunkKey],
        edits: impl IntoIterator<Item = &'a VoxelEditOp>,
    ) -> Vec<ChunkKey> {
        let mut set: HashMap<ChunkKey, Box<[i8]>> = HashMap::new();
        for &k in keys {
            if let Some(seed) = self.seeds.get(&[k[0], k[1]]) {
                set.insert(k, seed_chunk(k, seed));
            }
        }
        if set.is_empty() {
            return Vec::new();
        }
        let built: Vec<ChunkKey> = set.keys().copied().collect();
        {
            let (chunks, seeds) = (&self.chunks, &self.seeds);
            let outside = |i: [i32; 3]| sample_in(chunks, seeds, None, i);
            for e in edits {
                if built.iter().any(|&k| e.touches(k)) {
                    apply_edit_set(e, &mut set, &outside);
                }
            }
        }
        self.chunks.extend(set);
        built
    }

    /// The 34³ f32 SDF window of chunk `k` (lattice `32k-1 ..= 32k+32`, x fastest): the chunk's
    /// own samples + the apron from the owning neighbours (active density, else seed — through the
    /// owner column's seed if loaded, else this column's seed window).
    pub fn window(&self, k: ChunkKey) -> Vec<f32> {
        let own_seed = self.seeds.get(&[k[0], k[1]]).map(|s| &**s);
        // The 27 neighbour densities, indexed (dx+1) + 3(dy+1) + 9(dz+1).
        let mut nb: [Option<&[i8]>; 27] = [None; 27];
        let mut nseed: [Option<&ColumnSeed>; 9] = [None; 9];
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    nb[((dx + 1) + 3 * (dy + 1) + 9 * (dz + 1)) as usize] =
                        self.chunks.get(&[k[0] + dx, k[1] + dy, k[2] + dz]).map(|b| &**b);
                }
            }
        }
        for dy in -1..=1 {
            for dx in -1..=1 {
                nseed[((dx + 1) + 3 * (dy + 1)) as usize] = self
                    .seeds
                    .get(&[k[0] + dx, k[1] + dy])
                    .map(|s| &**s)
                    .or(own_seed);
            }
        }
        let mut sdf = vec![0f32; ChunkShape::USIZE];
        let base = [k[0] * C - 1, k[1] * C - 1, k[2] * C - 1];
        let split = |l: i32| -> (i32, i32) {
            // window-local 0..=33 → (neighbour offset, owned local)
            if l == 0 {
                (-1, C - 1)
            } else if l == SAMPLES as i32 - 1 {
                (1, 0)
            } else {
                (0, l - 1)
            }
        };
        for lz in 0..SAMPLES as i32 {
            let (dz, oz) = split(lz);
            for ly in 0..SAMPLES as i32 {
                let (dy, oy) = split(ly);
                for lx in 0..SAMPLES as i32 {
                    let (dx, ox) = split(lx);
                    let v = match nb[((dx + 1) + 3 * (dy + 1) + 9 * (dz + 1)) as usize] {
                        Some(d) => d[owned_index(ox, oy, oz)],
                        None => match nseed[((dx + 1) + 3 * (dy + 1)) as usize] {
                            Some(s) if s.covers(base[0] + lx, base[1] + ly) => {
                                s.value(base[0] + lx, base[1] + ly, base[2] + lz)
                            }
                            _ => AIR,
                        },
                    };
                    let li = ChunkShape::linearize([lx as u32, ly as u32, lz as u32]) as usize;
                    sdf[li] = v as f32 / DENSITY_SCALE;
                }
            }
        }
        sdf
    }

    /// Mesh chunk `k` from its [`Self::window`]. Positions are WoW-axis yards relative to the
    /// window origin [`window_origin`]; normals are the surface-nets gradients (WoW axes).
    pub fn mesh(&self, k: ChunkKey) -> VoxelMesh {
        let sdf = self.window(k);
        let mut buffer = SurfaceNetsBuffer::default();
        surface_nets(&sdf, &ChunkShape {}, [0; 3], [SAMPLES - 1; 3], &mut buffer);
        VoxelMesh {
            positions: buffer
                .positions
                .iter()
                .map(|p| [p[0] * VOXEL_SIZE, p[1] * VOXEL_SIZE, p[2] * VOXEL_SIZE])
                .collect(),
            normals: buffer.normals,
            indices: buffer.indices,
        }
    }

    /// The interpolated SDF (voxel units, negative = solid) at a WoW point, through active chunks
    /// and seeds (trilinear over the 8 lattice samples).
    pub fn density_at(&self, wow: [f32; 3]) -> f32 {
        let p = [
            wow[0] / VOXEL_SIZE,
            wow[1] / VOXEL_SIZE,
            wow[2] / VOXEL_SIZE,
        ];
        let i0 = [p[0].floor() as i32, p[1].floor() as i32, p[2].floor() as i32];
        let f = [p[0] - i0[0] as f32, p[1] - i0[1] as f32, p[2] - i0[2] as f32];
        let mut acc = 0.0;
        for dz in 0..2 {
            for dy in 0..2 {
                for dx in 0..2 {
                    let w = (if dx == 1 { f[0] } else { 1.0 - f[0] })
                        * (if dy == 1 { f[1] } else { 1.0 - f[1] })
                        * (if dz == 1 { f[2] } else { 1.0 - f[2] });
                    acc += w * self.sample([i0[0] + dx, i0[1] + dy, i0[2] + dz]) as f32;
                }
            }
        }
        acc / DENSITY_SCALE
    }
}

/// WoW position of a chunk's mesh-window origin (lattice `32k-1` per axis).
pub fn window_origin(k: ChunkKey) -> [f32; 3] {
    [
        lattice_to_wow(k[0] * C - 1),
        lattice_to_wow(k[1] * C - 1),
        lattice_to_wow(k[2] * C - 1),
    ]
}

/// March a ray through `density(wow) -> f32` (negative = solid) and return the first surface point
/// (refined by bisection), or `None` within `max_dist`. `dir` need not be normalised.
pub fn raymarch(
    origin: [f32; 3],
    dir: [f32; 3],
    max_dist: f32,
    step: f32,
    mut density: impl FnMut([f32; 3]) -> f32,
) -> Option<[f32; 3]> {
    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
    if len < 1e-6 {
        return None;
    }
    let d = [dir[0] / len, dir[1] / len, dir[2] / len];
    let at = |t: f32| [origin[0] + d[0] * t, origin[1] + d[1] * t, origin[2] + d[2] * t];
    let mut prev_t = 0.0;
    if density(at(0.0)) < 0.0 {
        return Some(origin); // starting inside solid
    }
    let mut t = step;
    while t <= max_dist {
        if density(at(t)) < 0.0 {
            let (mut a, mut b) = (prev_t, t);
            for _ in 0..12 {
                let m = 0.5 * (a + b);
                if density(at(m)) < 0.0 {
                    b = m;
                } else {
                    a = m;
                }
            }
            return Some(at(b));
        }
        prev_t = t;
        t += step;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Journal v2
// ---------------------------------------------------------------------------------------------

/// Per-region sync placeholders for the server lane (CONTRACT §2/§4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionSync {
    pub epoch: u16,
    pub last_seq: u32,
}

/// One map's edits in canonical order (confirmed by `seq` ascending, then unconfirmed in arrival
/// order) + per-region sync state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MapJournal {
    edits: Vec<VoxelEditOp>,
    /// How many leading entries of `edits` are confirmed (`seq: Some`).
    confirmed: usize,
    pub regions: BTreeMap<(i16, i16), RegionSync>,
}

/// Where [`MapJournal::insert`] put an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalInsert {
    /// Appended after everything already applied: apply incrementally.
    Appended,
    /// Inserted before already-applied edits: rebuild the chunks it touches (+ the later ones).
    OutOfOrder,
    /// Already present (seq seen): ignore.
    Duplicate,
}

impl MapJournal {
    /// The edits in canonical order.
    pub fn edits(&self) -> &[VoxelEditOp] {
        &self.edits
    }

    /// The local predictions still waiting for a server seq (arrival order).
    pub fn pending_local(&self) -> Vec<VoxelEditOp> {
        self.edits[self.confirmed..]
            .iter()
            .filter(|e| e.local)
            .copied()
            .collect()
    }

    /// Whether a confirmed `seq` is already in the journal.
    pub fn has_seq(&self, seq: u32) -> bool {
        self.edits[..self.confirmed]
            .binary_search_by_key(&seq, |e| e.seq.unwrap_or(0))
            .is_ok()
    }

    /// Insert an edit at its canonical position.
    pub fn insert(&mut self, e: VoxelEditOp) -> JournalInsert {
        match e.seq {
            None => {
                self.edits.push(e);
                JournalInsert::Appended
            }
            Some(seq) => {
                let conf = &self.edits[..self.confirmed];
                match conf.binary_search_by_key(&seq, |x| x.seq.unwrap_or(0)) {
                    Ok(_) => JournalInsert::Duplicate,
                    Err(pos) => {
                        self.edits.insert(pos, e);
                        self.confirmed += 1;
                        let reg = self.regions.entry(e.region()).or_default();
                        reg.last_seq = reg.last_seq.max(seq);
                        if pos + 1 == self.edits.len() {
                            JournalInsert::Appended
                        } else {
                            JournalInsert::OutOfOrder
                        }
                    }
                }
            }
        }
    }

    /// The server confirmed our prediction `nonce` as `seq`: move it into the confirmed prefix.
    /// Returns the edit if found (`None` = no such pending nonce). The density is unchanged
    /// unless the move reorders it past other edits — the caller rebuilds then (`OutOfOrder`).
    pub fn confirm(&mut self, nonce: u16, seq: u32) -> Option<(VoxelEditOp, JournalInsert)> {
        let i = self.edits[self.confirmed..]
            .iter()
            .position(|e| e.local && e.nonce == nonce)?
            + self.confirmed;
        let mut e = self.edits.remove(i);
        e.seq = Some(seq);
        let was_first_pending = i == self.confirmed;
        let r = self.insert(e);
        let r = match r {
            JournalInsert::Duplicate => JournalInsert::Duplicate,
            // It stays in place iff it was the first pending and lands at the confirmed end.
            _ if was_first_pending
                && self.edits[..self.confirmed]
                    .last()
                    .is_some_and(|x| x.seq == Some(seq)) =>
            {
                JournalInsert::Appended
            }
            _ => JournalInsert::OutOfOrder,
        };
        Some((e, r))
    }

    /// REGION-RESET: drop every edit (confirmed or not) whose centre lies in `(rx, ry)`, adopt the
    /// new epoch, `last_seq` = 0. Returns the dropped edits (the caller rebuilds their chunks).
    pub fn reset_region(&mut self, rx: i16, ry: i16, epoch: u16) -> Vec<VoxelEditOp> {
        let mut dropped = Vec::new();
        let mut kept = Vec::with_capacity(self.edits.len());
        let mut confirmed = 0;
        for (i, e) in self.edits.drain(..).enumerate() {
            if e.region() == (rx, ry) {
                dropped.push(e);
            } else {
                if i < self.confirmed {
                    confirmed += 1;
                }
                kept.push(e);
            }
        }
        self.edits = kept;
        self.confirmed = confirmed;
        self.regions.insert(
            (rx, ry),
            RegionSync {
                epoch,
                last_seq: 0,
            },
        );
        dropped
    }
}

/// The whole local journal (every map).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoxelJournal {
    pub maps: BTreeMap<u16, MapJournal>,
}

/// Why a journal file could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalError {
    /// Not a v2 journal (wrong magic) — e.g. a v1 file.
    BadMagic,
    /// A journal of another format version.
    Version(u16),
    /// Cut short / corrupt.
    Truncated,
}

const JOURNAL_MAGIC: &[u8; 4] = b"MVXW";
const JOURNAL_VERSION: u16 = 2;

impl VoxelJournal {
    /// Serialise (little-endian): `"MVXW", u16 version, u16 maps`, then per map `u16 map,
    /// u32 regions × (i16 rx, i16 ry, u16 epoch, u32 last_seq), u32 edits × (f32 x, f32 y, f32 z,
    /// f32 radius, u8 mode, u8 strength, u16 nonce, u8 flags (bit0 has_seq, bit1 local), u32 seq)`.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(JOURNAL_MAGIC);
        b.extend_from_slice(&JOURNAL_VERSION.to_le_bytes());
        b.extend_from_slice(&(self.maps.len() as u16).to_le_bytes());
        for (&map, j) in &self.maps {
            b.extend_from_slice(&map.to_le_bytes());
            b.extend_from_slice(&(j.regions.len() as u32).to_le_bytes());
            for (&(rx, ry), r) in &j.regions {
                b.extend_from_slice(&rx.to_le_bytes());
                b.extend_from_slice(&ry.to_le_bytes());
                b.extend_from_slice(&r.epoch.to_le_bytes());
                b.extend_from_slice(&r.last_seq.to_le_bytes());
            }
            b.extend_from_slice(&(j.edits.len() as u32).to_le_bytes());
            for e in &j.edits {
                for v in [e.pos[0], e.pos[1], e.pos[2], e.radius] {
                    b.extend_from_slice(&v.to_le_bytes());
                }
                b.push(e.mode);
                b.push(e.strength);
                b.extend_from_slice(&e.nonce.to_le_bytes());
                // Flags: bit 0 = has seq, bit 1 = local.
                b.push(e.seq.is_some() as u8 | (e.local as u8) << 1);
                b.extend_from_slice(&e.seq.unwrap_or(0).to_le_bytes());
            }
        }
        b
    }

    /// Parse [`Self::encode`]'s output.
    pub fn decode(b: &[u8]) -> Result<Self, JournalError> {
        struct R<'a>(&'a [u8], usize);
        impl R<'_> {
            fn take(&mut self, n: usize) -> Result<&[u8], JournalError> {
                let s = self.0.get(self.1..self.1 + n).ok_or(JournalError::Truncated)?;
                self.1 += n;
                Ok(s)
            }
            fn u8(&mut self) -> Result<u8, JournalError> {
                Ok(self.take(1)?[0])
            }
            fn u16(&mut self) -> Result<u16, JournalError> {
                let s = self.take(2)?;
                Ok(u16::from_le_bytes([s[0], s[1]]))
            }
            fn u32(&mut self) -> Result<u32, JournalError> {
                let s = self.take(4)?;
                Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            }
            fn f32(&mut self) -> Result<f32, JournalError> {
                Ok(f32::from_bits(self.u32()?))
            }
        }
        if b.len() < 4 || &b[..4] != JOURNAL_MAGIC {
            return Err(JournalError::BadMagic);
        }
        let mut r = R(b, 4);
        let v = r.u16()?;
        if v != JOURNAL_VERSION {
            return Err(JournalError::Version(v));
        }
        let mut out = Self::default();
        for _ in 0..r.u16()? {
            let map = r.u16()?;
            let mut j = MapJournal::default();
            for _ in 0..r.u32()? {
                let rx = r.u16()? as i16;
                let ry = r.u16()? as i16;
                let epoch = r.u16()?;
                let last_seq = r.u32()?;
                j.regions.insert((rx, ry), RegionSync { epoch, last_seq });
            }
            let n = r.u32()? as usize;
            let mut edits = Vec::with_capacity(n.min(1 << 20));
            for _ in 0..n {
                let pos = [r.f32()?, r.f32()?, r.f32()?];
                let radius = r.f32()?;
                let mode = r.u8()?;
                let strength = r.u8()?;
                let nonce = r.u16()?;
                let flags = r.u8()?;
                let has = flags & 1 != 0;
                let seq = r.u32()?;
                edits.push(VoxelEditOp {
                    map,
                    pos,
                    radius,
                    mode,
                    strength,
                    nonce,
                    seq: has.then_some(seq),
                    local: flags & 2 != 0,
                });
            }
            // Re-establish the canonical order whatever the file held.
            let regions = std::mem::take(&mut j.regions);
            for e in edits.iter().filter(|e| e.seq.is_some()) {
                j.insert(*e);
            }
            for e in edits.iter().filter(|e| e.seq.is_none()) {
                j.insert(*e);
            }
            j.regions = regions;
            out.maps.insert(map, j);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A gently sloped + bumpy deterministic ground (no holes).
    fn ground(x: f32, y: f32) -> Option<f32> {
        Some(20.0 + 0.07 * x - 0.05 * y + (x * 0.3).sin() * 0.8 + (y * 0.21).cos() * 0.6)
    }

    fn world_with(keys: &[ChunkKey], edits: &[VoxelEditOp]) -> VoxelWorldData {
        let mut w = VoxelWorldData::default();
        for k in keys {
            w.seeds
                .entry([k[0], k[1]])
                .or_insert_with(|| Arc::new(ColumnSeed::from_fn([k[0], k[1]], ground)));
        }
        w.build(keys, edits.iter());
        w
    }

    fn edit(pos: [f32; 3], radius: f32, mode: u8) -> VoxelEditOp {
        VoxelEditOp {
            map: 0,
            pos,
            radius,
            mode,
            strength: 255,
            nonce: 0,
            seq: None,
            local: true,
        }
    }

    #[test]
    fn adjacent_column_seeds_agree_on_their_overlap() {
        let a = ColumnSeed::from_fn([3, -2], ground);
        let b = ColumnSeed::from_fn([4, -2], ground);
        let mut shared = 0;
        for iy in -2 * 32 - 2..-2 * 32 + 34 {
            for ix in 4 * 32 - 2..3 * 32 + 34 {
                if a.covers(ix, iy) && b.covers(ix, iy) {
                    assert_eq!(a.height_lat(ix, iy), b.height_lat(ix, iy));
                    for iz in 30..50 {
                        assert_eq!(a.value(ix, iy, iz), b.value(ix, iy, iz));
                    }
                    shared += 1;
                }
            }
        }
        assert!(shared >= 4 * 36, "the windows overlap by 4 columns ({shared})");
    }

    #[test]
    fn apron_equals_the_neighbour_whether_active_or_seeded() {
        // Surface ~20 yd → kz = 1.
        let a = [2, 5, 1];
        let b = [3, 5, 1];
        let e = [edit([47.5, 88.0, 20.0], 3.0, 0)]; // a dig straddling the a|b seam (x = 48)
        // Both active: A's +x apron (window x = 33) == B's owned x = 0, through its density.
        let w = world_with(&[a, b], &e);
        let win = w.window(a);
        let bd = &w.chunks[&b];
        for z in 0..32 {
            for y in 0..32 {
                let li = ChunkShape::linearize([33, (y + 1) as u32, (z + 1) as u32]) as usize;
                assert_eq!(win[li], bd[owned_index(0, y, z)] as f32 / DENSITY_SCALE);
            }
        }
        // B inactive: A's apron is the seed of B's samples (identical through either window).
        let w2 = world_with(&[a], &[]);
        let win2 = w2.window(a);
        let sb = ColumnSeed::from_fn([3, 5], ground);
        for z in 0..32 {
            for y in 0..32 {
                let li = ChunkShape::linearize([33, (y + 1) as u32, (z + 1) as u32]) as usize;
                let v = sb.value(3 * 32, 5 * 32 + y, 32 + z);
                assert_eq!(win2[li], v as f32 / DENSITY_SCALE);
            }
        }
    }

    /// Weld every chunk's mesh into global WoW space and count edge uses: an edge used once is a
    /// boundary; none may lie on the shared interior faces of the 2×2×2 block.
    #[test]
    fn edit_spanning_2x2x2_chunks_is_crack_free() {
        // The block chunks (0..=1)^3 (0..32 yd); the ground is ~19 yd. A dig centred on the shared
        // corner (16, 16, 16.5) opens a crater whose walls cross all three internal planes, a
        // floating build sits above it and a smooth straddles the corner.
        let keys: Vec<ChunkKey> = (0..8).map(|i| [i & 1, (i >> 1) & 1, (i >> 2) & 1]).collect();
        let edits = [
            edit([16.0, 16.0, 16.5], 6.0, 0),
            edit([16.0, 16.0, 24.0], 3.0, 1),
            edit([16.3, 15.6, 17.0], 4.0, 2),
        ];
        let w = world_with(&keys, &edits);
        let q = |v: f32| (v * 1000.0).round() as i64;
        let mut verts: HashMap<[i64; 3], u32> = HashMap::new();
        let mut pos: Vec<[f32; 3]> = Vec::new();
        let mut edges: HashMap<(u32, u32), u32> = HashMap::new();
        let mut tris = 0;
        for &k in &keys {
            let m = w.mesh(k);
            let o = window_origin(k);
            let ids: Vec<u32> = m
                .positions
                .iter()
                .map(|p| {
                    let g = [p[0] + o[0], p[1] + o[1], p[2] + o[2]];
                    let key = [q(g[0]), q(g[1]), q(g[2])];
                    *verts.entry(key).or_insert_with(|| {
                        pos.push(g);
                        (pos.len() - 1) as u32
                    })
                })
                .collect();
            for t in m.indices.chunks_exact(3) {
                tris += 1;
                let t = [ids[t[0] as usize], ids[t[1] as usize], ids[t[2] as usize]];
                for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                    *edges.entry((a.min(b), a.max(b))).or_default() += 1;
                }
            }
        }
        assert!(tris > 500, "the edits produced a surface ({tris} tris)");
        let mut boundary = 0;
        for (&(a, b), &n) in &edges {
            if n != 1 {
                continue;
            }
            boundary += 1;
            let m = [
                (pos[a as usize][0] + pos[b as usize][0]) * 0.5,
                (pos[a as usize][1] + pos[b as usize][1]) * 0.5,
                (pos[a as usize][2] + pos[b as usize][2]) * 0.5,
            ];
            // A boundary edge must sit on the OUTER skin of the 32-yd block (within one voxel).
            let outer = (0..3).any(|ax| m[ax] < 1.0 || m[ax] > 31.0);
            assert!(outer, "crack: boundary edge at {m:?} inside the block");
        }
        assert!(boundary > 0, "the slab surface meets the outer skin");
    }

    #[test]
    fn incremental_equals_canonical_rebuild() {
        let keys: Vec<ChunkKey> = vec![[0, 0, 1], [1, 0, 1], [0, 1, 1], [1, 1, 1]];
        let edits = [
            edit([15.5, 16.2, 21.0], 4.0, 0),
            edit([17.0, 15.0, 22.0], 2.5, 1),
            edit([16.1, 16.4, 20.0], 3.0, 2),
        ];
        let canonical = world_with(&keys, &edits);
        let mut inc = world_with(&keys, &[]);
        for e in &edits {
            inc.apply_incremental(e);
        }
        for k in &keys {
            assert_eq!(canonical.chunks[k], inc.chunks[k], "chunk {k:?} diverged");
        }
    }

    #[test]
    fn edit_wire_and_hex_round_trip() {
        let e = VoxelEditOp {
            map: 1,
            pos: [-9555.25, -306.5, 62.125],
            radius: 2.0,
            mode: 2,
            strength: 128,
            nonce: 0xbeef,
            seq: None,
            local: false,
        };
        let h = e.to_hex();
        assert_eq!(h.len(), 46);
        assert_eq!(&h[..2], "01");
        assert_eq!(VoxelEditOp::from_hex(&h), Some(e));
        assert!(VoxelEditOp::from_hex("01").is_none());
        let mut bad = e;
        bad.radius = f32::NAN;
        assert!(VoxelEditOp::from_hex(&bad.to_hex()).is_none());
    }

    #[test]
    fn journal_v2_round_trips_and_orders() {
        let mut j = VoxelJournal::default();
        let m = j.maps.entry(0).or_default();
        let mut e = edit([10.0, 20.0, 30.0], 2.0, 0);
        e.nonce = 7;
        assert_eq!(m.insert(e), JournalInsert::Appended); // local prediction
        let mut s5 = edit([100.0, 20.0, 30.0], 3.0, 1);
        s5.seq = Some(5);
        assert_eq!(m.insert(s5), JournalInsert::OutOfOrder); // goes before the prediction
        let mut s3 = s5;
        s3.seq = Some(3);
        assert_eq!(m.insert(s3), JournalInsert::OutOfOrder);
        assert_eq!(m.insert(s3), JournalInsert::Duplicate);
        assert!(m.has_seq(5) && m.has_seq(3) && !m.has_seq(4));
        let seqs: Vec<_> = m.edits().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![Some(3), Some(5), None]);
        assert_eq!(m.pending_local().len(), 1);
        let (c, how) = m.confirm(7, 9).expect("pending nonce");
        assert_eq!(c.seq, Some(9));
        assert_eq!(how, JournalInsert::Appended, "first pending → confirmed tail: no reorder");
        assert!(m.pending_local().is_empty());
        m.regions.insert((1, -2), RegionSync { epoch: 3, last_seq: 9 });
        let mut other = edit([1.0, 2.0, 3.0], 1.0, 2);
        other.map = 1;
        j.maps.entry(1).or_default().insert(other);

        let bytes = j.encode();
        let back = VoxelJournal::decode(&bytes).expect("decode");
        assert_eq!(back, j);
        assert_eq!(VoxelJournal::decode(b"\x01\x02\x03"), Err(JournalError::BadMagic));
        let mut v1 = bytes.clone();
        v1[4] = 1;
        assert_eq!(VoxelJournal::decode(&v1), Err(JournalError::Version(1)));
        assert_eq!(
            VoxelJournal::decode(&bytes[..bytes.len() - 3]),
            Err(JournalError::Truncated)
        );
    }

    #[test]
    fn region_reset_drops_its_edits() {
        let mut m = MapJournal::default();
        m.insert(edit([10.0, 10.0, 0.0], 1.0, 0)); // region (0,0)
        m.insert(edit([70.0, 10.0, 0.0], 1.0, 0)); // region (1,0)
        let dropped = m.reset_region(0, 0, 4);
        assert_eq!(dropped.len(), 1);
        assert_eq!(m.edits().len(), 1);
        assert_eq!(m.regions[&(0, 0)], RegionSync { epoch: 4, last_seq: 0 });
    }

    #[test]
    fn surface_chunks_bracket_the_ground_and_raymarch_hits_it() {
        let s = ColumnSeed::from_fn([0, 0], |_, _| Some(31.9));
        // 31.9 + 0.15 = 32.05 → lattice 64.1: the surface crosses into kz 2 (from 32 yd).
        let (lo, hi) = s.surface_chunks().unwrap();
        assert!(lo <= 1 && hi >= 2, "{lo}..{hi}");
        let air = ColumnSeed::from_fn([0, 0], |_, _| None);
        assert!(air.surface_chunks().is_none());
        let mut w = VoxelWorldData::default();
        w.seeds.insert([0, 0], Arc::new(s));
        let hit = raymarch([8.0, 8.0, 60.0], [0.0, 0.1, -1.0], 100.0, 0.25, |p| w.density_at(p))
            .expect("hit the seed");
        assert!((hit[2] - 32.05).abs() < 0.1, "{hit:?}");
    }
}

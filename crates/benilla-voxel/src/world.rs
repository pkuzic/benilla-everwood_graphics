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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
/// MONKEY (voxel): the v3 EDIT3 op byte (CONTRACT §9.1).
pub const EDIT3_OP: u8 = 0x02;
/// MONKEY (voxel): bytes of the v3 EDIT3 body (`op u8, nonce u16, stroke u16, map u16, x y z radius
/// f32, mode u8, strength u8, param u16`, CONTRACT §9.1).
pub const EDIT3_WIRE_LEN: usize = 27;

/// MONKEY (voxel): edit modes (CONTRACT §1 + §9.3).
pub const MODE_DIG: u8 = 0;
pub const MODE_BUILD: u8 = 1;
pub const MODE_SMOOTH: u8 = 2;
/// Pull toward the plane `z + (param as i16) / 100`, blend `strength/255` ([`apply_edit_set`]).
pub const MODE_FLATTEN: u8 = 3;
/// Wire-defined (CONTRACT §9.3) but not applied by this client yet: journalled, no density change.
pub const MODE_PAINT: u8 = 4;
/// Wire-defined (CONTRACT §9.3) but not applied by this client yet: journalled, no density change.
pub const MODE_ROUGHEN: u8 = 5;
/// The highest mode the wire defines.
pub const MODE_MAX: u8 = MODE_ROUGHEN;

/// MONKEY (voxel, CONTRACT v3.3): `param` bit 15 on modes 0 / 1 / 2 = a SOFT brush (Noggit-like:
/// lower / raise / smooth by an amount with a radial falloff, see [`SoftBrush`]). `param` without
/// it (every v2 / v3.0 edit: 0) keeps the hard CSG dig / build and the uniform smooth.
pub const SOFT_FLAG: u16 = 0x8000;
/// The soft brushes' falloff curves (`param` bits 0-1).
pub const FALLOFF_SMOOTH: u8 = 0;
pub const FALLOFF_LINEAR: u8 = 1;
pub const FALLOFF_FLAT: u8 = 2;
/// MONKEY (voxel): the voxel field is DRAWN (and collided) this far below its density surface: the
/// seed sits [`SEED_LIFT`] above the ADT, so an unedited voxel surface then lies exactly on the
/// real ground (no "lifted border" step at the carve's edge) plus 0.02 yd, so where both overlap
/// (the carve's 0.5-yd inset band) the real ground wins the depth test. Client presentation only:
/// the density, the wire and the server replica are unchanged.
pub const RENDER_DROP: f32 = SEED_LIFT + 0.02;

/// MONKEY (voxel, CONTRACT v3.3): a decoded soft-brush `param` (modes 0 / 1 / 2 with
/// [`SOFT_FLAG`]). Bits: 0-1 falloff (0 smooth, 1 linear, 2 flat, 3 = smooth), 2-8 hardness
/// percent (0..=127, values over 100 count as 100), 9-14 reserved (sent 0, ignored), 15 = 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftBrush {
    pub falloff: u8,
    pub hardness: u8,
}

impl SoftBrush {
    /// Decode a `param`; `None` = not a soft brush (bit 15 clear).
    pub fn from_param(param: u16) -> Option<Self> {
        if param & SOFT_FLAG == 0 {
            return None;
        }
        let f = (param & 0x3) as u8;
        Some(Self {
            falloff: if f == 3 { FALLOFF_SMOOTH } else { f },
            hardness: (((param >> 2) & 0x7f) as u8).min(100),
        })
    }

    /// The wire `param` (bit 15 set, reserved bits 0).
    pub fn to_param(self) -> u16 {
        SOFT_FLAG | (self.falloff.min(2) as u16) | ((self.hardness.min(100) as u16) << 2)
    }

    /// The falloff weight of a sample at lattice distance `dist` from the centre of a brush of
    /// lattice radius `r` (0 outside). EXACT op order (the server's C++ replica copies it, IEEE
    /// f32, no FMA): `inner = r * (hardness as f32 / 100.0)`; `dist >= r` → 0; `dist <= inner` →
    /// 1; else `t = (dist - inner) / (r - inner)`, smooth `u = 1 - t; (u * u) * (3 - 2 * u)`,
    /// linear `1 - t`, flat `1`.
    #[inline]
    pub fn weight(self, dist: f32, r: f32) -> f32 {
        if dist >= r {
            return 0.0;
        }
        let inner = r * (self.hardness as f32 / 100.0);
        if dist <= inner {
            return 1.0;
        }
        let t = (dist - inner) / (r - inner);
        match self.falloff {
            FALLOFF_LINEAR => 1.0 - t,
            FALLOFF_FLAT => 1.0,
            _ => {
                let u = 1.0 - t;
                (u * u) * (3.0 - 2.0 * u)
            }
        }
    }
}

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

/// One terrain edit (CONTRACT §1 ops, §3 EDIT / §9.1 EDIT3 body, + the server-assigned `seq` once
/// confirmed).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoxelEditOp {
    pub map: u16,
    /// Brush centre, WoW yards (x north, y west, z up).
    pub pos: [f32; 3],
    /// Brush radius, yards.
    pub radius: f32,
    /// 0 dig (Subtract), 1 build (Add), 2 smooth, 3 flatten; 4 paint / 5 roughen are wire-defined
    /// but not applied yet (`MODE_*`).
    pub mode: u8,
    /// Smooth / flatten blend /255 (dig/build ignore it; sent as 255).
    pub strength: u8,
    /// Per-sender nonce (matches the server echo to the local prediction).
    pub nonce: u16,
    /// MONKEY (voxel): v3 stroke id (CONTRACT §9.4); 0 = no stroke (every v2 edit).
    pub stroke: u16,
    /// MONKEY (voxel): v3 mode parameter (CONTRACT §9.3): flatten = plane offset in cm as an
    /// **i16** two's complement; paint = material; roughen = seed; else 0.
    pub param: u16,
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
            && self.mode <= MODE_MAX
    }

    /// MONKEY (voxel): whether this mode changes density on this client (4 paint / 5 roughen are
    /// parsed + journalled but not applied yet).
    pub fn is_applied(&self) -> bool {
        self.mode <= MODE_FLATTEN
    }

    /// MONKEY (voxel): whether applying this edit READS samples it does not write (smooth's
    /// neighbour average) — such an edit links every chunk it touches for a canonical rebuild
    /// ([`MapJournal::read_closure`]). Dig / build / flatten are per-sample.
    pub fn reads_neighbours(&self) -> bool {
        self.mode == MODE_SMOOTH || self.soft_shift().is_some()
    }

    /// MONKEY (voxel, CONTRACT v3.3): the soft brush of a mode 0 / 1 / 2 edit (`param` bit 15).
    pub fn soft(&self) -> Option<SoftBrush> {
        match self.mode {
            MODE_DIG | MODE_BUILD | MODE_SMOOTH => SoftBrush::from_param(self.param),
            _ => None,
        }
    }

    /// MONKEY (voxel): a soft LOWER (mode 0) / RAISE (mode 1): the brush + the z direction it reads
    /// from (+1 = the sample above, lower; -1 = the sample below, raise). Reads its z neighbour.
    pub fn soft_shift(&self) -> Option<(SoftBrush, i32)> {
        match self.mode {
            MODE_DIG => self.soft().map(|b| (b, 1)),
            MODE_BUILD => self.soft().map(|b| (b, -1)),
            _ => None,
        }
    }

    /// MONKEY (voxel): the flatten plane height in WoW yards (`z + (param as i16) / 100`).
    pub fn flatten_plane(&self) -> f32 {
        self.pos[2] + (self.param as i16) as f32 / 100.0
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
            stroke: 0,
            param: 0,
        };
        e.is_valid().then_some(e)
    }

    /// MONKEY (voxel): the 27-byte v3 EDIT3 body (CONTRACT §9.1, little-endian).
    pub fn to_wire3(&self) -> [u8; EDIT3_WIRE_LEN] {
        let mut b = [0u8; EDIT3_WIRE_LEN];
        b[0] = EDIT3_OP;
        b[1..3].copy_from_slice(&self.nonce.to_le_bytes());
        b[3..5].copy_from_slice(&self.stroke.to_le_bytes());
        b[5..7].copy_from_slice(&self.map.to_le_bytes());
        b[7..11].copy_from_slice(&self.pos[0].to_le_bytes());
        b[11..15].copy_from_slice(&self.pos[1].to_le_bytes());
        b[15..19].copy_from_slice(&self.pos[2].to_le_bytes());
        b[19..23].copy_from_slice(&self.radius.to_le_bytes());
        b[23] = self.mode;
        b[24] = self.strength;
        b[25..27].copy_from_slice(&self.param.to_le_bytes());
        b
    }

    /// MONKEY (voxel): decode a v3 EDIT3 body (`seq` = `None`); `None` if malformed / invalid.
    pub fn from_wire3(b: &[u8]) -> Option<Self> {
        if b.len() != EDIT3_WIRE_LEN || b[0] != EDIT3_OP {
            return None;
        }
        let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let e = Self {
            nonce: u16::from_le_bytes([b[1], b[2]]),
            stroke: u16::from_le_bytes([b[3], b[4]]),
            map: u16::from_le_bytes([b[5], b[6]]),
            pos: [f(7), f(11), f(15)],
            radius: f(19),
            mode: b[23],
            strength: b[24],
            param: u16::from_le_bytes([b[25], b[26]]),
            seq: None,
            local: false,
        };
        e.is_valid().then_some(e)
    }

    /// MONKEY (voxel): lowercase hex of [`Self::to_wire3`] (54 chars).
    pub fn to_hex3(&self) -> String {
        self.to_wire3().iter().map(|x| format!("{x:02x}")).collect()
    }

    /// MONKEY (voxel): decode [`Self::to_hex3`].
    pub fn from_hex3(s: &str) -> Option<Self> {
        if s.len() != EDIT3_WIRE_LEN * 2 || !s.is_ascii() {
            return None;
        }
        let b: Vec<u8> = (0..EDIT3_WIRE_LEN)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        Self::from_wire3(&b)
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
    let soft = edit.soft();
    match edit.mode {
        0 | 1 if soft.is_none() => {
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
        MODE_FLATTEN => {
            // MONKEY (voxel): per-sample (reads only the sample it writes), so it never links
            // chunks. Exact op order (the server's C++ replica must match it bit for bit, IEEE f32,
            // no FMA): plane `zp = (z + (param as i16) as f32 / 100) / VOXEL_SIZE` (lattice);
            // `d = sqrt(dx*dx + dy*dy + dz*dz)`; weight `f = min((r - d) / band, 1)` with
            // `band = max(r * 0.25, 1)` (skip `f <= 0`, i.e. outside the sphere); target
            // `t = q((z - zp) * DENSITY_SCALE)`; `new = q(old + (t - old) * (s * f))`, `s` =
            // strength / 255, `q` = round-half-away + clamp to i8.
            let zp = edit.flatten_plane() / VOXEL_SIZE;
            let s = edit.strength as f32 / 255.0;
            let band = (r * 0.25).max(1.0);
            for k in chunks_of_box(lo, hi) {
                let Some(d) = set.get_mut(&k) else {
                    continue;
                };
                let base = [k[0] * C, k[1] * C, k[2] * C];
                let mut any = false;
                for z in lo[2].max(base[2])..=hi[2].min(base[2] + C - 1) {
                    let t = ((z as f32 - zp) * DENSITY_SCALE)
                        .round()
                        .clamp(i8::MIN as f32, i8::MAX as f32);
                    for y in lo[1].max(base[1])..=hi[1].min(base[1] + C - 1) {
                        for x in lo[0].max(base[0])..=hi[0].min(base[0] + C - 1) {
                            let (dx, dy, dz) = (x as f32 - c[0], y as f32 - c[1], z as f32 - c[2]);
                            let dist = (dx * dx + dy * dy + dz * dz).sqrt();
                            let f = ((r - dist) / band).min(1.0);
                            if f <= 0.0 {
                                continue;
                            }
                            let idx = owned_index(x - base[0], y - base[1], z - base[2]);
                            let old = d[idx] as f32;
                            let q = (old + (t - old) * (s * f))
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
        // MONKEY (voxel): smooth (hard or soft) and the CONTRACT v3.3 soft lower / raise — all read
        // the PRE-edit field around the sample they write (Jacobi), so they share the gather.
        MODE_DIG | MODE_BUILD | MODE_SMOOTH => {
            let strength = edit.strength as f32 / 255.0;
            // The z neighbour a soft lower (+1) / raise (-1) reads; 0 = smooth.
            let shift_dz = edit.soft_shift().map_or(0, |(_, d)| d);
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
                        // MONKEY (voxel, CONTRACT v3.3): a soft brush weighs each sample by its
                        // falloff `w` (skip `w <= 0`, i.e. outside the sphere); the hard smooth
                        // keeps its v2 maths bit for bit (uniform blend inside `d2 <= r2`).
                        let a = match soft {
                            Some(b) => {
                                let (dx, dy, dz) = (x as f32 - c[0], y as f32 - c[1], z as f32 - c[2]);
                                let w = b.weight((dx * dx + dy * dy + dz * dz).sqrt(), r);
                                if w <= 0.0 {
                                    continue;
                                }
                                strength * w
                            }
                            None => {
                                let d2 = (x as f32 - c[0]).powi(2)
                                    + (y as f32 - c[1]).powi(2)
                                    + (z as f32 - c[2]).powi(2);
                                if d2 > r2 {
                                    continue;
                                }
                                strength
                            }
                        };
                        let k = [chunk_of(x), chunk_of(y), chunk_of(z)];
                        let Some(d) = set.get_mut(&k) else {
                            continue;
                        };
                        let g = |dx: i32, dy: i32, dz: i32| grid[at([x + dx, y + dy, z + dz])] as f32;
                        let old = g(0, 0, 0);
                        let new = if shift_dz != 0 {
                            // Soft lower / raise: the field moves vertically by `a` voxels (≤ 1) —
                            // linear between this sample and its z neighbour, so a held brush lifts
                            // the ground like a heightmap (no clamp-saturation bubble).
                            let n = g(0, 0, shift_dz);
                            old + (n - old) * a
                        } else {
                            let avg = (g(1, 0, 0)
                                + g(-1, 0, 0)
                                + g(0, 1, 0)
                                + g(0, -1, 0)
                                + g(0, 0, 1)
                                + g(0, 0, -1))
                                / 6.0;
                            if soft.is_some() {
                                old + (avg - old) * a
                            } else {
                                old * (1.0 - strength) + avg * strength
                            }
                        };
                        let new = new
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
        // MONKEY (voxel): 4 paint / 5 roughen — wire-defined, not applied by this client yet
        // (the world layer logs that once); anything else is invalid and never reaches here.
        _ => {}
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
    ///
    /// MONKEY (voxel): = [`Self::build_job`] + [`VoxelBuildJob::run`] + [`Self::insert_built`]
    /// on the calling thread; the world layer runs the job on the async pool instead.
    pub fn build<'a>(
        &mut self,
        keys: &[ChunkKey],
        edits: impl IntoIterator<Item = &'a VoxelEditOp>,
    ) -> Vec<ChunkKey> {
        let job = self.build_job(keys, edits.into_iter().copied().collect());
        let out = job.run();
        self.insert_built(out)
    }

    /// MONKEY (voxel): snapshot everything a canonical build of `keys` reads into a
    /// self-contained, `Send` [`VoxelBuildJob`]: the set's column seeds, the edits (canonical
    /// order; ones not touching the set are skipped by the job), and — for smooth edits only, the
    /// one brush that reads past the samples it writes — the seeds + active densities of the
    /// chunks outside the set its read ring reaches. Running the job anywhere gives exactly what
    /// [`Self::build`] gives now.
    pub fn build_job(&self, keys: &[ChunkKey], edits: Vec<VoxelEditOp>) -> VoxelBuildJob {
        let keys: Vec<ChunkKey> = keys
            .iter()
            .copied()
            .filter(|k| self.seeds.contains_key(&[k[0], k[1]]))
            .collect();
        let mut seeds: HashMap<ColumnKey, Arc<ColumnSeed>> = HashMap::new();
        for k in &keys {
            let col = [k[0], k[1]];
            if let Some(s) = self.seeds.get(&col) {
                seeds.entry(col).or_insert_with(|| s.clone());
            }
        }
        let set: HashSet<ChunkKey> = keys.iter().copied().collect();
        let mut outside: HashMap<ChunkKey, Box<[i8]>> = HashMap::new();
        for e in &edits {
            if !e.reads_neighbours() || !keys.iter().any(|&k| e.touches(k)) {
                continue;
            }
            let (lo, hi) = e.lattice_bbox();
            for n in chunks_of_box([lo[0] - 1, lo[1] - 1, lo[2] - 1], [hi[0] + 1, hi[1] + 1, hi[2] + 1])
            {
                if set.contains(&n) {
                    continue;
                }
                let col = [n[0], n[1]];
                if let Some(s) = self.seeds.get(&col) {
                    seeds.entry(col).or_insert_with(|| s.clone());
                }
                if let Some(d) = self.chunks.get(&n) {
                    outside.entry(n).or_insert_with(|| d.clone());
                }
            }
        }
        VoxelBuildJob {
            keys,
            seeds,
            outside,
            edits,
        }
    }

    /// MONKEY (voxel): adopt a finished build's densities; returns their keys.
    pub fn insert_built(&mut self, out: Vec<(ChunkKey, Box<[i8]>)>) -> Vec<ChunkKey> {
        let keys = out.iter().map(|(k, _)| *k).collect();
        self.chunks.extend(out);
        keys
    }

    /// The 34³ f32 SDF window of chunk `k` (lattice `32k-1 ..= 32k+32`, x fastest): the chunk's
    /// own samples + the apron from the owning neighbours (active density, else seed — through the
    /// owner column's seed if loaded, else this column's seed window).
    pub fn window(&self, k: ChunkKey) -> Vec<f32> {
        self.window_i8(k)
            .iter()
            .map(|&v| v as f32 / DENSITY_SCALE)
            .collect()
    }

    /// MONKEY (voxel): [`Self::window`] in fixed point — the `Send` snapshot a mesh task takes
    /// (39 KB). The chunk's own 32³ is row-copied; only the 6.5 K apron samples are looked up.
    pub fn window_i8(&self, k: ChunkKey) -> Vec<i8> {
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
        let sample = |lx: i32, ly: i32, lz: i32| -> i8 {
            let ((dx, ox), (dy, oy), (dz, oz)) = (split(lx), split(ly), split(lz));
            match nb[((dx + 1) + 3 * (dy + 1) + 9 * (dz + 1)) as usize] {
                Some(d) => d[owned_index(ox, oy, oz)],
                None => match nseed[((dx + 1) + 3 * (dy + 1)) as usize] {
                    Some(s) if s.covers(base[0] + lx, base[1] + ly) => {
                        s.value(base[0] + lx, base[1] + ly, base[2] + lz)
                    }
                    _ => AIR,
                },
            }
        };
        let s = SAMPLES as i32;
        let lin = |x: i32, y: i32, z: i32| ChunkShape::linearize([x as u32, y as u32, z as u32]) as usize;
        let mut w = vec![AIR; ChunkShape::USIZE];
        match nb[13] {
            Some(own) => {
                // The owned interior: 32-sample rows, contiguous on both sides (x fastest).
                for lz in 1..=C {
                    for ly in 1..=C {
                        let dst = lin(1, ly, lz);
                        let src = owned_index(0, ly - 1, lz - 1);
                        w[dst..dst + C as usize].copy_from_slice(&own[src..src + C as usize]);
                    }
                }
                for lz in 0..s {
                    for ly in 0..s {
                        if lz == 0 || lz == s - 1 || ly == 0 || ly == s - 1 {
                            for lx in 0..s {
                                w[lin(lx, ly, lz)] = sample(lx, ly, lz);
                            }
                        } else {
                            w[lin(0, ly, lz)] = sample(0, ly, lz);
                            w[lin(s - 1, ly, lz)] = sample(s - 1, ly, lz);
                        }
                    }
                }
            }
            None => {
                for lz in 0..s {
                    for ly in 0..s {
                        for lx in 0..s {
                            w[lin(lx, ly, lz)] = sample(lx, ly, lz);
                        }
                    }
                }
            }
        }
        w
    }

    /// MONKEY (voxel): the SEEDED surface height (WoW z, lift included; `-inf` = no ground) of
    /// every lattice column of chunk `k`'s 34×34 mesh-window footprint (x fastest), read through
    /// the owner column's seed, else `k`'s own seed window, else `-inf`. The mesh job compares the
    /// voxel surface against it ([`edited_weights`]): dug / built faces read as earth.
    pub fn window_seed_heights(&self, k: ChunkKey) -> Vec<f32> {
        let s = SAMPLES as i32;
        let base = [k[0] * C - 1, k[1] * C - 1];
        let own = self.seeds.get(&[k[0], k[1]]);
        let mut out = Vec::with_capacity((s * s) as usize);
        for ly in 0..s {
            for lx in 0..s {
                let (ix, iy) = (base[0] + lx, base[1] + ly);
                let h = [self.seeds.get(&[chunk_of(ix), chunk_of(iy)]), own]
                    .into_iter()
                    .flatten()
                    .find(|sd| sd.covers(ix, iy))
                    .map_or(f32::NEG_INFINITY, |sd| sd.height_lat(ix, iy));
                out.push(h);
            }
        }
        out
    }

    /// Mesh chunk `k` from its [`Self::window`]. Positions are WoW-axis yards relative to the
    /// window origin [`window_origin`]; normals are the surface-nets gradients (WoW axes).
    pub fn mesh(&self, k: ChunkKey) -> VoxelMesh {
        mesh_window(&self.window_i8(k))
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

    /// MONKEY (voxel): the upward-facing surface ("top") over the WoW column `(x, y)` nearest
    /// `near_z`, within ±`reach` yd: `Some((z, outward unit normal))` where the field crosses from
    /// air above to solid below and the normal's z ≥ `min_up`. `None` when the column holds no
    /// such top in range (only walls / undersides, or all air / all solid) — a ground-effect tuft
    /// there has nothing to grow on. Bilinear in XY, linear in z between lattice samples.
    pub fn top_near(
        &self,
        x: f32,
        y: f32,
        near_z: f32,
        reach: f32,
        min_up: f32,
    ) -> Option<(f32, [f32; 3])> {
        let (px, py) = (x / VOXEL_SIZE, y / VOXEL_SIZE);
        let (ix, iy) = (px.floor() as i32, py.floor() as i32);
        let (fx, fy) = (px - ix as f32, py - iy as f32);
        let w = [
            (1.0 - fx) * (1.0 - fy),
            fx * (1.0 - fy),
            (1.0 - fx) * fy,
            fx * fy,
        ];
        let d = |iz: i32| -> f32 {
            (w[0] * self.sample([ix, iy, iz]) as f32
                + w[1] * self.sample([ix + 1, iy, iz]) as f32
                + w[2] * self.sample([ix, iy + 1, iz]) as f32
                + w[3] * self.sample([ix + 1, iy + 1, iz]) as f32)
                / DENSITY_SCALE
        };
        let lo = ((near_z - reach) / VOXEL_SIZE).floor() as i32;
        let hi = ((near_z + reach) / VOXEL_SIZE).ceil() as i32;
        let mut best: Option<(f32, [f32; 3])> = None;
        let mut above = d(hi);
        for iz in (lo..hi).rev() {
            let here = d(iz);
            if above >= 0.0 && here < 0.0 {
                // Air at iz+1, solid at iz: the crossing.
                let t = above / (above - here);
                let z = lattice_to_wow(iz + 1) - t * VOXEL_SIZE;
                let h = VOXEL_SIZE;
                let g = [
                    self.density_at([x + h, y, z]) - self.density_at([x - h, y, z]),
                    self.density_at([x, y + h, z]) - self.density_at([x, y - h, z]),
                    self.density_at([x, y, z + h]) - self.density_at([x, y, z - h]),
                ];
                let len = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt();
                if len > 1.0e-6 {
                    let n = [g[0] / len, g[1] / len, g[2] / len];
                    let closer =
                        best.is_none_or(|(bz, _)| (z - near_z).abs() < (bz - near_z).abs());
                    if n[2] >= min_up && closer {
                        best = Some((z, n));
                    }
                }
            }
            above = here;
        }
        best
    }
}

/// MONKEY (voxel): a self-contained canonical chunk build ([`VoxelWorldData::build_job`]) — `Send`,
/// so the world layer runs it on the async compute pool while the frame goes on.
pub struct VoxelBuildJob {
    keys: Vec<ChunkKey>,
    seeds: HashMap<ColumnKey, Arc<ColumnSeed>>,
    outside: HashMap<ChunkKey, Box<[i8]>>,
    edits: Vec<VoxelEditOp>,
}

impl VoxelBuildJob {
    /// The chunks this job builds (the requested keys whose seed is loaded).
    pub fn keys(&self) -> &[ChunkKey] {
        &self.keys
    }

    /// How many edits the job replays (before the touch filter).
    pub fn edit_count(&self) -> usize {
        self.edits.len()
    }

    /// Seed + replay: the built chunks' owned densities.
    pub fn run(self) -> Vec<(ChunkKey, Box<[i8]>)> {
        let mut set: HashMap<ChunkKey, Box<[i8]>> = HashMap::new();
        for &k in &self.keys {
            if let Some(seed) = self.seeds.get(&[k[0], k[1]]) {
                set.insert(k, seed_chunk(k, seed));
            }
        }
        if set.is_empty() {
            return Vec::new();
        }
        let (chunks, seeds) = (&self.outside, &self.seeds);
        let outside = |i: [i32; 3]| sample_in(chunks, seeds, None, i);
        for e in &self.edits {
            if self.keys.iter().any(|&k| e.touches(k)) {
                apply_edit_set(e, &mut set, &outside);
            }
        }
        let mut out: Vec<(ChunkKey, Box<[i8]>)> = set.into_iter().collect();
        out.sort_by_key(|(k, _)| *k);
        out
    }
}

/// MONKEY (voxel): surface-nets mesh of a [`VoxelWorldData::window_i8`] snapshot — pure, so it
/// runs on any thread and gives the same mesh as [`VoxelWorldData::mesh`].
pub fn mesh_window(win: &[i8]) -> VoxelMesh {
    debug_assert_eq!(win.len(), ChunkShape::USIZE);
    let sdf: Vec<f32> = win.iter().map(|&v| v as f32 / DENSITY_SCALE).collect();
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

/// MONKEY (voxel): FNV-1a-64 over the chunks in key order (`kx, ky, kz` as i32 LE, then the 32³
/// density bytes) — the "density hash" two replays are compared by (tests, golden vectors).
pub fn chunks_hash(chunks: &HashMap<ChunkKey, Box<[i8]>>) -> u64 {
    let mut keys: Vec<&ChunkKey> = chunks.keys().collect();
    keys.sort();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    for k in keys {
        for a in k {
            a.to_le_bytes().into_iter().for_each(&mut eat);
        }
        chunks[k].iter().for_each(|&v| eat(v as u8));
    }
    h
}

/// MONKEY (voxel): the CANONICAL reference: every chunk any valid edit touches is seeded from
/// `height` and the whole edit list is applied to the whole set in order (CONTRACT §1 "rebuild
/// chunk"). What every partial / incremental / async path must reproduce.
pub fn full_replay(
    edits: &[VoxelEditOp],
    mut height: impl FnMut(f32, f32) -> Option<f32>,
) -> VoxelWorldData {
    let mut keys: BTreeSet<ChunkKey> = BTreeSet::new();
    for e in edits.iter().filter(|e| e.is_valid() && e.is_applied()) {
        keys.extend(e.touched_chunks());
    }
    let mut w = VoxelWorldData::default();
    for k in &keys {
        w.seeds
            .entry([k[0], k[1]])
            .or_insert_with(|| Arc::new(ColumnSeed::from_fn([k[0], k[1]], &mut height)));
    }
    let keys: Vec<ChunkKey> = keys.into_iter().collect();
    w.build(&keys, edits.iter().filter(|e| e.is_valid()));
    w
}

/// MONKEY (voxel): how far (yd) a voxel surface may sit off the seeded ground and still count as
/// untouched ground (the seed is ADT + [`SEED_LIFT`]; surface nets moves a vertex ≤ ~0.25 yd).
pub const EDITED_FROM: f32 = 0.3;
/// … and over how many yards beyond that the "edited" weight ramps to 1.
pub const EDITED_RAMP: f32 = 0.5;

/// MONKEY (voxel): per vertex of a [`mesh_window`] mesh (positions relative to the window origin,
/// whose WoW z is `origin_z`), how EDITED the surface is there: 0 = on the seeded (real) ground,
/// 1 = ≥ [`EDITED_FROM`] + [`EDITED_RAMP`] yd off it (a pit wall / floor, a mound, an overhang).
/// `seed_h` = [`VoxelWorldData::window_seed_heights`] (bilinear between lattice columns; a missing
/// column counts as edited; any other length = all 0). The shader keeps natural slopes on the
/// ground's own top mapping (a seeded hillside matches the real ground beside it) but sends
/// steep EDITED faces to the earth / rock wall layer.
///
/// MONKEY (voxel fix, owner 2026-10-23 "smooth fucked up the cliff textures"): two tests, the
/// weight is the SMALLER of them, so an unedited surface is 0 at ANY slope:
/// - DENSITY: `win` (the mesh window, [`VoxelWorldData::window_i8`]) against the seed density of
///   the same samples — a vertex whose cube's 8 corners all still hold the seed value is 0 (no
///   edit touched it), ramping to 1 from 0.25 to 0.75 voxel of change ([`EDITED_DENSITY_FROM`]);
/// - GEOMETRY: the vertex's distance off the seeded ground measured along the ground's NORMAL
///   (`|dz| / sqrt(1 + |∇h|²)`, not vertically: surface nets places a steep cliff's vertices up to a
///   few tenths of a yard off it vertically), ramping from [`EDITED_FROM`] over [`EDITED_RAMP`] —
///   a lightly smoothed slope that still follows the ground keeps the ground's own texturing.
///
/// `win` of any other length skips the density test (geometry only).
pub fn edited_weights(m: &VoxelMesh, win: &[i8], seed_h: &[f32], origin_z: f32) -> Vec<f32> {
    let s = SAMPLES as usize;
    if seed_h.len() != s * s {
        return vec![0.0; m.positions.len()];
    }
    let at = |x: usize, y: usize| seed_h[x.min(s - 1) + y.min(s - 1) * s];
    // Per window sample: how far (voxels) the density left the seed.
    let diff: Option<Vec<f32>> = (win.len() == s * s * s).then(|| {
        let iz0 = (origin_z / VOXEL_SIZE).round() as i32;
        let mut d = vec![0.0f32; s * s * s];
        for z in 0..s {
            for y in 0..s {
                for x in 0..s {
                    let i = x + s * (y + s * z);
                    let seed = seed_density(at(x, y), iz0 + z as i32);
                    d[i] = (win[i] as f32 - seed as f32).abs() / DENSITY_SCALE;
                }
            }
        }
        d
    });
    m.positions
        .iter()
        .map(|p| {
            let (fx, fy) = (p[0] / VOXEL_SIZE, p[1] / VOXEL_SIZE);
            let (ix, iy) = (fx.floor().max(0.0) as usize, fy.floor().max(0.0) as usize);
            let (tx, ty) = (
                (fx - ix as f32).clamp(0.0, 1.0),
                (fy - iy as f32).clamp(0.0, 1.0),
            );
            let density_w = diff.as_ref().map_or(1.0, |d| {
                let iz = ((p[2] / VOXEL_SIZE).floor().max(0.0) as usize).min(s - 2);
                let (cx, cy) = (ix.min(s - 2), iy.min(s - 2));
                let mut m = 0.0f32;
                for c in 0..8 {
                    let (ox, oy, oz) = (c & 1, (c >> 1) & 1, c >> 2);
                    m = m.max(d[(cx + ox) + s * ((cy + oy) + s * (iz + oz))]);
                }
                ((m - EDITED_DENSITY_FROM) / EDITED_DENSITY_RAMP).clamp(0.0, 1.0)
            });
            if density_w <= 0.0 {
                return 0.0;
            }
            let hs = [at(ix, iy), at(ix + 1, iy), at(ix, iy + 1), at(ix + 1, iy + 1)];
            if hs.iter().any(|h| !h.is_finite()) {
                return density_w;
            }
            let h = (hs[0] * (1.0 - tx) + hs[1] * tx) * (1.0 - ty)
                + (hs[2] * (1.0 - tx) + hs[3] * tx) * ty;
            let gx = ((hs[1] - hs[0]) * (1.0 - ty) + (hs[3] - hs[2]) * ty) / VOXEL_SIZE;
            let gy = ((hs[2] - hs[0]) * (1.0 - tx) + (hs[3] - hs[1]) * tx) / VOXEL_SIZE;
            let dz = (p[2] + origin_z - h).abs() / (1.0 + gx * gx + gy * gy).sqrt();
            ((dz - EDITED_FROM) / EDITED_RAMP).clamp(0.0, 1.0).min(density_w)
        })
        .collect()
}

/// MONKEY (voxel): the density test of [`edited_weights`]: a change under this many voxels is
/// untouched ground …
pub const EDITED_DENSITY_FROM: f32 = 0.25;
/// … ramping to fully edited over this many more.
pub const EDITED_DENSITY_RAMP: f32 = 0.5;

/// MONKEY (voxel fix): the per-vertex CARVE-BAND distance — how far (yd, WoW XY) a vertex lies
/// OUTSIDE the carve region of the union `carved` (16-yd columns, the outer boundary inset by
/// `inset`, CONTRACT §1), ≤ 0 inside. `origin` = the mesh window's WoW origin. The voxel shader
/// discards UNEDITED fragments past [`BAND_DISCARD`]: there the real ADT ground is drawn (it is
/// only clipped inside the region), so an untouched voxel surface never doubles it.
pub fn carve_band(m: &VoxelMesh, origin: [f32; 3], carved: &dyn Fn(i32, i32) -> bool, inset: f32) -> Vec<f32> {
    m.positions
        .iter()
        .map(|p| {
            let (x, y) = (p[0] + origin[0], p[1] + origin[1]);
            let (kx, ky) = ((x / CHUNK_YARDS).floor() as i32, (y / CHUNK_YARDS).floor() as i32);
            if !carved(kx, ky) {
                // A point in an uncarved column: its distance to the nearest carved column's
                // region (approximate: the 8 neighbours; far = 16).
                let mut best = CHUNK_YARDS;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if (dx, dy) == (0, 0) || !carved(kx + dx, ky + dy) {
                            continue;
                        }
                        best = best.min(column_region_dist(kx + dx, ky + dy, x, y, carved, inset));
                    }
                }
                return best;
            }
            column_region_dist(kx, ky, x, y, carved, inset)
        })
        .collect()
}

/// Signed distance (outside > 0) of `(x, y)` to carved column `(kx, ky)`'s region rectangle (its
/// footprint, inset on each side whose neighbour is not carved).
fn column_region_dist(kx: i32, ky: i32, x: f32, y: f32, carved: &dyn Fn(i32, i32) -> bool, inset: f32) -> f32 {
    let ins = |dx: i32, dy: i32| if carved(kx + dx, ky + dy) { 0.0 } else { inset };
    let x0 = kx as f32 * CHUNK_YARDS + ins(-1, 0);
    let x1 = (kx + 1) as f32 * CHUNK_YARDS - ins(1, 0);
    let y0 = ky as f32 * CHUNK_YARDS + ins(0, -1);
    let y1 = (ky + 1) as f32 * CHUNK_YARDS - ins(0, 1);
    let ex = (x0 - x).max(x - x1);
    let ey = (y0 - y).max(y - y1);
    if ex <= 0.0 && ey <= 0.0 {
        ex.max(ey)
    } else {
        (ex.max(0.0).powi(2) + ey.max(0.0).powi(2)).sqrt()
    }
}

/// MONKEY (voxel): unedited voxel fragments farther than this OUTSIDE the carve region (yd,
/// [`carve_band`]) are not drawn — the real ground is there. The 0.1-yd strip just past the
/// region keeps both (the voxel [`RENDER_DROP`] 0.02 yd under), so no crack can open between them.
pub const BAND_DISCARD: f32 = 0.1;

/// MONKEY (voxel): `n` deterministic synthetic edits (the bench / `WOW_VOXEL_SYNTH_EDITS` hook)
/// spread over a `span`-yard square centred on `centre`, sitting on `ground(x, y)` (`None` →
/// `centre_z`): ~60 % digs, ~30 % builds, ~10 % smooths, radius 1.5–4 yd, all confirmed with
/// seq 1..=n (a synced journal).
pub fn synth_edits(
    n: usize,
    map: u16,
    centre: [f32; 2],
    centre_z: f32,
    span: f32,
    mut ground: impl FnMut(f32, f32) -> Option<f32>,
) -> Vec<VoxelEditOp> {
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f32 / (1u64 << 53) as f32
    };
    (0..n)
        .map(|i| {
            let x = centre[0] + (rnd() - 0.5) * span;
            let y = centre[1] + (rnd() - 0.5) * span;
            let z = ground(x, y).unwrap_or(centre_z) + (rnd() - 0.5) * 2.0;
            let m = rnd();
            let mode = if m < 0.6 {
                0
            } else if m < 0.9 {
                1
            } else {
                2
            };
            VoxelEditOp {
                map,
                pos: [x, y, z],
                radius: 1.5 + rnd() * 2.5,
                mode,
                strength: if mode == 2 { 128 } else { 255 },
                nonce: i as u16,
                seq: Some(i as u32 + 1),
                local: false,
                stroke: 0,
                param: 0,
            }
        })
        .collect()
}

/// MONKEY (voxel): hang a SKIRT (a short outward ledge, then straight down to `depth` yd below the
/// lip) from every open boundary edge of a
/// [`mesh_window`] mesh that lies on a flagged horizontal side of the window (`sides` = −x, +x,
/// −y, +y). Where the voxel layer ENDS (the neighbour chunk is not active) its surface is a lip
/// [`SEED_LIFT`] above the real ground it overlaps; a grazing ray could slip into that slit, pass
/// the terrain's carved edge and see the sky under the voxel surface (the owner's "wounds"). The
/// skirt closes the slit; it continues the lip's own vertex normals, so it shades like the ground.
///
/// Returns, per appended vertex (in order), the lip vertex it hangs from.
pub fn add_skirts(m: &mut VoxelMesh, sides: [bool; 4], depth: f32) -> Vec<u32> {
    let mut lips: Vec<u32> = Vec::new();
    if !sides.iter().any(|&s| s) || m.indices.len() < 3 {
        return lips;
    }
    // The outermost cube layer per side, in window yards: cube 0 spans [0, 1] samples, cube 32
    // spans [32, 33].
    let lo_edge = VOXEL_SIZE;
    let hi_edge = (SAMPLES - 2) as f32 * VOXEL_SIZE;
    let on_side = |p: [f32; 3]| -> [bool; 4] {
        [
            p[0] <= lo_edge,
            p[0] >= hi_edge,
            p[1] <= lo_edge,
            p[1] >= hi_edge,
        ]
    };
    let mut uses: HashMap<(u32, u32), (u32, u32, u32)> = HashMap::new();
    for t in m.indices.chunks_exact(3) {
        for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            let key = (a.min(b), a.max(b));
            let e = uses.entry(key).or_insert((0, a, b));
            e.0 += 1;
        }
    }
    let mut open: Vec<(u32, u32)> = uses
        .into_values()
        .filter(|&(n, a, b)| {
            if n != 1 {
                return false;
            }
            let (sa, sb) = (
                on_side(m.positions[a as usize]),
                on_side(m.positions[b as usize]),
            );
            (0..4).any(|i| sides[i] && sa[i] && sb[i])
        })
        .map(|(_, a, b)| (a, b))
        .collect();
    open.sort_unstable();
    // Per lip vertex: a gentle outward-and-down ledge (so the visible strip above the real ground
    // shades like ground, not a vertically smeared band) then a vertical drop to `depth` (so a
    // steep downhill outside still cannot open a slit). Outward = the flagged sides the vertex
    // lies on, so the pieces of neighbouring edges share their offset vertices (watertight).
    let dirs = [[-1.0f32, 0.0], [1.0, 0.0], [0.0, -1.0], [0.0, 1.0]];
    let mut lip: HashMap<u32, (u32, u32)> = HashMap::new();
    let mut ring = |m: &mut VoxelMesh, v: u32| -> (u32, u32) {
        *lip.entry(v).or_insert_with(|| {
            lips.push(v);
            lips.push(v);
            let p = m.positions[v as usize];
            let s = on_side(p);
            let mut d = [0.0f32, 0.0];
            for i in 0..4 {
                if sides[i] && s[i] {
                    d = [d[0] + dirs[i][0], d[1] + dirs[i][1]];
                }
            }
            let l = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1.0e-6);
            let (ox, oy) = (d[0] / l * SKIRT_OUT, d[1] / l * SKIRT_OUT);
            let n = m.normals[v as usize];
            let base = m.positions.len() as u32;
            m.positions
                .push([p[0] + ox, p[1] + oy, p[2] - SKIRT_LEDGE_DROP]);
            m.positions.push([p[0] + ox, p[1] + oy, p[2] - depth]);
            m.normals.push(n);
            m.normals.push(n);
            (base, base + 1)
        })
    };
    for (a, b) in open {
        let (am, ab) = ring(m, a);
        let (bm, bb) = ring(m, b);
        // a, b → the ledge → the drop, in the surface triangle's own edge order (cull is off, the
        // order only keeps the skirt consistent with its surface).
        m.indices
            .extend_from_slice(&[b, a, am, b, am, bm, bm, am, ab, bm, ab, bb]);
    }
    lips
}

/// MONKEY (voxel): how far a skirt's ledge reaches OUTWARD past the lip (yd) …
pub const SKIRT_OUT: f32 = 0.6;
/// … and how far it drops there: with the lip [`SEED_LIFT`] above the ground, the ledge meets the
/// ground ~0.2 yd out on level ground (a short 37° bevel), then the skirt falls straight down.
pub const SKIRT_LEDGE_DROP: f32 = 0.45;

/// MONKEY (voxel): the XY points (a `step`-yard grid over `cols`) where `inside(x, y)` holds — the
/// real ground is carved there — but NO triangle of the meshes of `keys` covers the point in XY
/// projection: a sky crack. `meshes` = `(key, mesh)` pairs (positions relative to
/// [`window_origin`]). Used by the coverage audit (`WOW_VOXEL_COVERAGE`) and the tests.
pub fn xy_coverage_gaps(
    meshes: &[(ChunkKey, VoxelMesh)],
    cols: &[ColumnKey],
    step: f32,
    inside: impl Fn(f32, f32) -> bool,
) -> Vec<[f32; 2]> {
    const BIN: f32 = 0.5;
    let bin = |v: f32| (v / BIN).floor() as i32;
    let mut tris: Vec<[[f32; 2]; 3]> = Vec::new();
    let mut bins: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
    for (k, m) in meshes {
        let o = window_origin(*k);
        for t in m.indices.chunks_exact(3) {
            let p = |i: u32| {
                let q = m.positions[i as usize];
                [q[0] + o[0], q[1] + o[1]]
            };
            let tri = [p(t[0]), p(t[1]), p(t[2])];
            let lo = [
                tri[0][0].min(tri[1][0]).min(tri[2][0]),
                tri[0][1].min(tri[1][1]).min(tri[2][1]),
            ];
            let hi = [
                tri[0][0].max(tri[1][0]).max(tri[2][0]),
                tri[0][1].max(tri[1][1]).max(tri[2][1]),
            ];
            let id = tris.len() as u32;
            tris.push(tri);
            for bx in bin(lo[0])..=bin(hi[0]) {
                for by in bin(lo[1])..=bin(hi[1]) {
                    bins.entry((bx, by)).or_default().push(id);
                }
            }
        }
    }
    let covers = |t: &[[f32; 2]; 3], x: f32, y: f32| {
        let d = |a: [f32; 2], b: [f32; 2]| (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0]);
        let (d0, d1, d2) = (d(t[0], t[1]), d(t[1], t[2]), d(t[2], t[0]));
        const E: f32 = 1.0e-4;
        (d0 >= -E && d1 >= -E && d2 >= -E) || (d0 <= E && d1 <= E && d2 <= E)
    };
    let n = (CHUNK_YARDS / step).round() as i32;
    let mut gaps = Vec::new();
    for c in cols {
        for j in 0..n {
            for i in 0..n {
                let x = c[0] as f32 * CHUNK_YARDS + (i as f32 + 0.5) * step;
                let y = c[1] as f32 * CHUNK_YARDS + (j as f32 + 0.5) * step;
                if !inside(x, y) {
                    continue;
                }
                let hit = bins
                    .get(&(bin(x), bin(y)))
                    .is_some_and(|ids| ids.iter().any(|&id| covers(&tris[id as usize], x, y)));
                if !hit {
                    gaps.push([x, y]);
                }
            }
        }
    }
    gaps
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

/// MONKEY (voxel): the canonical-order key of a journal entry — confirmed `(0, seq)`, a pending
/// one `(1, arrival stamp)`. Sorting by it IS the canonical order (confirmed by seq, then pending
/// in arrival order), and it is stable while other entries are inserted / removed around it.
pub type EditOrder = (u8, u64);

/// One map's edits in canonical order (confirmed by `seq` ascending, then unconfirmed in arrival
/// order) + per-region sync state.
///
/// MONKEY (voxel): + a per-chunk INDEX: every valid edit is filed under each chunk its bbox
/// touches ([`VoxelEditOp::touched_chunks`]), keyed by its [`EditOrder`], so building one chunk
/// reads only its own edits ([`Self::edits_for`]) instead of scanning the whole journal. Kept in
/// step by every mutation (insert / confirm / remove_pending / reset_region / decode).
#[derive(Clone, Debug, Default)]
pub struct MapJournal {
    edits: Vec<VoxelEditOp>,
    /// MONKEY (voxel): arrival stamp of each entry (parallel to `edits`; the pending entries' key).
    arrival: Vec<u64>,
    next_arrival: u64,
    /// How many leading entries of `edits` are confirmed (`seq: Some`).
    confirmed: usize,
    pub regions: BTreeMap<(i16, i16), RegionSync>,
    /// MONKEY (voxel): chunk → the edits touching it, in canonical order.
    index: HashMap<ChunkKey, BTreeMap<EditOrder, VoxelEditOp>>,
}

/// The journal's content (edits, order, sync points); the index + stamps are derived state.
impl PartialEq for MapJournal {
    fn eq(&self, o: &Self) -> bool {
        self.edits == o.edits && self.confirmed == o.confirmed && self.regions == o.regions
    }
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

    /// MONKEY (voxel): the canonical key of entry `i`.
    fn order_key(&self, i: usize) -> EditOrder {
        if i < self.confirmed {
            (0, self.edits[i].seq.unwrap_or(0) as u64)
        } else {
            (1, self.arrival[i])
        }
    }

    /// MONKEY (voxel): file `e` under every chunk it touches (invalid edits — e.g. a corrupt file's
    /// absurd radius — and the not-yet-applied modes 4 / 5 change nothing, so are never indexed).
    fn index_add(&mut self, key: EditOrder, e: &VoxelEditOp) {
        if !e.is_valid() || !e.is_applied() {
            return;
        }
        for k in e.touched_chunks() {
            self.index.entry(k).or_default().insert(key, *e);
        }
    }

    fn index_remove(&mut self, key: EditOrder, e: &VoxelEditOp) {
        if !e.is_valid() || !e.is_applied() {
            return;
        }
        for k in e.touched_chunks() {
            if let Some(m) = self.index.get_mut(&k) {
                m.remove(&key);
                if m.is_empty() {
                    self.index.remove(&k);
                }
            }
        }
    }

    /// MONKEY (voxel): every edit touching any of `keys`, in canonical order, each once — exactly
    /// the edits [`VoxelWorldData::build`] would pick out of the full journal for that set.
    pub fn edits_for(&self, keys: &[ChunkKey]) -> Vec<VoxelEditOp> {
        if let [k] = keys {
            return self
                .index
                .get(k)
                .map_or_else(Vec::new, |m| m.values().copied().collect());
        }
        let mut m: BTreeMap<EditOrder, VoxelEditOp> = BTreeMap::new();
        for k in keys {
            if let Some(ix) = self.index.get(k) {
                m.extend(ix.iter().map(|(o, e)| (*o, *e)));
            }
        }
        m.into_values().collect()
    }

    /// MONKEY (voxel): the CANONICAL REBUILD SET of `keys` — the server's P1 finding
    /// (`voxel-core/NAV-P1.md` §8): a smooth reads every chunk of its bbox at the moment it is
    /// applied, so rebuilding a chunk alone (its smooth partners read at their CURRENT density)
    /// differs from the full replay. The set is grown to the transitive closure of chunks linked
    /// by neighbour-reading edits ([`VoxelEditOp::reads_neighbours`]): every such edit touching a
    /// member brings all chunks it touches. Rebuilding the closure in lockstep equals the full
    /// replay for every member. Sorted; includes `keys`.
    pub fn read_closure(&self, keys: &[ChunkKey]) -> BTreeSet<ChunkKey> {
        let mut set: BTreeSet<ChunkKey> = keys.iter().copied().collect();
        let mut queue: Vec<ChunkKey> = set.iter().copied().collect();
        while let Some(k) = queue.pop() {
            let Some(ix) = self.index.get(&k) else {
                continue;
            };
            for e in ix.values().filter(|e| e.reads_neighbours()) {
                for t in e.touched_chunks() {
                    if set.insert(t) {
                        queue.push(t);
                    }
                }
            }
        }
        set
    }

    /// MONKEY (voxel): the chunks any (valid) edit touches — what entering the map activates.
    pub fn indexed_chunks(&self) -> impl Iterator<Item = ChunkKey> + '_ {
        self.index.keys().copied()
    }

    /// MONKEY (voxel): how many edits touch chunk `k`.
    pub fn chunk_edit_count(&self, k: ChunkKey) -> usize {
        self.index.get(&k).map_or(0, |m| m.len())
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
                let a = self.next_arrival;
                self.next_arrival += 1;
                self.edits.push(e);
                self.arrival.push(a);
                self.index_add((1, a), &e);
                JournalInsert::Appended
            }
            Some(seq) => {
                let conf = &self.edits[..self.confirmed];
                match conf.binary_search_by_key(&seq, |x| x.seq.unwrap_or(0)) {
                    Ok(_) => JournalInsert::Duplicate,
                    Err(pos) => {
                        // MONKEY (voxel): the region's sync point is NOT moved here: only a
                        // completed SYNC (`D`) or a live edit in an already-synced region moves it
                        // (the net layer decides), else a live edit heard from 300 yd would skip
                        // the older edits of a region never synced.
                        self.edits.insert(pos, e);
                        self.arrival.insert(pos, 0);
                        self.confirmed += 1;
                        self.index_add((0, seq as u64), &e);
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

    /// MONKEY (voxel): remove entry `i` (and its index entries).
    fn remove_at(&mut self, i: usize) -> VoxelEditOp {
        let key = self.order_key(i);
        let e = self.edits.remove(i);
        self.arrival.remove(i);
        if i < self.confirmed {
            self.confirmed -= 1;
        }
        self.index_remove(key, &e);
        e
    }

    /// The server confirmed our prediction `nonce` as `seq`: move it into the confirmed prefix.
    /// Returns the edit if found (`None` = no such pending nonce). The density is unchanged
    /// unless the move reorders it past other edits — the caller rebuilds then (`OutOfOrder`).
    pub fn confirm(&mut self, nonce: u16, seq: u32) -> Option<(VoxelEditOp, JournalInsert)> {
        let i = self.edits[self.confirmed..]
            .iter()
            .position(|e| e.local && e.nonce == nonce)?
            + self.confirmed;
        let mut e = self.remove_at(i);
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

    /// MONKEY (voxel): drop our pending prediction `nonce` (the server refused it with `R`, or it
    /// was never received). Returns it (the caller rebuilds its chunks).
    pub fn remove_pending(&mut self, nonce: u16) -> Option<VoxelEditOp> {
        let i = self.edits[self.confirmed..]
            .iter()
            .position(|e| e.local && e.nonce == nonce)?
            + self.confirmed;
        Some(self.remove_at(i))
    }

    /// REGION-RESET: drop every edit (confirmed or not) whose centre lies in `(rx, ry)`, adopt the
    /// new epoch, `last_seq` = 0. Returns the dropped edits (the caller rebuilds their chunks).
    pub fn reset_region(&mut self, rx: i16, ry: i16, epoch: u16) -> Vec<VoxelEditOp> {
        let mut dropped = Vec::new();
        let mut kept = Vec::with_capacity(self.edits.len());
        let mut kept_arrival = Vec::with_capacity(self.edits.len());
        let mut confirmed = 0;
        let keys: Vec<EditOrder> = (0..self.edits.len()).map(|i| self.order_key(i)).collect();
        let edits = std::mem::take(&mut self.edits);
        let arrival = std::mem::take(&mut self.arrival);
        for (i, (e, a)) in edits.into_iter().zip(arrival).enumerate() {
            if e.region() == (rx, ry) {
                self.index_remove(keys[i], &e);
                dropped.push(e);
            } else {
                if i < self.confirmed {
                    confirmed += 1;
                }
                kept.push(e);
                kept_arrival.push(a);
            }
        }
        self.edits = kept;
        self.arrival = kept_arrival;
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

// The whole local journal (every map).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoxelJournal {
    pub maps: BTreeMap<u16, MapJournal>,
}

/// Why a journal file could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalError {
    /// Not a v2/v3 journal (wrong magic) — e.g. a v1 file.
    BadMagic,
    /// A journal of another format version.
    Version(u16),
    /// Cut short / corrupt.
    Truncated,
}

const JOURNAL_MAGIC: &[u8; 4] = b"MVXW";
/// MONKEY (voxel): v3 adds `stroke u16, param u16` per edit; v2 files still load (both 0).
const JOURNAL_VERSION: u16 = 3;

impl VoxelJournal {
    /// Serialise (little-endian): `"MVXW", u16 version (= 3), u16 maps`, then per map `u16 map,
    /// u32 regions × (i16 rx, i16 ry, u16 epoch, u32 last_seq), u32 edits × (f32 x, f32 y, f32 z,
    /// f32 radius, u8 mode, u8 strength, u16 nonce, u8 flags (bit0 has_seq, bit1 local), u32 seq,
    /// u16 stroke, u16 param)`. A v2 file is the same without the last two fields.
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
                b.extend_from_slice(&e.stroke.to_le_bytes());
                b.extend_from_slice(&e.param.to_le_bytes());
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
        if v != 2 && v != JOURNAL_VERSION {
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
                let (stroke, param) = if v >= 3 { (r.u16()?, r.u16()?) } else { (0, 0) };
                edits.push(VoxelEditOp {
                    map,
                    pos,
                    radius,
                    mode,
                    strength,
                    nonce,
                    seq: has.then_some(seq),
                    local: flags & 2 != 0,
                    stroke,
                    param,
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
            ..Default::default()
        }
    }

    /// MONKEY (voxel): a tiny deterministic PRNG for the randomized tests.
    struct Lcg(u64);
    impl Lcg {
        fn f(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 40) as f32 / (1u64 << 24) as f32
        }
        fn n(&mut self, k: u32) -> u32 {
            (self.f() * k as f32) as u32 % k.max(1)
        }
    }

    /// The brute-force answer the index must give: every valid journal edit touching any key, in
    /// canonical (journal) order.
    fn brute(j: &MapJournal, keys: &[ChunkKey]) -> Vec<VoxelEditOp> {
        j.edits()
            .iter()
            .filter(|e| e.is_valid() && e.is_applied() && keys.iter().any(|&k| e.touches(k)))
            .copied()
            .collect()
    }

    fn check_index(j: &MapJournal, probe: &[ChunkKey]) {
        for k in probe {
            assert_eq!(j.edits_for(&[*k]), brute(j, &[*k]), "chunk {k:?}");
            assert_eq!(j.chunk_edit_count(*k), brute(j, &[*k]).len());
        }
        for w in probe.windows(3) {
            assert_eq!(j.edits_for(w), brute(j, w), "set {w:?}");
        }
        // Every indexed chunk is touched by something, and nothing touching is missing.
        let all: HashSet<ChunkKey> = j
            .edits()
            .iter()
            .filter(|e| e.is_valid() && e.is_applied())
            .flat_map(|e| e.touched_chunks())
            .collect();
        let idx: HashSet<ChunkKey> = j.indexed_chunks().collect();
        assert_eq!(all, idx);
    }

    #[test]
    fn journal_index_matches_brute_force_replay() {
        let mut rng = Lcg(42);
        let mut j = MapJournal::default();
        let mut nonce: u16 = 0;
        let probe: Vec<ChunkKey> = (0..40)
            .map(|i| [(i % 5) - 1, (i / 5 % 4) - 1, (i / 20)])
            .collect();
        for step in 0..600 {
            match rng.n(10) {
                // A local prediction (pending, arrival order).
                0..=3 => {
                    let mut e = edit(
                        [rng.f() * 64.0 - 16.0, rng.f() * 48.0 - 16.0, rng.f() * 30.0],
                        0.5 + rng.f() * 6.0,
                        rng.n(3) as u8,
                    );
                    nonce = nonce.wrapping_add(1);
                    e.nonce = nonce;
                    e.local = rng.n(4) != 0;
                    j.insert(e);
                }
                // A server edit with a random seq (in or out of order, maybe a duplicate).
                4..=6 => {
                    let mut e = edit(
                        [rng.f() * 64.0 - 16.0, rng.f() * 48.0 - 16.0, rng.f() * 30.0],
                        0.5 + rng.f() * 6.0,
                        rng.n(3) as u8,
                    );
                    e.local = false;
                    e.seq = Some(rng.n(400));
                    j.insert(e);
                }
                // Confirm a random pending local prediction (maybe with a taken seq).
                7 => {
                    let p = j.pending_local();
                    if !p.is_empty() {
                        let n = p[rng.n(p.len() as u32) as usize].nonce;
                        j.confirm(n, rng.n(400));
                    }
                }
                // Drop one.
                8 => {
                    let p = j.pending_local();
                    if !p.is_empty() {
                        let n = p[rng.n(p.len() as u32) as usize].nonce;
                        assert!(j.remove_pending(n).is_some());
                    }
                }
                // Reset a region (64-yd regions over the -16..48 area).
                _ => {
                    let r = (rng.n(2) as i16 - 1, rng.n(2) as i16 - 1);
                    j.reset_region(r.0, r.1, step as u16);
                }
            }
            check_index(&j, &probe);
        }
        assert!(j.edits().len() > 50, "the walk kept a real journal ({})", j.edits().len());
        // The index survives the file round trip.
        let mut file = VoxelJournal::default();
        file.maps.insert(0, j.clone());
        let back = VoxelJournal::decode(&file.encode()).expect("decode");
        check_index(&back.maps[&0], &probe);
        // An invalid edit (corrupt radius) is journalled but never indexed.
        let mut bad = edit([1.0, 1.0, 1.0], 1.0, 0);
        bad.radius = 1e9;
        let mut j2 = MapJournal::default();
        j2.insert(bad);
        assert_eq!(j2.indexed_chunks().count(), 0);
    }

    /// The window snapshot's fast path (row copies + apron) equals a per-sample read through the
    /// owners, and a mesh / build run on ANOTHER thread from the snapshots equals the sync one.
    #[test]
    fn async_build_and_mesh_equal_sync() {
        let keys: Vec<ChunkKey> = (0..8).map(|i| [i & 1, (i >> 1) & 1, (i >> 2) & 1]).collect();
        let edits = [
            edit([16.0, 16.0, 16.5], 6.0, 0),
            edit([16.0, 16.0, 24.0], 3.0, 1),
            edit([16.3, 15.6, 17.0], 4.0, 2),
            edit([31.5, 8.0, 19.0], 3.0, 2), // smooth reading past the set (x = 32 → kx 2)
        ];
        let mut w = world_with(&keys, &edits);
        // Every column's seed (so a per-sample read through the owners is the window's rule).
        for x in -1..=3 {
            for y in -1..=3 {
                w.seeds
                    .entry([x, y])
                    .or_insert_with(|| Arc::new(ColumnSeed::from_fn([x, y], ground)));
            }
        }
        // An active neighbour OUTSIDE the next build set: the smooth's read ring reaches it.
        w.build(&[[2, 0, 1]], edits.iter());
        for &k in &keys {
            let win = w.window_i8(k);
            let base = [k[0] * 32 - 1, k[1] * 32 - 1, k[2] * 32 - 1];
            for lz in 0..34u32 {
                for ly in 0..34u32 {
                    for lx in 0..34u32 {
                        let li = ChunkShape::linearize([lx, ly, lz]) as usize;
                        let i = [base[0] + lx as i32, base[1] + ly as i32, base[2] + lz as i32];
                        assert_eq!(win[li], w.sample(i), "chunk {k:?} sample {i:?}");
                    }
                }
            }
            let sync = w.mesh(k);
            let thread = std::thread::spawn(move || mesh_window(&win)).join().unwrap();
            assert_eq!(sync.positions, thread.positions);
            assert_eq!(sync.indices, thread.indices);
        }
        // Rebuild the 8 off-thread from a snapshot == the sync rebuild.
        let job = w.build_job(&keys, edits.to_vec());
        assert_eq!(job.keys().len(), 8);
        let off = std::thread::spawn(move || job.run()).join().unwrap();
        let mut sync = world_with(&keys, &[]);
        sync.seeds = w.seeds.clone();
        sync.chunks.insert([2, 0, 1], w.chunks[&[2, 0, 1]].clone());
        sync.build(&keys, edits.iter());
        for (k, d) in &off {
            assert_eq!(&sync.chunks[k], d, "chunk {k:?}");
        }
    }

    /// MONKEY (voxel): the replay bench (`cargo test -p benilla-voxel --release -- --ignored
    /// --nocapture bench_`): n synthetic edits over 100×100 yd; per-chunk build time through the
    /// old full-journal scan vs the index, and the mesh time.
    #[test]
    #[ignore]
    fn bench_chunk_build_replay() {
        for n in [50usize, 500, 2000] {
            let edits = synth_edits(n, 0, [0.0, 0.0], 20.0, 100.0, ground);
            let mut j = MapJournal::default();
            for e in &edits {
                j.insert(*e);
            }
            let mut keys: Vec<ChunkKey> = j.indexed_chunks().collect();
            keys.sort();
            let mut w = VoxelWorldData::default();
            for k in &keys {
                w.seeds
                    .entry([k[0], k[1]])
                    .or_insert_with(|| Arc::new(ColumnSeed::from_fn([k[0], k[1]], ground)));
            }
            let t = std::time::Instant::now();
            let mut worst_scan = 0f32;
            for k in &keys {
                let t1 = std::time::Instant::now();
                w.build(&[*k], j.edits().iter());
                worst_scan = worst_scan.max(t1.elapsed().as_secs_f32() * 1000.0);
            }
            let scan_ms = t.elapsed().as_secs_f32() * 1000.0;
            let t = std::time::Instant::now();
            let mut worst_idx = 0f32;
            for k in &keys {
                let t1 = std::time::Instant::now();
                let es = j.edits_for(&[*k]);
                w.build(&[*k], es.iter());
                worst_idx = worst_idx.max(t1.elapsed().as_secs_f32() * 1000.0);
            }
            let idx_ms = t.elapsed().as_secs_f32() * 1000.0;
            let t = std::time::Instant::now();
            let (mut worst_mesh, mut worst_win) = (0f32, 0f32);
            for k in &keys {
                let t1 = std::time::Instant::now();
                let win = w.window_i8(*k);
                worst_win = worst_win.max(t1.elapsed().as_secs_f32() * 1000.0);
                let _ = mesh_window(&win);
                worst_mesh = worst_mesh.max(t1.elapsed().as_secs_f32() * 1000.0);
            }
            let mesh_ms = t.elapsed().as_secs_f32() * 1000.0;
            println!(
                "bench n={n}: {} chunks; build full-scan {scan_ms:.1} ms (worst chunk {worst_scan:.2}) \
                 | indexed {idx_ms:.1} ms (worst {worst_idx:.2}) | mesh {mesh_ms:.1} ms (worst \
                 {worst_mesh:.2}, window snapshot worst {worst_win:.3})",
                keys.len()
            );
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
            ..Default::default()
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
        // A guild-relay edit from someone else (no seq, not local) is neither an uplink candidate
        // nor confirmable by our nonce.
        let mut relayed = edit([12.0, 20.0, 30.0], 1.0, 1);
        relayed.local = false;
        relayed.nonce = 8;
        m.insert(relayed);
        assert!(m.pending_local().is_empty());
        assert!(m.confirm(8, 10).is_none());
        m.regions.insert((1, -2), RegionSync { epoch: 3, last_seq: 9 });
        let mut other = edit([1.0, 2.0, 3.0], 1.0, 2);
        other.map = 1;
        j.maps.entry(1).or_default().insert(other);

        let bytes = j.encode();
        let back = VoxelJournal::decode(&bytes).expect("decode");
        assert_eq!(back, j);
        let flags: Vec<bool> = back.maps[&0].edits().iter().map(|e| e.local).collect();
        assert_eq!(flags, vec![true, true, true, false], "the local flag survives the file");
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
    fn region_sync_point_is_net_managed_and_persists() {
        let mut j = VoxelJournal::default();
        let m = j.maps.entry(0).or_default();
        let mut s = edit([10.0, 10.0, 0.0], 1.0, 0);
        s.seq = Some(40);
        m.insert(s);
        assert!(m.regions.is_empty(), "a live / synced E does not move the sync point");
        m.regions.insert((0, 0), RegionSync { epoch: 2, last_seq: 40 });
        m.regions.insert((-141, -2), RegionSync { epoch: 7, last_seq: 123_456 });
        let mut p = edit([20.0, 10.0, 0.0], 1.0, 1);
        p.local = true;
        p.nonce = 5;
        m.insert(p);
        assert_eq!(m.remove_pending(6), None);
        assert_eq!(m.remove_pending(5).map(|e| e.nonce), Some(5));
        assert!(m.pending_local().is_empty());
        let back = VoxelJournal::decode(&j.encode()).expect("decode");
        assert_eq!(back, j);
        assert_eq!(
            back.maps[&0].regions[&(-141, -2)],
            RegionSync { epoch: 7, last_seq: 123_456 }
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

    /// MONKEY (voxel): the per-vertex "edited" weight is 0 on the untouched seeded ground and 1
    /// down in a pit.
    #[test]
    fn edited_weights_mark_the_pit_not_the_ground() {
        let k = [0, 0, 1];
        let w = world_with(&[k], &[edit([8.0, 8.0, 20.0], 4.0, MODE_DIG)]);
        let m = w.mesh(k);
        let o = window_origin(k);
        let e = edited_weights(&m, &w.window_i8(k), &w.window_seed_heights(k), o[2]);
        assert_eq!(e.len(), m.positions.len());
        let (mut ground, mut pit) = (0, 0);
        for (p, &v) in m.positions.iter().zip(&e) {
            let (x, y, z) = (p[0] + o[0], p[1] + o[1], p[2] + o[2]);
            let g = ground_h(x, y) + SEED_LIFT;
            if ((x - 8.0).powi(2) + (y - 8.0).powi(2)).sqrt() > 6.0 {
                assert!(v < 0.05, "untouched ground at ({x},{y}) weight {v}");
                ground += 1;
            } else if z < g - 1.5 {
                assert!(v > 0.99, "pit at ({x},{y},{z}) weight {v}");
                pit += 1;
            }
        }
        assert!(ground > 50 && pit > 20, "{ground} ground / {pit} pit vertices");
        assert!(edited_weights(&m, &[], &[], 0.0).iter().all(|&v| v == 0.0));
    }

    fn ground_h(x: f32, y: f32) -> f32 {
        ground(x, y).unwrap()
    }

    /// MONKEY (voxel): v3 journal round trip (stroke + param survive), and a v2 file still loads.
    #[test]
    fn journal_v3_round_trips_and_v2_files_still_load() {
        let mut j = VoxelJournal::default();
        let m = j.maps.entry(0).or_default();
        let mut f = edit([10.0, 20.0, 30.0], 2.0, MODE_FLATTEN);
        f.stroke = 812;
        f.param = (-37i16) as u16;
        f.seq = Some(4);
        m.insert(f);
        let mut p = edit([11.0, 20.0, 30.0], 2.0, MODE_SMOOTH);
        p.stroke = 813;
        p.nonce = 5;
        m.insert(p);
        m.regions.insert((0, 0), RegionSync { epoch: 1, last_seq: 4 });
        let bytes = j.encode();
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 3, "written as v3");
        let back = VoxelJournal::decode(&bytes).expect("decode v3");
        assert_eq!(back, j);
        assert_eq!(back.maps[&0].edits()[0].param as i16, -37);
        assert_eq!(back.maps[&0].edits()[1].stroke, 813);
        // The same journal as a v2 file (no stroke / param): loads with both 0.
        let mut v2 = Vec::new();
        v2.extend_from_slice(b"MVXW");
        v2.extend_from_slice(&2u16.to_le_bytes());
        v2.extend_from_slice(&1u16.to_le_bytes());
        v2.extend_from_slice(&0u16.to_le_bytes());
        v2.extend_from_slice(&0u32.to_le_bytes());
        v2.extend_from_slice(&2u32.to_le_bytes());
        for e in j.maps[&0].edits() {
            for v in [e.pos[0], e.pos[1], e.pos[2], e.radius] {
                v2.extend_from_slice(&v.to_le_bytes());
            }
            v2.push(e.mode);
            v2.push(e.strength);
            v2.extend_from_slice(&e.nonce.to_le_bytes());
            v2.push(e.seq.is_some() as u8 | (e.local as u8) << 1);
            v2.extend_from_slice(&e.seq.unwrap_or(0).to_le_bytes());
        }
        let old = VoxelJournal::decode(&v2).expect("a v2 file loads");
        let es = old.maps[&0].edits();
        assert_eq!(es.len(), 2);
        assert!(es.iter().all(|e| e.stroke == 0 && e.param == 0));
        assert_eq!(es[0].pos, f.pos);
        assert_eq!(es[0].seq, Some(4));
        let mut v4 = bytes.clone();
        v4[4] = 4;
        assert_eq!(VoxelJournal::decode(&v4), Err(JournalError::Version(4)));
    }

    /// MONKEY (voxel): flatten pulls the ground onto its plane (offset by `param` cm), blends by
    /// strength, and is a no-op outside its sphere; paint / roughen change nothing.
    #[test]
    fn flatten_levels_to_the_plane_and_modes_4_5_are_noops() {
        let bumpy = |x: f32, y: f32| Some(20.0 + (x * 0.4).sin() * 1.2 + (y * 0.3).cos() * 0.9);
        let keys: Vec<ChunkKey> = vec![[0, 0, 1], [1, 0, 1], [0, 1, 1], [1, 1, 1], [0, 0, 0], [1, 1, 0]];
        let world = |es: &[VoxelEditOp]| {
            let mut w = VoxelWorldData::default();
            for k in &keys {
                w.seeds
                    .entry([k[0], k[1]])
                    .or_insert_with(|| Arc::new(ColumnSeed::from_fn([k[0], k[1]], bumpy)));
            }
            w.build(&keys, es.iter());
            w
        };
        let mut fl = edit([16.0, 16.0, 20.0], 6.0, MODE_FLATTEN);
        fl.param = 50; // plane 20.5
        let w = world(&[fl]);
        for (x, y) in [(16.0, 16.0), (14.0, 17.5), (18.2, 15.1)] {
            let (z, n) = w.top_near(x, y, 20.0, 8.0, 0.5).expect("a top");
            assert!((z - 20.5).abs() < 0.05, "({x},{y}) levelled to the plane: {z}");
            assert!(n[2] > 0.99, "flat: {n:?}");
        }
        // Outside the sphere the ground is untouched (seed = ADT + lift).
        let (z, _) = w.top_near(30.0, 30.0, 20.0, 8.0, 0.0).unwrap();
        assert!((z - (bumpy(30.0, 30.0).unwrap() + SEED_LIFT)).abs() < 0.05);
        // Half strength lands between the ground and the plane at the centre.
        let mut half = fl;
        half.strength = 128;
        half.param = (-200i16) as u16; // plane 18.0
        let g = bumpy(16.0, 16.0).unwrap() + SEED_LIFT;
        let (z, _) = world(&[half]).top_near(16.0, 16.0, 20.0, 8.0, 0.0).unwrap();
        assert!(z < g - 0.3 && z > 18.0 + 0.3, "half-way: ground {g} plane 18 got {z}");
        // Deterministic: the same op twice = the same bytes; paint / roughen = the seed.
        assert_eq!(world(&[fl]).chunks, w.chunks);
        let base = world(&[]);
        for m in [MODE_PAINT, MODE_ROUGHEN] {
            let mut e = fl;
            e.mode = m;
            assert!(e.is_valid() && !e.is_applied());
            assert_eq!(world(&[e]).chunks, base.chunks, "mode {m} is a no-op");
        }
    }

    fn rand_edits(rng: &mut Lcg, n: usize, ground: impl Fn(f32, f32) -> Option<f32>) -> Vec<VoxelEditOp> {
        (0..n)
            .map(|i| {
                let x = rng.f() * 56.0 - 12.0;
                let y = rng.f() * 56.0 - 12.0;
                let z = ground(x, y).unwrap_or(20.0) + (rng.f() - 0.5) * 3.0;
                let mode = match rng.n(10) {
                    0..=2 => MODE_DIG,
                    3..=4 => MODE_BUILD,
                    5..=7 => MODE_SMOOTH,
                    8 => MODE_FLATTEN,
                    _ => MODE_PAINT,
                };
                VoxelEditOp {
                    map: 0,
                    pos: [x, y, z],
                    radius: 0.5 + rng.f() * 7.5,
                    mode,
                    strength: 40 + rng.n(216) as u8,
                    nonce: i as u16,
                    seq: Some(i as u32 + 1),
                    local: false,
                    stroke: 1 + (i / 4) as u16,
                    param: ((rng.f() - 0.5) * 300.0) as i16 as u16,
                }
            })
            .collect()
    }

    /// MONKEY (voxel): the CANONICAL rebuild rule (server P1 finding): random edit lists with
    /// smooth + flatten — a partial rebuild of the read-closure, an incremental in-order apply,
    /// out-of-order arrival with closure rebuilds, and the off-thread job all equal the full
    /// replay (density hash equal). A partial rebuild WITHOUT the closure must differ somewhere
    /// (the test has teeth).
    #[test]
    fn closure_rebuilds_equal_full_replay() {
        let ground = |x: f32, y: f32| Some(20.0 + 0.04 * x - 0.03 * y + (x * 0.25).sin() * 0.7);
        let mut rng = Lcg(7);
        let mut teeth = 0;
        for round in 0..6 {
            let edits = rand_edits(&mut rng, 40, ground);
            let full = full_replay(&edits, ground);
            let keys: Vec<ChunkKey> = {
                let mut k: Vec<ChunkKey> = full.chunks.keys().copied().collect();
                k.sort();
                k
            };
            let h_full = chunks_hash(&full.chunks);
            let mut j = MapJournal::default();
            for e in &edits {
                j.insert(*e);
            }
            // Partial rebuilds: random subsets, grown to the closure, on top of a scrambled copy.
            for _ in 0..8 {
                let pick: Vec<ChunkKey> = keys.iter().copied().filter(|_| rng.n(5) == 0).collect();
                let set: Vec<ChunkKey> = j.read_closure(&pick).into_iter().collect();
                let mut w = VoxelWorldData {
                    chunks: full.chunks.clone(),
                    seeds: full.seeds.clone(),
                };
                for k in &set {
                    w.chunks.get_mut(k).unwrap().fill(-77); // garbage: the rebuild must not read it
                }
                w.build(&set, j.edits_for(&set).iter());
                assert_eq!(chunks_hash(&w.chunks), h_full, "round {round}: closure partial");
                // Without the closure (the old rule): the picked chunks alone, neighbours read at
                // their CURRENT (final) density.
                let mut w = VoxelWorldData {
                    chunks: full.chunks.clone(),
                    seeds: full.seeds.clone(),
                };
                w.build(&pick, j.edits_for(&pick).iter());
                if chunks_hash(&w.chunks) != h_full {
                    teeth += 1;
                }
            }
            // Incremental, in order, every chunk active from the start.
            let mut inc = VoxelWorldData {
                chunks: HashMap::new(),
                seeds: full.seeds.clone(),
            };
            inc.build(&keys, std::iter::empty());
            for e in &edits {
                inc.apply_incremental(e);
            }
            assert_eq!(chunks_hash(&inc.chunks), h_full, "round {round}: incremental");
            // Out of order: shuffled arrival; in-order ones incremental, out-of-order ones rebuild
            // the closure of what they touch (chunks activate as they are first touched).
            let mut order: Vec<usize> = (0..edits.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, rng.n(i as u32 + 1) as usize);
            }
            let mut j2 = MapJournal::default();
            let mut ooo = VoxelWorldData {
                chunks: HashMap::new(),
                seeds: full.seeds.clone(),
            };
            for &i in &order {
                let e = edits[i];
                let how = j2.insert(e);
                let fresh: Vec<ChunkKey> = e
                    .touched_chunks()
                    .into_iter()
                    .filter(|k| !ooo.is_active(*k))
                    .collect();
                if how == JournalInsert::Appended && fresh.is_empty() {
                    ooo.apply_incremental(&e);
                } else {
                    let set: Vec<ChunkKey> = j2.read_closure(&e.touched_chunks()).into_iter().collect();
                    ooo.build(&set, j2.edits_for(&set).iter());
                }
            }
            assert_eq!(chunks_hash(&ooo.chunks), h_full, "round {round}: out of order");
            // Off-thread: the closure groups as jobs on another thread.
            let mut asy = VoxelWorldData {
                chunks: HashMap::new(),
                seeds: full.seeds.clone(),
            };
            let mut left: BTreeSet<ChunkKey> = keys.iter().copied().collect();
            while let Some(&k) = left.iter().next() {
                let set: Vec<ChunkKey> = j.read_closure(&[k]).into_iter().collect();
                for s in &set {
                    left.remove(s);
                }
                let job = asy.build_job(&set, j.edits_for(&set));
                let out = std::thread::spawn(move || job.run()).join().unwrap();
                asy.insert_built(out);
            }
            assert_eq!(chunks_hash(&asy.chunks), h_full, "round {round}: async closure jobs");
        }
        assert!(teeth > 0, "a non-closure partial rebuild never differed: the test has no teeth");
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

    /// MONKEY (voxel): the ground-effect re-placement's top finder — a pit's floor, a mound's
    /// crown, the untouched surface, and nothing on a vertical wall.
    #[test]
    fn top_near_finds_pit_floor_mound_crown_and_no_wall_top() {
        let flat = |_: f32, _: f32| Some(20.0);
        let keys: Vec<ChunkKey> = vec![[0, 0, 0], [0, 0, 1], [1, 0, 0], [1, 0, 1]];
        let mut w = VoxelWorldData::default();
        for k in &keys {
            w.seeds
                .entry([k[0], k[1]])
                .or_insert_with(|| Arc::new(ColumnSeed::from_fn([k[0], k[1]], flat)));
        }
        let pit = edit([8.0, 8.0, 20.0], 3.0, 0);
        let mound = edit([24.0, 8.0, 20.0], 3.0, 1);
        w.build(&keys, [pit, mound].iter());
        // Untouched ground: the seeded surface (ADT + SEED_LIFT), facing up.
        let (z, n) = w.top_near(8.0, 2.0, 20.0, 24.0, 0.65).expect("ground top");
        assert!((z - 20.15).abs() < 0.1 && n[2] > 0.95, "{z} {n:?}");
        // Pit floor ≈ 17 (centre 20, radius 3), from the old ground height 20.
        let (z, n) = w.top_near(8.0, 8.0, 20.0, 24.0, 0.65).expect("pit floor");
        assert!((z - 17.0).abs() < 0.3 && n[2] > 0.9, "{z} {n:?}");
        // Mound crown ≈ 23.
        let (z, _) = w
            .top_near(24.0, 8.0, 20.0, 24.0, 0.65)
            .expect("mound crown");
        assert!((z - 23.0).abs() < 0.3, "{z}");
        // The steep flank of the pit, near its rim: no top steep enough to grow on.
        assert!(w.top_near(10.9, 8.0, 20.0, 24.0, 0.65).is_none());
    }

    /// MONKEY (voxel, CONTRACT v3.3): soft-brush `param` round trip + the falloff curves.
    #[test]
    fn soft_brush_param_and_falloff() {
        for f in [FALLOFF_SMOOTH, FALLOFF_LINEAR, FALLOFF_FLAT] {
            for h in [0u8, 1, 37, 99, 100] {
                let b = SoftBrush { falloff: f, hardness: h };
                assert_eq!(SoftBrush::from_param(b.to_param()), Some(b));
            }
        }
        assert_eq!(SoftBrush::from_param(0), None, "v2 / v3.0 edits stay hard");
        assert_eq!(SoftBrush::from_param(0x7fff), None);
        // Falloff 3 = smooth; hardness over 100 = 100; reserved bits ignored.
        let odd = SoftBrush::from_param(0x8000 | 3 | (127 << 2) | (0x3f << 9)).unwrap();
        assert_eq!(odd, SoftBrush { falloff: FALLOFF_SMOOTH, hardness: 100 });
        let s = SoftBrush { falloff: FALLOFF_SMOOTH, hardness: 0 };
        assert_eq!(s.weight(0.0, 8.0), 1.0);
        assert_eq!(s.weight(8.0, 8.0), 0.0);
        assert!((s.weight(4.0, 8.0) - 0.5).abs() < 1e-6);
        assert!(s.weight(2.0, 8.0) > 0.8 && s.weight(6.0, 8.0) < 0.2);
        let l = SoftBrush { falloff: FALLOFF_LINEAR, hardness: 50 };
        assert_eq!(l.weight(3.9, 8.0), 1.0);
        assert!((l.weight(6.0, 8.0) - 0.5).abs() < 1e-6);
        let fl = SoftBrush { falloff: FALLOFF_FLAT, hardness: 0 };
        assert_eq!(fl.weight(7.99, 8.0), 1.0);
        // Only modes 0-2 read the flag; flatten's param is its plane.
        let mut e = edit([0.0, 0.0, 20.0], 4.0, MODE_BUILD);
        e.param = s.to_param();
        assert!(e.soft().is_some() && e.reads_neighbours());
        e.mode = MODE_FLATTEN;
        assert!(e.soft().is_none() && !e.reads_neighbours());
    }

    /// The most air-over-solid crossings in any lattice column of `keys` (a hill with a floating
    /// bubble crosses twice).
    fn max_surfaces_per_column(w: &VoxelWorldData, keys: &[ChunkKey]) -> usize {
        let c = CHUNK as i32;
        let zlo = keys.iter().map(|k| k[2]).min().unwrap() * c;
        let zhi = (keys.iter().map(|k| k[2]).max().unwrap() + 1) * c - 1;
        let mut worst = 0;
        for k in keys {
            for y in 0..c {
                for x in 0..c {
                    let (ix, iy) = (k[0] * c + x, k[1] * c + y);
                    let mut n = 0;
                    for iz in zlo..zhi {
                        if w.sample([ix, iy, iz + 1]) >= 0 && w.sample([ix, iy, iz]) < 0 {
                            n += 1;
                        }
                    }
                    worst = worst.max(n);
                }
            }
        }
        worst
    }

    /// MONKEY (voxel, owner "tools way too strong"): a soft RAISE held 2 s (16 edits at the 8/s
    /// uplink cadence, 20 % strength, recentred on the new top each time like the tool) grows a
    /// gentle, single-surfaced hill: ~0.1 yd per edit at the centre, a smooth falloff to nothing at
    /// the radius, no bubble; a soft LOWER mirrors it. Deterministic, and the canonical rebuild of
    /// the journal equals the incremental apply.
    #[test]
    fn soft_raise_held_grows_a_gentle_hill_and_lower_mirrors_it() {
        let flat = |_: f32, _: f32| Some(20.0);
        let keys: Vec<ChunkKey> = (0..=1)
            .flat_map(|x| (0..=1).flat_map(move |y| (0..=2).map(move |z| [x, y, z])))
            .collect();
        let fresh = || {
            let mut w = VoxelWorldData::default();
            for k in &keys {
                w.seeds
                    .entry([k[0], k[1]])
                    .or_insert_with(|| Arc::new(ColumnSeed::from_fn([k[0], k[1]], flat)));
            }
            w.build(&keys, std::iter::empty());
            w
        };
        let run = |mode: u8| {
            let mut w = fresh();
            let brush = SoftBrush { falloff: FALLOFF_SMOOTH, hardness: 0 };
            for _ in 0..16 {
                let top = w
                    .top_near(16.0, 16.0, 20.0, 8.0, 0.0)
                    .map_or(20.0, |(z, _)| z);
                let mut e = edit([16.0, 16.0, top], 5.0, mode);
                e.strength = 51;
                e.param = brush.to_param();
                w.apply_incremental(&e);
            }
            w
        };
        let base = 20.0 + SEED_LIFT;
        let z = |w: &VoxelWorldData, x: f32| w.top_near(x, 16.0, 21.0, 8.0, 0.0).unwrap().0;
        let hill = run(MODE_BUILD);
        let rise = z(&hill, 16.0) - base;
        assert!((1.0..2.2).contains(&rise), "2 s at 20 % raises ~1.6 yd, got {rise}");
        let (r1, r2, r3) = (z(&hill, 17.5) - base, z(&hill, 19.0) - base, z(&hill, 20.5) - base);
        assert!(rise > r1 && r1 > r2 && r2 > r3 && r3 >= -0.02, "falloff {rise} {r1} {r2} {r3}");
        assert!((z(&hill, 23.0) - base).abs() < 0.02, "outside the brush: untouched");
        assert_eq!(max_surfaces_per_column(&hill, &keys), 1, "no bubble");
        let pit = run(MODE_DIG);
        let drop = base - z(&pit, 16.0);
        assert!((1.0..2.2).contains(&drop), "the soft lower mirrors it: {drop}");
        assert_eq!(max_surfaces_per_column(&pit, &keys), 1);
        assert_eq!(run(MODE_BUILD).chunks, hill.chunks, "deterministic");
        let mut j = MapJournal::default();
        let mut w = fresh();
        for (i, z0) in [20.15f32, 20.3, 20.5].iter().enumerate() {
            let mut e = edit([16.0, 16.0, *z0], 5.0, MODE_BUILD);
            e.strength = 200;
            e.param = SoftBrush { falloff: FALLOFF_LINEAR, hardness: 30 }.to_param();
            e.seq = Some(i as u32 + 1);
            e.local = false;
            j.insert(e);
            w.apply_incremental(&e);
        }
        let mut c = VoxelWorldData::default();
        c.seeds = w.seeds.clone();
        c.build(&keys, j.edits_for(&keys).iter());
        assert_eq!(chunks_hash(&c.chunks), chunks_hash(&w.chunks));
    }

    /// MONKEY (voxel fix, owner's cliff): an UNEDITED seeded surface is "untouched ground" at any
    /// slope (weight 0 on a 70° cliff), and a gentle soft smooth over it stays ground-textured.
    #[test]
    fn unedited_steep_cliff_has_zero_edited_weight() {
        let cliff = |x: f32, _: f32| Some(20.0 + 2.75 * (x - 8.0).clamp(-3.0, 3.0));
        let keys: Vec<ChunkKey> = (0..=2).map(|z| [0, 0, z]).collect();
        let mut w = VoxelWorldData::default();
        w.seeds.insert([0, 0], Arc::new(ColumnSeed::from_fn([0, 0], cliff)));
        w.build(&keys, std::iter::empty());
        let weights = |w: &VoxelWorldData| {
            let mut all = Vec::new();
            for k in &keys {
                let m = w.mesh(*k);
                let o = window_origin(*k);
                all.extend(edited_weights(&m, &w.window_i8(*k), &w.window_seed_heights(*k), o[2]));
            }
            all
        };
        let e = weights(&w);
        assert!(!e.is_empty() && e.iter().all(|&v| v == 0.0), "an unedited cliff is ground");
        let mut sm = edit([8.0, 8.0, 20.0], 6.0, MODE_SMOOTH);
        sm.strength = 64;
        sm.param = SoftBrush { falloff: FALLOFF_SMOOTH, hardness: 0 }.to_param();
        w.build(&keys, [sm].iter());
        let e = weights(&w);
        let hi = e.iter().filter(|&&v| v > 0.5).count();
        assert!(hi * 20 < e.len(), "a gentle smooth keeps the cliff ground: {hi}/{} edited", e.len());
    }

    /// MONKEY (voxel fix): the carve-band distance — ≤ 0 inside the carve region (no inset on an
    /// internal seam), the inset band and uncarved neighbours positive.
    #[test]
    fn carve_band_is_negative_inside_and_positive_past_the_inset() {
        let carved = |kx: i32, ky: i32| (kx, ky) == (0, 0) || (kx, ky) == (1, 0);
        // Window-local positions; the origin (-0.5, -0.5) is added back.
        let pts = [
            [9.0f32, 9.5],  // (8.5, 9.0): inside column (0,0)
            [17.5, 9.5],    // (17, 9): inside, past the internal seam
            [16.75, 2.0],   // (16.25, 1.5): past the seam, 1 yd inside the −y edge
            [9.0, 0.75],    // (8.5, 0.25): the −y inset band, 0.25 past the region
            [9.0, 0.0],     // (8.5, -0.5): the uncarved column below, 1.0 past the region
        ];
        let m = VoxelMesh {
            positions: pts.iter().map(|p| [p[0], p[1], 0.0]).collect(),
            normals: vec![[0.0, 0.0, 1.0]; pts.len()],
            indices: vec![],
        };
        let b = carve_band(&m, [-0.5, -0.5, 0.0], &carved, 0.5);
        assert!(b[0] < 0.0 && b[1] < 0.0 && b[2] < 0.0, "{b:?}");
        assert!((b[3] - 0.25).abs() < 1e-5, "{b:?}");
        assert!((b[4] - 1.0).abs() < 1e-5, "{b:?}");
    }
}

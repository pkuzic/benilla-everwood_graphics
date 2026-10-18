//! benilla-voxel — voxel terrain (option C): the format + mesher foundation.
//!
//! This crate is deliberately **engine-free** (no Bevy): it owns the voxel *data* and turns a
//! chunk of it into a plain triangle mesh ([`VoxelMesh`]). The benilla renderer (`benilla-world`,
//! Lane B) consumes [`VoxelMesh`] and uploads it as a Bevy mesh with the triplanar terrain
//! material; the mangosd server consumes the same mesh for collision/navmesh. Keeping the mesher
//! here, behind the density field, means it is swappable (dual-contouring test later) with zero
//! change to either consumer.
//!
//! Decisions baked in (owner, 2026-10-16): **0.5 yd voxels, 32³ chunks, fast-surface-nets.** See
//! `DESIGN-VoxelTerrain.md`.
//!
//! ## Sign convention
//! Density is a fixed-point signed distance field. **Negative = solid (inside), positive = air**
//! (the fast-surface-nets convention). Stored as `i8` for a deterministic, compact format; the
//! live SDF is `density as f32 / DENSITY_SCALE`, in voxel units, clamped to ±2 voxels (only the
//! sign and the near-surface gradient matter to the mesher).

use fast_surface_nets::ndshape::{ConstShape, ConstShape3u32};
use fast_surface_nets::{surface_nets, SurfaceNetsBuffer};

/// Voxels along a chunk edge (owner decision — sticky; baked into stored data).
pub const CHUNK: u32 = 32;
/// Samples along a chunk edge: `CHUNK` + a 1-voxel pad on each side. Surface-nets reads the +1
/// neighbour to place a vertex and the −1 neighbour for the normal, so the meshable interior is the
/// inner `CHUNK`³ and the one-voxel skin is shared with the neighbouring chunk (seamless stitch).
pub const SAMPLES: u32 = CHUNK + 2;
/// World size of one voxel, in yards (owner decision — sticky).
pub const VOXEL_SIZE: f32 = 0.5;
/// i8 ↔ SDF scale. `i8` [-128, 127] maps to SDF [-2.0, +1.98] voxel units — ample for the sign
/// change plus sub-voxel (1/64 yd) placement precision near the surface.
pub const DENSITY_SCALE: f32 = 64.0;

/// 34³ sample grid for one chunk (`CHUNK` + 2 pad).
pub type ChunkShape = ConstShape3u32<34, 34, 34>;

/// One voxel chunk: a `SAMPLES`³ fixed-point SDF grid. Geometry only in P0 (no per-voxel material
/// yet — that arrives with painting in P1).
#[derive(Clone)]
pub struct Chunk {
    /// Fixed-point SDF, length `ChunkShape::USIZE`; negative = solid.
    pub density: Box<[i8]>,
}

impl Chunk {
    /// An all-air chunk (no geometry).
    pub fn air() -> Self {
        Self {
            density: vec![i8::MAX; ChunkShape::USIZE].into_boxed_slice(),
        }
    }

    /// Build a chunk by sampling an SDF `f(voxel_coord) -> signed distance in voxel units`
    /// (negative = solid). Values are clamped to ±2 voxels and quantised to `i8`.
    pub fn from_sdf(mut f: impl FnMut([u32; 3]) -> f32) -> Self {
        let mut density = vec![0i8; ChunkShape::USIZE];
        for (i, cell) in density.iter_mut().enumerate() {
            let p = ChunkShape::delinearize(i as u32);
            let v = (f(p) * DENSITY_SCALE).round();
            *cell = v.clamp(i8::MIN as f32, i8::MAX as f32) as i8;
        }
        Self {
            density: density.into_boxed_slice(),
        }
    }

    /// Mesh this chunk with surface-nets. Positions are scaled to world yards (local to the chunk
    /// origin); normals are the surface-nets gradient normals. Returns an empty mesh for an
    /// all-air or all-solid chunk.
    pub fn mesh(&self) -> VoxelMesh {
        debug_assert_eq!(self.density.len(), ChunkShape::USIZE);
        let sdf: Vec<f32> = self
            .density
            .iter()
            .map(|&d| d as f32 / DENSITY_SCALE)
            .collect();
        let mut buffer = SurfaceNetsBuffer::default();
        surface_nets(&sdf, &ChunkShape {}, [0; 3], [SAMPLES - 1; 3], &mut buffer);
        let positions = buffer
            .positions
            .iter()
            .map(|p| [p[0] * VOXEL_SIZE, p[1] * VOXEL_SIZE, p[2] * VOXEL_SIZE])
            .collect();
        VoxelMesh {
            positions,
            normals: buffer.normals,
            indices: buffer.indices,
        }
    }

    /// Apply a spherical brush to the density field (the P1 edit primitive). `center` and `radius`
    /// are in **voxel/sample coordinates** (the caller converts a world point via the chunk's
    /// transform). CSG against the SDF, so edges stay smooth after re-meshing:
    /// - [`BrushMode::Subtract`] (dig) carves solid away inside the sphere → `max(old, r - |p-c|)`.
    /// - [`BrushMode::Add`] deposits solid inside the sphere → `min(old, |p-c| - r)`.
    ///
    /// Returns `true` if any voxel changed (so the caller can skip a re-mesh when nothing did).
    /// Only the sphere's bounding box (+1 voxel) is touched, so cost scales with brush size, not
    /// chunk size.
    pub fn edit_sphere(&mut self, center: [f32; 3], radius: f32, mode: BrushMode) -> bool {
        let dim = SAMPLES as i32;
        let lo = |c: f32| ((c - radius - 1.0).floor() as i32).max(0);
        let hi = |c: f32| ((c + radius + 1.0).ceil() as i32).min(dim - 1);
        let mut changed = false;
        for z in lo(center[2])..=hi(center[2]) {
            for y in lo(center[1])..=hi(center[1]) {
                for x in lo(center[0])..=hi(center[0]) {
                    let brush = ((x as f32 - center[0]).powi(2)
                        + (y as f32 - center[1]).powi(2)
                        + (z as f32 - center[2]).powi(2))
                    .sqrt()
                        - radius; // negative inside the brush
                    let i = ChunkShape::linearize([x as u32, y as u32, z as u32]) as usize;
                    let old = self.density[i] as f32 / DENSITY_SCALE;
                    let new = match mode {
                        BrushMode::Subtract => old.max(-brush),
                        BrushMode::Add => old.min(brush),
                        // Smoothing is a blur, not CSG — it goes through `smooth_sphere`; no-op here.
                        BrushMode::Smooth => old,
                    };
                    let q = (new * DENSITY_SCALE)
                        .round()
                        .clamp(i8::MIN as f32, i8::MAX as f32) as i8;
                    if q != self.density[i] {
                        self.density[i] = q;
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Smooth the density field inside a sphere toward the local average (a box blur of the 6-
    /// neighbourhood, lerped by `strength` in 0..1). Softens sharp dig/build edges. Keeps a 1-voxel
    /// border so neighbour reads stay in-bounds. Returns whether anything changed.
    pub fn smooth_sphere(&mut self, center: [f32; 3], radius: f32, strength: f32) -> bool {
        let dim = SAMPLES as i32;
        let at =
            |x: i32, y: i32, z: i32| ChunkShape::linearize([x as u32, y as u32, z as u32]) as usize;
        let lo = |c: f32| ((c - radius).floor() as i32).max(1);
        let hi = |c: f32| ((c + radius).ceil() as i32).min(dim - 2);
        let r2 = radius * radius;
        let mut updates: Vec<(usize, i8)> = Vec::new();
        for z in lo(center[2])..=hi(center[2]) {
            for y in lo(center[1])..=hi(center[1]) {
                for x in lo(center[0])..=hi(center[0]) {
                    let d2 = (x as f32 - center[0]).powi(2)
                        + (y as f32 - center[1]).powi(2)
                        + (z as f32 - center[2]).powi(2);
                    if d2 > r2 {
                        continue;
                    }
                    let avg = (self.density[at(x + 1, y, z)] as f32
                        + self.density[at(x - 1, y, z)] as f32
                        + self.density[at(x, y + 1, z)] as f32
                        + self.density[at(x, y - 1, z)] as f32
                        + self.density[at(x, y, z + 1)] as f32
                        + self.density[at(x, y, z - 1)] as f32)
                        / 6.0;
                    let i = at(x, y, z);
                    let old = self.density[i] as f32;
                    let new = (old * (1.0 - strength) + avg * strength)
                        .round()
                        .clamp(i8::MIN as f32, i8::MAX as f32) as i8;
                    if new != self.density[i] {
                        updates.push((i, new));
                    }
                }
            }
        }
        let changed = !updates.is_empty();
        for (i, v) in updates {
            self.density[i] = v;
        }
        changed
    }
}

/// What a brush stroke does to the density field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrushMode {
    /// Carve solid away (dig a hole / tunnel).
    Subtract,
    /// Deposit solid (build up terrain).
    Add,
    /// Smooth / soften the surface toward the local average.
    Smooth,
}

/// A flat ground block: solid below `surface` on the **Y (up) axis** in voxel coords, air above — a
/// terraform sandbox with a flat top you can dig pits into and pile mounds onto. (Bevy +Y is up:
/// WoW z-up maps to Bevy Y.)
pub fn slab(surface: f32) -> Chunk {
    Chunk::from_sdf(move |[_, y, _]| y as f32 - surface) // negative below the surface = solid
}

/// A plain triangle mesh — the contract between the mesher and every consumer (benilla-world render,
/// mangosd collision). Engine-neutral: `[f32; 3]` positions/normals + `u32` indices (triangle list).
#[derive(Clone, Default, Debug)]
pub struct VoxelMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

impl VoxelMesh {
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
    /// Number of triangles.
    pub fn tri_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// Dense per-chunk density footprint, in bytes (i8 SDF incl. the 1-voxel pad).
pub const fn chunk_bytes() -> usize {
    (SAMPLES as usize).pow(3)
}

/// Dense density footprint, in bytes, of a volume `dims` voxels on each axis (geometry only, incl.
/// per-chunk padding). Used to sanity-check the sticky 0.5 yd decision.
pub fn volume_density_bytes(dims: [u32; 3]) -> usize {
    let chunks = |n: u32| n.div_ceil(CHUNK) as usize;
    chunks(dims[0]) * chunks(dims[1]) * chunks(dims[2]) * chunk_bytes()
}

// ---------------------------------------------------------------------------------------------
// Test-volume generators (P0 has no file writer yet — volumes are code-generated).
// ---------------------------------------------------------------------------------------------

/// A solid sphere centred in the chunk (a basic smooth-surface sanity shape).
pub fn sphere(radius: f32, center: [f32; 3]) -> Chunk {
    Chunk::from_sdf(move |[x, y, z]| {
        let d = ((x as f32 - center[0]).powi(2)
            + (y as f32 - center[1]).powi(2)
            + (z as f32 - center[2]).powi(2))
        .sqrt();
        d - radius // negative inside
    })
}

/// A torus in the XY plane — the P0 **overhang proof**. A torus is genuinely non-heightmap: along a
/// column through the tube there are *two* surfaces (top and bottom of the ring) plus the central
/// hole, which a single-valued heightmap can never represent.
pub fn torus(major: f32, minor: f32, center: [f32; 3]) -> Chunk {
    Chunk::from_sdf(move |[x, y, z]| {
        let dx = x as f32 - center[0];
        let dy = y as f32 - center[1];
        let dz = z as f32 - center[2];
        let q = ((dx * dx + dy * dy).sqrt() - major, dz);
        (q.0 * q.0 + q.1 * q.1).sqrt() - minor // negative inside the tube
    })
}

/// A multi-chunk voxel volume: a grid of `chunks` chunks sharing **one global density field**, so
/// neighbouring chunks mesh seamlessly (their 34³ surface-nets windows overlap by the shared
/// padding, meshing the seam identically). Meshes to a single combined [`VoxelMesh`]. The interior
/// spans `chunks * CHUNK` voxels per axis; the global grid carries a 1-voxel pad on each side.
#[derive(Clone)]
pub struct VoxelVolume {
    chunks: [u32; 3],
    gdim: [usize; 3], // global sample grid dims = chunks*CHUNK + 2
    density: Vec<i8>, // negative = solid
}

impl VoxelVolume {
    fn dims(chunks: [u32; 3]) -> [usize; 3] {
        [
            (chunks[0] * CHUNK + 2) as usize,
            (chunks[1] * CHUNK + 2) as usize,
            (chunks[2] * CHUNK + 2) as usize,
        ]
    }

    #[inline]
    fn gidx(&self, x: usize, y: usize, z: usize) -> usize {
        x + y * self.gdim[0] + z * self.gdim[0] * self.gdim[1]
    }

    /// Chunks per axis.
    pub fn chunk_dims(&self) -> [u32; 3] {
        self.chunks
    }

    /// Interior size in world yards per axis (the footprint you can sculpt).
    pub fn world_size(&self) -> [f32; 3] {
        [
            self.chunks[0] as f32 * CHUNK as f32 * VOXEL_SIZE,
            self.chunks[1] as f32 * CHUNK as f32 * VOXEL_SIZE,
            self.chunks[2] as f32 * CHUNK as f32 * VOXEL_SIZE,
        ]
    }

    /// The up-axis surface height (in VOXEL units, grid space) at the column `(x_vox, z_vox)` — the
    /// highest air→solid crossing marching the density column top-down, bilinear in x/z. `None` if
    /// the column is entirely air/solid or outside the grid. For standing/walking on the voxel
    /// surface: a caller maps a world point to local voxel coords
    /// `((world - origin) / VOXEL_SIZE)`, calls this, and maps the result back
    /// `origin.y + height * VOXEL_SIZE`. Density convention: negative = solid, positive = air.
    pub fn surface_height_at(&self, x_vox: f32, z_vox: f32) -> Option<f32> {
        if x_vox < 0.0 || z_vox < 0.0 {
            return None;
        }
        let x0 = x_vox.floor() as usize;
        let z0 = z_vox.floor() as usize;
        if x0 + 1 >= self.gdim[0] || z0 + 1 >= self.gdim[2] {
            return None;
        }
        let fx = x_vox - x0 as f32;
        let fz = z_vox - z0 as f32;
        let sample = |y: usize| -> f32 {
            let d00 = self.density[self.gidx(x0, y, z0)] as f32;
            let d10 = self.density[self.gidx(x0 + 1, y, z0)] as f32;
            let d01 = self.density[self.gidx(x0, y, z0 + 1)] as f32;
            let d11 = self.density[self.gidx(x0 + 1, y, z0 + 1)] as f32;
            let a = d00 + (d10 - d00) * fx;
            let b = d01 + (d11 - d01) * fx;
            a + (b - a) * fz
        };
        // Scan top-down for the first air (>= 0) above solid (< 0): the top surface of that column.
        let mut above = sample(self.gdim[1] - 1);
        for y in (0..self.gdim[1] - 1).rev() {
            let here = sample(y);
            if above >= 0.0 && here < 0.0 {
                // Linear zero-crossing between node y (here) and y+1 (above).
                return Some(y as f32 + (-here) / (above - here));
            }
            above = here;
        }
        None
    }

    /// Build by sampling an SDF over global voxel coords (negative = solid).
    pub fn from_sdf(chunks: [u32; 3], mut f: impl FnMut([u32; 3]) -> f32) -> Self {
        let gdim = Self::dims(chunks);
        let mut density = vec![0i8; gdim[0] * gdim[1] * gdim[2]];
        let mut i = 0;
        for z in 0..gdim[2] {
            for y in 0..gdim[1] {
                for x in 0..gdim[0] {
                    let v = (f([x as u32, y as u32, z as u32]) * DENSITY_SCALE).round();
                    density[i] = v.clamp(i8::MIN as f32, i8::MAX as f32) as i8;
                    i += 1;
                }
            }
        }
        Self {
            chunks,
            gdim,
            density,
        }
    }

    /// A flat ground block across the whole volume: solid below `surface` on the Y (up) axis.
    pub fn slab(chunks: [u32; 3], surface: f32) -> Self {
        Self::from_sdf(chunks, move |[_, y, _]| y as f32 - surface)
    }

    /// A volume whose surface follows a per-column WORLD height — seeded from the real heightmap, so
    /// the voxel region starts as an exact copy of the ground it sits on (then a GM sculpts it into
    /// overhangs/caves the heightmap can't express). `heights` is the horizontal grid of ground
    /// heights, row-major `gx = chunks[0]*CHUNK+2` wide and `gz = chunks[2]*CHUNK+2` tall, in the same
    /// world-y units as `origin_world_y + voxel_row * VOXEL_SIZE`. Solid below the height, air above
    /// (SDF = world_y − height, the slab law generalised per column). A column whose height is NaN is
    /// treated as all-air (unloaded ground: leave it empty rather than guess).
    pub fn from_heightmap(chunks: [u32; 3], origin_world_y: f32, heights: &[f32]) -> Self {
        let gx = (chunks[0] * CHUNK + 2) as usize;
        Self::from_sdf(chunks, |[x, y, z]| {
            let h = heights[x as usize + z as usize * gx];
            if h.is_nan() {
                return (SAMPLES * CHUNK) as f32; // far outside: all air
            }
            (origin_world_y + y as f32 * VOXEL_SIZE) - h
        })
    }

    /// Sphere brush over the whole volume (`center`/`radius` in global voxel coords). Spans chunk
    /// boundaries freely since the density is one grid. Returns whether anything changed.
    pub fn edit_sphere(&mut self, center: [f32; 3], radius: f32, mode: BrushMode) -> bool {
        let lo = |c: f32| (c - radius - 1.0).floor().max(0.0) as usize;
        let hi =
            |c: f32, d: usize| ((c + radius + 1.0).ceil() as i64).clamp(0, d as i64 - 1) as usize;
        let mut changed = false;
        for z in lo(center[2])..=hi(center[2], self.gdim[2]) {
            for y in lo(center[1])..=hi(center[1], self.gdim[1]) {
                for x in lo(center[0])..=hi(center[0], self.gdim[0]) {
                    let brush = ((x as f32 - center[0]).powi(2)
                        + (y as f32 - center[1]).powi(2)
                        + (z as f32 - center[2]).powi(2))
                    .sqrt()
                        - radius;
                    let i = self.gidx(x, y, z);
                    let old = self.density[i] as f32 / DENSITY_SCALE;
                    let new = match mode {
                        BrushMode::Subtract => old.max(-brush),
                        BrushMode::Add => old.min(brush),
                        BrushMode::Smooth => old,
                    };
                    let q = (new * DENSITY_SCALE)
                        .round()
                        .clamp(i8::MIN as f32, i8::MAX as f32) as i8;
                    if q != self.density[i] {
                        self.density[i] = q;
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Smooth brush over the whole volume (global voxel coords). Keeps a 1-voxel border.
    pub fn smooth_sphere(&mut self, center: [f32; 3], radius: f32, strength: f32) -> bool {
        let lo = |c: f32| (c - radius).floor().max(1.0) as usize;
        let hi = |c: f32, d: usize| ((c + radius).ceil() as i64).clamp(1, d as i64 - 2) as usize;
        let r2 = radius * radius;
        let mut updates: Vec<(usize, i8)> = Vec::new();
        for z in lo(center[2])..=hi(center[2], self.gdim[2]) {
            for y in lo(center[1])..=hi(center[1], self.gdim[1]) {
                for x in lo(center[0])..=hi(center[0], self.gdim[0]) {
                    let d2 = (x as f32 - center[0]).powi(2)
                        + (y as f32 - center[1]).powi(2)
                        + (z as f32 - center[2]).powi(2);
                    if d2 > r2 {
                        continue;
                    }
                    let avg = (self.density[self.gidx(x + 1, y, z)] as f32
                        + self.density[self.gidx(x - 1, y, z)] as f32
                        + self.density[self.gidx(x, y + 1, z)] as f32
                        + self.density[self.gidx(x, y - 1, z)] as f32
                        + self.density[self.gidx(x, y, z + 1)] as f32
                        + self.density[self.gidx(x, y, z - 1)] as f32)
                        / 6.0;
                    let i = self.gidx(x, y, z);
                    let old = self.density[i] as f32;
                    let new = (old * (1.0 - strength) + avg * strength)
                        .round()
                        .clamp(i8::MIN as f32, i8::MAX as f32) as i8;
                    if new != self.density[i] {
                        updates.push((i, new));
                    }
                }
            }
        }
        let changed = !updates.is_empty();
        for (i, v) in updates {
            self.density[i] = v;
        }
        changed
    }

    /// Mesh every chunk and combine into one [`VoxelMesh`]. Positions are world yards relative to the
    /// volume origin; adjacent chunk windows share global samples, so the seams line up.
    pub fn mesh(&self) -> VoxelMesh {
        let mut out = VoxelMesh::default();
        let mut sdf = vec![0f32; ChunkShape::USIZE];
        for cz in 0..self.chunks[2] {
            for cy in 0..self.chunks[1] {
                for cx in 0..self.chunks[0] {
                    let (ox, oy, oz) = (
                        (cx * CHUNK) as usize,
                        (cy * CHUNK) as usize,
                        (cz * CHUNK) as usize,
                    );
                    for li in 0..ChunkShape::USIZE {
                        let [lx, ly, lz] = ChunkShape::delinearize(li as u32);
                        sdf[li] = self.density
                            [self.gidx(ox + lx as usize, oy + ly as usize, oz + lz as usize)]
                            as f32
                            / DENSITY_SCALE;
                    }
                    let mut buffer = SurfaceNetsBuffer::default();
                    surface_nets(&sdf, &ChunkShape {}, [0; 3], [SAMPLES - 1; 3], &mut buffer);
                    if buffer.positions.is_empty() {
                        continue;
                    }
                    let base = out.positions.len() as u32;
                    for p in &buffer.positions {
                        out.positions.push([
                            (p[0] + ox as f32) * VOXEL_SIZE,
                            (p[1] + oy as f32) * VOXEL_SIZE,
                            (p[2] + oz as f32) * VOXEL_SIZE,
                        ]);
                    }
                    out.normals.extend_from_slice(&buffer.normals);
                    out.indices.extend(buffer.indices.iter().map(|i| i + base));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fast_surface_nets::ndshape::ConstShape;
    use std::collections::HashMap;

    #[test]
    fn sphere_meshes_to_a_closed_shell() {
        let c = sphere(12.0, [17.0, 17.0, 17.0]);
        let m = c.mesh();
        assert!(!m.is_empty(), "sphere produced no geometry");
        assert_eq!(m.indices.len() % 3, 0, "indices not a triangle list");
        assert_eq!(m.normals.len(), m.positions.len(), "one normal per vertex");
        // Every vertex sits ~on the sphere surface (within ~1 voxel = 0.5 yd of radius 6.0 yd).
        let r = 12.0 * VOXEL_SIZE;
        let c_w = [17.0 * VOXEL_SIZE, 17.0 * VOXEL_SIZE, 17.0 * VOXEL_SIZE];
        for p in &m.positions {
            let d = ((p[0] - c_w[0]).powi(2) + (p[1] - c_w[1]).powi(2) + (p[2] - c_w[2]).powi(2))
                .sqrt();
            assert!(
                (d - r).abs() < VOXEL_SIZE * 1.5,
                "vertex off-surface: d={d} r={r}"
            );
        }
    }

    #[test]
    fn torus_proves_overhang_geometry() {
        // Tube centred at z=17; major 10, minor 4 → the ring body spans two z-bands per column.
        let c = torus(10.0, 4.0, [17.0, 17.0, 17.0]);
        let m = c.mesh();
        assert!(!m.is_empty(), "torus produced no geometry");

        // Bin vertices by integer (x,y) column; find a column whose vertices split into two
        // z-clusters separated by a gap — i.e. two surfaces over one (x,y). Impossible on a
        // heightmap; this is the whole point of voxels.
        let mut columns: HashMap<(i32, i32), Vec<f32>> = HashMap::new();
        for p in &m.positions {
            let key = (
                (p[0] / VOXEL_SIZE).round() as i32,
                (p[1] / VOXEL_SIZE).round() as i32,
            );
            columns.entry(key).or_default().push(p[2]);
        }
        let overhang = columns.values().any(|zs| {
            let mut zs = zs.clone();
            zs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            // a gap larger than one voxel between consecutive z's ⇒ two separate surfaces
            zs.windows(2).any(|w| (w[1] - w[0]) > VOXEL_SIZE * 2.0)
        });
        assert!(
            overhang,
            "torus mesh has no two-surface column — overhang not represented"
        );
    }

    #[test]
    fn meshing_is_deterministic() {
        let c = torus(10.0, 4.0, [17.0, 17.0, 17.0]);
        let a = c.mesh();
        let b = c.mesh();
        assert_eq!(a.positions, b.positions);
        assert_eq!(a.indices, b.indices);
        assert_eq!(a.normals, b.normals);
    }

    #[test]
    fn air_chunk_is_empty() {
        assert!(Chunk::air().mesh().is_empty());
    }

    #[test]
    fn subtract_brush_digs_a_hole() {
        let mut c = sphere(12.0, [17.0, 17.0, 17.0]);
        let ci = ChunkShape::linearize([17, 17, 17]) as usize;
        assert!(c.density[ci] < 0, "sphere centre should start solid");
        let before = c.mesh().tri_count();

        let changed = c.edit_sphere([17.0, 17.0, 17.0], 5.0, BrushMode::Subtract);
        assert!(changed, "dig changed nothing");
        assert!(c.density[ci] > 0, "dug centre should now be air");
        assert_ne!(
            before,
            c.mesh().tri_count(),
            "mesh unchanged after digging a hole"
        );
    }

    #[test]
    fn add_brush_deposits_solid() {
        let mut c = Chunk::air();
        assert!(c.mesh().is_empty());
        let changed = c.edit_sphere([17.0, 17.0, 17.0], 6.0, BrushMode::Add);
        assert!(changed, "add changed nothing");
        assert!(!c.mesh().is_empty(), "add brush produced no geometry");
        let ci = ChunkShape::linearize([17, 17, 17]) as usize;
        assert!(
            c.density[ci] < 0,
            "centre of an added sphere should be solid"
        );
    }

    #[test]
    fn brush_outside_range_is_a_noop() {
        let mut c = sphere(12.0, [17.0, 17.0, 17.0]);
        // A tiny brush far in the air (corner) touches nothing solid and must report no change.
        assert!(!c.edit_sphere([2.0, 2.0, 2.0], 1.0, BrushMode::Subtract));
    }

    #[test]
    fn slab_is_solid_below_air_above() {
        let c = slab(20.0);
        assert!(
            c.density[ChunkShape::linearize([17, 10, 17]) as usize] < 0,
            "below surface = solid"
        );
        assert!(
            c.density[ChunkShape::linearize([17, 30, 17]) as usize] > 0,
            "above surface = air"
        );
        assert!(!c.mesh().is_empty(), "slab produced no surface");
    }

    #[test]
    fn smooth_changes_a_sharp_edit() {
        let mut c = slab(20.0);
        c.edit_sphere([17.0, 20.0, 17.0], 4.0, BrushMode::Subtract); // dig a sharp pit
        assert!(
            c.smooth_sphere([17.0, 20.0, 17.0], 6.0, 0.5),
            "smooth changed nothing"
        );
    }

    #[test]
    fn volume_slab_meshes() {
        let v = VoxelVolume::slab([2, 1, 2], 16.0);
        assert_eq!(v.world_size(), [32.0, 16.0, 32.0]); // 2 * 32 * 0.5 yd
        let m = v.mesh();
        assert!(!m.is_empty(), "volume slab produced no surface");
        assert_eq!(m.indices.len() % 3, 0, "indices not a triangle list");
        assert_eq!(m.normals.len(), m.positions.len(), "one normal per vertex");
    }

    #[test]
    fn volume_scales_with_chunk_count() {
        let one = VoxelVolume::slab([1, 1, 1], 16.0).mesh().positions.len();
        let four = VoxelVolume::slab([2, 1, 2], 16.0).mesh().positions.len();
        assert!(
            four > one * 2,
            "a 2x2 footprint should have well more surface verts than 1x1"
        );
    }

    #[test]
    fn volume_edit_spans_chunk_boundary() {
        // An edit centred on the seam between chunk 0 and chunk 1 (global x = 32) must change it.
        let mut v = VoxelVolume::slab([2, 1, 1], 16.0);
        assert!(v.edit_sphere([32.0, 16.0, 16.0], 6.0, BrushMode::Subtract));
    }

    #[test]
    fn from_heightmap_follows_a_sloped_ground() {
        // A ground that ramps along x: height = 8 + 0.1 * world_x. The volume's surface should track
        // it (higher surface at larger x), proving the region seeds from the real heightmap.
        let chunks = [2u32, 2, 2];
        let gx = (chunks[0] * CHUNK + 2) as usize;
        let gz = (chunks[2] * CHUNK + 2) as usize;
        let origin_y = 0.0;
        let mut heights = vec![0f32; gx * gz];
        for z in 0..gz {
            for x in 0..gx {
                heights[x + z * gx] = 8.0 + 0.1 * (x as f32 * VOXEL_SIZE);
            }
        }
        let v = VoxelVolume::from_heightmap(chunks, origin_y, &heights);
        // Surface height is in VOXEL units: world_y = origin_y + h_vox * VOXEL_SIZE, so h_vox = height / VOXEL_SIZE.
        let near = v.surface_height_at(10.0, 32.0).expect("surface near x-lo");
        let far = v.surface_height_at(50.0, 32.0).expect("surface near x-hi");
        assert!(
            far > near + 2.0,
            "ramp: surface rises with x ({near} -> {far} voxels)"
        );
        assert!(!v.mesh().is_empty(), "heightmap volume produced no surface");
    }

    #[test]
    fn surface_height_tracks_the_slab_and_edits() {
        // A flat slab at surface y=16: the height query returns ~16 across the interior.
        let v = VoxelVolume::slab([2, 1, 2], 16.0);
        let h = v
            .surface_height_at(32.0, 32.0)
            .expect("slab has a surface mid-volume");
        assert!((h - 16.0).abs() < 0.6, "flat slab height ~16, got {h}");
        // Build a mound (add material above the slab) and the surface there rises.
        let mut v2 = VoxelVolume::slab([2, 1, 2], 16.0);
        v2.edit_sphere([32.0, 20.0, 32.0], 6.0, BrushMode::Add);
        let hm = v2
            .surface_height_at(32.0, 32.0)
            .expect("mound has a surface");
        assert!(
            hm > h + 1.0,
            "mound should raise the surface above the flat slab ({hm} vs {h})"
        );
        // Outside the grid → None.
        assert!(
            v.surface_height_at(-5.0, 10.0).is_none(),
            "outside grid is None"
        );
    }

    #[test]
    fn density_footprint_at_half_yard_is_sane() {
        // 100 × 100 × 50 yd volume at 0.5 yd = 200 × 200 × 100 voxels.
        let bytes = volume_density_bytes([200, 200, 100]);
        let mib = bytes as f64 / (1024.0 * 1024.0);
        // Density-only (no material yet), incl. per-chunk padding. Expect ~7–8 MiB — confirms the
        // sticky 0.5 yd choice is affordable; sparse/near-surface storage + u8 material come later.
        println!("density footprint of 100x100x50yd @ 0.5yd = {mib:.2} MiB ({bytes} bytes)");
        assert!(mib < 12.0, "unexpectedly large: {mib:.2} MiB");
    }
}

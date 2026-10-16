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
        Self { density: vec![i8::MAX; ChunkShape::USIZE].into_boxed_slice() }
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
        Self { density: density.into_boxed_slice() }
    }

    /// Mesh this chunk with surface-nets. Positions are scaled to world yards (local to the chunk
    /// origin); normals are the surface-nets gradient normals. Returns an empty mesh for an
    /// all-air or all-solid chunk.
    pub fn mesh(&self) -> VoxelMesh {
        debug_assert_eq!(self.density.len(), ChunkShape::USIZE);
        let sdf: Vec<f32> = self.density.iter().map(|&d| d as f32 / DENSITY_SCALE).collect();
        let mut buffer = SurfaceNetsBuffer::default();
        surface_nets(&sdf, &ChunkShape {}, [0; 3], [SAMPLES - 1; 3], &mut buffer);
        let positions = buffer
            .positions
            .iter()
            .map(|p| [p[0] * VOXEL_SIZE, p[1] * VOXEL_SIZE, p[2] * VOXEL_SIZE])
            .collect();
        VoxelMesh { positions, normals: buffer.normals, indices: buffer.indices }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
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
            assert!((d - r).abs() < VOXEL_SIZE * 1.5, "vertex off-surface: d={d} r={r}");
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
            let key = ((p[0] / VOXEL_SIZE).round() as i32, (p[1] / VOXEL_SIZE).round() as i32);
            columns.entry(key).or_default().push(p[2]);
        }
        let overhang = columns.values().any(|zs| {
            let mut zs = zs.clone();
            zs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            // a gap larger than one voxel between consecutive z's ⇒ two separate surfaces
            zs.windows(2).any(|w| (w[1] - w[0]) > VOXEL_SIZE * 2.0)
        });
        assert!(overhang, "torus mesh has no two-surface column — overhang not represented");
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

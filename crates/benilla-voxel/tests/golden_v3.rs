//! MONKEY (voxel): golden vectors shared with the server's C++ replica
//! (`voxel-core/tw/tools/MonkeyVoxelGolden`, `src/game/Maps/MonkeyVoxelField.cpp`).
//!
//! - **P1** (modes 0-2, the server's `golden_p1.cpp` set): 60 edits, the client's activation
//!   (`want` + surface-column expansion), canonical build, per chunk the density + surface-nets mesh
//!   FNV, and a 48×48 down-ray height hash. If the C++ output file exists
//!   (`MONKEY_VOXEL_CPP_P1`, default `G:/mangos/disposable/voxel-core/golden/cpp_p1.txt`) every
//!   `chunk …` / `heights …` line must match it exactly.
//! - **V3** (P1 + flatten edits + paint/roughen no-ops): the same lines for the C++ port of mode 3 to
//!   compare against; written to `MONKEY_VOXEL_GOLDEN_OUT` when set. The V3 set must also pass the
//!   closure rule (every edit's closure rebuild == the full replay).
use benilla_voxel::world::*;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

/// Arithmetic only (+,*): bit-identical in Rust and MSVC by construction (= golden_p1.cpp).
fn height(x: f32, y: f32) -> Option<f32> {
    let dx = x + 8940.0;
    let dy = y + 70.0;
    if dx > 9.0 && dx < 11.0 && dy > -3.0 && dy < -1.0 {
        return None;
    }
    Some(80.0 + 0.0625 * dx - 0.03125 * dy + 0.0009765625 * dx * dy - 0.00048828125 * dx * dx)
}

fn fnv_bytes(h: &mut u64, d: &[u8]) {
    for &b in d {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn fnv(d: &[i8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in d {
        h ^= b as u8 as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn triangle_z_at(a: [f32; 3], b: [f32; 3], c: [f32; 3], x: f32, y: f32) -> Option<f32> {
    let det = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
    if det.abs() < 1.0e-9 {
        return None;
    }
    let l1 = ((b[1] - c[1]) * (x - c[0]) + (c[0] - b[0]) * (y - c[1])) / det;
    let l2 = ((c[1] - a[1]) * (x - c[0]) + (a[0] - c[0]) * (y - c[1])) / det;
    let l3 = 1.0 - l1 - l2;
    if l1 < 0.0 || l2 < 0.0 || l3 < 0.0 {
        return None;
    }
    Some(l1 * a[2] + l2 * b[2] + l3 * c[2])
}

fn op(pos: [f32; 3], radius: f32, mode: u8, strength: u8, seq: u32, param: i16) -> VoxelEditOp {
    VoxelEditOp {
        map: 0,
        pos,
        radius,
        mode,
        strength,
        nonce: 1000 + seq as u16,
        seq: Some(seq),
        local: false,
        stroke: 0,
        param: param as u16,
    }
}

/// = navgold `p1_edits` = golden_p1.cpp.
fn p1_edits() -> Vec<VoxelEditOp> {
    let mut edits = synth_edits(48, 0, [-8940.0, -70.0], 82.0, 40.0, height);
    let extra: [([f32; 3], f32, u8, u8); 12] = [
        ([-8944.0, -64.0, 81.0], 12.0, 0, 255),
        ([-8925.5, -80.25, 84.0], 16.0, 1, 255),
        ([-8944.0, -70.0, 82.0], 6.0, 2, 200),
        ([-8951.3, -61.7, 80.4], 0.5, 0, 255),
        ([-8930.1, -59.9, 83.3], 0.5, 1, 255),
        ([-8939.75, -71.25, 79.5], 0.5, 2, 255),
        ([-8936.0, -64.0, 88.0], 4.0, 1, 255),
        ([-8948.0, -76.0, 70.0], 5.0, 0, 255),
        ([-8944.0, -64.0, 80.0], 9.0, 2, 128),
        ([-8920.0, -96.0, 85.0], 3.0, 0, 255),
        ([-8960.0, -48.0, 81.5], 7.5, 1, 255),
        ([-8960.0, -48.0, 82.0], 7.5, 2, 77),
    ];
    let n0 = edits.len() as u32;
    for (i, (p, r, m, s)) in extra.iter().enumerate() {
        edits.push(op(*p, *r, *m, *s, n0 + 1 + i as u32, 0));
    }
    edits
}

/// P1 + 10 v3 edits: flattens (positive / negative / extreme plane offsets, partial strength, a
/// chunk-corner straddle, max radius, the tiny radius, over a smooth) and one paint + one roughen
/// (no-ops on this client: never indexed, never activate).
fn v3_edits() -> Vec<VoxelEditOp> {
    let mut e = p1_edits();
    let n = e.len() as u32;
    let extra: [([f32; 3], f32, u8, u8, i16); 10] = [
        ([-8944.0, -64.0, 81.0], 6.0, 3, 255, 0),         // level the big dig's floor at its centre
        ([-8936.0, -80.0, 83.0], 8.0, 3, 255, 150),       // plane +1.5 yd on a chunk corner
        ([-8950.25, -70.5, 81.7], 4.5, 3, 128, -220),     // half strength, plane -2.2
        ([-8925.5, -80.25, 90.0], 16.0, 3, 200, -400),    // max radius over the big build
        ([-8939.75, -71.25, 79.5], 0.5, 3, 255, 25),      // tiny
        ([-8960.0, -48.0, 82.0], 7.0, 3, 77, 32767),      // plane far above (carves the sphere)
        ([-8930.0, -60.0, 83.0], 3.0, 3, 255, -32768),    // plane far below (fills the sphere)
        ([-8944.0, -70.0, 82.0], 6.0, 2, 160, 0),         // smooth across the flattened area
        ([-8915.0, -50.0, 84.0], 3.0, 4, 255, 7),         // paint (no-op)
        ([-8905.0, -90.0, 84.0], 3.0, 5, 255, 4242),      // roughen (no-op)
    ];
    for (i, (p, r, m, s, prm)) in extra.iter().enumerate() {
        e.push(op(*p, *r, *m, *s, n + 1 + i as u32, *prm));
    }
    e
}

/// The client's activation (benilla-world `want` + `expand_column`) over every edit.
fn activate(edits: &[VoxelEditOp]) -> (Vec<ChunkKey>, HashMap<ColumnKey, Arc<ColumnSeed>>) {
    fn want(
        k: ChunkKey,
        seeds: &mut HashMap<ColumnKey, Arc<ColumnSeed>>,
        wanted: &mut HashMap<ColumnKey, BTreeSet<i32>>,
    ) {
        let col = [k[0], k[1]];
        if wanted.get(&col).is_some_and(|s| s.contains(&k[2])) {
            return;
        }
        wanted.entry(col).or_default().insert(k[2]);
        let seed = seeds
            .entry(col)
            .or_insert_with(|| Arc::new(ColumnSeed::from_fn(col, height)))
            .clone();
        let Some((lo, hi)) = seed.surface_chunks() else {
            return;
        };
        if wanted.get(&col).is_some_and(|s| s.range(lo - 1..=hi + 1).next().is_some()) {
            for kz in lo..=hi {
                want([col[0], col[1], kz], seeds, wanted);
            }
        }
    }
    let mut seeds = HashMap::new();
    let mut wanted = HashMap::new();
    for e in edits.iter().filter(|e| e.is_applied()) {
        for k in e.touched_chunks() {
            want(k, &mut seeds, &mut wanted);
        }
    }
    let mut keys: Vec<ChunkKey> = wanted
        .iter()
        .flat_map(|(c, kzs)| kzs.iter().map(move |kz| [c[0], c[1], *kz]))
        .collect();
    keys.sort();
    (keys, seeds)
}

/// The golden text (the C++ tool's exact format, + `param` on v3 edit lines).
fn golden(edits: &[VoxelEditOp], v3: bool) -> (String, VoxelWorldData, Vec<ChunkKey>) {
    let mut out = String::new();
    for e in edits {
        let _ = write!(
            out,
            "edit {} {} {:08x} {:08x} {:08x} {:08x} {}",
            e.seq.unwrap(),
            e.mode,
            e.pos[0].to_bits(),
            e.pos[1].to_bits(),
            e.pos[2].to_bits(),
            e.radius.to_bits(),
            e.strength
        );
        if v3 {
            let _ = write!(out, " {}", e.param as i16);
        }
        out.push('\n');
    }
    let (keys, seeds) = activate(edits);
    let mut w = VoxelWorldData {
        seeds,
        ..Default::default()
    };
    w.build(&keys, edits.iter());
    let _ = writeln!(out, "chunks {}", keys.len());
    let mut tris: Vec<[[f32; 3]; 3]> = Vec::new();
    for k in &keys {
        let m = w.mesh(*k);
        let mut h: u64 = 0xcbf29ce484222325;
        for p in &m.positions {
            for v in p {
                fnv_bytes(&mut h, &v.to_bits().to_le_bytes());
            }
        }
        for i in &m.indices {
            fnv_bytes(&mut h, &i.to_le_bytes());
        }
        let _ = writeln!(
            out,
            "chunk {} {} {} {:016x} mesh {:016x} v{} t{}",
            k[0],
            k[1],
            k[2],
            fnv(&w.chunks[k]),
            h,
            m.positions.len(),
            m.indices.len() / 3
        );
        let o = window_origin(*k);
        for t in m.indices.chunks_exact(3) {
            let p = |i: u32| {
                let q = m.positions[i as usize];
                [o[0] + q[0], o[1] + q[1], o[2] + q[2]]
            };
            tris.push([p(t[0]), p(t[1]), p(t[2])]);
        }
    }
    let mut h: u64 = 0xcbf29ce484222325;
    let mut n = 0;
    for j in 0..48 {
        for i in 0..48 {
            let x = -8964.0f32 + i as f32 * 1.03;
            let y = -94.0f32 + j as f32 * 1.03;
            let mut best: Option<f32> = None;
            for t in &tris {
                if let Some(z) = triangle_z_at(t[0], t[1], t[2], x, y) {
                    if best.is_none_or(|b| z > b) {
                        best = Some(z);
                    }
                }
            }
            let z = best.unwrap_or(-200000.0);
            if best.is_some() {
                n += 1;
            }
            fnv_bytes(&mut h, &z.to_bits().to_le_bytes());
            if (i + j * 48) % 397 == 0 {
                let _ = writeln!(out, "height {i} {j} {z:.5}");
            }
        }
    }
    let _ = writeln!(out, "heights {n} {h:016x}");
    let _ = writeln!(out, "density_hash {:016x}", chunks_hash(&w.chunks));
    (out, w, keys)
}

#[test]
fn p1_matches_the_cpp_replica() {
    let (text, _, _) = golden(&p1_edits(), false);
    let path = std::env::var("MONKEY_VOXEL_CPP_P1")
        .unwrap_or_else(|_| "G:/mangos/disposable/voxel-core/golden/cpp_p1.txt".into());
    let Ok(cpp) = std::fs::read_to_string(&path) else {
        eprintln!("golden: {path} not found — C++ comparison skipped");
        return;
    };
    let pick = |s: &str| -> Vec<String> {
        s.lines()
            .map(|l| l.trim_start_matches('\u{feff}').trim().to_string())
            .filter(|l| l.starts_with("chunk") || l.starts_with("heights") || l.starts_with("edit"))
            .collect()
    };
    let (ours, theirs) = (pick(&text), pick(&cpp));
    assert!(theirs.len() > 60, "the C++ file holds the vectors ({})", theirs.len());
    assert_eq!(ours, theirs, "Rust world.rs != C++ MonkeyVoxelField on the P1 set");
}

#[test]
fn v3_vectors_follow_the_closure_rule_and_are_written() {
    let edits = v3_edits();
    let (text, full, keys) = golden(&edits, true);
    let h_full = chunks_hash(&full.chunks);
    let mut j = MapJournal::default();
    for e in &edits {
        j.insert(*e);
    }
    let (mut sets, mut avg, mut bad) = (0, 0usize, 0);
    for e in &edits {
        let set: Vec<ChunkKey> = j
            .read_closure(&e.touched_chunks())
            .into_iter()
            .filter(|k| full.chunks.contains_key(k))
            .collect();
        let mut w = VoxelWorldData {
            chunks: full.chunks.clone(),
            seeds: full.seeds.clone(),
        };
        w.build(&set, j.edits_for(&set).iter());
        sets += 1;
        avg += set.len();
        if chunks_hash(&w.chunks) != h_full {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "a closure rebuild differed from the full replay");
    let text = format!(
        "# MONKEY voxel golden v3 (benilla-voxel tests/golden_v3.rs, CONTRACT v3 §9.3). Height fn, P1 edits,\n\
         # activation and line format = tools/MonkeyVoxelGolden/golden_p1.cpp; edit lines add `param` (i16).\n\
         # Edits 61-67 flatten (mode 3, op order in world.rs apply_edit_set MODE_FLATTEN), 68 smooth,\n\
         # 69 paint + 70 roughen = no-ops (never activate, like the server). density_hash = world.rs chunks_hash.\n\
         {text}closure_rebuild sets {sets} avg_chunks {:.1} differing_sets {bad}\nactive_chunks {}\n",
        avg as f32 / sets as f32,
        keys.len()
    );
    if let Ok(out) = std::env::var("MONKEY_VOXEL_GOLDEN_OUT") {
        std::fs::write(&out, &text).expect("write the golden file");
        eprintln!("golden: wrote {out}");
    }
}

/// MONKEY (voxel, CONTRACT v3.3): V3 + 14 SOFT-brush edits (`param` bit 15 on modes 0 / 1 / 2:
/// soft lower / raise / smooth, every falloff, hardness 0 / 30 / 50 / 100, partial and full
/// strength, the 0.5 and 16 yd radii, a chunk-corner straddle, a held brush (4 edits on one spot), a
/// hard dig over soft ground, and a robustness param with falloff 3 + hardness 127 + reserved bits).
fn v33_edits() -> Vec<VoxelEditOp> {
    let mut e = v3_edits();
    let n = e.len() as u32;
    let soft = |falloff: u16, hardness: u16| (0x8000u16 | falloff | (hardness << 2)) as i16;
    let extra: [([f32; 3], f32, u8, u8, i16); 14] = [
        ([-8944.0, -64.0, 81.0], 6.0, 1, 51, soft(0, 0)),      // raise the big dig's floor
        ([-8944.0, -64.0, 81.2], 6.0, 1, 51, soft(0, 0)),      // held: 4 edits on one spot
        ([-8944.0, -64.0, 81.4], 6.0, 1, 51, soft(0, 0)),
        ([-8944.0, -64.0, 81.6], 6.0, 1, 51, soft(0, 0)),
        ([-8944.0, -64.0, 82.0], 16.0, 1, 255, soft(1, 50)),   // max radius, chunk corner, linear
        ([-8950.25, -70.5, 81.7], 4.5, 0, 128, soft(2, 0)),    // lower, flat falloff, half
        ([-8939.75, -71.25, 79.5], 0.5, 0, 200, soft(0, 100)), // tiny, hardness 100
        ([-8936.0, -80.0, 84.5], 8.0, 0, 77, soft(0, 30)),     // lower over the +1.5 flatten
        ([-8944.0, -70.0, 82.0], 8.0, 2, 64, soft(0, 0)),      // soft smooth
        ([-8930.1, -59.9, 83.3], 3.0, 2, 255, soft(1, 25)),    // soft smooth, linear, full
        ([-8925.5, -80.25, 90.0], 7.0, 2, 160, soft(2, 50)),   // soft smooth, flat, on the build
        ([-8948.0, -76.0, 72.0], 5.0, 0, 255, 0),              // a HARD dig over soft ground
        ([-8960.0, -48.0, 82.0], 7.5, 1, 90, -1),              // param 0xffff: falloff 3, h 127, reserved
        ([-8912.0, -56.0, 84.0], 4.0, 1, 255, soft(0, 0)),     // fresh ground, full strength
    ];
    for (i, (p, r, m, s, prm)) in extra.iter().enumerate() {
        e.push(op(*p, *r, *m, *s, n + 1 + i as u32, *prm));
    }
    e
}

#[test]
fn v33_soft_vectors_follow_the_closure_rule_and_are_written() {
    let edits = v33_edits();
    assert!(edits[edits.len() - 14..].iter().filter(|e| e.soft().is_some()).count() == 13);
    let (text, full, keys) = golden(&edits, true);
    let h_full = chunks_hash(&full.chunks);
    let mut j = MapJournal::default();
    for e in &edits {
        j.insert(*e);
    }
    let (mut sets, mut avg, mut bad) = (0, 0usize, 0);
    for e in &edits {
        let set: Vec<ChunkKey> = j
            .read_closure(&e.touched_chunks())
            .into_iter()
            .filter(|k| full.chunks.contains_key(k))
            .collect();
        let mut w = VoxelWorldData {
            chunks: full.chunks.clone(),
            seeds: full.seeds.clone(),
        };
        w.build(&set, j.edits_for(&set).iter());
        sets += 1;
        avg += set.len();
        if chunks_hash(&w.chunks) != h_full {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "a closure rebuild differed from the full replay");
    // The soft edits really changed the field (vs the V3 set alone).
    let (_, v3, _) = golden(&v3_edits(), true);
    assert_ne!(chunks_hash(&v3.chunks), h_full);
    let text = format!(
        "# MONKEY voxel golden v3.3 (benilla-voxel tests/golden_v3.rs, CONTRACT v3.3 soft brushes). Height fn,\n\
         # P1 + V3 edits, activation and line format = golden-v3.txt (edit lines: param printed as i16; soft\n\
         # params have bit 15 set, so they print negative: e.g. -32768 = 0x8000 = soft, smooth falloff, hardness 0).\n\
         # Edits 71-84 = the soft set (world.rs apply_edit_set: SoftBrush::weight + the MODE_DIG|BUILD|SMOOTH\n\
         # gather branch); 82 = a hard dig. density_hash = world.rs chunks_hash.\n\
         {text}closure_rebuild sets {sets} avg_chunks {:.1} differing_sets {bad}\nactive_chunks {}\n",
        avg as f32 / sets as f32,
        keys.len()
    );
    if let Ok(out) = std::env::var("MONKEY_VOXEL_GOLDEN33_OUT") {
        std::fs::write(&out, &text).expect("write the golden file");
        eprintln!("golden: wrote {out}");
    }
}

/// MONKEY (voxel, CONTRACT v3.6): V3.3 + 16 SHAPED edits (`param` bits 9-10 on modes 0 / 1: 1 cube,
/// 2 vertical cylinder; 3 reserved = sphere): hard cube / cylinder dig + build, soft cube raise and
/// soft cylinder lower (every falloff, hardness 0 / 40 / 100), a max-radius cube on a chunk corner, a
/// tiny cylinder, a cube deck of a bridge (3 cube builds in a row), a cylinder tunnel (3 cylinder digs
/// through the deck), shape 3 + reserved bits 11-14 (= sphere), and shape bits on a smooth (ignored).
fn v36_edits() -> Vec<VoxelEditOp> {
    let mut e = v33_edits();
    let n = e.len() as u32;
    let sh = |shape: u16| (shape << 9) as i16;
    let soft = |falloff: u16, hardness: u16, shape: u16| {
        (0x8000u16 | falloff | (hardness << 2) | (shape << 9)) as i16
    };
    let extra: [([f32; 3], f32, u8, u8, i16); 16] = [
        ([-8952.0, -60.0, 81.0], 4.0, 0, 255, sh(1)),           // hard cube pit
        ([-8936.5, -66.25, 84.0], 3.0, 1, 255, sh(1)),          // hard cube block
        ([-8946.0, -84.0, 80.5], 5.0, 0, 255, sh(2)),           // hard cylinder well
        ([-8928.0, -72.0, 85.0], 2.5, 1, 255, sh(2)),           // hard cylinder pillar
        ([-8944.0, -64.0, 82.0], 6.0, 1, 128, soft(0, 0, 1)),   // soft cube raise, smooth falloff
        ([-8950.25, -70.5, 81.7], 4.5, 0, 200, soft(1, 40, 2)), // soft cylinder lower, linear h40
        ([-8960.0, -64.0, 82.0], 16.0, 0, 255, sh(1)),          // max-radius cube on a chunk corner
        ([-8939.75, -71.25, 79.5], 0.5, 1, 255, sh(2)),         // tiny cylinder
        ([-8956.0, -56.0, 84.0], 2.0, 1, 255, sh(1)),           // bridge deck (cube) ...
        ([-8955.0, -56.0, 84.0], 2.0, 1, 255, sh(1)),
        ([-8954.0, -56.0, 84.0], 2.0, 1, 255, sh(1)),
        ([-8955.0, -57.5, 83.0], 1.5, 0, 255, sh(2)),           // tunnel (cylinder) through it ...
        ([-8955.0, -56.75, 83.0], 1.5, 0, 255, sh(2)),
        ([-8955.0, -56.0, 83.0], 1.5, 0, 77, soft(2, 100, 2)),  // soft cylinder, flat h100
        ([-8920.0, -96.0, 85.0], 3.0, 1, 255, 0x7e00u16 as i16), // shape 3 + bits 11-14 = sphere
        ([-8944.0, -70.0, 82.0], 6.0, 2, 160, sh(1)),           // shape bits on a hard smooth: ignored
    ];
    for (i, (p, r, m, s, prm)) in extra.iter().enumerate() {
        e.push(op(*p, *r, *m, *s, n + 1 + i as u32, *prm));
    }
    e
}

/// MONKEY (voxel, CONTRACT v3.6): the shape distances and weights (unit level).
#[test]
fn v36_shape_distances_and_weights() {
    // Sphere = the v3.3 expression.
    assert_eq!(shape_dist(SHAPE_SPHERE, 3.0, 4.0, 0.0), 5.0);
    // Cube = Chebyshev: a corner offset is as "near" as the face centre.
    assert_eq!(shape_dist(SHAPE_CUBE, 3.0, -3.0, 3.0), 3.0);
    assert_eq!(shape_dist(SHAPE_CUBE, -1.0, 2.5, 0.5), 2.5);
    // Cylinder = max(horizontal radius, |dz|): flat caps, round sides.
    assert_eq!(shape_dist(SHAPE_CYLINDER, 3.0, 4.0, 1.0), 5.0);
    assert_eq!(shape_dist(SHAPE_CYLINDER, 0.0, 1.0, -4.0), 4.0);
    // The param field: bits 9-10, 3 = sphere, other bits ignored.
    assert_eq!(shape_of_param(0x0200), SHAPE_CUBE);
    assert_eq!(shape_of_param(0x8000 | 0x0400 | 0x7), SHAPE_CYLINDER);
    assert_eq!(shape_of_param(0x0600), SHAPE_SPHERE);
    assert_eq!(shape_of_param(0xffff), SHAPE_SPHERE);
    assert_eq!(shape_of_param(0x7800), SHAPE_SPHERE);
    let e = |mode: u8, param: u16| VoxelEditOp {
        mode,
        param,
        radius: 4.0,
        ..Default::default()
    };
    assert_eq!(e(0, 0x0200).shape(), SHAPE_CUBE);
    assert_eq!(e(1, 0x8000 | 0x0400).shape(), SHAPE_CYLINDER);
    assert_eq!(e(2, 0x0200).shape(), SHAPE_SPHERE, "smooth ignores the shape");
    assert_eq!(e(3, 0x0200).shape(), SHAPE_SPHERE, "flatten's param is its plane");
    // A soft cube's corner gets the full weight inside the hardness core; a sphere's does not.
    let b = SoftBrush {
        falloff: FALLOFF_LINEAR,
        hardness: 50,
    };
    let r = 8.0;
    assert_eq!(b.weight(shape_dist(SHAPE_CUBE, 3.9, 3.9, 3.9), r), 1.0);
    assert!(b.weight(shape_dist(SHAPE_SPHERE, 3.9, 3.9, 3.9), r) < 1.0);
    assert_eq!(b.weight(shape_dist(SHAPE_CUBE, 8.0, 0.0, 0.0), r), 0.0);
    // Hard: a cube carves its corner, a sphere of the same radius does not.
    let mut field: HashMap<ChunkKey, Box<[i8]>> = HashMap::new();
    field.insert([0, 0, 0], vec![-64i8; OWNED].into_boxed_slice());
    let air = |_: [i32; 3]| AIR;
    let dig = |param: u16| VoxelEditOp {
        pos: [8.0, 8.0, 8.0],
        radius: 4.0,
        mode: MODE_DIG,
        param,
        ..Default::default()
    };
    let at = |x: usize, y: usize, z: usize| x + 32 * (y + 32 * z);
    let mut cube = field.clone();
    apply_edit_set(&dig(0x0200), &mut cube, &air);
    let mut sph = field.clone();
    apply_edit_set(&dig(0), &mut sph, &air);
    // Lattice 16 + 7 on every axis = 3.5 yd off the centre: inside the cube, outside the sphere.
    let corner = at(23, 23, 23);
    assert!(cube[&[0, 0, 0]][corner] > 0, "cube corner carved: {}", cube[&[0, 0, 0]][corner]);
    assert!(sph[&[0, 0, 0]][corner] < 0, "sphere corner solid: {}", sph[&[0, 0, 0]][corner]);
    // A cylinder carves straight down to its flat floor: 3.5 yd off on x, 3.5 yd down.
    let mut cyl = field.clone();
    apply_edit_set(&dig(0x0400), &mut cyl, &air);
    assert!(cyl[&[0, 0, 0]][at(23, 16, 9)] > 0);
    assert!(sph[&[0, 0, 0]][at(23, 16, 9)] < 0);
}

#[test]
fn v36_shape_vectors_follow_the_closure_rule_and_are_written() {
    let edits = v36_edits();
    let shaped = edits[edits.len() - 16..]
        .iter()
        .filter(|e| e.shape() != SHAPE_SPHERE)
        .count();
    assert_eq!(shaped, 14);
    let (text, full, keys) = golden(&edits, true);
    let h_full = chunks_hash(&full.chunks);
    let mut j = MapJournal::default();
    for e in &edits {
        j.insert(*e);
    }
    let (mut sets, mut avg, mut bad) = (0, 0usize, 0);
    for e in &edits {
        let set: Vec<ChunkKey> = j
            .read_closure(&e.touched_chunks())
            .into_iter()
            .filter(|k| full.chunks.contains_key(k))
            .collect();
        let mut w = VoxelWorldData {
            chunks: full.chunks.clone(),
            seeds: full.seeds.clone(),
        };
        w.build(&set, j.edits_for(&set).iter());
        sets += 1;
        avg += set.len();
        if chunks_hash(&w.chunks) != h_full {
            bad += 1;
        }
    }
    assert_eq!(bad, 0, "a closure rebuild differed from the full replay");
    // The shaped edits changed the field vs the v3.3 set, and a shape is not its sphere.
    let (_, v33, _) = golden(&v33_edits(), true);
    assert_ne!(chunks_hash(&v33.chunks), h_full);
    let n = edits.len();
    let mut spheres = edits.clone();
    for e in spheres.iter_mut().skip(n - 16) {
        e.param &= !SHAPE_MASK;
    }
    let (_, sph, _) = golden(&spheres, true);
    assert_ne!(chunks_hash(&sph.chunks), h_full, "the shape bits change the density");
    let text = format!(
        "# MONKEY voxel golden v3.6 (benilla-voxel tests/golden_v3.rs, CONTRACT v3.6 brush shapes). Height fn,\n\
         # P1 + V3 + V3.3 edits, activation and line format = golden-v3.3.txt (edit lines: param printed as i16).\n\
         # Edits 85-100 = the shape set: param bits 9-10 = shape (1 cube, 2 vertical cylinder, 3 = sphere) on\n\
         # modes 0 / 1, hard and soft (world.rs shape_dist + apply_edit_set); 99 = shape 3 + reserved bits 11-14\n\
         # (a sphere); 100 = shape bits on a hard smooth (ignored). density_hash = world.rs chunks_hash.\n\
         {text}closure_rebuild sets {sets} avg_chunks {:.1} differing_sets {bad}\nactive_chunks {}\n",
        avg as f32 / sets as f32,
        keys.len()
    );
    if let Ok(out) = std::env::var("MONKEY_VOXEL_GOLDEN36_OUT") {
        std::fs::write(&out, &text).expect("write the golden file");
        eprintln!("golden: wrote {out}");
    }
}

// MONKEY (voxel): the world voxel layer surface. It wears the REAL ADT tileset of the ground it replaces and
// is lit by the SAME shared global light as `terrain.wgsl`, so a voxel top pixel-matches the real
// ground beside it at any time of day:
//   · TOPS (slope up to ~45°) use the terrain's own planar mapping: layer UV = chunk UV × 8 (u east,
//     v south, phase locked to the MCNK grid), the per-chunk alpha map at the chunk UV with the same
//     half-texel inset, the same `mix` chain, the same MCSH bake, the same sampler (one repeating
//     sampler for layers + alpha + shadow, exactly as terrain binds 105).
//   · MONKEY (voxel): one material per ADT TILE (the world voxel layer's chunks wear the tile under
//     them); the MCNK chunk is chosen PER FRAGMENT from the tile's full 16×16 table, clamped to the
//     tile (across a tile border the nearest in-tile chunk is used — a known limit).
//   · WALLS / cliffs use a sharp triplanar (|n|^6) of the chunk's most rock/dirt-like layer (lowest
//     saturation of the layer's mean colour) with a macro-scale second tap + a low-frequency tone
//     variation so a big wall does not read as a stamp; overhang UNDERSIDES use the same layer
//     projected from below.
//   · LIGHT: `terrainp_s` — ambient + sun·N·L (+ the exterior point lights, picked by the MCNK cell
//     box like terrain), realtime sun/moon shadow, MCSH `0.3·s + 0.7`, masked sun sheen, wet, fog.
// Until the tile is resident the albedo falls back to a procedural grass/rock/dirt slope-blend
// (layers 1/2/3 of a startup-built array); until the shared light buffer is bound the light falls
// back to a baked sun (`misc.z`). Not torch-shadowed (the night torch cube maps are not bound here).

#import bevy_pbr::{
    forward_io::VertexOutput,
    mesh_view_bindings::view,
}
#import benilla::shadow_hook
#import benilla::moonlight_hook
#import benilla::monkey_frame
#import benilla::fog_hook
#import benilla::wet_hook

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var layer_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var layer_samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var alpha_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var shadow_array: texture_2d_array<f32>;

// Packed uniform at binding 104; MUST match the Rust `MonkeyVoxelParams` (ShaderType) field order.
struct MonkeyVoxelParams {
    // The tile's 16×16 chunks' 4 layer-array indices, row-major (row = south step, col = east step).
    layers: array<vec4<f32>, 256>,
    // x = alpha-array index; y = shadow-array index (< 0 = no MCSH); zw unused.
    aux: array<vec4<f32>, 256>,
    // x = WoW X (north) of the tile's NW corner; y = its WoW Y (west); z = chunk size (yd);
    // w = layer repeats per chunk (TERRAIN_LAYER_TILES = 8).
    grid: vec4<f32>,
    // x = procedural tiles per yard; y = has_real tileset (>= 0.5); z = has shared light (>= 0.5).
    misc: vec4<f32>,
};
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var<uniform> vp: MonkeyVoxelParams;

// The shared global light — COPIED from terrain.wgsl's `WowLight` (the row layout of
// `lighting::global_light`; keep in sync).
struct WowLight {
    light_ambient: vec4<f32>,
    light_diffuse: vec4<f32>,
    light_sun: vec4<f32>,
    light_spec: vec4<f32>,
    fog_color: vec4<f32>,
    fog_params: vec4<f32>,
    _sh: array<vec4<f32>, 6>,
    sh_c16: vec4<f32>,
    _water: array<vec4<f32>, 4>,
    grade: vec4<f32>,
    _wmo_fog: array<vec4<f32>, 2>,
    point_count: vec4<f32>,
    points: array<vec4<f32>, 512>,
    monkey: monkey_frame::MonkeyFrame,
    shelter_hdr: vec4<f32>,
    shelter_cfg: vec4<f32>,
    shelter: array<u32, 16384>,
};
@group(#{MATERIAL_BIND_GROUP}) @binding(90) var<storage, read> wow_light: WowLight;

// terrain.wgsl's character-shadow floor.
const SHADOW_SUN_FLOOR: f32 = 0.45;
// terrain.wgsl's point-light candidacy half-width and MCNK cell half-extent (keep in sync).
const TERRAIN_REACH: f32 = 33.570166;
const MCNK_CELL_HALF: f32 = 533.33333 / 32.0;
const EXT_SEL_K: u32 = 12u;
const EXT_SEL_EMPTY: u32 = 255u;

fn ext_sel_get(sel: vec4<u32>, s: u32) -> u32 {
    return (sel[s >> 2u] >> (8u * (s & 3u))) & 255u;
}

// terrain.wgsl's `point_light_pick` (exterior lane, Chebyshev candidacy, ranked from the cell box).
// Evaluated per fragment here (the voxel has no custom vertex stage); the anchor is the MCNK cell,
// so the selection is constant per cell exactly as on the real ground.
fn point_light_pick(anchor: vec3<f32>, box: f32) -> vec4<u32> {
    let count = u32(wow_light.point_count.x);
    var sel = array<u32, EXT_SEL_K>();
    var sd = array<f32, EXT_SEL_K>();
    for (var s = 0u; s < EXT_SEL_K; s = s + 1u) {
        sd[s] = 1e30;
    }
    for (var i = 0u; i < count; i = i + 1u) {
        if (wow_light.points[2u * i + 1u].w > 0.5) {
            continue;
        }
        let dv = wow_light.points[2u * i].xyz - anchor;
        if (max(abs(dv.x), abs(dv.z)) > TERRAIN_REACH) {
            continue;
        }
        let e = max(abs(dv.xz) - vec2<f32>(box), vec2<f32>(0.0));
        let d2 = dot(e, e) + dv.y * dv.y;
        if (d2 >= sd[EXT_SEL_K - 1u]) {
            continue;
        }
        var r = EXT_SEL_K;
        for (var s = 0u; s < EXT_SEL_K; s = s + 1u) {
            if (d2 < sd[s]) {
                r = s;
                break;
            }
        }
        if (r < EXT_SEL_K) {
            for (var s = EXT_SEL_K - 1u; s > r; s = s - 1u) {
                sd[s] = sd[s - 1u];
                sel[s] = sel[s - 1u];
            }
            sd[r] = d2;
            sel[r] = i;
        }
    }
    var packed = vec4<u32>(0u, 0u, 0u, 0u);
    for (var s = 0u; s < EXT_SEL_K; s = s + 1u) {
        let idx = select(EXT_SEL_EMPTY, sel[s], sd[s] <= 9.9e29);
        packed[s >> 2u] |= idx << (8u * (s & 3u));
    }
    return packed;
}

fn point_light_eval(sel: vec4<u32>, P: vec3<f32>, N: vec3<f32>) -> vec3<f32> {
    var sum = vec3<f32>(0.0);
    for (var s = 0u; s < EXT_SEL_K; s = s + 1u) {
        let idx = ext_sel_get(sel, s);
        if (idx == EXT_SEL_EMPTY) {
            break;
        }
        let to_light = wow_light.points[2u * idx].xyz - P;
        let d = length(to_light);
        let atten = 1.0 / (0.7 * d + 0.03 * d * d);
        let nl = max(dot(N, to_light / max(d, 1e-4)), 0.0);
        sum += wow_light.points[2u * idx + 1u].rgb * (atten * nl);
    }
    return sum;
}

fn mcnk_cell_anchor(P: vec3<f32>) -> vec3<f32> {
    let cell = 533.33333 / 16.0;
    let half = 32.0 * 533.33333;
    let ix = floor((half + P.x) / cell);
    let iz = floor((half + P.z) / cell);
    return vec3<f32>((ix + 0.5) * cell - half, P.y, (iz + 0.5) * cell - half);
}

// Smooth 3-D value noise (0..1) for the walls' low-frequency tone variation.
fn monkey_voxel_hash3(p: vec3<f32>) -> f32 {
    return fract(sin(dot(p, vec3<f32>(127.1, 311.7, 74.7))) * 43758.547);
}
fn monkey_voxel_noise3(p: vec3<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = mix(monkey_voxel_hash3(i), monkey_voxel_hash3(i + vec3<f32>(1.0, 0.0, 0.0)), u.x);
    let b = mix(monkey_voxel_hash3(i + vec3<f32>(0.0, 1.0, 0.0)), monkey_voxel_hash3(i + vec3<f32>(1.0, 1.0, 0.0)), u.x);
    let c = mix(monkey_voxel_hash3(i + vec3<f32>(0.0, 0.0, 1.0)), monkey_voxel_hash3(i + vec3<f32>(1.0, 0.0, 1.0)), u.x);
    let d = mix(monkey_voxel_hash3(i + vec3<f32>(0.0, 1.0, 1.0)), monkey_voxel_hash3(i + vec3<f32>(1.0, 1.0, 1.0)), u.x);
    return mix(mix(a, b, u.y), mix(c, d, u.y), u.z);
}

// How rock/dirt-like a layer is: low saturation of its mean colour (its last mip). Grass is the most
// saturated ground texture, rock the least, so the minimum picks rock > dirt > grass for a wall.
fn monkey_voxel_saturation(layer: i32, level: f32) -> f32 {
    let m = textureSampleLevel(layer_array, layer_samp, vec2<f32>(0.5), layer, level).rgb;
    let hi = max(m.r, max(m.g, m.b));
    let lo = min(m.r, min(m.g, m.b));
    return (hi - lo) / max(hi, 1e-3);
}

// One wall tap with a macro-scale second tap blended in, to break obvious repetition on a big face.
fn monkey_voxel_wall_tap(layer: i32, uv: vec2<f32>) -> vec4<f32> {
    let a = textureSampleBias(layer_array, layer_samp, uv, layer, view.mip_bias);
    let b = textureSampleBias(layer_array, layer_samp, uv * 0.37 + vec2<f32>(0.31, 0.57), layer, view.mip_bias);
    return mix(a, b, 0.35);
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // The far-clip wall, as terrain.
    if (vp.misc.z >= 0.5 && wow_light.fog_params.w > 0.0) {
        let clip_z = -(view.view_from_world * vec4<f32>(in.world_position.xyz, 1.0)).z;
        if (clip_z > wow_light.fog_params.w) {
            discard;
        }
    }

    let n = normalize(in.world_normal);
    let p = in.world_position.xyz;
    let an = abs(n);

    // Mapping weights. Tops up to ~45° take the ground's own planar mapping (as the real terrain,
    // which never triplanars); steeper faces go to a SHARP triplanar so no projection ghosts over
    // another; faces pointing down use the wall layer projected from below.
    let top_w = smoothstep(0.5, 0.7, n.y);
    let under_w = smoothstep(0.5, 0.7, -n.y);
    let side_w = max(1.0 - top_w - under_w, 0.0);
    let sx = pow(an.x, 6.0);
    let sz = pow(an.z, 6.0);
    let side_x = side_w * sx / max(sx + sz, 1e-6);
    let side_z = side_w - side_x;

    var color: vec3<f32>;
    var specmask = 0.0;
    var shadow_lit = 1.0;

    if (vp.misc.y >= 0.5) {
        // This fragment in the tile's 16×16 chunk grid: (east, south) in chunks from the tile's NW
        // corner (WoW X = north = -bevy.z, WoW Y = west = -bevy.x).
        let cs = vp.grid.z;
        let g = vec2<f32>((vp.grid.y + p.x) / cs, (vp.grid.x + p.z) / cs);
        let cell = clamp(floor(g), vec2<f32>(0.0), vec2<f32>(15.0));
        let ci = i32(cell.y) * 16 + i32(cell.x);
        let li = vec4<i32>(round(vp.layers[ci]));
        let aux = vp.aux[ci];
        // The chunk UV (0..1 over the chunk, clamped when the footprint leaves the grid) — the
        // terrain's `in.uv` — and its half-texel inset alpha/shadow UV.
        let local = clamp(g - cell, vec2<f32>(0.0), vec2<f32>(1.0));
        let auv = clamp(local, vec2<f32>(0.5 / 64.0), vec2<f32>(1.0 - 0.5 / 64.0));
        // Layer UV: chunk UV × 8, continuous across the grid (the grid origin is a chunk corner, so
        // the phase is the terrain's own).
        let tiled = g * vp.grid.w;

        let s0 = textureSampleBias(layer_array, layer_samp, tiled, li.x, view.mip_bias);
        let s1 = textureSampleBias(layer_array, layer_samp, tiled, li.y, view.mip_bias);
        let s2 = textureSampleBias(layer_array, layer_samp, tiled, li.z, view.mip_bias);
        let s3 = textureSampleBias(layer_array, layer_samp, tiled, li.w, view.mip_bias);
        let a = textureSample(alpha_array, layer_samp, auv, i32(round(aux.x)));
        var top = s0;
        top = mix(top, s1, a.r);
        top = mix(top, s2, a.g);
        top = mix(top, s3, a.b);

        // The chunk's most rock/dirt-like layer for walls and undersides.
        let level = f32(textureNumLevels(layer_array) - 1u);
        let q0 = monkey_voxel_saturation(li.x, level);
        let q1 = monkey_voxel_saturation(li.y, level);
        let q2 = monkey_voxel_saturation(li.z, level);
        let q3 = monkey_voxel_saturation(li.w, level);
        var wl = li.x;
        var wq = q0;
        if (q1 < wq) { wl = li.y; wq = q1; }
        if (q2 < wq) { wl = li.z; wq = q2; }
        if (q3 < wq) { wl = li.w; wq = q3; }

        // Wall projections at the ground's texel density (repeats per yard = 8 / chunk size).
        let k = vp.grid.w / cs;
        let wx = monkey_voxel_wall_tap(wl, vec2<f32>(p.z, -p.y) * k);
        let wz = monkey_voxel_wall_tap(wl, vec2<f32>(p.x, -p.y) * k);
        let wy = monkey_voxel_wall_tap(wl, vec2<f32>(p.x, p.z) * k);
        var wall = wx * side_x + wz * side_z + wy * under_w;
        // Low-frequency tone variation (≈7 yd and ≈2.5 yd scales), walls only — the tops stay
        // byte-identical to the ground beside them.
        let tone = 0.86 + 0.2 * monkey_voxel_noise3(p * 0.14) + 0.08 * (monkey_voxel_noise3(p * 0.4) - 0.5);
        wall = vec4<f32>(wall.rgb * tone, wall.a);

        let mixed = top * top_w + wall;
        color = mixed.rgb;
        specmask = mixed.a;

        // MCSH baked shadow (255 shadowed, 0 lit; no map ⇒ lit), sampled like terrain.
        let mcsh = textureSample(shadow_array, layer_samp, auv, max(i32(round(aux.y)), 0)).r;
        shadow_lit = select(1.0, 1.0 - mcsh, aux.y >= 0.0);
    } else {
        // PROCEDURAL fallback (tile not resident): grass(1)/rock(2)/dirt(3) slope-blend.
        let w = an / max(dot(an, vec3<f32>(1.0)), 1e-5);
        let q = p * vp.misc.x;
        let slope = clamp(n.y, 0.0, 1.0);
        let rock_to_dirt = smoothstep(0.45, 0.70, slope);
        let dirt_to_grass = smoothstep(0.80, 0.94, slope);
        let grass = textureSample(layer_array, layer_samp, q.zy, 1).rgb * w.x
            + textureSample(layer_array, layer_samp, q.xz, 1).rgb * w.y
            + textureSample(layer_array, layer_samp, q.xy, 1).rgb * w.z;
        let rock = textureSample(layer_array, layer_samp, q.zy, 2).rgb * w.x
            + textureSample(layer_array, layer_samp, q.xz, 2).rgb * w.y
            + textureSample(layer_array, layer_samp, q.xy, 2).rgb * w.z;
        let dirt = textureSample(layer_array, layer_samp, q.zy, 3).rgb * w.x
            + textureSample(layer_array, layer_samp, q.xz, 3).rgb * w.y
            + textureSample(layer_array, layer_samp, q.xy, 3).rgb * w.z;
        color = mix(mix(rock, dirt, rock_to_dirt), grass, dirt_to_grass);
    }

    if (vp.misc.z < 0.5) {
        // No shared light bound yet: the old baked sun + ambient.
        let sun = normalize(vec3<f32>(0.35, 0.8, 0.5));
        let light = 0.45 + 0.55 * (dot(n, sun) * 0.5 + 0.5);
        return vec4<f32>(clamp(color * light, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }

    // ---- terrain.wgsl's lighting, per fragment ----
    let l = -normalize(wow_light.light_sun.xyz);
    let to_eye = normalize(view.world_position.xyz - p);
    let ndoth = max(dot(n, normalize(l + to_eye)), 0.0);
    let specular = clamp(wow_light.light_spec.rgb * pow(ndoth, wow_light.light_spec.w),
        vec3<f32>(0.0), vec3<f32>(1.0));
    let ndotl = max(dot(n, l), 0.0);
    let base_lit = wow_light.light_ambient.rgb + wow_light.light_diffuse.rgb * ndotl;
    let sel = point_light_pick(mcnk_cell_anchor(p), MCNK_CELL_HALF);
    let points = point_light_eval(sel, p, n);
    var primary = clamp(base_lit, vec3<f32>(0.0), vec3<f32>(1.0));
    if (ext_sel_get(sel, 0u) != EXT_SEL_EMPTY) {
        primary = clamp(base_lit + points, vec3<f32>(0.0), vec3<f32>(1.0));
    }

    let view_z = (view.view_from_world * in.world_position).z;
    let cam_dist = distance(p, view.world_position.xyz);
    let sun_shadow_strength = shadow_hook::sun_shadow_w(wow_light.fog_params.z);
    let moon_shadow_strength = shadow_hook::moon_shadow_w(wow_light.fog_params.z);
    let shadow_terms = shadow_hook::realtime_shadow_moonlit(
        in.world_position,
        n,
        view_z,
        cam_dist,
        wow_light._wmo_fog[1].z,
        sun_shadow_strength,
        moon_shadow_strength,
        wow_light.monkey.misc.w,
    );
    let world_shadow = shadow_terms.x;
    if (shadow_terms.y < 1.0) {
        primary = clamp(base_lit * shadow_terms.y + points, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    if (wow_light.monkey.moon.w > 0.0) {
        primary = clamp(
            primary + moonlight_hook::moon_light(n, wow_light.monkey.moon) * shadow_terms.z,
            vec3<f32>(0.0),
            vec3<f32>(1.0),
        );
    }

    let world_shadow_lane = wow_light.sh_c16.w > 0.5;
    let mcsh_suppress = select(0.0, sun_shadow_strength, world_shadow_lane);
    let shadow_lit_eff = mix(shadow_lit, 1.0, mcsh_suppress);
    let spec_gate = min(shadow_lit, world_shadow);
    let character_shadow_term = mix(SHADOW_SUN_FLOOR, 1.0, world_shadow);

    var wet_open = 1.0;
    if (wow_light.monkey.wet_a.y > 0.0) {
        let wet_st = wet_hook::shelter_taps(p, wow_light.shelter_hdr, wow_light.shelter_cfg);
        if (wet_st.live > 0.0) {
            wet_open = 1.0 - wet_hook::shelter_amount(wet_st, wow_light.shelter[wet_st.idx.x],
                wow_light.shelter[wet_st.idx.y], wow_light.shelter[wet_st.idx.z],
                wow_light.shelter[wet_st.idx.w], p.y, wow_light.shelter_cfg);
        }
    }
    let wet = wet_hook::wet_puddles(wet_hook::wet_surface(color, n, p, wet_open, wow_light.monkey),
        n, p, wet_open, wow_light.monkey);
    color = wet.albedo;
    let diffuse_term = color * primary * (0.3 * shadow_lit_eff + 0.7) * character_shadow_term;
    let spec_term = specular * specmask * spec_gate;
    var tuned = clamp(diffuse_term + spec_term, vec3<f32>(0.0), vec3<f32>(1.0));
    if (wet.boost > 0.0) {
        tuned = clamp(tuned + spec_term * wet.boost + wet_hook::wet_sheen(wet.boost, n, to_eye, l,
            wow_light.light_diffuse.rgb, wow_light.fog_color.rgb, spec_gate), vec3<f32>(0.0), vec3<f32>(1.0));
    }

    if (wow_light.fog_color.w > 0.5) {
        let eye_z = -(view.view_from_world * vec4<f32>(p, 1.0)).z;
        tuned = fog_hook::apply_fog(tuned, wow_light.fog_color.xyz, wow_light.fog_params.xy, eye_z,
            p, view.world_position, true, wow_light.monkey);
    }
    // Raw gamma out, as terrain.
    return vec4<f32>(tuned, 1.0);
}

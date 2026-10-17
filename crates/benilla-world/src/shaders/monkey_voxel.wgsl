// MONKEY voxel build site: wears the REAL ADT tileset of the ground it sits on. The covering tile's
// `layer_array` (a texture_2d_array of the ground BLPs) is triplanar-sampled — three world-axis
// projections weighted by |normal|, so the sculpted overhangs/cliffs never smear — and the four
// layers are blended by the covering chunk's alpha map, exactly as the real terrain does. Until the
// tile is resident it falls back to a procedural grass/rock/dirt slope-blend (layers 1/2/3 of the
// startup-built array). Unlit with its own baked sun + ambient, because benilla's world light blacks
// out lit materials.

#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var layer_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var layer_samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var alpha_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var alpha_samp: sampler;

// Packed uniform at binding 104; the field order MUST match the Rust `MonkeyVoxelExt` uniforms.
struct MonkeyVoxelParams {
    layers: vec4<f32>, // the covering chunk's 4 layer-array indices (real path)
    chunk: vec4<f32>,  // x = alpha-array index; y = chunk NW north (wow x); z = chunk NW west (wow y); w = chunk size (yd)
    misc: vec4<f32>,   // x = tiles per world yard; y = has_real (>=0.5 real, else procedural); zw unused
};
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var<uniform> vp: MonkeyVoxelParams;

// Triplanar-sample one array layer at world position `p` (yards), the three projections weighted by
// `w` (= |normal| normalised). `tiles` is repeats per yard, matching the ground's texel scale.
fn monkey_voxel_triplanar(layer: i32, p: vec3<f32>, w: vec3<f32>, tiles: f32) -> vec3<f32> {
    let q = p * tiles;
    let x = textureSample(layer_array, layer_samp, q.zy, layer).rgb;
    let y = textureSample(layer_array, layer_samp, q.xz, layer).rgb;
    let z = textureSample(layer_array, layer_samp, q.xy, layer).rgb;
    return x * w.x + y * w.y + z * w.z;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let n = normalize(in.world_normal);
    let w = abs(n) / max(dot(abs(n), vec3<f32>(1.0)), 1e-5);
    let p = in.world_position.xyz;
    let tiles = vp.misc.x;

    var base: vec3<f32>;
    if (vp.misc.y >= 0.5) {
        // REAL tileset: the covering chunk's four layers, blended by its alpha map.
        let li = vec4<i32>(round(vp.layers));
        let s0 = monkey_voxel_triplanar(li.x, p, w, tiles);
        let s1 = monkey_voxel_triplanar(li.y, p, w, tiles);
        let s2 = monkey_voxel_triplanar(li.z, p, w, tiles);
        let s3 = monkey_voxel_triplanar(li.w, p, w, tiles);
        // This fragment's world XZ in WoW (north = -bevy.z, west = -bevy.x), mapped into the covering
        // chunk's 0..1 alpha UV from its NW corner (u = east, v = south — the ADT chunk UV order).
        let wx = -p.z;
        let wy = -p.x;
        let south = (vp.chunk.y - wx) / vp.chunk.w;
        let east = (vp.chunk.z - wy) / vp.chunk.w;
        // Half-texel inset so the clamp sampler never wraps a 64-texel map at the chunk edge.
        let auv = clamp(vec2<f32>(east, south), vec2<f32>(0.5 / 64.0), vec2<f32>(1.0 - 0.5 / 64.0));
        let a = textureSample(alpha_array, alpha_samp, auv, i32(round(vp.chunk.x)));
        base = s0;
        base = mix(base, s1, a.r);
        base = mix(base, s2, a.g);
        base = mix(base, s3, a.b);
    } else {
        // PROCEDURAL fallback (tile not resident): grass(1)/rock(2)/dirt(3) slope-blend, as before.
        let slope = clamp(n.y, 0.0, 1.0);
        let rock_to_dirt = smoothstep(0.45, 0.70, slope);
        let dirt_to_grass = smoothstep(0.80, 0.94, slope);
        let grass = monkey_voxel_triplanar(1, p, w, tiles);
        let rock = monkey_voxel_triplanar(2, p, w, tiles);
        let dirt = monkey_voxel_triplanar(3, p, w, tiles);
        base = mix(mix(rock, dirt, rock_to_dirt), grass, dirt_to_grass);
    }

    // Baked shading: benilla's custom world light blacks out lit materials, so the fragment supplies
    // its own sun + ambient (the same law the procedural material used).
    let h = sin(dot(p, vec3<f32>(12.9898, 51.233, 78.233))) * 43758.547;
    let mottle = 1.0 + (fract(h) - 0.5) * 0.12;
    let sun = normalize(vec3<f32>(0.35, 0.8, 0.5));
    let ndl = dot(n, sun);
    let light = (0.45 + 0.55 * (ndl * 0.5 + 0.5)) * mottle;
    return vec4<f32>(clamp(base * light, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

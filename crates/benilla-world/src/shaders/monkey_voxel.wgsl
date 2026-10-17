// MONKEY voxel build site: three periodic tiles projected from world position, then slope-blended.
// The fragment supplies its own sun and ambient because benilla's world light is custom.

#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var grass: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var grass_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var rock: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var rock_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var dirt: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(105) var dirt_sampler: sampler;

fn triplanar(tex: texture_2d<f32>, samp: sampler, p: vec3<f32>, weights: vec3<f32>) -> vec3<f32> {
    // One repeat per two world yards, giving the build site readable close-up detail.
    let q = p * 0.5;
    let x = textureSample(tex, samp, q.zy).rgb;
    let y = textureSample(tex, samp, q.xz).rgb;
    let z = textureSample(tex, samp, q.xy).rgb;
    return x * weights.x + y * weights.y + z * weights.z;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let n = normalize(in.world_normal);
    let weights = abs(n) / max(dot(abs(n), vec3<f32>(1.0)), 1e-5);
    let p = in.world_position.xyz;
    let slope = clamp(n.y, 0.0, 1.0);
    let rock_to_dirt = smoothstep(0.45, 0.70, slope);
    let dirt_to_grass = smoothstep(0.80, 0.94, slope);
    let rock_rgb = triplanar(rock, rock_sampler, p, weights);
    let dirt_rgb = triplanar(dirt, dirt_sampler, p, weights);
    let grass_rgb = triplanar(grass, grass_sampler, p, weights);
    let base = mix(mix(rock_rgb, dirt_rgb, rock_to_dirt), grass_rgb, dirt_to_grass);

    let h = sin(dot(p, vec3<f32>(12.9898, 51.233, 78.233))) * 43758.547;
    let mottle = 1.0 + (fract(h) - 0.5) * 0.12;
    let sun = normalize(vec3<f32>(0.35, 0.8, 0.5));
    let ndl = dot(n, sun);
    let light = (0.45 + 0.55 * (ndl * 0.5 + 0.5)) * mottle;
    return vec4<f32>(clamp(base * light, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

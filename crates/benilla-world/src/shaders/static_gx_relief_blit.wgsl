// MONKEY (model relief): resample one offline relief map (any size, its own full box mip chain,
// Rgba8Unorm linear) into ONE mip of ONE layer of the retained pass's fixed-size relief array
// (`static_gx/relief.rs`, `RELIEF_POOL_SIZE` square, Rgba8Unorm). A GPU copy needs equal sizes, and
// the maps keep their native size, so each destination mip is drawn by a fullscreen triangle that
// reads the source at the matching level: at a power-of-two size that is a texel-centre tap of
// the source's own box-filtered level (exact), above the source's top level a bilinear upscale.
// Linear data in, linear data out: no sRGB on either side.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;

struct BlitMip {
    // x = this destination mip's edge in texels, y = its mip index.
    mip: vec4<f32>,
}
@group(1) @binding(0) var<uniform> blit: BlitMip;

@vertex
fn vertex(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // The fullscreen triangle: (-1,-1), (3,-1), (-1,3).
    let xy = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    return vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fragment(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    // Framebuffer coordinates are top-left-origin, as texture uvs are.
    let uv = pos.xy / blit.mip.x;
    let dims = vec2<f32>(textureDimensions(src, 0));
    // The source level whose size matches this destination mip (`log2(src / dst)`), never below 0.
    let lod = max(log2(max(dims.x, dims.y) / blit.mip.x), 0.0);
    return textureSampleLevel(src, src_samp, uv, lod);
}

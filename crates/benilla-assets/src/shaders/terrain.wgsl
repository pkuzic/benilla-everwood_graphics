// Terrain splat shader: a custom vertex and fragment stage on Bevy's StandardMaterial. Blends up to
// four tiled layers by a per-chunk alpha map, lit as the reference's fixed-function terrain:
//   diffuse  = clamp(ambient (row 1) + diffuse (row 0)·max(N·L, 0) + Σ point·att·N·L), evaluated
//     sun/ambient interpolates from the vertices; local points evaluate per pixel, then the sum
//     clamps and modulates the texture. Deviation, because sparse MCNK geometry must not miss candle pools;
//   specular = clamp(row 9 · max(N·H, 0)^20) per vertex (local viewer), times the sheen mask (the
//     `_s` texture's per-texel alpha, blended like the colour), added after the modulate.
// Evaluated per pixel, or unmasked, the near-white row 9 would wash lit ground to cream.
//
// Bevy's `VertexOutput` has no slot for the interpolated specular, so the vertex transform and IO
// struct are our own and `main_pass_post_lighting_processing` is dropped; fog is in-shader.

#import bevy_pbr::{
    mesh_functions,
    forward_io::Vertex,
    view_transformations::position_world_to_clip,
    mesh_view_bindings::{lights, view},
    shadows,
}
// MONKEY (shadow hook): the realtime directional-shadow term (fetch + edge/night fade) lives here.
#import benilla::shadow_hook
// GFX (moonlight): the additive moon term.
#import benilla::moonlight_hook
// MONKEY (p0 MonkeyFrame): the programme block's struct, mirrored after the point table.
#import benilla::monkey_frame
// MONKEY (p0 fog hook): the one distance-fog law every receiver calls.
#import benilla::fog_hook
// MONKEY (wet): rain on surfaces (wet_hook.wgsl).
#import benilla::wet_hook

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var layer_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var alpha_array: texture_2d_array<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(105) var splat_samp: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(110) var shadow_array: texture_2d_array<f32>;
// MONKEY (ground relief): the tile's relief maps, indexed like `layer_array` (RGBA linear: normal
// X (+u) / Y (+v) as `(byte-128)/127`, B cavity, A height); sampled through `splat_samp`.
@group(#{MATERIAL_BIND_GROUP}) @binding(107) var relief_array: texture_2d_array<f32>;

// A fully covered character shadow retains 45% of the authored terrain colour.
const SHADOW_SUN_FLOOR: f32 = 0.45;

// Per-tile Vec4 uniforms packed into ONE buffer (binding 106) — the field order here MUST match the
// Rust `TerrainExtension` struct. Light + fog live in the shared global-light storage buffer (below);
// what's left is just the per-tile layer tiling factor.
//   params — x = layer tiling factor.
//   MONKEY (ground relief): y = relief mode (`groundRelief`: 0 off, 1 normals, 2 +POM,
//   3 +POM self-shadow, 4 debug height, 5 debug normal), z = this tile's relief layer count (0 = the
//   tile has no relief map), w = POM depth in tiled-UV units (`groundReliefDepth`).
//   MONKEY (ground height blend): params_b — x = height-aware layer blend on, y = its depth.
//   MONKEY (ground anti-tile): params_b — z = mode (0 off, 1 macro variation, 2 + far dual-scale),
//   w = strength (0..2).
struct TerrainParams {
    params: vec4<f32>,
    params_b: vec4<f32>,
};
@group(#{MATERIAL_BIND_GROUP}) @binding(106) var<uniform> t: TerrainParams;

// The shared global light (`lighting::global_light`), updated in place once a frame; this mirrors
// its row layout, which must match.
struct WowLight {
    light_ambient: vec4<f32>, // rgb = row 1 ambient; w = Mod2x scale (×1).
    light_diffuse: vec4<f32>, // rgb = row 0 sun diffuse; w = clamp-light flag (>0.5 ⇒ saturate).
    light_sun: vec4<f32>,     // xyz = world-space sun travel direction (to-light = −xyz); w unused.
    light_spec: vec4<f32>,    // rgb = row 9 specular color; w = shininess (20). rgb == 0 disables.
    fog_color: vec4<f32>,     // rgb = row 7 fog (raw, gamma 0..1); w = enable (>0.5 ⇒ blend).
    fog_params: vec4<f32>,    // x = fog_start yd; y = fog_end yd; z = signed +sun / -moon shadow weight; w = farclip wall.
    _sh: array<vec4<f32>, 6>, // rows 6-11: the model SH coeffs (live in wow_model.wgsl, 0354) — unread by terrain.
    sh_c16: vec4<f32>,        // row 12: xyz the models' c16 quad band; .w a FREE lane (the 0273 point gain is retired).
    _water: array<vec4<f32>, 4>, // rows 13-16: the liquid swatches — unread by terrain.
    grade: vec4<f32>,         // reserved (the 0282 interior A/B retired; 0163). Layout only.
    _wmo_fog: array<vec4<f32>, 2>, // rows 18-19: the interior fog triple — unread by terrain.
    // The dynamic point-light table (decision 0278), packed by `global_light::build_light_data`:
    // row 20 `.x` = live entry count; then TWO rows per light — `[pos.xyz, range]`, `[rgb, lane]`.
    // MONKEY (light lanes): `lane` = 0 for an EXTERIOR light, > 0.5 (and equal to the fixture's
    // reach in yards) for an INTERIOR one. Terrain consumes only the exterior half.
    point_count: vec4<f32>,
    points: array<vec4<f32>, 512>,
    // MONKEY (p0 MonkeyFrame): the programme block after the point table (monkey_frame.wgsl).
    monkey: monkey_frame::MonkeyFrame,
    // MONKEY (rainshelter): the rain-occlusion grid (`weather/shelter.rs`): `[origin_x, origin_z,
    // 1/cell, active]`, `[base_y, cells per side, 0, 0]`, then one packed word a cell (wet_hook.wgsl).
    shelter_hdr: vec4<f32>,
    shelter_cfg: vec4<f32>,
    shelter: array<u32, 16384>,
};
@group(#{MATERIAL_BIND_GROUP}) @binding(90) var<storage, read> wow_light: WowLight;

// MONKEY (outdoor torch shadows: terrain): the GROUND receiver's torch bindings - the SAME depth
// array static_gx's group 3 samples and the SAME 6416-byte table, riding this material's own group
// (`TerrainExtension` bindings 91/92/93) because a Bevy material draw sets groups 0/1/2 only.
// ALWAYS bound (the image and buffer exist from startup), so no shader-def guards this block; only
// the fragment stage reads it. The struct text is COPIED VERBATIM from wow_model.wgsl - std430 here,
// std140 in static_gx, identical bytes because every member is 16-aligned - and a drift between the
// three copies would not be a compile error, it would be terrain sampling the wrong matrices.
struct TorchTable {
    // MONKEY (static torch cache): byte-identical in ALL THREE shaders and TorchTableUniform.
    // count@0 (16): x high-water slot count, y soft*100, z dynamic/live-bank mask, w flags.
    // positions@16 (256), view_projs@272 (6144): total 6416 bytes. A mismatch hides buildings.
    // MONKEY (live bank rank): 96 static layers + 48 live layers; matrices stay slot-addressed.
    count: vec4<u32>,
    positions: array<vec4<f32>, 16>,
    view_projs: array<mat4x4<f32>, 96>,
}
@group(#{MATERIAL_BIND_GROUP}) @binding(91) var torch_depth: texture_depth_2d_array;
@group(#{MATERIAL_BIND_GROUP}) @binding(92) var torch_samp: sampler_comparison;
@group(#{MATERIAL_BIND_GROUP}) @binding(93) var<storage, read> torch_table: TorchTable;
// Reverse-Z receiver bias - KEEP IN SYNC with static_gx.wgsl / wow_model.wgsl's TORCH_BIAS (0.001,
// the known-good value; larger detaches every shadow far from the torch).
const TORCH_BIAS: f32 = 0.001;
// NORMAL-OFFSET for the GROUND (yd): sample the map a hand's width above the surface along its own
// normal. Terrain is the one receiver whose casters REST on it - a crate, a fence post, a standing
// NPC - so the contact point is where receiver and caster depths agree to within a texel, i.e.
// exactly where a plain compare produces acne. Offsetting along the terrain normal (not straight
// up) is what keeps a SLOPE clean: on a 30 deg bank a vertical offset buys only `0.15*cos(30)` of
// separation while the depth gradient across a texel has grown by `1/cos(30)`, so the margin
// collapses on precisely the surfaces that need it most. The same 0.15 the entity lane uses, so a
// character and the ground under it are offset alike and their shadows meet without a gap.
const TORCH_NORMAL_OFFSET: f32 = 0.15;
// MONKEY (torch caster selection, MIRRORED from static_gx.wgsl): the live PCF tap-radius scale
// (count.y LOW half) and MONKEY (shadow floor)'s strength (HIGH half),
// unpacked from the table's `count.y` (stored x100 - the row is `vec4<u32>`).
fn torch_soft() -> f32 {
    return max(f32(torch_table.count.y & 0xffffu) * 0.01, 0.05);
}
// MONKEY (shadow floor): the live SHADOW STRENGTH (`torchShadowStrength`, 0..1, default 0.7),
// unpacked from `count.y`'s HIGH half (`(strength x 100) << 16 | soft x 100` - see
// `TorchTableUniform::pack`; the low half is `torch_soft` above and `count.w`'s flags are
// untouched). It is applied as `mix(1, s, w * strength)` where the cross-fade weight already
// multiplies the shadow factor, which is algebraically the same thing as flooring the factor
// itself (`1 - w*strength*(1-s)` either way) for one extra multiply and no extra tap.
//
// WHY a floor at all: a torch map is the ONLY occlusion in the direct term, so a blocked fragment
// used to drop that term to exactly zero - a pitch-black, razor-edged scar of tent canvas across
// Darkmoon's grass, table legs printed on the Darkshire inn floor. Nothing in this renderer
// bounces, so the 30 % that survives at the default IS the bounce: the fill/ambient arms are
// untouched (they never saw this factor), only the DIRECT arm is floored. 1 restores the shipped
// pitch-black look exactly, 0 disables torch shadows without disturbing the lane behind them.
fn torch_strength() -> f32 {
    return clamp(f32(torch_table.count.y >> 16u) * 0.01, 0.0, 1.0);
}
// MONKEY (outdoor torch shadows, MIRRORED from static_gx.wgsl - keep in sync): the EXTERIOR lane's
// world-distance fade radius (yd) and the CPU's one-bit lane gate in `count.w`. See the static_gx
// copies for the full rationale (the interior `ndc.z` fade is already down to 5 % at 10 yd, which
// erases a campfire's 15-25 yd pool).
const TORCH_EXT_FADE_YD: f32 = 44.0;
const TORCH_EXT_LANE: u32 = 1u;
// MONKEY (torch lane perf, MIRRORED from static_gx.wgsl): below THIS much unshadowed direct
// contribution a fragment skips the torch table scan and its four comparison taps entirely, because
// the shadow factor multiplies that contribution and can therefore only ever subtract less than this
// from the frame. Terrain's combine is a plain modulate rather than the interior exposure rolloff,
// so the worst possible step across the guard's boundary is 1e-4 of a fully-lit texel - two orders
// under one 8-bit code. Most of the skips are exact zeros anyway (`nl` IS 0 on ground facing away
// from the fire), so the threshold only has to be small enough to be invisible.
const TORCH_SKIP_EPS: f32 = 1e-4;
// The CPU's one-bit verdict: `exteriorShadows` AND night AND at least one promoted exterior
// fixture. EVERYTHING this lane costs terrain hangs off this single uniform read.
fn torch_ext_on() -> bool {
    return (torch_table.count.w & TORCH_EXT_LANE) != 0u;
}

// MONKEY (Phase 5, COPIED VERBATIM from wow_model.wgsl - keep in sync): the cube face that contains
// direction `d` (fixture -> fragment) - the major axis, signed. Face order is the contract with
// `benilla_app::torch_shadow::cube_view_projs`: 0 +X, 1 -X, 2 +Y, 3 -Y, 4 +Z, 5 -Z.
fn torch_face(d: vec3<f32>) -> u32 {
    let a = abs(d);
    if (a.x >= a.y && a.x >= a.z) {
        return select(1u, 0u, d.x > 0.0);
    }
    if (a.y >= a.z) {
        return select(3u, 2u, d.y > 0.0);
    }
    return select(5u, 4u, d.z > 0.0);
}

// MONKEY (outdoor torch shadows: terrain): this fixture's OWN cast shadow on a GROUND fragment -
// `wow_model.wgsl`'s `torch_entity_shadow_at` with the exterior fade already chosen, because terrain
// has no interior lane to share the body with (an MCNK cell is outdoors by definition, and the
// interior half of the light table is skipped before ranking). Correlate the `wow_light` fixture at
// `light_pos` to its promoted torch (nearest position within 0.1 yd), pick the cube face facing the
// fragment, and sample that layer through the shared projector. 1.0 (unshadowed) when no map matches
// - the lane is off, or the fixture was not among the promoted slots.
fn torch_terrain_shadow(light_pos: vec3<f32>, P: vec3<f32>, N: vec3<f32>) -> f32 {
    // MONKEY (torch lane perf): the empty table is the common case, so answer it before the normal
    // offset and the loop set-up.
    if (torch_table.count.x == 0u) {
        return 1.0;
    }
    // Normal-offset the sample point off the ground (see TORCH_NORMAL_OFFSET).
    let Ps = P + N * TORCH_NORMAL_OFFSET;
    // MONKEY (light identity): nearby spell lights must not borrow a candle's map. The small
    // tolerance matches torch_shadow::TORCH_STALE_DRIFT_SQ; nearest agrees with both model paths.
    var nearest = torch_table.count.x;
    var nearest_d2 = 0.010001;
    for (var candidate = 0u; candidate < torch_table.count.x; candidate += 1u) {
        // MONKEY (static torch cache): holes and pending uploads never sample stale layers.
        if (torch_table.positions[candidate].w <= 0.0) { continue; }
        let delta = torch_table.positions[candidate].xyz - light_pos;
        let d2 = dot(delta, delta);
        if (d2 < nearest_d2) {
            nearest = candidate;
            nearest_d2 = d2;
        }
    }
    if (nearest < torch_table.count.x) {
            let i = nearest;
            let fixture = torch_table.positions[i].xyz;
            let face = torch_face(Ps - fixture);
            let layer = i * 6u + face;
            // MONKEY (live bank rank): count.z is CPU-ready-filtered; holes must not consume live
            // cubes. Keep the projection on the static slot while compacting depth only.
            let rank = countOneBits(torch_table.count.z & ((1u << i) - 1u));
            let depth_layer = select(layer, 96u + 6u * rank + face, (torch_table.count.z & (1u << i)) != 0u);
            // The EXTERIOR world-distance fade, measured from the FIXTURE (not the camera): this is
            // "how far does this fire's shadow carry", a property of the fire. Chosen just inside the
            // cube projection's own 48 yd far plane so the frustum edge is never a hard cut.
            let fade = 1.0 - smoothstep(0.8 * TORCH_EXT_FADE_YD, TORCH_EXT_FADE_YD, distance(fixture, Ps));
            // MONKEY (slope bias): `N` is the ground normal the sample point was already
            // offset along, so the plane the projector biases against is the one this fragment
            // actually lies on.
            let s = shadow_hook::torch_map_shadow(
                torch_table.view_projs[layer], i32(depth_layer), Ps, N, torch_depth, torch_samp,
                TORCH_BIAS, torch_soft(), fade);
            // MONKEY (torch caster selection): `.w` is the slot's FADE WEIGHT, so a promoted fixture's
            // shadow ramps in over ~1/3 s and a demoted one ramps out. The ground and the NPC standing
            // on it MUST use the same weight or one shadow would pop while the other faded.
            // MONKEY (shadow floor): and `torch_strength()` is the DIRECT-term floor folded
            // into that same weight (see the function). `w * strength` rather than a second `mix`
            // because the two are the same expression.
            return mix(1.0, s, torch_table.positions[i].w * torch_strength());
    }
    return 1.0;
}

// MONKEY (torch debug, interiorDebug 2 on TERRAIN): the MIN raw cube-map shadow factor over every
// promoted fixture (ignoring the fixture match) - mirrors static_gx's `torch_debug_factor` and
// wow_model's `torch_entity_debug_factor`, so the GROUND's sampling can be SEEN as greyscale:
// all-white = the table is empty / the projection misses the ground; shaped dark = the depth map
// reaches the terrain and any remaining fault is downstream of sampling (correlation, weighting).
fn torch_terrain_debug_factor(P: vec3<f32>, N: vec3<f32>) -> f32 {
    let Ps = P + N * TORCH_NORMAL_OFFSET;
    var s = 1.0;
    for (var i = 0u; i < torch_table.count.x; i = i + 1u) {
        if (torch_table.positions[i].w <= 0.0) { continue; }
        let face = torch_face(Ps - torch_table.positions[i].xyz);
        let layer = i * 6u + face;
        // MONKEY (live bank rank): debug samples the same compact bank as the lit path.
        let rank = countOneBits(torch_table.count.z & ((1u << i) - 1u));
        let depth_layer = select(layer, 96u + 6u * rank + face, (torch_table.count.z & (1u << i)) != 0u);
        // MONKEY (slope bias): the overlay biases against the SAME plane the lit path does, or
        // `interiorDebug 2` would keep showing acne the render no longer has.
        let raw = shadow_hook::torch_map_shadow(
            torch_table.view_projs[layer], i32(depth_layer), Ps, N, torch_depth, torch_samp,
            TORCH_BIAS, torch_soft(), -1.0);
        // MONKEY (torch caster selection): the WEIGHTED factor, matching the real render.
        s = min(s, mix(1.0, raw, torch_table.positions[i].w));
    }
    return s;
}

// Custom vertex→fragment payload: the standard fields the splat fragment needs, PLUS `specular` — the
// per-vertex-clamped sun glint that must be Gouraud-interpolated (Q14). Same struct on both stages, so
// `@builtin(position)` is clip-position out / frag-coord in (`world_position` is unused until Step 5 fog).
struct TerrainVsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world_position: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) uv_b: vec2<f32>,
    @location(4) color: vec4<f32>,
    @location(5) specular: vec3<f32>,
    // Preserve the reference's clamped sun/ambient interpolation when no point was selected.
    @location(6) primary: vec3<f32>,
    // The receiver normal is carried so the real cascaded shadow lookup can apply a slope-aware
    // bias. This is forward-pass data only; the terrain shadow caster is a stock Bevy proxy.
    @location(7) world_normal: vec3<f32>,
    // MONKEY (outdoor torch shadows: terrain; ext light k8): WHICH <=`EXT_SEL_K` exterior table
    // entries the fragment evaluates, packed 8 bits each across FOUR u32s (see
    // `EXT_SEL_EMPTY` / `ext_sel_get`). FLAT, because it is a CHOICE and not a quantity -
    // interpolating packed indices across a triangle would produce a different, meaningless one. It
    // lets the fragment stage re-shadow the VERTEX's own selection instead of re-ranking, which is
    // what keeps the selection popping at chunk granularity (the authored behaviour) rather than
    // drawing a hard line mid-cell. Widened from ONE u32 of three 10-bit ranks to TWO u32s of eight
    // 8-bit ranks and then to FOUR u32s of twelve - three extra interstage components in all (28
    // for this struct, against a 60-component limit) - see `EXT_SEL_K` for why three, and then
    // eight, stopped being enough.
    @location(8) @interpolate(flat) ext_sel: vec4<u32>,
    // Unclamped sun/ambient interpolates separately; local lights are evaluated per pixel.
    @location(9) base_lit: vec3<f32>,
}

// Terrain's point-light candidacy half-width (yd): `w + 10`, `w` being the chunk's bounding-sphere
// radius `sqrt(2·16.666666² + (zExtent/2)²)`, 23.570166 on flat ground (constant at `0x68dfac`).
// This takes the flat `w` everywhere, so steep chunks gather slightly narrower than the reference;
// the lights that differ are 33 yd out or more, where att ≈ 0.02.
const TERRAIN_REACH: f32 = 33.570166;

// The dynamic point-light term at a world-space point (decisions 0016/0273/0278, selection 0285) —
// the reference commits AT MOST THREE point lights per draw, the nearest to the receiving unit's own
// position (the gather sorts by squared distance from the unit, the commit seats GL slots 1-3 —
// wow-re `wmo-surface-dynamic-light` §4/§6). Terrain's unit is the 33.33-yd MCNK chunk, so the anchor
// is the chunk cell center under the vertex (our tiles merge chunks into one world-space mesh —
// `mcnk_cell_anchor` recovers the chunk analytically). Pass 1 selects; pass 2 evaluates only the
// selected lights at the vertex — the byte-verified falloff `1/(0.7·d + 0.03·d²)`, diffuse-only
// (committed ambient/specular are zero — VERIFIED: the staging struct's ambient/specular slots are
// explicitly zeroed at `71c7e3`-`71c7ec` right before the point loop), on the submitted vertex
// normal, no distance cutoff on the EVALUATION (a committed GL light has none; candidacy alone
// bounds the set).
//
// **Candidacy is the reference's spatial-hash sweep, not a radius** (VERIFIED, wow-re
// `terrain/scratch/terrain-dynamic-light-gather.md`): the gather walks a 20-yd-cell hash over a cell
// span `[floor((c − w − 10)/20), floor((c + w + 10)/20)]` — a **Chebyshev box**, no circular reject —
// where terrain's own `w` is the chunk's bounding-SPHERE radius `chunk+0x68`
// (`sqrt(2·16.666666² + (zExtent/2)²)` = 23.570166 on flat ground, `0x71bc30` copies it at
// `71bc47`-`71bc5f`). That gives a guaranteed covered half-width of `w + 10` = [`TERRAIN_REACH`].
// We used a 48-yd SPHERE here — an INFERRED stand-in from decision 0285, before anyone had read the
// terrain side. It over-gathered: with 29 live point lights measured around Darkshire against three
// committed slots, every extra candidate is another chance for a chunk's ≤3 set to flip as an
// emitter moves, which is chunk-shaped popping the reference never has.
//
// The M2 lane in wow_model.wgsl still uses the packed per-light range — its own `w` is 0 (a ±10 yd
// box, a *different* byte-pinned number), a change that would move the approved interior look, so it
// is the director's to weigh. The two lanes diverge on purpose; do not "fix" them back into one.
// MONKEY (outdoor torch shadows: terrain, MIRRORED from wow_model.wgsl / static_gx.wgsl - keep in
// sync): the <=`EXT_SEL_K`-nearest selection packed into FOUR u32s, twelve 8-bit indices (rank 0 in
// the low byte of `.x`) with `EXT_SEL_EMPTY` for an unfilled rank. It exists so the per-FRAGMENT
// shadowed term below can re-evaluate the VERTEX stage's choice rather than making its own -
// re-ranking per fragment would draw a hard line wherever the ranking flips, and on terrain that
// line would run straight through the middle of an MCNK cell, which is exactly the chunk-shaped
// popping the 0285 anchor removed.
//
// MONKEY (ext light k12) - **WHY K WENT 3 -> 8.** Three is the reference FFP's own commit limit (GL
// slots 1-3; wow-re `wmo-surface-dynamic-light` sections 4/6) and it was the right number for the
// reference's SPARSE, hand-authored light set: with two or three authored lights in a village the
// nearest three IS all of them, and no two draw units can disagree. benilla does not have that set
// - `fire_light.rs` SYNTHESISES a point light for every torch, lantern, brazier and campfire
// GameObject/doodad in range, so a lamp-lit set piece now puts 15+ exterior fixtures inside ONE
// chunk's candidacy box (measured at the Darkmoon Faire: 12 `Free Standing Torch 01` + 3
// `General Lantern 01` within reach of the player's chunk, plus a stall lamp and the fireworks'
// spell lights). Once the candidates outnumber the slots, adjacent draw units keep DIFFERENT
// threes; and because the boundary between draw units is a straight line - an MCNK cell edge, or
// the jump from a cell-anchored chunk to the origin-anchored bench standing on it - the
// disagreement reads as a HARD STRAIGHT EDGE across a torch's pool of light (the director's
// "scars"), and as a bench lit by a firework over ground that is not. More slots make neighbouring
// units AGREE, so a pool ends where the FALLOFF ends instead of where the cell does; the only open
// question is how many is enough, and the next paragraph answers it with the measurement.
//
// 8 bits per rank caps the LIVE table at 255 real entries, `EXT_SEL_EMPTY` = 255 being the
// sentinel; `global_light::MAX_LIVE_POINT_LIGHTS` enforces that CPU-side. The buffer still carries
// 256 slots, so `LightStd430` keeps its 8528 B and no mirror struct moves.
//
// MONKEY (ext light k12) - **AND WHY 8 -> 12, one round later.** Eight was MARGINAL the day it
// landed and the note here said so. The Darkmoon Faire's densest chunk holds THIRTEEN candidates
// and EIGHT of them stand strictly inside its own 33.33 yd cell, so a torch 2.5 yd outside the edge
// - lighting the grass right at the seam - cannot win a slot under any ranking, and that chunk goes
// on disagreeing with its neighbour about it. On that particular seam K=8 did not help, it made
// things WORSE (0.198 -> 0.356 of a falloff unit): raising K let the SPARSE neighbour take in
// lights the dense chunk still had no room for, so the two sets grew further apart, not closer.
// Twelve is the first K at which every chunk around the player keeps everything it can see bar one:
//
//   worst ABSOLUTE seam jump, the four chunks meeting at the corner nearest WoW (-9557, 99)
//   (`k12_table.py` in the scratchpad; falloff units, colour and N.L factored out, the byte-verified
//   `1/(0.7d + 0.03d^2)`, 200 samples along each shared edge)
//     seam                     cands      K=3      K=8     K=12     K=16
//     (-288,2)|(-288,3)         6 | 2   0.0418   0.1117   0.1117   0.1117
//     (-288,2)|(-287,2)        6 | 13   0.6425   0.0882   0.0669   0.1281
//     (-288,3)|(-287,3)         2 | 7   0.2888   0.0862   0.0862   0.0862
//     (-287,2)|(-287,3)        13 | 7   0.1980   0.3556   0.1144   0.1406
//     WORST                            0.6425   0.3556   0.1144   0.1406
//
// Read the last two columns together. At K>=13 every chunk keeps ALL its candidates, so 0.1406 is
// the CANDIDACY BOX's own floor - the residual disagreement two chunks have because their gather
// boxes are centred 33 yd apart - and no K can touch it (widening the box is a separate change, and
// it is the byte-verified one). K=12 measures 0.1144, slightly UNDER that floor, because the dense
// chunk drops exactly one far candidate its neighbour also barely weights. So twelve ENDS this line
// of attack: 16 is not better, only wider.
//
// The own-origin case in the same table - a bench standing on the ground, ranking from its own
// position, against the chunk under it - goes 79 % / 71 % / 52 % apart at K=3, to 31 % / 27 % / 5 %
// at K=8, to 3 % / 1 % / 5 % at K=12. That is the "bench lit by a firework over dark grass" closing.
//
// RAISING K FURTHER: `ext_sel_get` and the pack tail index the selection vector dynamically
// (`sel[s >> 2u]`) and the rank arrays are zero-constructed and filled, so K is not spelled out
// anywhere but here. The vector is `vec4<u32>` now and holds SIXTEEN rank slots, so K = 13..16 is
// this constant ALONE - no type site, no interstage field, no `EXT_SEL_NONE` edit. Past 16 the next
// step is a second `vec4<u32>` (four more interstage components) and `ext_sel_get` learning which
// vector to read, which is a real change rather than a constant.
const EXT_SEL_K: u32 = 12u;
const EXT_SEL_EMPTY: u32 = 255u;

// MONKEY (ext light k12): how many of the `EXT_SEL_K` ranks pay for a CUBE-MAP OCCLUSION lookup in
// the night lane. The ranking is by distance, so ranks 0..2 are the three fixtures whose term
// dominates this fragment; ranks 3..11 are the long tail that fixes the SELECTION (the scars) and
// contribute a soft, low-amplitude wash where a hard-edged shadow would not be legible anyway.
// Holding the shadowed count at the OLD K keeps the per-fragment cost of the night lane exactly
// what it was - three table scans and their taps - while the selection itself gets twelve deep.
const EXT_SEL_SHADOWED: u32 = 3u;

// Unpack rank `s` (0..`EXT_SEL_K`-1) from the four-word selection. MIRRORED - keep in sync.
fn ext_sel_get(sel: vec4<u32>, s: u32) -> u32 {
    return (sel[s >> 2u] >> (8u * (s & 3u))) & 255u;
}

// The RANKING half of the old `point_light_sum`: the same interior-lane skip, the same Chebyshev
// candidacy box, the same tie order (strictly-less inserts, so an equal distance leaves the earlier
// table index at the better rank).
//
// MONKEY (ext light k12): `box` is the draw unit's HORIZONTAL half-extent in yards, and it changes
// what "nearest" MEANS - ranking is now by the distance from the light to the unit's AABB, not to
// the unit's anchor POINT. Terrain's unit is the 33.33 yd MCNK cell, so a torch standing 2 yd
// outside a cell's edge is 2 yd from the nearest ground that cell draws, yet ~18 yd from its
// CENTRE, which is how it used to lose its slot to three torches clustered near the middle while
// lighting nothing of the grass right under it. Clamping the light into the cell box
// (`max(|d| - box, 0)` per horizontal axis) ranks it the way the ground actually sees it, and - the
// point of the exercise - makes two ADJACENT cells rank a light on their shared edge almost
// identically, so their sets agree there. The VERTICAL stays unbounded, exactly as the reference's
// hash sweep is and as `mcnk_cell_anchor` (which keeps the vertex's own y) already implies.
// `box = 0` reproduces the old point-anchored ranking BIT-FOR-BIT - that is what the own-origin
// units (props, entities) pass.
//
// CANDIDACY deliberately stays on the anchor POINT: it is the byte-verified gather box, and
// widening it is a different question from how the survivors are ordered.
fn point_light_pick(anchor: vec3<f32>, box: f32) -> vec4<u32> {
    let count = u32(wow_light.point_count.x);
    var sel = array<u32, EXT_SEL_K>();
    var sd = array<f32, EXT_SEL_K>();
    for (var s = 0u; s < EXT_SEL_K; s = s + 1u) {
        sd[s] = 1e30;
    }
    for (var i = 0u; i < count; i = i + 1u) {
        // MONKEY (light lanes): skip INTERIOR fixtures. The colour row's `.w` is `0` on an exterior
        // source and the fixture's reach in yards (always ≥ 1) on one that claims a room, so
        // `> 0.5` is the lane test. Terrain is exterior by definition: an inn's candles reaching
        // the grass at the base of its wall — a warm pool on the lawn at night — is the whole bug,
        // and it is here rather than in the pack because the fixtures MUST stay packed for the
        // interior lane to light the room they belong to. Skipped BEFORE the ≤`EXT_SEL_K`
        // ranking, so an interior fixture cannot even occupy a slot an outdoor fire should
        // have had.
        if (wow_light.points[2u * i + 1u].w > 0.5) {
            continue;
        }
        let pos_range = wow_light.points[2u * i];
        let dv = pos_range.xyz - anchor;
        // The hash sweep is horizontal (the grid keys on WoW x/y = Bevy z/x) and has no vertical
        // bound; the ranking below is the full 3-D distance, as `0x71bf90` does.
        if (max(abs(dv.x), abs(dv.z)) > TERRAIN_REACH) {
            continue;
        }
        // MONKEY (ext light k12): distance to the draw unit's BOX (see the header note). With
        // `box = 0` both `max`es are identities and this is exactly the old `dot(dv, dv)`.
        let e = max(abs(dv.xz) - vec2<f32>(box), vec2<f32>(0.0));
        let d2 = dot(e, e) + dv.y * dv.y;
        // MONKEY (ext light k12): an `EXT_SEL_K`-deep insertion in place of the hand-unrolled 3-deep
        // cascade. Both loops are bounded by a module const, so the compiler unrolls them; the
        // comparison is strictly-less, which keeps the old tie order (first-found wins).
        //
        // The guard on the WORST kept rank first: `sd` is sorted, so a candidate that cannot beat
        // `sd[K-1]` cannot beat anything, and the scan below would walk all eight slots only to
        // decide that. It makes the REJECT path - which is what nearly every table entry takes once
        // the set is full - ONE comparison, i.e. cheaper than the three the old cascade spent, so
        // widening K did not make the common case more expensive. Exactly equivalent to letting the
        // scan run: `d2 >= sd[K-1]` is precisely the condition under which it returns `r = K`.
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
    // Pack low-byte-first, `EXT_SEL_EMPTY` for a rank nothing reached. No real index can collide
    // with the sentinel: the live table caps at 255 (`MAX_LIVE_POINT_LIGHTS`).
    var packed = vec4<u32>(0u, 0u, 0u, 0u);
    for (var s = 0u; s < EXT_SEL_K; s = s + 1u) {
        let idx = select(EXT_SEL_EMPTY, sel[s], sd[s] <= 9.9e29);
        packed[s >> 2u] |= idx << (8u * (s & 3u));
    }
    return packed;
}

// The EVALUATION half - the byte-verified falloff `1/(0.7d + 0.03d^2)` x `max(N.L, 0)` x the
// committed colour, in rank order, stopping at the first empty rank exactly as the old
// `sd[s] > 9.9e29` break did (an unfilled rank packs as `EXT_SEL_EMPTY`, and no real index can
// collide with it: the live table caps at 255). MONKEY (local-light consistency): evaluate the
// selected terms per fragment, without re-ranking.
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

// MONKEY (outdoor torch shadows: terrain): the SHADOWED evaluation - the SAME entries the
// vertex picked, each multiplied by its OWN fixture's cube-map occlusion, so a crate between the
// campfire and this patch of grass darkens the campfire's term while the brazier across the road is
// untouched. This is what step (a) already did for WMO exteriors and models; without it a
// character's shadow stopped dead at the grass.
//
// MONKEY (ext light k12): the SUM runs to `EXT_SEL_K`, but only the first `EXT_SEL_SHADOWED` ranks
// pay for an occlusion lookup - each of those costs a <=16-slot table scan and four comparison taps,
// so the per-fragment cost of this lane is pinned at exactly what it was before the widening while
// the selection itself got twelve deep. Ranks 3..11 are the distance-ordered tail: they are what
// makes a torch's pool agree across a cell edge, and they arrive unshadowed, which at their
// amplitude is not a look the eye can separate from a shadowed one. Reached only under
// `torch_ext_on()` - so it is night-only and dead with `exteriorShadows 0`. `TORCH_SKIP_EPS` drops
// the scan entirely wherever the unshadowed term is already invisible, which on terrain is most of
// the frame (ground facing away from the fire, or far enough out that the falloff has taken the
// term below a code).
// MONKEY (ground relief): `Ns` is the SHADING normal (the relief-perturbed one, or `N` itself with
// the lane off); `N` stays the geometric normal the cube-map lookup normal-offsets along.
fn point_light_eval_shadowed(sel: vec4<u32>, P: vec3<f32>, N: vec3<f32>, Ns: vec3<f32>) -> vec3<f32> {
    var sum = vec3<f32>(0.0);
    for (var s = 0u; s < EXT_SEL_K; s = s + 1u) {
        let idx = ext_sel_get(sel, s);
        if (idx == EXT_SEL_EMPTY) {
            break;
        }
        let fixture = wow_light.points[2u * idx].xyz;
        let to_light = fixture - P;
        let d = length(to_light);
        let atten = 1.0 / (0.7 * d + 0.03 * d * d);
        let nl = max(dot(Ns, to_light / max(d, 1e-4)), 0.0);
        // MONKEY (torch lane perf): the occlusion is a factor on a term that is already zero on any
        // ground facing away from the fire.
        let ext_w = atten * nl;
        var occ = 1.0;
        if (s < EXT_SEL_SHADOWED && ext_w > TORCH_SKIP_EPS) {
            occ = torch_terrain_shadow(fixture, P, N);
        }
        sum += wow_light.points[2u * idx + 1u].rgb * (ext_w * occ);
    }
    return sum;
}

// The MCNK chunk cell centre under a world point: the reference's light anchor, the chunk's world
// AABB centre `CMapChunk+0x5c..0x64` (written by `0x6b0e50`). The grid is fixed world-wide (chunk
// 533.33333/16 yd, half-extent 32 tiles) and symmetric, and WoW x/y are Bevy −z/−x, so snapping
// Bevy x/z lands on the same cells; wow_model.wgsl mirrors it for clutter.
// The anchor height is the vertex's own y, not the chunk's mid-height `(minH + maxH)/2`, so on
// relief one chunk's vertices can select different lights: the per-chunk constant is not plumbed
// to the vertex stage. On flat ground the two agree.
fn mcnk_cell_anchor(P: vec3<f32>) -> vec3<f32> {
    let cell = 533.33333 / 16.0;
    let half = 32.0 * 533.33333;
    let ix = floor((half + P.x) / cell);
    let iz = floor((half + P.z) / cell);
    return vec3<f32>((ix + 0.5) * cell - half, P.y, (iz + 0.5) * cell - half);
}

// MONKEY (ext light k12): the MCNK cell's HORIZONTAL half-extent (yd) - the `box` a cell-anchored
// draw unit ranks by. The same grid constant as above, halved: 533.33333/16/2 = 16.666666. An
// own-origin unit passes 0 instead. MIRRORED in wow_model.wgsl / static_gx.wgsl - keep in sync.
const MCNK_CELL_HALF: f32 = 533.33333 / 32.0;

// ── MONKEY (ground relief) ───────────────────────────────────────────────────────────────────────
// Per-pixel normal mapping, parallax occlusion mapping (POM) and parallax self-shadowing on the
// splat, from the offline relief maps in `relief_array`. Everything hangs off `relief_on` in the
// fragment (a UNIFORM test: the mode cvar and this tile's relief layer count), so with the lane
// off the fragment runs today's taps and today's light sum, instruction for instruction.
//
// The POM depth fades to zero between these camera distances (yd) and the loops are skipped past
// the end; minified relief is flat anyway, and the fade hides the swim of a vanishing offset.
const RELIEF_FADE_START: f32 = 25.0;
const RELIEF_FADE_END: f32 = 45.0;
// POM march steps: many at a grazing view, few looking straight down.
const RELIEF_POM_STEPS_MAX: f32 = 16.0;
const RELIEF_POM_STEPS_MIN: f32 = 4.0;
// The self-shadow march toward the sun, and how hard a blocking crest darkens (per unit height).
const RELIEF_SHADOW_STEPS: u32 = 8u;
const RELIEF_SHADOW_K: f32 = 4.0;
// A layer with no relief map (or one past the array) samples as this texel:
// zero tilt under the `(byte-128)/127` decode, fully open, top height.
const RELIEF_FLAT: vec4<f32> = vec4<f32>(128.0 / 255.0, 128.0 / 255.0, 1.0, 1.0);

// One layer's relief texel. Explicit gradients: these taps run in loops and at offset uvs, where
// implicit derivatives are meaningless (and illegal in non-uniform control flow).
fn relief_tap(uv: vec2<f32>, layer: i32, count: i32, gx: vec2<f32>, gy: vec2<f32>) -> vec4<f32> {
    if (layer < count) {
        return textureSampleGrad(relief_array, splat_samp, uv, layer, gx, gy);
    }
    return RELIEF_FLAT;
}

// The four layers' relief, splat-blended exactly like the colour (`mix` by the alpha map).
fn relief_blend(uv: vec2<f32>, li: vec4<i32>, a: vec4<f32>, count: i32, gx: vec2<f32>,
    gy: vec2<f32>) -> vec4<f32> {
    var r = relief_tap(uv, li.x, count, gx, gy);
    r = mix(r, relief_tap(uv, li.y, count, gx, gy), a.r);
    r = mix(r, relief_tap(uv, li.z, count, gx, gy), a.g);
    r = mix(r, relief_tap(uv, li.w, count, gx, gy), a.b);
    return r;
}

// ── MONKEY (ground height blend) ────────────────────────────────────────────────────────────
// Height-aware layer blending, where relief exists. `layer_weights` are the weights the stock
// `mix` chain applies (layer 0 base, 1..3 painted over by their alpha), so a fully painted layer
// still fully covers and an unpainted one contributes nothing. The height blend then lets the
// taller layer win the transition: each layer's score is its height plus its weight, layers within
// `depth` of the best score share the pixel, the rest drop out. A layer's height only counts as
// its weight ramps in (`g`), which keeps w = 0 absolutely out (no bleed through a zero alpha) and
// the result continuous in the alphas (no popping). The SAME weights drive colour, sheen mask,
// relief normal / cavity and the POM height, so the surface stays one surface.
fn layer_weights(a: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(
        (1.0 - a.r) * (1.0 - a.g) * (1.0 - a.b),
        a.r * (1.0 - a.g) * (1.0 - a.b),
        a.g * (1.0 - a.b),
        a.b,
    );
}

fn height_blend_weights(w: vec4<f32>, h: vec4<f32>, depth: f32) -> vec4<f32> {
    let g = clamp(w * 8.0, vec4<f32>(0.0), vec4<f32>(1.0));
    let score = h * g + w;
    let m = max(max(score.x, score.y), max(score.z, score.w)) - depth;
    // The best-scoring layer has w > 0 (the top score is >= 0.25 and a zero-weight score is 0), so
    // its term is `depth * g` > 0 and the sum below cannot vanish.
    let b = max(score - vec4<f32>(m), vec4<f32>(0.0)) * g;
    return b / max(dot(b, vec4<f32>(1.0)), 1e-6);
}

struct ReliefW {
    r: vec4<f32>,   // the blended relief texel
    w: vec4<f32>,   // the per-layer weights that produced it (colour uses the same ones)
};

// One relief tap per layer (the same four as `relief_blend`), blended by the height-aware weights.
// A layer with no real relief map samples as the exact flat texel (top height), which would win
// every height contest; such a layer, where it has any weight, pulls the pixel back toward today's
// linear weights (`f`, continuous in the layer's weight).
fn relief_blend_hb(uv: vec2<f32>, li: vec4<i32>, a: vec4<f32>, count: i32, gx: vec2<f32>,
    gy: vec2<f32>, depth: f32) -> ReliefW {
    let t0 = relief_tap(uv, li.x, count, gx, gy);
    let t1 = relief_tap(uv, li.y, count, gx, gy);
    let t2 = relief_tap(uv, li.z, count, gx, gy);
    let t3 = relief_tap(uv, li.w, count, gx, gy);
    let w = layer_weights(a);
    let hw = height_blend_weights(w, vec4<f32>(t0.a, t1.a, t2.a, t3.a), depth);
    let eps = vec4<f32>(1.0 / 512.0);
    let is_flat = vec4<f32>(
        f32(all(abs(t0 - RELIEF_FLAT) < eps)),
        f32(all(abs(t1 - RELIEF_FLAT) < eps)),
        f32(all(abs(t2 - RELIEF_FLAT) < eps)),
        f32(all(abs(t3 - RELIEF_FLAT) < eps)),
    );
    let gate = clamp(w * 8.0, vec4<f32>(0.0), vec4<f32>(1.0)) * is_flat;
    let f = 1.0 - max(max(gate.x, gate.y), max(gate.z, gate.w));
    let wf = mix(w, hw, f);
    return ReliefW(t0 * wf.x + t1 * wf.y + t2 * wf.z + t3 * wf.w, wf);
}

// The lane's one entry: the height-aware blend when `hb_on` (uniform), else today's `mix` chain.
fn relief_sample(uv: vec2<f32>, li: vec4<i32>, a: vec4<f32>, count: i32, gx: vec2<f32>,
    gy: vec2<f32>, hb_on: bool, depth: f32) -> ReliefW {
    if (hb_on) {
        return relief_blend_hb(uv, li, a, count, gx, gy, depth);
    }
    return ReliefW(relief_blend(uv, li, a, count, gx, gy), layer_weights(a));
}

// ── MONKEY (ground anti-tile) ───────────────────────────────────────────────────────────────
// Colour-only anti-tiling for the repeating splat layers. Mode 1: a low-frequency value noise of
// WORLD xz (continuous across chunk / tile borders, period `ANTITILE_PERIOD_YD` ~ 6 texture
// repeats) nudges the final layer colour: brightness +-8% and a saturation / warm-cool shift,
// both scaled by the strength (the noise is zero-mean, so the average is kept). ALU only.
// Mode 2 adds, beyond `ANTITILE_FAR_START_YD` (full by `ANTITILE_FAR_END_YD`), a second tap of
// each layer at `ANTITILE_FAR_FREQ` x the UV frequency mixed into the colour (4 extra fetches,
// gated on the mode UNIFORM; the taps live in `antitile_far_colour`, a function body top level).
// The relief normal / cavity / POM are untouched. Mode 0 never runs any of it.
const ANTITILE_PERIOD_YD: f32 = 25.0;
const ANTITILE_FAR_START_YD: f32 = 30.0;
const ANTITILE_FAR_END_YD: f32 = 80.0;
const ANTITILE_FAR_FREQ: f32 = 0.37;

fn antitile_hash(p: vec2<f32>) -> vec2<f32> {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * vec3<f32>(0.1031, 0.1030, 0.0973));
    p3 = p3 + vec3<f32>(dot(p3, p3.yzx + vec3<f32>(33.33)));
    return fract((p3.xx + p3.yz) * p3.zy);
}

// Smooth value noise, two independent channels, each in 0..1.
fn antitile_noise(p: vec2<f32>) -> vec2<f32> {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (vec2<f32>(3.0) - 2.0 * f);
    let a = antitile_hash(i);
    let b = antitile_hash(i + vec2<f32>(1.0, 0.0));
    let c = antitile_hash(i + vec2<f32>(0.0, 1.0));
    let d = antitile_hash(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn antitile_macro(color: vec3<f32>, world_xz: vec2<f32>, strength: f32) -> vec3<f32> {
    let c = antitile_noise(world_xz / ANTITILE_PERIOD_YD) * 2.0 - vec2<f32>(1.0);
    let l = dot(color, vec3<f32>(0.299, 0.587, 0.114));
    let sat = 1.0 + 0.15 * strength * c.y;
    var o = mix(vec3<f32>(l), color, sat);
    o = o * vec3<f32>(1.0 + 0.03 * strength * c.y, 1.0, 1.0 - 0.03 * strength * c.y);
    return max(o * (1.0 + 0.08 * strength * c.x), vec3<f32>(0.0));
}

// The second, lower-frequency colour tap of the four layers, weighted like the main colour.
fn antitile_far_colour(uv: vec2<f32>, li: vec4<i32>, w: vec4<f32>) -> vec3<f32> {
    let fuv = uv * ANTITILE_FAR_FREQ + vec2<f32>(0.37, 0.11);
    let f0 = textureSampleBias(layer_array, splat_samp, fuv, li.x, view.mip_bias).rgb;
    let f1 = textureSampleBias(layer_array, splat_samp, fuv, li.y, view.mip_bias).rgb;
    let f2 = textureSampleBias(layer_array, splat_samp, fuv, li.z, view.mip_bias).rgb;
    let f3 = textureSampleBias(layer_array, splat_samp, fuv, li.w, view.mip_bias).rgb;
    return f0 * w.x + f1 * w.y + f2 * w.z + f3 * w.w;
}

// Tangent-space normal from a blended texel: `(byte-128)/127`, so 128 is exactly flat.
fn relief_normal_ts(r: vec4<f32>) -> vec3<f32> {
    let xy = clamp((r.rg * 255.0 - 128.0) / 127.0, vec2<f32>(-1.0), vec2<f32>(1.0));
    return vec3<f32>(xy, sqrt(max(1.0 - dot(xy, xy), 0.0)));
}

// The vertex stage's sun specular at an arbitrary normal (white material, row-9 colour, local
// viewer), so the relief lane can swap the normal by DIFFERENCE and stay exact on flat relief.
fn relief_sun_spec(n: vec3<f32>, P: vec3<f32>) -> vec3<f32> {
    let l = -normalize(wow_light.light_sun.xyz);
    let v = normalize(view.world_position.xyz - P);
    let h = normalize(l + v);
    let ndoth = max(dot(n, h), 0.0);
    return clamp(wow_light.light_spec.rgb * pow(ndoth, wow_light.light_spec.w), vec3<f32>(0.0), vec3<f32>(1.0));
}

@vertex
fn vertex(in: Vertex) -> TerrainVsOut {
    var out: TerrainVsOut;
    let world_from_local = mesh_functions::get_world_from_local(in.instance_index);
    out.world_position =
        mesh_functions::mesh_position_local_to_world(world_from_local, vec4<f32>(in.position, 1.0));
    out.clip_position = position_world_to_clip(out.world_position.xyz);
    let world_normal = mesh_functions::mesh_normal_local_to_world(in.normal, in.instance_index);
    out.world_normal = world_normal;
    out.uv = in.uv;
    out.uv_b = in.uv_b;
    out.color = in.color;

    // Sun specular per vertex: white material, row-9 colour, shininess `light_spec.w` (20), local
    // viewer; clamped here, then Gouraud-interpolated.
    let n = normalize(world_normal);
    let l = -normalize(wow_light.light_sun.xyz); // to-light (the sun travels along +light_sun)
    let v = normalize(view.world_position.xyz - out.world_position.xyz); // to-eye (local viewer)
    let h = normalize(l + v);
    let ndoth = max(dot(n, h), 0.0);
    out.specular = clamp(wow_light.light_spec.rgb * pow(ndoth, wow_light.light_spec.w), vec3<f32>(0.0), vec3<f32>(1.0));

    // STEP 3 at the faithful locus — the WHOLE diffuse sum, clamped HERE per vertex (see the
    // struct note). Sun: `ambient + diffuse·max(N·L,0)` on the MCNR normal; points: the
    // ≤`EXT_SEL_K`-nearest committed lights of this vertex's MCNK chunk cell — the terrain draw
    // unit (0285), ranked from the CELL BOX (`MCNK_CELL_HALF`, see `point_light_pick`) — at the
    // byte-verified `1/(0.7d + 0.03d²)`, raw over-gamut colours in.
    let ndotl = max(dot(n, l), 0.0);
    // MONKEY (outdoor torch shadows: terrain): the selection is PUBLISHED so the fragment stage can
    // shadow this same selection at night without re-ranking. One table walk still, not two -
    // the sum was always `eval(pick(..))`, it is just no longer inlined.
    let sel = point_light_pick(mcnk_cell_anchor(out.world_position.xyz), MCNK_CELL_HALF);
    out.ext_sel = sel;
    // MONKEY (local-light consistency): pick once per draw unit; sum per fragment.
    // Sparse MCNK vertices otherwise miss a candle between them.
    let sun_lighting = wow_light.light_diffuse.rgb * ndotl;
    out.base_lit = wow_light.light_ambient.rgb + sun_lighting;
    out.primary = clamp(out.base_lit, vec3<f32>(0.0), vec3<f32>(1.0));
    return out;
}

@fragment
fn fragment(in: TerrainVsOut) -> @location(0) vec4<f32> {
    // The far-clip wall: the reference clips the detailed world per pixel at its far plane
    // (`farclip`, about 777 yd). Discard beyond `fog_params.w` (0 disables) on planar eye-Z.
    if (wow_light.fog_params.w > 0.0) {
        let clip_z = -(view.view_from_world * vec4<f32>(in.world_position.xyz, 1.0)).z;
        if (clip_z > wow_light.fog_params.w) {
            discard;
        }
    }

    // Per-chunk array indices baked per vertex, COLOR the 4 layers and UV1.x the alpha; constant
    // across a chunk, so they interpolate back to the integer.
    let li = vec4<i32>(round(in.color));
    let ai = i32(round(in.uv_b.x));

    let tiled = in.uv * t.params.x;
    // The alpha and shadow maps (one untiled 64² grid per chunk) share the layers' repeat sampler,
    // so a linear footprint at a chunk edge would wrap to the far edge; a half-texel inset clamps.
    // MONKEY (ground relief): fetched BEFORE the layer taps (it was right after them; the two are
    // independent) because the relief blend below needs the splat weights. Always on `in.uv`:
    // parallax never moves the per-chunk alpha / MCSH / shadow lookups.
    let auv = clamp(in.uv, vec2<f32>(0.5 / 64.0), vec2<f32>(1.0 - 0.5 / 64.0));
    let a = textureSample(alpha_array, splat_samp, auv, ai);

    // MONKEY (ground relief): the lane's gate is UNIFORM (this tile's material uniform), so the
    // derivatives and the implicit-derivative taps below stay in uniform control flow.
    let relief_mode = u32(max(t.params.y, 0.0) + 0.5);
    let relief_layers = i32(max(t.params.z, 0.0) + 0.5);
    let relief_on = relief_mode != 0u && relief_layers > 0;
    let relief_pom = relief_on && relief_mode >= 2u;
    // MONKEY (ground height blend): uniform; only meaningful (and only read) with the lane on.
    let hb_on = relief_on && t.params_b.x > 0.5;
    let hb_depth = clamp(t.params_b.y, 0.01, 0.2);
    var hb_w = vec4<f32>(0.0);      // the height-aware layer weights at `relief_uv` (set when `hb_on`)
    var relief_uv = tiled;          // the parallax-offset tiled uv (== `tiled` without POM)
    var relief = RELIEF_FLAT;       // the splat-blended relief texel at `relief_uv`
    var relief_n = vec3<f32>(0.0);  // the perturbed world normal (set only when `relief_on`)
    var relief_sun = 1.0;           // the self-shadow factor on the SUN arm only
    var relief_gx = vec2<f32>(0.0); // the ORIGINAL tiled-uv gradients, mip-biased
    var relief_gy = vec2<f32>(0.0);
    // MONKEY (ground relief, sync 1109): the frame's derivatives are taken here, outside the gate,
    // so no implicit derivative sits in a nested block (`derivative_samples_stay_in_uniform_control_
    // flow`); unused, and so free, with the lane off.
    let dp1 = dpdx(in.world_position.xyz);
    let dp2 = dpdy(in.world_position.xyz);
    let duv1 = dpdx(tiled);
    let duv2 = dpdy(tiled);
    if (relief_on) {
        // Schüler's cotangent frame ("normal mapping without precomputed tangents"): T along +u,
        // B along +v of the TILED uv, both in the plane of the geometric normal.
        let rp = in.world_position.xyz;
        let rn = normalize(in.world_normal);
        let dp2perp = cross(dp2, rn);
        let dp1perp = cross(rn, dp1);
        let rt_raw = dp2perp * duv1.x + dp1perp * duv2.x;
        let rb_raw = dp2perp * duv1.y + dp1perp * duv2.y;
        let rt = rt_raw * inverseSqrt(max(dot(rt_raw, rt_raw), 1e-20));
        let rb = rb_raw * inverseSqrt(max(dot(rb_raw, rb_raw), 1e-20));
        // `view.mip_bias` folded into the gradients, as the biased taps take it.
        let bias_scale = exp2(view.mip_bias);
        relief_gx = duv1 * bias_scale;
        relief_gy = duv2 * bias_scale;

        let rdist = distance(rp, view.world_position.xyz);
        let rfade = 1.0 - smoothstep(RELIEF_FADE_START, RELIEF_FADE_END, rdist);
        let rdepth = max(t.params.w, 0.0) * rfade;
        let to_eye = normalize(view.world_position.xyz - rp);
        let v_ts = vec3<f32>(dot(to_eye, rt), dot(to_eye, rb), dot(to_eye, rn));
        // The POM hit's height (1 = top, where an un-displaced sample sits).
        var hit_h = 1.0;
        if (relief_pom && rdepth > 1e-5 && v_ts.z > 0.0) {
            // Steep parallax on the BLENDED height (depth = 1 - height), then a linear refinement
            // between the last two layers (classic POM).
            let steps = mix(RELIEF_POM_STEPS_MAX, RELIEF_POM_STEPS_MIN, clamp(v_ts.z, 0.0, 1.0));
            let n_steps = u32(steps + 0.5);
            let layer_d = 1.0 / f32(n_steps);
            let duv = v_ts.xy / max(v_ts.z, 0.1) * rdepth * layer_d;
            var cur_uv = tiled;
            var cur_d = 0.0;
            var cur_map = 1.0 - relief_sample(cur_uv, li, a, relief_layers, relief_gx, relief_gy, hb_on, hb_depth).r.a;
            var prev_uv = cur_uv;
            var prev_map = cur_map;
            for (var i = 0u; i < n_steps && cur_d < cur_map; i = i + 1u) {
                prev_uv = cur_uv;
                prev_map = cur_map;
                cur_uv = cur_uv - duv;
                cur_d = cur_d + layer_d;
                cur_map = 1.0 - relief_sample(cur_uv, li, a, relief_layers, relief_gx, relief_gy, hb_on, hb_depth).r.a;
            }
            let after = cur_map - cur_d;
            let before = prev_map - (cur_d - layer_d);
            let denom = after - before;
            let w = select(0.0, clamp(after / denom, 0.0, 1.0), abs(denom) > 1e-6);
            relief_uv = mix(cur_uv, prev_uv, w);
            hit_h = 1.0 - mix(cur_d, cur_d - layer_d, w);
        }
        let rs = relief_sample(relief_uv, li, a, relief_layers, relief_gx, relief_gy, hb_on, hb_depth);
        relief = rs.r;
        hb_w = rs.w;
        let nts = relief_normal_ts(relief);
        relief_n = normalize(rt * nts.x + rb * nts.y + rn * nts.z);

        // Mode 3+: march from the POM hit toward the sun; a crest above the ray shades the SUN arm
        // only. Faded like POM, and by the sun's own shadow weight (0 at night: skipped).
        let rsun_w = clamp(shadow_hook::sun_shadow_w(wow_light.fog_params.z), 0.0, 1.0);
        if (relief_mode >= 3u && rdepth > 1e-5 && rsun_w > 0.01) {
            let to_sun = -normalize(wow_light.light_sun.xyz);
            let l_ts = vec3<f32>(dot(to_sun, rt), dot(to_sun, rb), dot(to_sun, rn));
            let dh = (1.0 - hit_h) / f32(RELIEF_SHADOW_STEPS);
            if (l_ts.z > 0.0 && dh > 1e-4) {
                let suv = l_ts.xy / max(l_ts.z, 0.1) * rdepth * dh;
                var occ = 0.0;
                for (var i = 1u; i <= RELIEF_SHADOW_STEPS; i = i + 1u) {
                    let fi = f32(i);
                    let ray_h = hit_h + dh * fi;
                    let hs = relief_sample(relief_uv + suv * fi, li, a, relief_layers, relief_gx,
                        relief_gy, hb_on, hb_depth).r.a;
                    // Nearer crests weigh more: the penumbra softens with distance along the ray.
                    occ = max(occ, (hs - ray_h) * (1.0 - (fi - 1.0) / f32(RELIEF_SHADOW_STEPS)));
                }
                let soft = 1.0 - clamp(occ * RELIEF_SHADOW_K, 0.0, 1.0);
                relief_sun = mix(1.0, soft, rfade * rsun_w);
            }
        }
    }

    // `.a` is the `_s` texture's sheen mask, 0 without an `_s`. `view.mip_bias` undoes the coarser
    // mip a render scale below 1 picks; the alpha and shadow arrays have one mip, so only the
    // layers take it.
    // MONKEY (ground relief, sync 1109): the stock taps run unconditionally (implicit derivatives
    // stay in uniform control flow) and POM replaces them with explicit-gradient taps.
    var s0 = textureSampleBias(layer_array, splat_samp, tiled, li.x, view.mip_bias);
    var s1 = textureSampleBias(layer_array, splat_samp, tiled, li.y, view.mip_bias);
    var s2 = textureSampleBias(layer_array, splat_samp, tiled, li.z, view.mip_bias);
    var s3 = textureSampleBias(layer_array, splat_samp, tiled, li.w, view.mip_bias);
    if (relief_pom) {
        // MONKEY (ground relief): the parallax-offset uv with the ORIGINAL (biased) gradients, so
        // the offset's discontinuities never pick a wrong mip.
        s0 = textureSampleGrad(layer_array, splat_samp, relief_uv, li.x, relief_gx, relief_gy);
        s1 = textureSampleGrad(layer_array, splat_samp, relief_uv, li.y, relief_gx, relief_gy);
        s2 = textureSampleGrad(layer_array, splat_samp, relief_uv, li.z, relief_gx, relief_gy);
        s3 = textureSampleGrad(layer_array, splat_samp, relief_uv, li.w, relief_gx, relief_gy);
    }

    var color = s0.rgb;
    color = mix(color, s1.rgb, a.r);
    color = mix(color, s2.rgb, a.g);
    color = mix(color, s3.rgb, a.b);
    if (hb_on) {
        // MONKEY (ground height blend): the colour takes the relief's own weights.
        color = s0.rgb * hb_w.x + s1.rgb * hb_w.y + s2.rgb * hb_w.z + s3.rgb * hb_w.w;
    }

    // The sheen mask, blended with the colour's splat weights (the fragment program's
    // `secondary · texture[0].w`).
    var specmask = s0.a;
    specmask = mix(specmask, s1.a, a.r);
    specmask = mix(specmask, s2.a, a.g);
    specmask = mix(specmask, s3.a, a.b);
    if (hb_on) {
        specmask = s0.a * hb_w.x + s1.a * hb_w.y + s2.a * hb_w.z + s3.a * hb_w.w;
    }

    // MONKEY (ground anti-tile): uniform gates; mode 0 runs none of this.
    let at_mode = i32(clamp(t.params_b.z + 0.5, 0.0, 2.0));
    let at_str = clamp(t.params_b.w, 0.0, 2.0);
    if (at_mode >= 2 && at_str > 0.0) {
        var at_w = layer_weights(a);
        if (hb_on) {
            at_w = hb_w;
        }
        let at_far = antitile_far_colour(tiled, li, at_w);
        let at_d = distance(in.world_position.xyz, view.world_position.xyz);
        let at_k = smoothstep(ANTITILE_FAR_START_YD, ANTITILE_FAR_END_YD, at_d) * clamp(0.5 * at_str, 0.0, 1.0);
        color = mix(color, at_far, at_k);
    }
    if (at_mode >= 1 && at_str > 0.0) {
        color = antitile_macro(color, in.world_position.xz, at_str);
    }

    // Sun/ambient arrives unclamped from the vertex stage; selected local lights join per
    // fragment before the light sum clamps and modulates the texture.
    // Fetch the character coverage here, but apply it as a NEUTRAL scalar after the terrain's
    // authored lighting below. Subtracting the warm sun RGB left only Elwynn's green ambient and
    // produced a green silhouette on yellow morning ground. The era-style projected shadow is a
    // greyscale attenuation: it darkens the existing hue instead of changing it.
    // Keep the raw fetch for the specular gate below. The realtime map now contains CHARACTERS
    // only; MCSH contains the static world's baked blockers. They are independent occluders, so
    // the character map must never replace (and thereby brighten) the authored terrain bake.
    // MONKEY (shadow hook): the realtime shadow — fetch + edge fade (toward the `shadowDistance`
    // range in `_wmo_fog[1].z`) + night fade (`fog_params.z`) — is computed by `benilla::shadow_hook`.
    // Terrain keeps only its own MCSH suppression + spec gate below; `sun_shadow_strength` feeds the
    // MCSH fade-back.
    let view_z = (view.view_from_world * in.world_position).z;
    let cam_dist = distance(in.world_position.xyz, view.world_position.xyz);
    // MONKEY (moon shadows): the lane is SIGNED now — `+` the sun's weight, `-` the moon's (see
    // `shadow_hook::sun_shadow_w`). `max(z, 0)` is the identity on every value this lane ever held
    // before the feature, so the name, the MCSH gate and the torch-lane decode below are unchanged.
    let sun_shadow_strength = shadow_hook::sun_shadow_w(wow_light.fog_params.z);
    let moon_shadow_strength = shadow_hook::moon_shadow_w(wow_light.fog_params.z);
    // Hoisted out of the `realtime_shadow` call (same expression, same bits) because the torch lane
    // below needs the same normal for its normal-offset sample.
    let n_lit = normalize(in.world_normal);
    // GFX (moonlight): the same fetch, plus `.z` = the moonlight's own visibility.
    let shadow_terms = shadow_hook::realtime_shadow_moonlit(
        in.world_position,
        n_lit,
        view_z,
        cam_dist,
        wow_light._wmo_fog[1].z,
        sun_shadow_strength,
        moon_shadow_strength,
        wow_light.monkey.misc.w,
    );
    let world_shadow = shadow_terms.x;
    // MONKEY (ground relief): the SHADING normal (lighting: points, moon, wet) and the sun/ambient
    // inputs. With the lane off these are `n_lit` / `in.base_lit` / `in.primary` / `in.specular`
    // themselves; `n_lit` stays the geometric normal for every shadow lookup and normal offset.
    // On: the vertex sum used the smooth MCNR normal, so the sun arm is swapped by DIFFERENCE —
    // `+ diffuse·(max(Np·L,0)·selfshadow − max(N·L,0))` — which is exactly 0 on flat relief
    // (`Np == N`, self-shadow 1), keeping the authored Gouraud look wherever the map is flat.
    var n_shade = n_lit;
    var base_lit = in.base_lit;
    var primary_in = in.primary;
    var specular_in = in.specular;
    if (relief_on) {
        n_shade = relief_n;
        let to_sun = -normalize(wow_light.light_sun.xyz);
        let sun_delta = wow_light.light_diffuse.rgb
            * (max(dot(relief_n, to_sun), 0.0) * relief_sun - max(dot(n_lit, to_sun), 0.0));
        base_lit = in.base_lit + sun_delta;
        primary_in = clamp(in.primary + sun_delta, vec3<f32>(0.0), vec3<f32>(1.0));
        let spec_delta = relief_sun_spec(relief_n, in.world_position.xyz)
            - relief_sun_spec(n_lit, in.world_position.xyz);
        specular_in = clamp((in.specular + spec_delta) * relief_sun, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    // MONKEY (local-light consistency): per-pixel falloff on terrain, as on WMO and props.
    // Night adds each selected fixture's own occlusion on the existing smooth dusk clock.
    var points = point_light_eval(in.ext_sel, in.world_position.xyz, n_shade);
    let ext_night_w = select(0.0, clamp(1.0 - sun_shadow_strength, 0.0, 1.0), torch_ext_on());
    if (ext_night_w > 0.0) {
        points = mix(points,
            point_light_eval_shadowed(in.ext_sel, in.world_position.xyz, n_lit, n_shade), ext_night_w);
    }
    var primary = primary_in;
    if (ext_sel_get(in.ext_sel, 0u) != EXT_SEL_EMPTY) {
        primary = clamp(base_lit + points, vec3<f32>(0.0), vec3<f32>(1.0));
    }

    // MONKEY (moon shadows): attenuate the SKY (ambient AND directional) BEFORE adding points
    // and saturating. Subtracting from primary loses over-gamut torch energy: sky .2 + point 1
    // must remain 1 under a .35 moon shadow, not .93. Point lights keep their energy;
    // only the sky is attenuated before the final light-sum clamp.
    if (shadow_terms.y < 1.0) {
        primary = clamp(base_lit * shadow_terms.y + points, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    // GFX (moonlight): ADD the moon (cool, N·L, shadowed by the moon's own map) onto the stock
    // night. `moon.w` is exactly 0 by day and with `moonLight 0`, so the branch is not entered.
    if (wow_light.monkey.moon.w > 0.0) {
        primary = clamp(
            primary + moonlight_hook::moon_light(n_shade, wow_light.monkey.moon) * shadow_terms.z,
            vec3<f32>(0.0),
            vec3<f32>(1.0),
        );
    }

    // MCSH baked shadow. With `pixelShaders` and `specular` on, terrain is one pass through
    // `terrainp_s.bls`, which reads the MCSH bit from the blend texture's alpha (1 lit, 0 shadowed)
    // and uses it twice, below; with them off the reference draws a separate ambient-tint overlay.
    // Our `shadow_array` is 255 shadowed, 0 lit; a chunk with no MCSH map (`uv_b.y < 0`) is lit.
    // The sample runs unconditionally, at layer 0 for a chunk with none, and `si` selects after:
    // implicit derivatives need uniform control flow, and `si` is a varying.
    let si = in.uv_b.y;
    let mcsh = textureSample(shadow_array, splat_samp, auv, max(i32(round(si)), 0)).r;
    let shadow_lit = select(1.0, 1.0 - mcsh, si >= 0.0);
    // MONKEY (world shadows): when the realtime WORLD-shadow lane is active (`worldShadows` on), the
    // BAKED MCSH terrain shadows are redundant — the realtime map now shadows the static world
    // (trees + buildings) too — and keeping both double-shadows. So drop MCSH then. This keys on the
    // world lane flag (`sh_c16.w`, packed by `global_light::build_light_data`), NOT on the mere
    // presence of a directional light: the shared shadow sun ALSO exists for CHARACTER-only shadows,
    // and those must leave the world's baked MCSH intact. With the world lane off, `shadow_lit_eff`
    // keeps MCSH and only the realtime `character_shadow_term` adds the dynamic character shadow.
    // The world lane suppresses baked MCSH — but only by `sun_shadow_strength`, so as the realtime
    // world shadow fades at night the baked MCSH fades back IN. By day (strength 1) MCSH is fully
    // replaced; at night (strength 0) the authored bake returns; dusk crossfades. Character-only
    // (lane flag off) never suppresses MCSH.
    // MONKEY (moon shadows): this stays on the SUN weight alone — deliberately. The symmetric
    // reading ("a moon shadow replaces 35 % of the bake too") lightens MCSH everywhere, including
    // the ground BEYOND the cascade where the realtime lane resolves nothing and there would be no
    // replacement for what was removed. The two also compose rather than double-count: the moon
    // arm scales the light SUM (`primary`, above) and MCSH multiplies the result, and the moon arm
    // is ≤ 0.35 of a term the bake is already a flat −30 % of. The visible consequence is that a
    // tree at night wears its baked noon-sun shadow AND a faint moon-direction one — which is worth
    // eyes on a capture before it is called wrong.
    let world_shadow_lane = wow_light.sh_c16.w > 0.5;
    let mcsh_suppress = select(0.0, sun_shadow_strength, world_shadow_lane);
    let shadow_lit_eff = mix(shadow_lit, 1.0, mcsh_suppress);
    let spec_gate = min(shadow_lit, world_shadow);
    let character_shadow_term = mix(SHADOW_SUN_FLOOR, 1.0, world_shadow);

    // The faithful `terrainp_s` combine (Q15):
    //   diffuse  = tex · primary · (0.3·shadow + 0.7)   → a flat −30% in shadow, NO colour tint
    //   specular = per-vertex sheen · gloss_mask · shadow → gated to ZERO in shadow (no sheen in shade)
    // (`tex·primary` is the MODULATE-1× diffuse; the sheen is added after, separate-specular.) Then
    // LDR-clamp; gamma/byte throughout; raw gamma out (GAMMA LANE, 0161).
    // MONKEY (rainshelter): nothing gets wet under a roof, porch or bridge (the shelter grid).
    var wet_open = 1.0;
    if (wow_light.monkey.wet_a.y > 0.0) {
        let wet_st = wet_hook::shelter_taps(in.world_position.xyz, wow_light.shelter_hdr,
            wow_light.shelter_cfg);
        if (wet_st.live > 0.0) {
            wet_open = 1.0 - wet_hook::shelter_amount(wet_st, wow_light.shelter[wet_st.idx.x],
                wow_light.shelter[wet_st.idx.y], wow_light.shelter[wet_st.idx.z],
                wow_light.shelter[wet_st.idx.w], in.world_position.y, wow_light.shelter_cfg);
        }
    }
    // MONKEY (wet): rain-darkened ground + puddles (wet_hook.wgsl); a dry frame returns `color`.
    // MONKEY (ground relief): `n_shade` is `n_lit` with the relief lane off.
    let wet = wet_hook::wet_puddles(wet_hook::wet_surface(color, n_shade, in.world_position.xyz,
        wet_open, wow_light.monkey), n_shade, in.world_position.xyz, wet_open, wow_light.monkey);
    color = wet.albedo;
    var diffuse_term = color * primary * (0.3 * shadow_lit_eff + 0.7) * character_shadow_term;
    // MONKEY (ground relief): a mild cavity darkening (B = 1 open), relief lane only.
    if (relief_on) {
        diffuse_term = diffuse_term * mix(1.0, relief.b, 0.5);
    }
    let spec_term = specular_in * specmask * spec_gate;
    var tuned = clamp(diffuse_term + spec_term, vec3<f32>(0.0), vec3<f32>(1.0));
    // MONKEY (wet): the wet sheen, and the authored `_s` sheen brightens with it.
    if (wet.boost > 0.0) {
        tuned = clamp(tuned + spec_term * wet.boost + wet_hook::wet_sheen(wet.boost, n_shade,
            normalize(view.world_position - in.world_position.xyz), -normalize(wow_light.light_sun.xyz),
            wow_light.light_diffuse.rgb, wow_light.fog_color.rgb, spec_gate), vec3<f32>(0.0), vec3<f32>(1.0));
    }

    // MONKEY (torch debug, interiorDebug 2 on TERRAIN): the ground's OWN cube-map sampling as
    // greyscale, the same instrument static_gx and wow_model already paint on walls and bodies. The
    // mode rides `wmo_fog_params.w` packed as `1 + debug` (rows 18-19 here, hence `_wmo_fog[1]`), so
    // a zero - the interior lane off entirely - decodes to mode 0 and this is dead. Only mode 2 is
    // answered: 1/3/4 are questions about the INTERIOR lane and the WMO group classes, and terrain
    // belongs to neither, so leaving it normally shaded is what makes those three views readable.
    // Placed before the fog so a distant reading still fogs like the surfaces beside it.
    let idbg = u32(max(wow_light._wmo_fog[1].w - 1.0, 0.0) + 0.5);
    if (idbg == 2u) {
        tuned = vec3<f32>(torch_terrain_debug_factor(in.world_position.xyz, n_lit));
    }

    // STEP 5: gamma-space linear fog (q6). Applied AFTER tone/curve (so the diagnostic knobs see
    // the unfogged lit pixel) and BEFORE the viz bypass + raw output. The fog coordinate is
    // PLANAR eye-Z (view-space depth along the camera forward axis), NOT radial distance: the
    // reference VS computes `fogcoord` as a linear function of a single axial coordinate (eye-Z) —
    // VERIFIED from the host GLSL fog source in two apitraces (WoW.5/WoW.8). Radial `length()` is
    // ≥ planar everywhere off-axis, so it OVER-fogs the screen edges (worst in short-fog zones like
    // Duskwood, +0.26–0.32 fog factor at the corners). The per-vertex T&L original and this
    // per-pixel form land on the same byte under LINEAR fog (the factor interpolates exactly), and
    // the per-pixel form is what survives Bevy's mesh interpolators without a custom slot.
    if (wow_light.fog_color.w > 0.5) {
        let eye_z = -(view.view_from_world * vec4<f32>(in.world_position.xyz, 1.0)).z;
        // MONKEY (p0 fog hook): the shared fog law (fog_hook.wgsl); classic is bit-identical.
        tuned = fog_hook::apply_fog(tuned, wow_light.fog_color.xyz, wow_light.fog_params.xy, eye_z,
            in.world_position.xyz, view.world_position, true, wow_light.monkey);
    }

    // MONKEY (ground relief): the debug views (`groundRelief 4` = the blended height as grey,
    // `5` = the perturbed normal as `Np*0.5+0.5`), unfogged so the map reads at any distance.
    if (relief_on && relief_mode == 4u) {
        tuned = vec3<f32>(relief.a);
    } else if (relief_on && relief_mode >= 5u) {
        tuned = relief_n * 0.5 + 0.5;
    }

    // Raw gamma out: the framebuffer holds gamma bytes and blends in gamma like the reference's;
    // the frame's one decode is the FFXGlow combine.
    return vec4<f32>(tuned, 1.0);
}

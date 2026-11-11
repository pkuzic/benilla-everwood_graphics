//! Video settings that reach the window and the presentation path: 1.12's `gxWindow` and
//! `gxVSync` rows (`OptionsFrame.lua:14`, `:9`) and `gxResolution`, the windowed size and, in
//! fullscreen, the size the screen runs at.
//!
//! Deviation: `gxWindow "0"` is borderless fullscreen, not 1.12's exclusive mode-set at
//! `gxResolution`, because Wayland has no client mode-setting (winit ignores
//! `Fullscreen::Exclusive` there), and X11's XRandR changes the desktop's mode and a crash leaves
//! it changed. Bevy's `WindowMode::Fullscreen` also panics on a live change that cannot resolve a
//! monitor (`bevy_winit::winit_windows:91`, `system.rs:333`).
//!
//! The `gx*` rows are latched until `RestartGx()`, as in 1.12; a restart re-asserts them against
//! the window rather than re-creating the device, since wgpu reconfigures the surface live.
//!
//! Uncapped is `AutoNoVsync`, never `Immediate`: on Metal `Immediate` takes ~1 s `nextDrawable`
//! stalls ([`crate::capture::probe_uncap_mode`]).
//!
//! Deviation: on Windows, NVIDIA's Vulkan driver (610.88 seen) crashes inside `vkQueuePresentKHR`
//! on the first present after any swapchain resize while the present mode is `Fifo`, `FifoRelaxed`
//! or `AutoVsync` (a maximize is a resize). The fault is in the driver's own DXGI present
//! (`d3d11.dll`): it stays with `VK_LAYER_NV_present` disabled, and the validation layer reports
//! nothing before it. `Mailbox`, `AutoNoVsync` and DX12 survive, so there
//! "vsync on" is `AutoNoVsync` paced to the monitor's refresh rate ([`present_choice`]) because
//! that keeps the synced look without the crash. `$WOW_PRESENT=fifo` restores plain `Fifo`.

use benilla_ui::script::{ScreenResolution, UiScript};
use bevy::prelude::*;
use bevy::render::renderer::RenderAdapterInfo;
use bevy::window::{
    Monitor, MonitorSelection, PresentMode, PrimaryWindow, WindowMode, WindowResolution,
};
use std::time::{Duration, Instant};

/// `$WOW_NOVSYNC=1`: uncap for this session only; it never reaches `config.toml` (the
/// `session_owned` set in [`crate::cvars`]).
pub(crate) fn novsync_env() -> bool {
    std::env::var("WOW_NOVSYNC").as_deref() == Ok("1")
}

/// Whether this run sizes its own window (a capture, a background run) and so stays windowed.
/// Session-only: `gxWindow`/`gxResolution` are env-overridden while it holds, never saved over.
pub(crate) fn windowed_env() -> bool {
    std::env::var_os("WOW_WIN").is_some()
        || std::env::var_os("WOW_CAPTURE").is_some()
        || std::env::var_os("WOW_CAPTURE_UI").is_some()
        || benilla_world::bgwin::background_run()
}

/// `$WOW_WIN=WxH` in logical px; the one parser, so the size asked for and the size checked agree.
pub(crate) fn requested_window_size() -> Option<UVec2> {
    let v = std::env::var("WOW_WIN").ok()?;
    let (w, h) = v.split_once('x')?;
    Some(UVec2::new(w.parse().ok()?, h.parse().ok()?))
}

/// `$WOW_DPI=<f32>`: render at another display's pixel grid, since text rasterizing and snapping
/// quantize in device pixels. It overrides the window's scale factor, so `WOW_WIN` then means
/// physical pixels and a capture is the framebuffer that display would show.
pub(crate) fn requested_dpi() -> Option<f32> {
    let v: f32 = std::env::var("WOW_DPI").ok()?.parse().ok()?;
    (v.is_finite() && v > 0.0).then_some(v)
}

/// Apply [`requested_dpi`] to a window resolution; the one place the knob is spent.
pub(crate) fn at_requested_dpi(res: WindowResolution) -> WindowResolution {
    match requested_dpi() {
        Some(dpi) => res.with_scale_factor_override(dpi),
        None => res,
    }
}

/// The display modes benilla ships; neither is the reference's mode-setting fullscreen.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum DisplayMode {
    /// Borderless, filling the monitor: `gxWindow "0"`, the reference's default.
    #[default]
    Fullscreen,
    /// A window at [`VideoConfig::windowed`]. `gxWindow "1"`.
    Windowed,
}

/// The windowed size a fresh config gets.
pub(crate) const DEFAULT_WINDOWED: UVec2 = UVec2::new(1600, 900);

/// `gxWindow`'s value to the mode, by the reference's 0/1 parse (int, `!= 0`); shared by
/// [`crate::cvars`]'s arm and the boot read.
pub(crate) fn display_from_flag(v: f32) -> DisplayMode {
    if v != 0.0 {
        DisplayMode::Windowed
    } else {
        DisplayMode::Fullscreen
    }
}

/// `gxResolution`'s value (`"1280x800"`, parsed by the reference with `sscanf("%d%c%d")`) to a
/// size. A zero extent is refused rather than handed to the windowing system.
pub(crate) fn parse_resolution(value: &str) -> Option<UVec2> {
    let (w, h) = value.split_once(['x', 'X'])?;
    let size = UVec2::new(w.trim().parse().ok()?, h.trim().parse().ok()?);
    (size.x > 0 && size.y > 0).then_some(size)
}

/// The `WindowMode` a display mode means, on a given monitor. `maximize` is `gxMaximize`, which
/// counts only while windowed: the reference's window rebuild (`0x58cf10`) gives a windowed,
/// maximized window the popup style `0x90000000`, no caption and no border, sized to the screen
/// (`GetSystemMetrics` 0 and 1) at its origin, which is a borderless window over the monitor.
pub(crate) fn window_mode(
    display: DisplayMode,
    maximize: bool,
    monitor: MonitorSelection,
) -> WindowMode {
    match (display, maximize) {
        (DisplayMode::Fullscreen, _) | (DisplayMode::Windowed, true) => {
            WindowMode::BorderlessFullscreen(monitor)
        }
        (DisplayMode::Windowed, false) => WindowMode::Windowed,
    }
}

/// The mode the primary window is born in, resolved before the `App` exists so a fullscreen launch
/// does not flash windowed until `Startup`. `MonitorSelection::Primary`, since `Current` has no
/// answer before the window exists (`bevy_winit::select_monitor`).
pub(crate) fn boot_window_mode() -> WindowMode {
    // Quiet and lazy: the log plugin does not exist yet, and a config holding both rows needs no
    // locale at all.
    boot_window_mode_for(
        || crate::ui_script::boot_client_locale().index(),
        crate::cvars::boot_cvar,
        windowed_env(),
    )
}

/// [`boot_window_mode`] on a client whose locale index `locale` answers, asked only when a row is
/// not stored, with `stored` answering for `config.toml`: a row the file does not hold boots at its
/// registered default there, which zhCN's differs from the other locales' (`0x63a738`). A stored
/// row reads as the load then applies it, through the record's float and integer
/// ([`crate::cvars::CvarChanged::num`], [`crate::cvars::CvarChanged::flag`]), so text that is no
/// number reads 0 on both.
fn boot_window_mode_for(
    locale: impl Fn() -> usize,
    stored: impl Fn(&str) -> Option<String>,
    windowed_env: bool,
) -> WindowMode {
    let text = |name: &str| {
        stored(name)
            .or_else(|| crate::cvars::registered_default(name, locale()).map(str::to_string))
    };
    // A run that sizes its own window owns both rows for the session.
    let (display, maximize) = if windowed_env {
        (DisplayMode::Windowed, false)
    } else {
        (
            text("gxWindow").map_or_else(DisplayMode::default, |v| {
                display_from_flag(crate::cvars::record_float(&v))
            }),
            text("gxMaximize").is_some_and(|v| benilla_ui::script::s_str_to_int(&v) != 0),
        )
    };
    window_mode(display, maximize, MonitorSelection::Primary)
}

/// The windowed size the primary window is born at, `gxResolution`, read as [`boot_window_mode`]
/// is; `bevy_winit` ignores it while fullscreen.
pub(crate) fn boot_windowed_size() -> UVec2 {
    crate::cvars::boot_cvar("gxResolution")
        .and_then(|v| parse_resolution(&v))
        .unwrap_or(DEFAULT_WINDOWED)
}

/// The video knobs a CVar write lands on. Defaults read only the environment; `load_config`
/// applies the file at `Startup`, which the boot window already matches.
// MONKEY (lighting): no `Eq` — `shadow_distance` is an f32 (`PartialEq` is enough for `!=`).
#[derive(Resource, Clone, Copy, PartialEq, Debug)]
pub(crate) struct VideoConfig {
    pub(crate) vsync: bool,
    /// Water tier: 0 Classic, 1 Enhanced (default), 2 High with opt-in mirror reflections.
    /// Published to the water renderer by `dynamic_interior::bridge`.
    pub(crate) water_quality: u8,
    // MONKEY (volumetric fog): live camera raymarch tier: 0 Off, 1 Low, 2 High.
    pub(crate) volumetric_fog: u8,
    // MONKEY (post): emissive HDR + bloom quality, 0 Off / 1 Low / 2 High.
    pub(crate) bloom: u8,
    // MONKEY (post): depth-occluded radial sun shafts.
    pub(crate) sun_shafts: bool,
    // MONKEY (post): zone/day-night LUT grading at the world-to-UI boundary.
    pub(crate) color_grading: bool,
    // MONKEY (sky): the sky tier: 0 Classic, 1 Enhanced, 2 High (`sky_quality::bridge`).
    pub(crate) sky_quality: u8,
    // MONKEY (ao): screen-space contact shadows: 0 Off, 1 Low, 2 High.
    pub(crate) ambient_occlusion: u8,
    // MONKEY (lampfog): point-light fog tier: 0 Off, 1 nearest 16, 2 nearest 32.
    pub(crate) lamp_fog: u8,
    // GFX (volumetric light): shadow-mapped sun/moon light shafts, 0 Off / 1 Medium / 2 High.
    pub(crate) volumetric_light: u8,
    // GFX (volumetric light): the shafts' brightness multiplier, 0..2 (1 = the shipped tuning).
    pub(crate) volumetric_light_strength: f32,
    // GFX (moonlight): the additive moon term's multiplier, 0..2 (0 = the stock night exactly).
    // Bridged to `benilla_world::lighting::MoonLight` by `dynamic_interior::bridge`.
    pub(crate) moon_light: f32,
    // MONKEY (ground relief): terrain relief mode 0..5 (0 = today's terrain exactly; 1 normals,
    // 2 + parallax, 3 + parallax self-shadow, 4/5 debug) and the parallax depth (tiled-UV units).
    // Bridged to `benilla_world::terrain_stream::GroundRelief` by `dynamic_interior::bridge`.
    pub(crate) ground_relief: u8,
    pub(crate) ground_relief_depth: f32,
    /// MONKEY (ground height blend): height-aware layer blending toggle + its depth.
    pub(crate) ground_height_blend: bool,
    pub(crate) ground_height_blend_depth: f32,
    /// MONKEY (ground anti-tile): mode 0 off / 1 macro / 2 + far dual-scale, and strength 0..2.
    pub(crate) ground_anti_tile: u8,
    pub(crate) ground_anti_tile_strength: f32,
    // MONKEY (model relief): WMO + doodad relief mode 0..5 (0 = today's models exactly; 1 normals,
    // 2/3 + WMO parallax, 4/5 debug) and the parallax depth (base-texture UV units). Bridged to
    // `benilla_world::model_render::ModelRelief` by `dynamic_interior::bridge`.
    pub(crate) model_relief: u8,
    pub(crate) model_relief_depth: f32,
    /// Brightness of lava lighting its surroundings, 0..4; 0 disables the glow.
    /// Published to `benilla_world::lighting::LavaLightGain` by `dynamic_interior::bridge`.
    pub(crate) lava_light_gain: f32,
    /// Whether the STATIC WORLD (trees, buildings, foliage) casts realtime shadows and baked MCSH
    /// terrain shadows are suppressed. Independent of [`Self::character_shadows`] — either drives
    /// the shared shadow rig (`character_shadow` / `world_shadow`).
    pub(crate) world_shadows: bool,
    /// Whether CHARACTERS (players, NPCs, creatures, mounts) cast realtime silhouettes instead of
    /// the legacy oval blob. Independent of [`Self::world_shadows`].
    pub(crate) character_shadows: bool,
    /// Realtime-shadow render distance in yards (the `shadowDistance` slider) — the shadow-map
    /// cascade range + caster reach. Clamped to `shadow_core::SHADOW_DISTANCE_RANGE`.
    pub(crate) shadow_distance: f32,
    /// MONKEY (sun shadow perf): the directional shadow map's edge in texels (`shadowMapSize`;
    /// 1024/2048/4096, default 2048). The rig shipped a 4096 literal, and at 1080p the two sun
    /// lanes measured ~5 ms/frame together — a shadow map is quadratic in this number, so halving
    /// the edge quarters the pass's fill AND the depth texture (4096² D32 = 64 MB, 2048² = 16 MB).
    /// 2048 over one 80 yd cascade is ~26 texels/yd, still finer than the receivers' Gaussian
    /// kernel resolves. Applied to Bevy's `DirectionalLightShadowMap` resource, which
    /// `extract_lights` re-publishes on change and `prepare_lights` re-keys the texture cache with
    /// — so the map is genuinely re-created, live, with no restart.
    pub(crate) shadow_map_size: u32,
    /// MONKEY (sun shadow perf): the receivers' PCF kernel (`shadowFilter`; 0 = Hardware2x2, one
    /// hardware comparison sample, 1 = Gaussian, nine). Default **1** — the current look — because
    /// this one is a taste call the user A/Bs, not a free win: `0` is 9× fewer shadow-map fetches
    /// per lit fragment (the RECEIVER half of the cost, where `shadowMapSize` is the caster half)
    /// at the price of visibly stair-stepped edges. Applied BOTH to the world camera's
    /// `ShadowFilteringMethod` (which keys every Bevy-material receiver — terrain, `wow_model`)
    /// AND to the retained `static_gx` pipeline's shader def, which is specialized by hand and
    /// would otherwise keep whichever branch was compiled in.
    pub(crate) shadow_filter: u32,
    /// MONKEY (sun shadow perf): Hz cap on the CHARACTER lane's proxy re-skin
    /// (`characterShadowRate`, 0..120, default 30; `0` = every frame, the pre-cvar behaviour). The
    /// lane CPU-skins every admitted unit and mutates a `Mesh` asset, which costs a full
    /// vertex+index re-upload — the character lane's ~3 ms. The shadow MAP is still rendered every
    /// frame from the last proxy, so a capped rate does not flicker; it only lets a running NPC's
    /// silhouette lag by up to 1/rate s.
    pub(crate) character_shadow_rate: u32,
    /// MONKEY (sun shadow perf): the same cap for the WORLD lane's per-frame ENTITY caster
    /// (`worldShadowRate`, 0..120, default 30) — gameobjects, distance-faded doodads, WMO props.
    /// A SEPARATE row from [`Self::character_shadow_rate`] on purpose: its population is nearly
    /// static (a swinging lamp, a fading doodad) where the character lane's is animated every
    /// frame, so it tolerates a much lower rate — and one dial named for characters silently
    /// governing the world lane is the kind of thing nobody finds again. The world lane's STATIC
    /// casters are untouched: they already rebuild only on 16 yd camera drift.
    pub(crate) world_shadow_rate: u32,
    /// MONKEY (sun shadow perf): multiplier on the CASTER-COLLECTION reach (`shadowCasterReach`,
    /// 0.25..2, default 1 = unchanged). Collection reaches past the resolve range on purpose (a
    /// tree standing outside the cascade still throws a shadow into it), which at `shadowDistance`
    /// 80 admits 112 yd of entities and up to 204 yd of statics, with `NoFrustumCulling` on the
    /// proxies. Trimming it is the direct dial on caster POPULATION — the input to both lanes'
    /// per-rebuild cost — at the risk of a tall caster's shadow popping in as you approach.
    pub(crate) shadow_caster_reach: f32,
    /// MONKEY (moon shadows): how dark a MOON-shadowed fragment is allowed to get at night
    /// (`moonShadowStrength`, 0..1, default 0.35 = the fragment keeps 65 % of the night
    /// directional term). Bridged to [`benilla_world::lighting::MoonShadowStrength`] by
    /// `dynamic_interior`, beside the spell gain and for the same reasons — one live `f32`, one
    /// resource, one guard. `0` is the faithful null: every consumer of the resource guards on it,
    /// so a night at 0 renders as the build before the moon lane existed.
    pub(crate) moon_shadow_strength: f32,
    /// MONKEY (dynamic interiors): WMO interiors + their props light from the room's live fixtures
    /// (`interiorLight`) instead of the baked path. The three knobs are `interiorAmbient` (base
    /// ambient, 0..1), `interiorFill` (per-fixture bounce gain, 0..2) and `interiorExposure`
    /// (light-budget multiplier, 0.25..8) — bridged to benilla-world by `dynamic_interior`.
    pub(crate) interior_light: bool,
    pub(crate) interior_ambient: f32,
    pub(crate) interior_fill: f32,
    pub(crate) interior_exposure: f32,
    /// MONKEY (soft falloff): live scale on every interior fixture's AUTHORED attenuation window
    /// (`interiorAttenScale`, 0..8) — the fixture's EFFECTIVE RADIUS is `authored end × this`. A
    /// WMO MOLT record's `+0x2c` (an M2 source buckets by intensity, its authored pair being a
    /// template default rather than a reach) is a "full brightness ends here" number, not a
    /// "nothing past here" one, so `1` gave a hard-edged disc at exactly the authored end. The
    /// default is **2.5**: a 5 yd candle now tails smoothly out to 12.5, reading ~⅓ of its 1 yd
    /// brightness at the authored 5 and ~8 % at 10. `0` still means "no window" (the 48 yd lane).
    /// Bridged to benilla-world's `DynamicInteriors::atten_scale`, which the light packer folds
    /// into each interior entry's packed radius, so the dial moves the frame it changes.
    pub(crate) interior_atten_scale: f32,
    /// MONKEY (torch shadows, Stage B): whether interior fixtures cast real shadows (the nearest few
    /// promoted to cube-map casters — `torch_shadow`). Only meaningful with `interior_light` on.
    pub(crate) interior_shadows: bool,
    /// MONKEY (outdoor torch shadows): whether EXTERIOR fire lights (campfires, braziers,
    /// lampposts, bonfires — the point table's exterior half, colour row `.w == 0`) cast real
    /// cube-map shadows onto WMO outdoor surfaces, doodads and models AT NIGHT (`exteriorShadows`,
    /// default on). Deliberately NOT gated on `interior_light`: the exterior receivers were never
    /// part of the dynamic-interior feature and draw identically with it off. By day the lane is
    /// inert on both sides — no candidates, no maps, and the receivers' own `night_w` is exactly 0
    /// — so this dial has no daylight effect to have.
    ///
    /// It shares `interior_shadow_casters`' sixteen resident cube slots, capped at half of them
    /// (`torch_shadow::exterior_budget`) so a village square cannot evict an inn's candles.
    pub(crate) exterior_shadows: bool,
    /// MONKEY (daylight: terrain torch casters): whether the GROUND casts into an exterior torch's
    /// cube map (`torchTerrainShadows`, default off; High = on) — a hill or bank between a fire and
    /// the slope behind it blocks the fire. Only settled exterior slots gather it
    /// (`torch_shadow`); `0` leaves the torch lane exactly as it was.
    pub(crate) torch_terrain_shadows: bool,
    /// MONKEY (static torch cache): resident fixture budget (1..16, default 12). Static
    /// geometry renders only on promotion/residency changes; lowering this fades extra slots out.
    pub(crate) interior_shadow_casters: u32,
    /// MONKEY (static torch cache): nearest promoted fixtures with per-frame entity overlays
    /// (0..16, default 4). Zero keeps all static shadows and disables only the moving casters.
    pub(crate) interior_shadow_dynamic: u32,
    /// MONKEY (torch lane perf): how often (Hz) the moving-caster mesh is REGATHERED
    /// (`interiorShadowEntityRate`, 0..240, default 30; `0` = every frame, the pre-feature
    /// behaviour). The gather CPU-skins every admitted unit inside the dynamic fixtures' reach and
    /// then MUTATES the aggregate `Mesh` asset, which costs a full vertex+index re-extraction and
    /// GPU re-upload plus an `AssetChanged<Mesh3d>` fan-out through material specialisation - a
    /// fixed per-frame charge that neither `interiorShadowCasters` nor `interiorShadowDynamic`
    /// could reduce (both were measured to change nothing). The six overlay passes still run EVERY
    /// frame from the LAST mesh, so lowering this cannot blink a shadow off; it only ages the pose
    /// the mesh was gathered at. At 30 Hz on a 46 fps frame that is "regather about two frames in
    /// three", and a walking NPC's shadow lags its body by at most one frame's stride.
    pub(crate) interior_shadow_entity_rate: u32,
    /// MONKEY (torch caster selection): the PCF tap-radius scale for the torch maps
    /// (`interiorShadowSoft`, 0.5..3, default **1.5**). A candle cluster casts many hard-edged
    /// overlapping shadows; widening the 4-tap kernel is the cheap softening. Rides the torch
    /// table's `count.y` (as `x100`, LOW half) rather than a `DynamicInteriors` field, because it
    /// belongs to the shadow table's own bytes.
    ///
    /// MONKEY (pcss): it is now the CONTACT radius, not the radius everywhere — the projector's
    /// blocker search grows the kernel with the receiver's distance from its caster and clamps at
    /// 4x this. So this dial sets how sharp the sharpest edge in the scene is, and 1 (the old
    /// default) now reads sharper at a contact than it used to read anywhere; 1.5 restores the
    /// shipped softness at a contact and lets the penumbra open up from there.
    pub(crate) interior_shadow_soft: f32,
    /// MONKEY (shadow floor): how much of the DIRECT term a torch shadow removes
    /// (`torchShadowStrength`, 0..1, default **0.7**). A torch map is the only occlusion the
    /// direct arm has, so a blocked fragment used to lose all of it — the pitch-black razor-edged
    /// "scars" the Darkmoon tents printed on the grass and the Darkshire chairs printed on the inn
    /// floor. Nothing in this renderer bounces light, so the 30 % left standing at the default IS
    /// the bounce. Rides the torch table's `count.y` HIGH half beside `interior_shadow_soft`, and
    /// the receivers fold it into the slot's cross-fade weight (one multiply, no extra tap), so it
    /// touches the direct arm only — fill and ambient never saw this factor. `1` restores the
    /// shipped look exactly; `0` turns torch shadows off without disturbing the lane behind them.
    pub(crate) torch_shadow_strength: f32,
    /// MONKEY (room gate): whether an interior fixture may only light the ROOMS IT CLAIMS
    /// (`interiorRoomGate`, default on). Off = the pre-gate behaviour, where every interior fixture
    /// in range lights every interior surface in range and the only occlusion is the handful of
    /// promoted cube-shadow casters — the live A/B for "did the gate darken this room, or was it
    /// always unlit?". Bridged to benilla-world's `DynamicInteriors::room_gate`, which the light
    /// packer applies at PACK time (an ungated pack is one `count = 0` head per light), so it moves
    /// the frame it changes and costs the shader nothing.
    pub(crate) interior_room_gate: bool,
    /// MONKEY (interior debug): the interior-lane diagnostic overlay (`interiorDebug`, 0..4). See
    /// [`benilla_world::lighting::DynamicInteriors::debug`].
    pub(crate) interior_debug: u32,
    /// MONKEY (darkness gains): the exterior night dim (`nightGain`, 0.2..1.5, default **0.45** =
    /// nights 55 % darker). Bridged to `DynamicInteriors::night_gain`, which the light packer folds
    /// into the packed ambient/diffuse/specular rows on a `mix(1, gain, night_w)` ramp — so it is
    /// exactly inert while the sun is up and live the frame it changes after dark.
    pub(crate) night_gain: f32,
    /// MONKEY (lighting debug panel): the interior dim (`interiorGain`, 0.2..1.5, default **0.5** =
    /// room inputs 50 % weaker). Bridged to `DynamicInteriors::interior_gain`, which scales the room
    /// lane's INPUTS (base ambient, per-fixture fill, every interior fixture's colour) and not
    /// `interiorExposure` — that stays the user's own dial, and this composes with it.
    pub(crate) interior_gain: f32,
    /// MONKEY (enclosed day floor): the DAYLIGHT floor a room inside a building gets by day
    /// (`interiorDaylight`, 0..1, default **0.12**). Bridged to
    /// [`benilla_world::lighting::DynamicInteriors::daylight`], packed into the free fraction of
    /// the interior lane's on/off word, and added to the room law's ambient budget for batches the
    /// record table flags as enclosed. `0` restores the pre-feature look exactly; the night look is
    /// unaffected at any value (the term is scaled by the sun's own day envelope).
    pub(crate) interior_daylight: f32,
    /// MONKEY (fix-daylight): split a district's oversized window batch into window-sized
    /// daylight apertures (`daylightWindowSplit`, default on = the merged behaviour; a future
    /// High-only preset member). Applies to WMOs loaded after a change.
    pub(crate) daylight_window_split: bool,
    /// MONKEY (bake floor): the share of a WMO interior batch's OWN MOCV bake every interior-lane
    /// fragment keeps whether or not a fixture reaches it (`interiorBakeFloor`, 0..1, default
    /// **0.12**). Bridged to [`benilla_world::lighting::DynamicInteriors::bake_floor`], packed
    /// (times `interiorGain`) into the free fraction of the world-shadow lane, and added to the
    /// room law's budget inside its rolloff. It is what stops a room the fixture table cannot
    /// reach — the Lion's Pride Inn's east vestibule — rendering black; `0` restores the
    /// pre-feature look exactly.
    pub(crate) interior_bake_floor: f32,
    /// MONKEY (fire GO lights): gain on every light SYNTHESISED from a fire prop's flame emitter
    /// (`fireLightGain`, 0..4; `0` = the invented-light lane off). Bridged to benilla-world's
    /// [`benilla_world::lighting::FireLightGain`] by `dynamic_interior`, and applied at PACK time
    /// so it is live.
    pub(crate) fire_light_gain: f32,
    /// MONKEY (spellLightGain): gain on every light a SPELL EFFECT invented — a kit's aura glow, a
    /// missile's core, an impact flash, a firework shell's burst (`spellLightGain`, 0..4; `0` = the
    /// spell-light lane off). Bridged to benilla-world's
    /// [`benilla_world::lighting::SpellLightGain`] by `dynamic_interior` and applied at PACK time,
    /// so it is live. Deliberately NOT folded into `fireLightGain`: a spell light is tagged
    /// synthetic too, and one dial over both would mean turning the world's hearths down darkened
    /// every fireball in the game.
    pub(crate) spell_light_gain: f32,
    /// MONKEY (flame flicker): how strongly every FLAME's brightness wobbles (`fireFlicker`, 0..2;
    /// `1` = the authored per-kind amplitudes, `0` = the pre-feature steady constants, `2` =
    /// doubled). Bridged to `DynamicInteriors::flicker` and applied at PACK time, so it is live —
    /// and separate from `fireLightGain`, which scales only the SYNTHESISED lane while a flicker
    /// belongs to authored wall torches too.
    pub(crate) fire_flicker: f32,
    pub(crate) display: DisplayMode,
    /// `gxMaximize`: a windowed window fills the monitor, borderless ([`window_mode`]).
    pub(crate) maximize: bool,
    /// The windowed size, `gxResolution`'s own value. Kept while fullscreen, where the row
    /// answers the screen's size ([`show_screen_size`]), so leaving it can restore it.
    pub(crate) windowed: UVec2,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            vsync: !novsync_env(),
            world_shadows: true,
            character_shadows: true,
            shadow_distance: crate::shadow_core::DEFAULT_SHADOW_DISTANCE,
            // MONKEY (sun shadow perf): 2048, not the rig's old 4096 literal — see the field docs.
            shadow_map_size: crate::shadow_core::DEFAULT_SHADOW_MAP_SIZE,
            shadow_filter: crate::shadow_core::DEFAULT_SHADOW_FILTER,
            character_shadow_rate: crate::shadow_core::DEFAULT_SHADOW_RATE,
            world_shadow_rate: crate::shadow_core::DEFAULT_SHADOW_RATE,
            shadow_caster_reach: 1.0,
            // MONKEY (moon shadows): 0.35 — a hint of a silhouette, not a daylight-hard shadow.
            moon_shadow_strength: 0.35,
            // The cvar defaults are the source of truth at load; these only stand in until then.
            interior_light: true,
            interior_ambient: 0.015,
            interior_fill: 0.08,
            interior_exposure: 2.5,
            // MONKEY (soft falloff): 2.5, not 1 — see the field doc.
            interior_atten_scale: 1.6,
            interior_shadows: true,
            // MONKEY (outdoor torch shadows): on — a night campfire with no shadow is the thing
            // this lane exists to fix, and it costs nothing whenever the sun is up.
            exterior_shadows: true,
            torch_terrain_shadows: false,
            // MONKEY (static torch cache): 12 resident maps, four moving-caster overlays.
            interior_shadow_casters: 12,
            interior_shadow_dynamic: 4,
            // MONKEY (torch lane perf): 30 Hz - see the field doc.
            interior_shadow_entity_rate: 30,
            // MONKEY (pcss): 1.5 — see the field doc; `soft` is now the CONTACT radius.
            interior_shadow_soft: 1.5,
            // MONKEY (shadow floor): 0.7 — a shadow takes 70 % of the direct term, not all of it.
            torch_shadow_strength: 0.7,
            // MONKEY (room gate): on — without it a building's fixtures light through its own
            // floors and walls.
            interior_room_gate: true,
            interior_debug: 0,
            // MONKEY (lighting debug panel): nights 20 % darker, interior inputs 50 % weaker.
            night_gain: 0.45,
            interior_gain: 0.5,
            // MONKEY (enclosed day floor): calibrated so the Goldshire inn's entry floor reads
            // ~50 % of the sunlit threshold beside it — see `lighting::DAYLIGHT_LANE_SCALE`.
            interior_daylight: 0.0,
            daylight_window_split: true,
            // MONKEY (bake floor): an eighth of the authored bake — measured to lift the inn's
            // black door band from 0.019 to 0.108 x tex while moving candle-lit surfaces by
            // under 10 % (see `lighting::DynamicInteriors::bake_floor`).
            interior_bake_floor: 0.12,
            fire_light_gain: 1.0,
            spell_light_gain: 1.0,
            // MONKEY (planar water): High water (the planar mirror's tier) is the shipped High
            // preset's, measured ~1 ms (gfx-1009 water report).
            water_quality: 2,
            // MONKEY (volumetric fog): default to the inexpensive atmosphere.
            volumetric_fog: 1,
            // MONKEY (post): the shipped High graphics preset uses the full-resolution tier.
            bloom: 2,
            // MONKEY (post): part of the shipped High graphics preset.
            sun_shafts: true,
            color_grading: true,
            // MONKEY (sky): Classic until a preset or the player picks a tier.
            sky_quality: 0,
            // MONKEY (ao): opt-in; the future Graphics preset sets High = 2.
            ambient_occlusion: 0,
            // MONKEY (lampfog): opt-in; zero is exactly the pre-lane render.
            lamp_fog: 0,
            // GFX (volumetric light) / (moonlight): opt-in; the Graphics Preset turns them on.
            volumetric_light: 0,
            volumetric_light_strength: 1.0,
            moon_light: 0.0,
            // MONKEY (ground relief): opt-in experiment, off in every preset.
            ground_relief: 0,
            ground_relief_depth: 0.06,
            ground_height_blend: true,
            ground_height_blend_depth: 0.2,
            ground_anti_tile: 1,
            ground_anti_tile_strength: 1.0,
            // MONKEY (model relief): opt-in experiment, off in every preset.
            model_relief: 0,
            model_relief_depth: 0.05, // MONKEY (model relief): yards
            lava_light_gain: 1.0,
            fire_flicker: 1.0,
            display: if windowed_env() {
                DisplayMode::Windowed
            } else {
                DisplayMode::default()
            },
            maximize: false,
            windowed: DEFAULT_WINDOWED,
        }
    }
}

/// The present mode a vsync setting means. On is `PresentMode::default()` (`Fifo`, never tears),
/// not `AutoVsync`, which resolves to `FifoRelaxed` and can tear on a late frame.
pub(crate) fn present_mode(vsync: bool) -> PresentMode {
    if vsync {
        PresentMode::default()
    } else {
        PresentMode::AutoNoVsync
    }
}

/// NVIDIA's PCI vendor id, as `wgpu::AdapterInfo::vendor` reports it.
const VENDOR_NVIDIA: u32 = 0x10DE;

/// `$WOW_PRESENT=fifo`: take plain `Fifo` for vsync on even where [`present_choice`] avoids it, to
/// test a driver again.
const PRESENT_ENV: &str = "WOW_PRESENT";

/// What vsync on means on a given adapter: the present mode, and whether the main loop must be
/// paced to the monitor's refresh rate because the mode itself no longer waits for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PresentChoice {
    pub(crate) mode: PresentMode,
    pub(crate) paced: bool,
}

/// The present mode for `vsync` on `adapter` (vendor id and backend; `None` before the renderer is
/// up), on Windows or not, with `$WOW_PRESENT` as `env`. Vsync off is [`present_mode`] everywhere.
/// Vsync on is `Fifo` ([`present_mode`]), except NVIDIA on Vulkan on Windows, where `Fifo` crashes
/// the driver after a resize: there it is `AutoNoVsync` (`Mailbox` > `Immediate` > `Fifo`, which
/// cannot fail validation), paced to the refresh rate so it still does not run flat out.
pub(crate) fn present_choice(
    vsync: bool,
    adapter: Option<(u32, wgpu::Backend)>,
    windows: bool,
    env: Option<&str>,
) -> PresentChoice {
    let avoid_fifo = vsync
        && windows
        && adapter == Some((VENDOR_NVIDIA, wgpu::Backend::Vulkan))
        && !env.is_some_and(|v| v.trim().eq_ignore_ascii_case("fifo"));
    if avoid_fifo {
        PresentChoice {
            mode: PresentMode::AutoNoVsync,
            paced: true,
        }
    } else {
        PresentChoice {
            mode: present_mode(vsync),
            paced: false,
        }
    }
}

/// The frame period for the fastest of `refresh_millihertz` (a monitor each; `None` where the OS
/// does not say). Fastest, since the window's monitor is not tracked: a cap never undershoots the
/// screen it is on. 60 Hz when none says.
pub(crate) fn refresh_period(
    refresh_millihertz: impl IntoIterator<Item = Option<u32>>,
) -> Duration {
    let mhz = refresh_millihertz
        .into_iter()
        .flatten()
        .filter(|&m| m > 0)
        .max()
        .unwrap_or(60_000);
    Duration::from_secs_f64(1000.0 / f64::from(mhz))
}

/// How much longer the frame that began at `last` must wait to last a whole `period`.
fn pace_wait(last: Option<Instant>, now: Instant, period: Duration) -> Duration {
    last.map_or(Duration::ZERO, |last| {
        period.saturating_sub(now.saturating_duration_since(last))
    })
}

/// Whether [`pace_to_refresh`] holds the loop; set by [`apply_present_mode`].
#[derive(Resource, Default, PartialEq, Eq, Debug)]
pub(crate) struct RefreshPacing(pub(crate) bool);

/// Sleep out the rest of the frame so a non-waiting present mode runs at the monitor's rate. A
/// frame that overran starts the next period from now: no catch-up burst.
fn pace_to_refresh(
    pacing: Res<RefreshPacing>,
    monitors: Query<&Monitor>,
    mut last: Local<Option<Instant>>,
) {
    if !pacing.0 {
        return;
    }
    let period = refresh_period(monitors.iter().map(|m| m.refresh_rate_millihertz));
    let wait = pace_wait(*last, Instant::now(), period);
    if !wait.is_zero() {
        // A sleep overshoots by up to a timer tick, so the last millisecond spins.
        const SPIN: Duration = Duration::from_millis(1);
        let deadline = Instant::now() + wait;
        if wait > SPIN {
            std::thread::sleep(wait - SPIN);
        }
        while Instant::now() < deadline {
            std::hint::spin_loop();
        }
    }
    *last = Some(Instant::now());
}

pub(crate) struct VideoPlugin;

/// `widescreen` (`0x63a74f`, default "1"), as its readers take its integer: the resolution list
/// (`0x48bcac`) and the display aspect (`0x46ab40`, `0x4901e0`).
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Widescreen(pub(crate) bool);

impl Default for Widescreen {
    fn default() -> Self {
        Self(true)
    }
}

/// The knobs the video rows' change callbacks write ([`VideoKnobs::on_cvar`]).
#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct VideoKnobs<'w> {
    cfg: ResMut<'w, VideoConfig>,
    view: ResMut<'w, benilla_world::view::ViewDistance>,
    msaa: ResMut<'w, benilla_world::view::MsaaSetting>,
    msaa_formats: Res<'w, benilla_world::view::MsaaFormats>,
    tex_filter: ResMut<'w, benilla_assets::TexFilterSetting>,
    clutter: ResMut<'w, benilla_world::clutter::ClutterConfig>,
    weather: ResMut<'w, benilla_world::weather::WeatherState>,
    particles: Option<ResMut<'w, benilla_world::particles::ParticleTuning>>,
    spell_effect_level: ResMut<'w, SpellEffectLevel>,
    ffx: ResMut<'w, benilla_world::ffx_glow::FfxSwitches>,
    widescreen: ResMut<'w, Widescreen>,
}

impl VideoKnobs<'_> {
    /// The change callbacks of the reference's video-options registration block (`0x688470`);
    /// each arm writes only its own knob.
    pub(crate) fn on_cvar(
        &mut self,
        ev: &crate::cvars::CvarChanged,
        cvars: &mut crate::cvars::Cvars,
    ) {
        let VideoKnobs {
            cfg,
            view,
            msaa,
            msaa_formats,
            tex_filter,
            clutter,
            weather,
            particles,
            spell_effect_level,
            ffx,
            widescreen,
        } = self;
        use benilla_world::view::{FARCLIP_RANGE, MSAA_RANGE};
        let v = ev.num();
        match ev.key().as_str() {
            // A value that is not a size is ignored with a warning.
            "gxresolution" => match parse_resolution(&ev.new) {
                Some(size) => cfg.windowed = size,
                None => warn!("cvar gxResolution: unparseable value '{}' ignored", ev.new),
            },
            "gxvsync" => cfg.vsync = ev.flag(),
            "widescreen" => widescreen.0 = ev.flag(),
            // The reference's polarity: `1` is windowed (the row is "Windowed Mode").
            "gxwindow" => cfg.display = display_from_flag(v),
            // ── MONKEY (lighting): the dynamic light + shadow system's 34 rows ────────────────────
            // MONKEY (lampfog): lampFog is one of these live VideoConfig rows too.
            // They live in THIS callback, and not in one of their own beside `shadow_core` /
            // `dynamic_interior`, because of the law the arm above states: *each arm writes only its
            // own resource*. Every one of these knobs IS a field of [`VideoConfig`] — the lanes read
            // that resource per frame (`shadow_core::update_shadows`, `dynamic_interior::bridge`,
            // `torch_shadow`), none of them owns a resource of its own — so a second callback beside
            // them would be a second writer of this one resource for no gain, splitting one match
            // over two files while dirtying exactly the same thing.
            //
            // What it DOES cost is the precision of `Res<VideoConfig>::is_changed()`: on upstream's
            // struct that signal means "a Video Options row moved", and here it means "a video OR a
            // lighting row moved". The one consumer that cares is `apply_present_mode`, which keeps
            // 2303's retired value compare for exactly this reason — see its doc.
            //
            // Clamps are each row's own, stated beside it, exactly as for the reference rows above;
            // the `ours(...)` entries in `cvars::REGISTERED` carry the matching defaults, and
            // MONKEY (lampfog): the two atmospheric tiers bring the defaults weld to 34 pairs.
            "waterquality" => cfg.water_quality = v.clamp(0.0, 2.0) as u8,
            // MONKEY (volumetric fog): constrain UI/console writes to supported tiers.
            "volumetricfog" => cfg.volumetric_fog = v.clamp(0.0, 2.0) as u8,
            // MONKEY (post): constrain UI/console writes to the supported bloom tiers.
            "bloom" => cfg.bloom = v.clamp(0.0, 2.0) as u8,
            // MONKEY (post): the shafts lane is binary.
            "sunshafts" => cfg.sun_shafts = v != 0.0,
            "colorgrading" => cfg.color_grading = v != 0.0,
            // MONKEY (sky): the sky tier, clamped to Classic..High.
            "skyquality" => cfg.sky_quality = v.clamp(0.0, 2.0) as u8,
            // MONKEY (ao): constrain UI/console writes to supported tiers.
            "ambientocclusion" => cfg.ambient_occlusion = v.clamp(0.0, 2.0) as u8,
            // MONKEY (lampfog): 0 Off / 1 nearest 16 / 2 nearest 32.
            "lampfog" => cfg.lamp_fog = v.clamp(0.0, 2.0) as u8,
            // GFX (volumetric light): 0 Off / 1 Medium / 2 High, and its 0..2 strength.
            "volumetriclight" => cfg.volumetric_light = v.clamp(0.0, 2.0) as u8,
            "volumetriclightstrength" => cfg.volumetric_light_strength = v.clamp(0.0, 2.0),
            // GFX (moonlight): 0 is meaningful (the stock night, bit for bit).
            "moonlight" => cfg.moon_light = v.clamp(0.0, 2.0),
            // MONKEY (ground relief): 0 is meaningful (today's terrain, bit for bit).
            "groundrelief" => cfg.ground_relief = v.clamp(0.0, 5.0) as u8,
            "groundreliefdepth" => cfg.ground_relief_depth = v.clamp(0.0, 0.2),
            "groundheightblend" => cfg.ground_height_blend = v != 0.0,
            "groundheightblenddepth" => cfg.ground_height_blend_depth = v.clamp(0.01, 0.2),
            // MONKEY (ground anti-tile): 0 is meaningful (today's colour, bit for bit).
            "groundantitile" => cfg.ground_anti_tile = v.clamp(0.0, 2.0) as u8,
            "groundantitilestrength" => cfg.ground_anti_tile_strength = v.clamp(0.0, 2.0),
            // MONKEY (model relief): 0 is meaningful (today's models, bit for bit).
            "modelrelief" => cfg.model_relief = v.clamp(0.0, 5.0) as u8,
            "modelreliefdepth" => cfg.model_relief_depth = v.clamp(0.0, 0.2), // MONKEY (model relief): yards
            "lavalightgain" => cfg.lava_light_gain = v.clamp(0.0, 4.0),
            "worldshadows" => cfg.world_shadows = ev.flag(),
            "charactershadows" => cfg.character_shadows = ev.flag(),
            "shadowdistance" => {
                cfg.shadow_distance = v.clamp(
                    *crate::shadow_core::SHADOW_DISTANCE_RANGE.start(),
                    *crate::shadow_core::SHADOW_DISTANCE_RANGE.end(),
                )
            }
            // MONKEY (sun shadow perf): the five cost dials, clamped at the edge like every numeric row
            // here. `shadowMapSize` SNAPS onto the power-of-two ladder rather than clamping into a
            // range — an off-ladder value is not a weaker setting, it is one Bevy silently rounds UP
            // into a bigger and slower map than the one that was typed.
            "shadowmapsize" => {
                cfg.shadow_map_size = crate::shadow_core::clamp_shadow_map_size(v.max(0.0) as u32)
            }
            "shadowfilter" => {
                cfg.shadow_filter = (v.max(0.0) as u32).min(crate::shadow_core::MAX_SHADOW_FILTER)
            }
            // `0` is MEANINGFUL on both rate rows (the pre-cvar every-frame rebuild), so they floor at
            // 0 rather than at 1 — the shadow off-switches are `characterShadows` / `worldShadows`.
            "charactershadowrate" => {
                cfg.character_shadow_rate = (v.max(0.0) as u32).min(crate::shadow_core::MAX_SHADOW_RATE)
            }
            "worldshadowrate" => {
                cfg.world_shadow_rate = (v.max(0.0) as u32).min(crate::shadow_core::MAX_SHADOW_RATE)
            }
            "shadowcasterreach" => {
                cfg.shadow_caster_reach = v.clamp(
                    *crate::shadow_core::CASTER_REACH_RANGE.start(),
                    *crate::shadow_core::CASTER_REACH_RANGE.end(),
                )
            }
            // MONKEY (moon shadows): the night lane's shadow darkness. 0 IS meaningful (the lane off,
            // bit-identical to the pre-feature night), so it floors at 0 rather than at a
            // minimum-useful value; 1 is a daylight-hard silhouette by moonlight.
            "moonshadowstrength" => cfg.moon_shadow_strength = v.clamp(0.0, 1.0),
            // MONKEY (dynamic interiors): the interior lane's on/off + knobs, clamped at the edge like
            // every other numeric row. `dynamic_interior::bridge` publishes them to benilla-world.
            "interiorlight" => cfg.interior_light = ev.flag(),
            "interiorambient" => cfg.interior_ambient = v.clamp(0.0, 1.0),
            "interiorfill" => cfg.interior_fill = v.clamp(0.0, 2.0),
            "interiorexposure" => cfg.interior_exposure = v.clamp(0.25, 8.0),
            // MONKEY (interior attenuation): the authored-window scale. `0` is a MEANINGFUL value here
            // (the window off), so the range floors at 0 rather than at a small positive.
            "interiorattenscale" => cfg.interior_atten_scale = v.clamp(0.0, 8.0),
            "interiorroomgate" => cfg.interior_room_gate = ev.flag(),
            "interiorshadows" => cfg.interior_shadows = ev.flag(),
            // MONKEY (outdoor torch shadows): a flag like every other checkbox here. Live — the lane
            // reads `VideoConfig` every frame, so `0` fades the outdoor shadows out (the slots evict
            // through the same cross-fade a walked-away fixture does) and `1` fades them back in.
            "exteriorshadows" => cfg.exterior_shadows = ev.flag(),
            // MONKEY (daylight: terrain torch casters)
            "torchterrainshadows" => cfg.torch_terrain_shadows = ev.flag(),
            // MONKEY (torch caster selection): the working-set size and the PCF radius, clamped at the
            // edge like every other numeric row. `casters` floors at 1, not 0 — `interiorShadows 0` is
            // already the off switch, and a 0 here would be a second, confusing one.
            "interiorshadowcasters" => cfg.interior_shadow_casters = (v.max(1.0) as u32).clamp(1, 16),
            // MONKEY (static torch cache): the live bank rank, 1..`MAX_TORCH_DYNAMIC`.
            "interiorshadowdynamic" => {
                cfg.interior_shadow_dynamic =
                    (v.max(1.0) as u32).clamp(1, crate::torch_shadow::MAX_TORCH_DYNAMIC as u32)
            }
            // MONKEY (torch lane perf): the moving-caster regather cadence in Hz. `0` is MEANINGFUL
            // here (every frame -- the behaviour before the gate), so unlike `casters` this floors at
            // 0 rather than at 1. Ceiling 240 so a typo cannot ask for a per-frame rebuild AND a
            // divide by a huge number; anything at or above the frame rate is already "every frame".
            "interiorshadowentityrate" => {
                cfg.interior_shadow_entity_rate = (v.max(0.0) as u32).min(240)
            }
            "interiorshadowsoft" => cfg.interior_shadow_soft = v.clamp(0.5, 3.0),
            // MONKEY (shadow floor): 0 IS meaningful (shadows off), so this floors at 0, not at a
            // minimum-useful value; 1 is the pre-feature pitch black.
            "torchshadowstrength" => cfg.torch_shadow_strength = v.clamp(0.0, 1.0),
            "interiordebug" => cfg.interior_debug = (v.max(0.0) as u32).min(4),
            // MONKEY (darkness gains): the two dim dials, clamped at the edge like every numeric row
            // here. The floor is 0.2 rather than 0: a true 0 would be indistinguishable from a broken
            // light pack (black world / black room), and the off switch people actually want is `1`.
            "nightgain" => cfg.night_gain = v.clamp(0.2, 1.5),
            "interiorgain" => cfg.interior_gain = v.clamp(0.2, 1.5),
            // MONKEY (enclosed day floor): 0 IS meaningful here (it restores the pre-feature look
            // exactly), unlike the two dim dials above whose 0 would be a broken-looking world.
            "interiordaylight" => cfg.interior_daylight = v.clamp(0.0, 1.0),
            // MONKEY (fix-daylight)
            "daylightwindowsplit" => cfg.daylight_window_split = ev.flag(),
            // MONKEY (bake floor): 0 IS meaningful here too (it restores the pre-feature look exactly).
            // The upper clamp matters more than usual: the packer multiplies this by `interiorGain`
            // (up to 1.5) and rides the product in a lane fraction that must stay under 0.5 after
            // scaling, so a value that escaped this clamp would reach the world-shadow flag it shares
            // a lane with. `pack_bake_lane` clamps the product too — belt and braces, one at each end.
            "interiorbakefloor" => cfg.interior_bake_floor = v.clamp(0.0, 1.0),
            // MONKEY (fire GO lights): the synthesised-fire gain, clamped at the edge like the rest.
            "firelightgain" => cfg.fire_light_gain = v.clamp(0.0, 4.0),
            // MONKEY (spellLightGain): the spell lane's gain, same range and same edge clamp — and `0`
            // is meaningful here (the lane off) exactly as it is for the fire gain above.
            "spelllightgain" => cfg.spell_light_gain = v.clamp(0.0, 4.0),
            // MONKEY (flame flicker): 0..2 — the amplitudes are authored at 1, and 2 is the deliberate
            // over-drive for judging the shape. Clamped at the edge like every knob here.
            "fireflicker" => cfg.fire_flicker = v.clamp(0.0, 2.0),
            // ── end MONKEY (lighting) ─────────────────────────────────────────────────────────────
            // Latched like `gxWindow`: the commit is `RestartGx`, which rebuilds the window
            // (`0x58cf10` reads `+0x09`); `apply_window_mode` is that rebuild.
            "gxmaximize" => cfg.maximize = ev.flag(),
            // The FFX pass's three switches, read every frame by the reference (`0x6cd8a6`, `0x6cc5a8`,
            // `0x6cdf10`), so a write shows on the next frame.
            "ffx" | "ffxglow" | "ffxdeath" => {
                match ev.key().as_str() {
                    "ffx" => ffx.master = ev.flag(),
                    "ffxglow" => ffx.glow = ev.flag(),
                    _ => ffx.death = ev.flag(),
                }
                info!("video: full-screen effects {:?}", **ffx);
            }
            // The registry refuses a value outside the callback's range (`0x688d40`); the clamp only
            // keeps the knob in it.
            "farclip" => view.farclip = v.clamp(*FARCLIP_RANGE.start(), *FARCLIP_RANGE.end()),
            // Clamped, where the reference refuses an out-of-range write and keeps the value
            // (`0x688d90` echoes "NearClip must be in range 0.01 - 0.33" and returns 0).
            "nearclip" => view.set_nearclip(v),
            // The reference's `atoi` and clamp to `[1, 16]` (`0x63b250`), then this GPU's ceiling:
            // an unoffered count is a wgpu validation error on the first frame. The camera reads it
            // once, at spawn.
            "gxmultisample" => {
                let asked = (v as u32).clamp(*MSAA_RANGE.start(), *MSAA_RANGE.end());
                let granted = msaa_formats.clamp(asked);
                if granted != asked {
                    warn!("cvar gxMultisample: this GPU does not offer {asked}x multisampling — using {granted}x");
                }
                msaa.samples = granted;
            }
            // Not applied until the next launch (the filter policy is published once at the end of
            // `CvarLoad`); the reference registers both with `flags = 1` and applies them live.
            // `anisotropic` is refused outside `[1, 16]` before it reaches here (`0x689110`,
            // `check_anisotropic`); the clamp covers a saved value, which the file read does not check.
            "trilinear" => tex_filter.trilinear = ev.flag(),
            "anisotropic" => {
                tex_filter.aniso = (v as u32).clamp(
                    *benilla_assets::ANISO_RANGE.start(),
                    *benilla_assets::ANISO_RANGE.end(),
                )
            }
            // The panel's 0/1/2 is the density multiplier x1/x2/x3, clamped to the 1.12 slider's
            // range. `frillDensity` is the same knob, mirrored so `GetCVar` never answers two levels.
            "worlddetail" => {
                clutter.density = v.clamp(0.0, 2.0) + 1.0;
                cvars.mirror(
                    benilla_ui::script::CVAR_FRILL_DENSITY,
                    &clutter.frill_density().to_string(),
                );
                // The stop's other half, as `SetWorldDetail` writes it, so a stop set as a CVar keeps
                // `SmallCull` in step too.
                cvars.mirror(
                    benilla_ui::script::CVAR_SMALL_CULL,
                    &benilla_ui::script::small_cull_text(v.clamp(0.0, 2.0) as usize),
                );
            }
            // The same knob in the reference's cells per chunk, which the registry holds to `[1, 256]`
            // (`0x688de0`); the callback hands on `SStrToInt`'s integer (`0x688dfa`), so "48.9" is 48.
            // The loaded tiles re-scatter off the change.
            "frilldensity" => {
                clutter.set_frill_density(benilla_ui::script::s_str_to_int(&ev.new) as f32);
                cvars.mirror(
                    benilla_ui::script::CVAR_WORLD_DETAIL,
                    &(clutter.density - 1.0).to_string(),
                );
            }
            // Weather Intensity 0..3: the reference's callback `0x67b870` jumps (`0x67b8e8`) to the
            // quality cells {0.1, 0.33, 0.66, 1.0} at `[0x8680ec]`, and refuses a value outside 0..=3
            // before it applies (`check_weather_density`); the clamp covers a saved value.
            "weatherdensity" => weather.weather_density = v.trunc().clamp(0.0, 3.0) as u8,
            // Spell Detail (`0x689510`): `SStrToInt`, clamped to [0, 2] in the handler's own copy,
            // echoed, then the emission scalar 0.33, 0.66 or 1.0 through `0x7adfb0`, shared with
            // `particleDensity`. The reference runs it on every write, an unchanged one included;
            // ours runs only on a change.
            "spelleffectlevel" => {
                spell_effect_level.0 = benilla_ui::script::s_str_to_int(&ev.new);
                let level = spell_effect_level.0.clamp(0, 2);
                cvars.print(format!("Spell effect level set to {level}."));
                if let Some(particles) = particles.as_deref_mut() {
                    particles.set_density(spell_effect_scale(level));
                }
            }
            _ => {}
        }
    }
}

/// The `spellEffectLevel` record's integer (`rec+0x28`, `SStrToInt` of the value), unclamped: what
/// the dynamic-object shard emitter reads at spawn (`0x6eb967`), where the handler clamps only its
/// own copy.
#[derive(Resource)]
pub(crate) struct SpellEffectLevel(pub(crate) i32);

impl Default for SpellEffectLevel {
    /// The registered "2".
    fn default() -> Self {
        Self(2)
    }
}

/// The `spellEffectLevel` emission factor, 0.33, 0.66 or 1.0: the handler's f32 immediates
/// (`0x68956a` `0x3ea8f5c3`, `0x689588` `0x3f28f5c3`, `0x689561` `0x3f800000`) and the shard
/// emitter's `.rdata` pair (`0x808300`, `0x81199c`) are the same three. Any level but 0 or 1 is 1.0.
pub(crate) fn spell_effect_scale(level: i32) -> f32 {
    match level {
        0 => 0.33,
        1 => 0.66,
        _ => 1.0,
    }
}

impl Plugin for VideoPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SpellEffectLevel>()
            .init_resource::<Widescreen>();
        app.init_resource::<VideoConfig>()
            .init_resource::<GxRestarts>()
            .init_resource::<RefreshPacing>()
            .add_systems(Last, pace_to_refresh)
            // Ahead of the glue's size callback (`ui_script::glue::watch_glue_aspect`) and of
            // every `GetCVar`, so both read the screen the window has this frame.
            .add_systems(PreUpdate, show_screen_size)
            .add_systems(Startup, (log_display_session, check_window_pinned).chain())
            .add_systems(
                Update,
                (
                    // After the tick and the CVar sync: the stock Okay handler calls `SetCVar` per
                    // row, then `RestartGx()`, so the staged rows are registered before the commit.
                    (drain_restart_gx, (apply_present_mode, apply_window_mode))
                        .chain()
                        .in_set(crate::ui_script::UiDrain)
                        .after(crate::cvars::sync_cvars),
                ),
            );
        #[cfg(windows)]
        app.add_message::<bevy::window::WindowCreated>()
            .add_systems(First, drop_accessibility_adapter);
    }
}

/// Drops the AccessKit adapter `bevy_winit` gives a new window, which restores winit's window
/// procedure. The reference's window serves no accessibility client (`WoW.exe` imports no such
/// library and names none of its calls), and benilla builds no tree for one to read. The adapter's
/// UI Automation provider is not inert where a client runs: Proton-based Wine's `tabtip.exe` queries
/// the focused window, and that traffic faults in `rpcrt4` on reused memory and damages the heap.
#[cfg_attr(not(windows), allow(dead_code))]
fn drop_accessibility_adapter(
    mut created: MessageReader<bevy::window::WindowCreated>,
    _main_thread: bevy::ecs::system::NonSendMarker,
) {
    bevy::winit::accessibility::ACCESS_KIT_ADAPTERS.with_borrow_mut(|adapters| {
        for created in created.read() {
            adapters.remove(&created.window);
        }
    });
}

/// How many `RestartGx()` calls the interface has made. A counter, not a flag: each applier keeps
/// its own last value, so one bump forces one re-assertion in each, in any order.
#[derive(Resource, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct GxRestarts(u32);

/// `gxRestart`'s handler (`0x639f60`) inside the call that ran it
/// ([`benilla_ui::script::ScriptCall::RestartGx`]): the latched `gx*` rows (flags `3`) committed,
/// where the reference's restart calls `CVar::Update` (`0x63e060`) on each (`0x639ec0`), their
/// callbacks run and the VM's mirror synced, so `GetCVar` reads the committed value on the
/// verb's next line. The device switch it asks, a [`GxRestarts`] bump, is the window appliers' in
/// the frame's drain: no script reads the window but through these rows.
#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct GxRestartSteps<'w> {
    cvars: crate::cvars::CvarSteps<'w>,
    /// Absent in a harness without the video plugin.
    restarts: Option<ResMut<'w, GxRestarts>>,
}

impl GxRestartSteps<'_> {
    /// `runs` restarts at once.
    pub(crate) fn restart(&mut self, script: &mut UiScript, runs: u32) {
        // The VM's writes no call took first, so a stage made before the restart is committed.
        self.cvars.sync(Some(script));
        let Some(cvars) = self.cvars.cvars() else {
            return;
        };
        let committed = cvars.commit_latched();
        if let Some(restarts) = self.restarts.as_deref_mut() {
            restarts.0 = restarts.0.wrapping_add(runs);
        }
        self.cvars.sync(Some(script));
        info!(
            "video: RestartGx — {committed} staged setting(s) committed; re-asserting the \
               display mode and present mode"
        );
    }
}

/// The restarts no call took, a VM's with no host lent ([`GxRestartSteps`]).
fn drain_restart_gx(mut vm: crate::script_calls::VmWith<GxRestartSteps<'static>>) {
    vm.turn(|script, mut steps| {
        let Some(script) = script else {
            return;
        };
        let runs = script.take_restart_gx_asks();
        // A `ResMut` deref-mut is a change signal, so a quiet frame touches nothing.
        if runs > 0 {
            steps.restart(script, runs);
        }
    })
}

/// The reference's three filters on the resolution list (`0x48bcfa` to `0x48bd18`): keep when
/// `w/h >= 1.248` (`[0x804570]`, just under 5:4, so square and portrait fail), `w >= 800` and
/// `h >= 600`.
fn offerable(r: ScreenResolution) -> bool {
    r.width >= 800 && r.height >= 600 && f64::from(r.width) / f64::from(r.height) >= 1.248
}

/// The reference's four hardcoded modes, in its append order (`0x48bda2`, `0x48bddf`, `0x48be43`,
/// `0x48bea7`), used when no enumerated mode survives [`offerable`], and alone when `widescreen`
/// is 0 (`0x48bcac`-`0x48bcc1`).
const SCREEN_FALLBACK: [ScreenResolution; 4] = [
    ScreenResolution {
        width: 800,
        height: 600,
    },
    ScreenResolution {
        width: 1024,
        height: 768,
    },
    ScreenResolution {
        width: 1280,
        height: 1024,
    },
    ScreenResolution {
        width: 1600,
        height: 1200,
    },
];

/// The rows `GetScreenResolutions` offers from the monitors' sizes and modes: [`offerable`] ones
/// in ascending area, else [`SCREEN_FALLBACK`], which `widescreen` off takes whatever the
/// display offers, never mixed with its modes (`0x48bd79`-`0x48bd91`).
fn offered_modes<'a>(
    monitors: impl IntoIterator<Item = &'a Monitor>,
    widescreen: bool,
) -> Vec<ScreenResolution> {
    if !widescreen {
        return SCREEN_FALLBACK.to_vec();
    }
    let mut offered: Vec<ScreenResolution> = Vec::new();
    for m in monitors {
        // The monitor's own scale, not the window's: these rows describe the panel.
        let scale = if m.scale_factor > 0.0 {
            m.scale_factor
        } else {
            1.0
        };
        let logical = |size: UVec2| ScreenResolution {
            width: (size.x as f64 / scale).round() as u32,
            height: (size.y as f64 / scale).round() as u32,
        };
        offered.push(logical(m.physical_size()));
        offered.extend(m.video_modes.iter().map(|v| logical(v.physical_size)));
    }
    offered.retain(|r| offerable(*r));
    if offered.is_empty() {
        offered.extend(SCREEN_FALLBACK);
    }
    offered.sort_by_key(|r| (u64::from(r.width) * u64::from(r.height), r.width, r.height));
    offered.dedup();
    offered
}

/// The host half of `GetScreenResolutions` / `GetCurrentResolution`: the monitors' mode sizes and
/// full sizes through [`offerable`], else [`SCREEN_FALLBACK`], with the live window size as the
/// current entry; a pick sets the windowed size, `gxResolution`. The engine adds the live window
/// size when missing, which `CT_Viewport.lua:201` needs. Deviation: sizes are logical px, because
/// the windowed `gxResolution` is a logical inner size here. Read at each verb ([`DisplayLive`]),
/// so the list follows the display and `widescreen`; the reference builds its list once and never
/// invalidates it.
fn display_modes<'a>(
    window: &Window,
    monitors: impl IntoIterator<Item = &'a Monitor>,
    widescreen: bool,
) -> (Vec<ScreenResolution>, Option<ScreenResolution>) {
    let res = &window.resolution;
    let current = Some(ScreenResolution {
        width: res.width() as u32,
        height: res.height() as u32,
    })
    .filter(|r| r.width > 0 && r.height > 0);
    (offered_modes(monitors, widescreen), current)
}

/// The resolution list read at the call, the call host's half of
/// [`benilla_ui::script::ScreenModesQuery`]: the reference builds it on the first use of any
/// resolution verb (`0x48bc90`), so a file-scope `GetScreenResolutions()` during the UI load
/// answers the full list ([`display_modes`]), and a listener's `SetCVar("widescreen")` filters the
/// next verb's (`0x48bcac`); the multisample list likewise
/// ([`benilla_ui::script::MultisampleFormatsQuery`]). Nothing pushes the VM a copy: each verb seats
/// what it read, and the VM's own list stands only where no host is lent.
#[derive(bevy::ecs::system::SystemParam)]
pub(crate) struct DisplayLive<'w, 's> {
    windows: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
    monitors: Query<'w, 's, &'static Monitor>,
    widescreen: Option<Res<'w, Widescreen>>,
    /// The device's multisample formats, which the multisample verbs read (`0x48c3e0`).
    msaa: Option<Res<'w, benilla_world::view::MsaaFormats>>,
}

impl crate::script_calls::live::LiveFamily for DisplayLive<'_, '_> {
    fn answer(&mut self, ask: &mut benilla_ui::script::live::LiveAsk<'_>) -> bool {
        ask.answer(|_: &benilla_ui::script::ScreenModesQuery| {
            let window = self.windows.single().ok()?;
            let widescreen = self.widescreen.as_deref().is_none_or(|w| w.0);
            let (offered, current) = display_modes(window, self.monitors.iter(), widescreen);
            Some(benilla_ui::script::ScreenModes { offered, current })
        }) || ask.answer(|_: &benilla_ui::script::MultisampleFormatsQuery| {
            self.msaa
                .as_deref()
                .map(|f| crate::cvars::multisample_formats(Some(f)))
        })
    }
}

/// Check the window got the size `$WOW_WIN` asked for: the window manager may clamp it to the
/// display (macOS does). Fatal under a capture, whose diffs assume the scenario's size; a warning
/// otherwise.
fn check_window_pinned(
    windows: Query<&Window, With<PrimaryWindow>>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(want) = requested_window_size() else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    // Under `$WOW_DPI`, `$WOW_WIN` is physical px, so compare the physical size.
    let res = &window.resolution;
    let got = match requested_dpi() {
        Some(_) => UVec2::new(res.physical_width(), res.physical_height()),
        None => UVec2::new(res.width() as u32, res.height() as u32),
    };
    if got == want {
        return;
    }
    let unit = if requested_dpi().is_some() {
        "physical"
    } else {
        "logical"
    };
    let capturing =
        std::env::var_os("WOW_CAPTURE").is_some() || std::env::var_os("WOW_CAPTURE_UI").is_some();
    if !capturing {
        warn!(
            "window: asked for {}x{} {unit}, got {}x{} — the window manager clamped it to the \
             display. Harmless here; it would invalidate a capture.",
            want.x, want.y, got.x, got.y
        );
        return;
    }
    error!(
        "window: REFUSING this capture — asked for {}x{} {unit}, got {}x{}. The window manager \
         clamped the request to the display this window opened on, so the image would not be the \
         size the scenario is denominated in and any diff against it would be meaningless. Use a \
         size that fits the current display (or move the window to a bigger one) and re-run.",
        want.x, want.y, got.x, got.y
    );
    exit.write(AppExit::error());
}

/// One boot line naming what the window got and, on Linux, the backend and any nested compositor
/// (mouse-look's `CursorGrabMode::Locked` is rejected on X11). Not dev-only: players paste it.
fn log_display_session(windows: Query<&Window, With<PrimaryWindow>>) {
    let Ok(window) = windows.single() else {
        return;
    };
    let res = &window.resolution;
    info!(
        "video: {:?}, {}x{} logical / {}x{} physical (scale {}){}",
        window.mode,
        res.width(),
        res.height(),
        res.physical_width(),
        res.physical_height(),
        res.scale_factor(),
        display_session(),
    );
}

/// The display-server facts as a trailing clause; empty except on Linux/BSD, where the windowing
/// backend is chosen at runtime.
fn display_session() -> String {
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    {
        let set = |k: &str| std::env::var_os(k).is_some();
        // winit prefers Wayland when `WAYLAND_DISPLAY` is set, else X11 (XWayland under a
        // Wayland compositor); `bevy`'s `default_platform` builds both.
        let backend = match (set("WAYLAND_DISPLAY"), set("DISPLAY")) {
            (true, _) => "wayland",
            (false, true) => "x11",
            (false, false) => "none",
        };
        // gamescope exports its socket name to children; the SteamOS session stamps the Deck.
        let nested = if set("GAMESCOPE_WAYLAND_DISPLAY") {
            ", gamescope"
        } else {
            ""
        };
        let deck = if set("SteamDeck") { ", steamdeck" } else { "" };
        format!(
            " [{backend}{nested}{deck}, XDG_SESSION_TYPE={}]",
            std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unset".into()),
        )
    }
    #[cfg(not(all(unix, not(any(target_os = "macos", target_os = "android")))))]
    String::new()
}

/// Push vsync to the window only when the setting moves, a `RestartGx()` asks or the render
/// adapter first appears (the boot window is born before it is known, on plain `Fifo`, and a
/// resize in the first moments is safe): the capture probes write `present_mode` directly, and
/// their override must stick, so the adapter's arrival swaps the boot mode alone.
///
/// MONKEY (lighting): the value compare on top of `is_changed()` stays. This branch hangs its
/// lighting knobs off `VideoConfig` too, so a `SetCVar("interiorGain")` marks the resource
/// changed; without the compare it would re-assert vsync and undo a probe's `AutoNoVsync`.
pub(crate) fn apply_present_mode(

    cfg: Res<VideoConfig>,
    restarts: Res<GxRestarts>,
    adapter: Option<Res<RenderAdapterInfo>>,
    mut pacing: ResMut<RefreshPacing>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut last: Local<Option<bool>>,
    mut last_restart: Local<u32>,
) {
    // A `RestartGx()` re-asserts even when nothing moved.
    let forced = std::mem::replace(&mut *last_restart, restarts.0) != restarts.0;
    let adapter_arrived = adapter.as_ref().is_some_and(|a| a.is_added());
    // MONKEY (lighting): "asked" is a vsync value that MOVED, not merely a changed resource.
    let asked = (cfg.is_changed() && last.replace(cfg.vsync) != Some(cfg.vsync)) || forced;
    if !asked && !adapter_arrived {

        return;
    }
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    if !asked && window.present_mode != present_mode(cfg.vsync) {
        return;
    }
    let choice = present_choice(
        cfg.vsync,
        adapter.as_ref().map(|a| (a.vendor, a.backend)),
        cfg!(windows),
        std::env::var(PRESENT_ENV).ok().as_deref(),
    );
    if pacing.0 != choice.paced {
        pacing.0 = choice.paced;
    }
    // A spurious change to `Window` is a surface reconfigure.
    if window.present_mode != choice.mode {
        window.present_mode = choice.mode;
        info!(
            "video: vsync {} ({:?}{})",
            if cfg.vsync { "on" } else { "off" },
            choice.mode,
            if choice.paced {
                ", paced to the refresh rate: Fifo crashes NVIDIA Vulkan on Windows after a resize; WOW_PRESENT=fifo restores it"
            } else {
                ""
            }
        );
    }
}

/// Push the display mode to the window, gated as [`apply_present_mode`] is. The guard matches the
/// variant, not the monitor: birth uses `Primary` and a live toggle `Current`, so a `!=` compare
/// would re-assert fullscreen on every launch.
pub(crate) fn apply_window_mode(
    cfg: Res<VideoConfig>,
    restarts: Res<GxRestarts>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut last_restart: Local<u32>,
) {
    // A `RestartGx()` re-asserts even when nothing moved, re-applying `gxResolution` to a window
    // that is already windowed.
    let forced = std::mem::replace(&mut *last_restart, restarts.0) != restarts.0;
    if !cfg.is_changed() && !forced {
        return;
    }
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    let want = window_mode(cfg.display, cfg.maximize, MonitorSelection::Current);
    let already = matches!(
        (&window.mode, &want),
        (WindowMode::Windowed, WindowMode::Windowed)
            | (
                WindowMode::BorderlessFullscreen(_),
                WindowMode::BorderlessFullscreen(_)
            )
    );
    if already && !forced {
        return;
    }
    // Leaving fullscreen hands the size back: entering it overwrote `window.resolution` with the
    // monitor's (`bevy_window`'s documented behaviour). Both writes land in one frame, as
    // `bevy_winit::changed_windows` applies `mode` before `resolution`.
    if want == WindowMode::Windowed {
        window
            .resolution
            .set(cfg.windowed.x as f32, cfg.windowed.y as f32);
    }
    window.mode = want;
    info!(
        "video: display mode {:?}{} ({want:?})",
        cfg.display,
        if cfg.maximize && cfg.display == DisplayMode::Windowed {
            ", maximized"
        } else {
            ""
        }
    );
}

/// `gxResolution` answers the size the screen runs at. 1.12.1's fullscreen runs at the mode the
/// CVar names, so the CVar always describes the backbuffer; benilla's fullscreen is the borderless
/// window over the monitor, so while it holds, the row shows that window's device pixels
/// ([`crate::cvars::Cvars::show`]). The windowed size, [`VideoConfig::windowed`], stays the row's
/// own value: it is what the save writes and what the row answers again once the window is back.
pub(crate) fn show_screen_size(
    cfg: Res<VideoConfig>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cvars: Option<ResMut<crate::cvars::Cvars>>,
) {
    let Some(mut cvars) = cvars else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };
    let res = &window.resolution;
    let screen = (cfg.display == DisplayMode::Fullscreen
        && matches!(window.mode, WindowMode::BorderlessFullscreen(_))
        && res.physical_width() > 0
        && res.physical_height() > 0)
        .then(|| format!("{}x{}", res.physical_width(), res.physical_height()));
    if cvars.shown(GX_RESOLUTION) == screen.as_deref() {
        return;
    }
    cvars.show(GX_RESOLUTION, screen.as_deref());
}

const GX_RESOLUTION: &str = "gxResolution";

#[cfg(test)]
mod tests {
    use super::*;

    /// A Vulkan presentation stall past wgpu-core's 1 s frame timeout is waited out, not turned
    /// into a lost device, only while the lock resolves wgpu-hal to `third_party/wgpu-hal`, the
    /// copy with the stall budget (`third_party/wgpu-hal/BENILLA.md`). A registry entry in the
    /// lock, a bumped version or a dropped hunk fails here, not on a Windows or Linux player.
    #[test]
    fn wgpu_hal_resolves_to_the_vendored_stall_patch() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let read = |rel: &str| {
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
        };

        let lock = read("Cargo.lock");
        let entry = lock
            .split("[[package]]")
            .find(|p| p.trim_start().starts_with("name = \"wgpu-hal\""))
            .expect("Cargo.lock has no wgpu-hal");
        assert!(
            !entry.lines().any(|l| l.starts_with("source = ")),
            "wgpu-hal resolves to a registry copy, not the path patch:{entry}"
        );
        assert!(
            read("Cargo.toml").contains("wgpu-hal = { path = \"third_party/wgpu-hal\" }"),
            "the root Cargo.toml lost its [patch.crates-io] wgpu-hal path"
        );

        let version = |manifest: &str| {
            manifest
                .lines()
                .find_map(|l| l.strip_prefix("version = "))
                .map(str::to_owned)
        };
        assert_eq!(
            version(entry),
            version(&read("third_party/wgpu-hal/Cargo.toml")),
            "the vendored wgpu-hal is not the version the lock resolves"
        );

        let instance = read("third_party/wgpu-hal/src/vulkan/instance.rs");
        for hunk in [
            "const STALL_BUDGET",
            "Err(vk::Result::TIMEOUT) if retry_stall(stall_start, timeout_ns)",
            "vkWaitForFences on the acquired image returned",
            "waiting for the previous submission timed out",
        ] {
            assert!(
                instance.contains(hunk),
                "the vendored acquire_texture lost `{hunk}`"
            );
        }
        assert!(
            instance
                .matches("retry_stall(stall_start, timeout_ns)")
                .count()
                == 3,
            "acquire_texture retries the previous-submission wait, the acquire and the post-acquire fence wait"
        );
    }

    /// Synced is `Fifo`, not `AutoVsync` (`FifoRelaxed`, which tears on a late frame).
    #[test]
    fn the_default_is_synced_like_the_window_literal() {
        assert!(VideoConfig::default().vsync);
        assert_eq!(present_mode(true), PresentMode::default());
        assert_eq!(present_mode(true), PresentMode::Fifo);
    }

    /// On Metal `Immediate` takes ~1 s `nextDrawable` stalls.
    #[test]
    fn uncapped_is_autonovsync_not_immediate() {
        assert_eq!(present_mode(false), PresentMode::AutoNoVsync);
    }

    const NVIDIA: u32 = 0x10DE;
    const AMD: u32 = 0x1002;
    const INTEL: u32 = 0x8086;

    /// NVIDIA's Vulkan driver crashes in `Fifo` after a resize; that pairing on Windows with vsync
    /// on is the one that avoids it, and paces instead.
    #[test]
    fn nvidia_vulkan_on_windows_trades_fifo_for_a_paced_autonovsync() {
        assert_eq!(
            present_choice(true, Some((NVIDIA, wgpu::Backend::Vulkan)), true, None),
            PresentChoice {
                mode: PresentMode::AutoNoVsync,
                paced: true
            }
        );
    }

    /// Every other vendor, backend and OS keeps `Fifo`.
    #[test]
    fn everything_else_keeps_fifo() {
        let fifo = PresentChoice {
            mode: PresentMode::Fifo,
            paced: false,
        };
        for vendor in [AMD, INTEL, 0x106B, 0] {
            assert_eq!(
                present_choice(true, Some((vendor, wgpu::Backend::Vulkan)), true, None),
                fifo,
                "{vendor:#x}"
            );
        }
        for backend in [
            wgpu::Backend::Dx12,
            wgpu::Backend::Metal,
            wgpu::Backend::Gl,
            wgpu::Backend::BrowserWebGpu,
        ] {
            assert_eq!(
                present_choice(true, Some((NVIDIA, backend)), true, None),
                fifo,
                "{backend:?}"
            );
        }
        // Linux and macOS NVIDIA Vulkan are not the Windows driver.
        assert_eq!(
            present_choice(true, Some((NVIDIA, wgpu::Backend::Vulkan)), false, None),
            fifo
        );
        // Before the renderer is up the adapter is unknown: the boot window's `Fifo`.
        assert_eq!(present_choice(true, None, true, None), fifo);
    }

    /// Vsync off is `AutoNoVsync` and unpaced on every adapter: the existing uncapped path.
    #[test]
    fn vsync_off_is_unchanged_everywhere() {
        for (adapter, windows) in [
            (Some((NVIDIA, wgpu::Backend::Vulkan)), true),
            (Some((AMD, wgpu::Backend::Vulkan)), true),
            (Some((NVIDIA, wgpu::Backend::Vulkan)), false),
            (None, true),
        ] {
            assert_eq!(
                present_choice(false, adapter, windows, None),
                PresentChoice {
                    mode: PresentMode::AutoNoVsync,
                    paced: false
                }
            );
        }
    }

    /// `$WOW_PRESENT=fifo` (any case, padded) restores `Fifo` on the avoided pairing; other values
    /// change nothing.
    #[test]
    fn wow_present_fifo_overrides_the_avoidance() {
        let nv = Some((NVIDIA, wgpu::Backend::Vulkan));
        for v in ["fifo", "FIFO", " Fifo "] {
            assert_eq!(
                present_choice(true, nv, true, Some(v)),
                PresentChoice {
                    mode: PresentMode::Fifo,
                    paced: false
                },
                "{v:?}"
            );
        }
        for v in ["", "mailbox", "1"] {
            assert!(present_choice(true, nv, true, Some(v)).paced, "{v:?}");
        }
    }

    /// The boot window is built before the adapter is known, and the arrival check reads its mode
    /// as `present_mode`'s.
    #[test]
    fn the_boot_mode_is_the_adapterless_choice() {
        for vsync in [true, false] {
            assert_eq!(
                present_choice(vsync, None, true, None).mode,
                present_mode(vsync)
            );
        }
    }

    /// The pace follows the fastest monitor and falls back to 60 Hz.
    #[test]
    fn the_pace_is_the_fastest_monitors_refresh_rate() {
        assert_eq!(
            refresh_period([Some(60_000), Some(144_000), None]),
            Duration::from_secs_f64(1.0 / 144.0)
        );
        for none in [vec![], vec![None], vec![Some(0)]] {
            assert_eq!(refresh_period(none), Duration::from_secs_f64(1.0 / 60.0));
        }
    }

    /// A frame waits out the rest of its period, never more; the first frame and an overrun wait 0.
    #[test]
    fn a_frame_waits_out_the_rest_of_its_period() {
        let t = Instant::now();
        let period = Duration::from_millis(10);
        assert_eq!(pace_wait(None, t, period), Duration::ZERO);
        assert_eq!(
            pace_wait(Some(t), t + Duration::from_millis(4), period),
            Duration::from_millis(6)
        );
        assert_eq!(
            pace_wait(Some(t), t + Duration::from_millis(25), period),
            Duration::ZERO
        );
    }

    /// `WindowMode::Fullscreen` is ignored on Wayland and panics without a monitor.
    #[test]
    fn the_default_is_borderless_fullscreen_not_exclusive() {
        assert_eq!(DisplayMode::default(), DisplayMode::Fullscreen);
        assert!(matches!(
            window_mode(DisplayMode::Fullscreen, false, MonitorSelection::Primary),
            WindowMode::BorderlessFullscreen(_)
        ));
        assert_eq!(
            window_mode(DisplayMode::Windowed, false, MonitorSelection::Primary),
            WindowMode::Windowed
        );
    }

    /// The window is born in the mode its rows register at on this locale, a stored value over
    /// that, and a run that sizes its own window over both. zhCN registers windowed and maximized
    /// (`0x63a738`), which is borderless over the monitor, as every other locale's fullscreen is
    /// here; they part when the player stores one of the two rows: a stored "0" for `gxMaximize`
    /// leaves zhCN windowed where another locale stays fullscreen.
    #[test]
    fn the_window_is_born_at_the_locales_registered_modes() {
        let none = |_: &str| None;
        let fullscreen = WindowMode::BorderlessFullscreen(MonitorSelection::Primary);
        for locale in 0..8 {
            assert_eq!(boot_window_mode_for(|| locale, none, false), fullscreen);
        }
        // zhCN with the player's stored windowed, unmaximized choice.
        let stored = |name: &str| match name {
            "gxWindow" => Some("1".to_string()),
            "gxMaximize" => Some("0".to_string()),
            _ => None,
        };
        assert_eq!(
            boot_window_mode_for(|| 4, stored, false),
            WindowMode::Windowed
        );
        assert_eq!(
            boot_window_mode_for(|| 0, stored, false),
            WindowMode::Windowed
        );
        // The player un-maximizes only the maximize row: zhCN's windowed default then stands
        // unmaximized, enUS's fullscreen default stands fullscreen.
        let unmaximized = |name: &str| (name == "gxMaximize").then(|| "0".to_string());
        assert_eq!(
            boot_window_mode_for(|| 4, unmaximized, false),
            WindowMode::Windowed
        );
        assert_eq!(boot_window_mode_for(|| 0, unmaximized, false), fullscreen);
        // An instrumented run stays windowed whatever the locale registers.
        assert_eq!(boot_window_mode_for(|| 4, none, true), WindowMode::Windowed);
    }

    /// A stored value that is no number loads as it is and reads 0 (`InternalSet` `0x63e0b0` has
    /// no parse check), so the window is born as the load then applies it, instead of flipping
    /// after: zhCN with `gxWindow = "x"` is fullscreen, as enUS is; and `gxMaximize = "0.5"` reads
    /// the record's integer, 0.
    #[test]
    fn a_stored_row_that_is_no_number_boots_as_the_load_applies_it() {
        let junk = |name: &str| match name {
            "gxWindow" => Some("x".to_string()),
            "gxMaximize" => Some("0".to_string()),
            _ => None,
        };
        for locale in [4, 0] {
            assert_eq!(
                boot_window_mode_for(|| locale, junk, false),
                WindowMode::BorderlessFullscreen(MonitorSelection::Primary),
                "locale {locale}"
            );
        }
        let half = |name: &str| match name {
            "gxWindow" => Some("1".to_string()),
            "gxMaximize" => Some("0.5".to_string()),
            _ => None,
        };
        assert_eq!(
            boot_window_mode_for(|| 0, half, false),
            WindowMode::Windowed
        );
    }

    /// Both rows stored, the locale is never asked, so the boot opens no chain for them.
    #[test]
    fn a_fully_stored_config_asks_no_locale() {
        let both = |_: &str| Some("0".to_string());
        let mode = boot_window_mode_for(|| panic!("the locale was read"), both, false);
        assert_eq!(
            mode,
            WindowMode::BorderlessFullscreen(MonitorSelection::Primary)
        );
    }

    /// The reference's polarity: the row is "Windowed Mode".
    #[test]
    fn gxwindow_one_is_windowed() {
        assert_eq!(display_from_flag(0.0), DisplayMode::Fullscreen);
        assert_eq!(display_from_flag(1.0), DisplayMode::Windowed);
    }

    /// A window born fullscreen is not re-asserted on the first frame, and leaving fullscreen
    /// hands `gxResolution` back rather than the monitor's size.
    #[test]
    fn the_fullscreen_round_trip_keeps_the_monitor_out_of_the_windowed_size() {
        let mut app = App::new();
        app.insert_resource(VideoConfig {
            vsync: true,
            world_shadows: false,
            character_shadows: false,
            shadow_distance: 80.0,
            display: DisplayMode::Fullscreen,
            maximize: false,
            windowed: UVec2::new(1024, 768),
            // MONKEY (review fixes): this window test inherits unrelated lighting defaults.
            ..Default::default()
        })
        // Seated by hand: this test runs the one system, not the plugin.
        .init_resource::<GxRestarts>()
        .add_systems(Update, apply_window_mode);
        let win = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        // Born as `lib.rs` builds it: fullscreen, on the boot monitor selection.
        app.world_mut()
            .entity_mut(win)
            .get_mut::<Window>()
            .unwrap()
            .mode = window_mode(DisplayMode::Fullscreen, false, MonitorSelection::Primary);

        app.update();
        assert!(
            matches!(
                app.world().entity(win).get::<Window>().unwrap().mode,
                WindowMode::BorderlessFullscreen(MonitorSelection::Primary)
            ),
            "an already-fullscreen window must not be re-asserted onto another monitor selection"
        );

        // Stand in for the compositor: fullscreen makes the resolution the monitor's. Then leave.
        app.world_mut()
            .entity_mut(win)
            .get_mut::<Window>()
            .unwrap()
            .resolution
            .set(3440.0, 1440.0);
        app.world_mut().resource_mut::<VideoConfig>().display = DisplayMode::Windowed;
        app.update();
        let w = app.world().entity(win).get::<Window>().unwrap();
        assert_eq!(w.mode, WindowMode::Windowed);
        assert_eq!(
            (w.resolution.width(), w.resolution.height()),
            (1024.0, 768.0),
            "leaving fullscreen restores gxResolution, never the monitor's size"
        );

        // Back in, on `Current`: the monitor the window is on by now.
        app.world_mut().resource_mut::<VideoConfig>().display = DisplayMode::Fullscreen;
        app.update();
        assert!(matches!(
            app.world().entity(win).get::<Window>().unwrap().mode,
            WindowMode::BorderlessFullscreen(MonitorSelection::Current)
        ));
    }

    /// In fullscreen `gxResolution` answers the size the screen runs at, the borderless window's
    /// device pixels, as 1.12.1's fullscreen runs at the mode it names; the windowed size stays
    /// the row's saved value and comes back when the window does.
    #[test]
    fn gxresolution_reads_the_screen_size_in_fullscreen() {
        let mut app = App::new();
        app.insert_resource(VideoConfig {
            vsync: true,
            display: DisplayMode::Fullscreen,
            maximize: false,
            windowed: UVec2::new(1600, 900),
            ..Default::default()
        })
        .insert_resource(crate::cvars::Cvars::with_value("gxResolution", "1600x900"))
        .add_systems(Update, show_screen_size);
        let mut window = Window {
            mode: WindowMode::BorderlessFullscreen(MonitorSelection::Primary),
            ..default()
        };
        window.resolution = WindowResolution::new(2880, 1800).with_scale_factor_override(2.0);
        let win = app.world_mut().spawn((window, PrimaryWindow)).id();
        app.update();
        let gx = |app: &App| {
            app.world()
                .resource::<crate::cvars::Cvars>()
                .get("gxResolution")
                .map(str::to_string)
        };
        assert_eq!(gx(&app).as_deref(), Some("2880x1800"));
        app.world_mut().resource_mut::<VideoConfig>().display = DisplayMode::Windowed;
        {
            let mut e = app.world_mut().entity_mut(win);
            let mut w = e.get_mut::<Window>().unwrap();
            w.mode = WindowMode::Windowed;
            w.resolution.set(1600.0, 900.0);
        }
        app.update();
        assert_eq!(gx(&app).as_deref(), Some("1600x900"));
    }

    /// `gxMaximize` counts only while windowed, and there it is the reference's popup over the
    /// whole screen (`0x58cf10`: style `0x90000000`, `GetSystemMetrics` 0 and 1).
    #[test]
    fn a_maximized_window_is_borderless_over_the_monitor() {
        let m = MonitorSelection::Current;
        assert_eq!(
            window_mode(DisplayMode::Windowed, true, m),
            WindowMode::BorderlessFullscreen(m)
        );
        assert_eq!(
            window_mode(DisplayMode::Windowed, false, m),
            WindowMode::Windowed
        );
        assert_eq!(
            window_mode(DisplayMode::Fullscreen, true, m),
            WindowMode::BorderlessFullscreen(m),
            "fullscreen ignores it"
        );
    }

    /// The `RestartGx` rebuild applies a committed `gxMaximize`, and un-maximizing hands the
    /// windowed size back.
    #[test]
    fn the_restart_rebuild_applies_a_committed_maximize() {
        let mut app = App::new();
        app.insert_resource(VideoConfig {
            vsync: true,
            display: DisplayMode::Windowed,
            maximize: false,
            windowed: UVec2::new(1024, 768),
            // MONKEY (merge): this window test inherits unrelated lighting defaults.
            ..Default::default()
        })
        .init_resource::<GxRestarts>()
        .add_systems(Update, apply_window_mode);
        let win = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        app.update();
        assert_eq!(
            app.world().entity(win).get::<Window>().unwrap().mode,
            WindowMode::Windowed
        );
        // The commit's callback write, then the restart the stock Okay asks for.
        app.world_mut().resource_mut::<VideoConfig>().maximize = true;
        app.world_mut().resource_mut::<GxRestarts>().0 += 1;
        app.update();
        assert!(matches!(
            app.world().entity(win).get::<Window>().unwrap().mode,
            WindowMode::BorderlessFullscreen(MonitorSelection::Current)
        ));
        app.world_mut()
            .entity_mut(win)
            .get_mut::<Window>()
            .unwrap()
            .resolution
            .set(2560.0, 1440.0);
        app.world_mut().resource_mut::<VideoConfig>().maximize = false;
        app.world_mut().resource_mut::<GxRestarts>().0 += 1;
        app.update();
        let w = app.world().entity(win).get::<Window>().unwrap();
        assert_eq!(w.mode, WindowMode::Windowed);
        assert_eq!(
            (w.resolution.width(), w.resolution.height()),
            (1024.0, 768.0)
        );
    }

    /// With `widescreen` 0 the list is the four fixed sizes (`0x48bcac`-`0x48bcc1`), never mixed
    /// with the display's modes (`0x48bd79`-`0x48bd91`); on, the display's offerable modes.
    #[test]
    fn widescreen_off_offers_the_four_fixed_sizes() {
        let monitor = Monitor {
            name: None,
            physical_height: 1080,
            physical_width: 1920,
            physical_position: IVec2::ZERO,
            refresh_rate_millihertz: None,
            scale_factor: 1.0,
            video_modes: vec![bevy::window::VideoMode {
                physical_size: UVec2::new(1280, 720),
                bit_depth: 32,
                refresh_rate_millihertz: 60_000,
            }],
        };
        let size = |r: &ScreenResolution| (r.width, r.height);
        assert_eq!(
            offered_modes([&monitor], true)
                .iter()
                .map(size)
                .collect::<Vec<_>>(),
            [(1280, 720), (1920, 1080)]
        );
        assert_eq!(
            offered_modes([&monitor], false)
                .iter()
                .map(size)
                .collect::<Vec<_>>(),
            [(800, 600), (1024, 768), (1280, 1024), (1600, 1200)]
        );
    }

    #[test]
    fn gxresolution_parses_the_reference_spelling() {
        assert_eq!(parse_resolution("1280x800"), Some(UVec2::new(1280, 800)));
        assert_eq!(parse_resolution("1600X900"), Some(UVec2::new(1600, 900)));
        assert_eq!(parse_resolution("1024 x 768"), Some(UVec2::new(1024, 768)));
        assert_eq!(parse_resolution("0x600"), None);
        assert_eq!(parse_resolution("1280x"), None);
        assert_eq!(parse_resolution("fullscreen"), None);
    }
}

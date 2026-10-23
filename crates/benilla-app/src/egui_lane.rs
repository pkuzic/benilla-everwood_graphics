//! The one egui lane: `bevy_egui` on its own full-window overlay camera (the PRIMARY context),
//! shared by every egui surface — the dev debug panel / inspector (feature `dev`) and the EME map
//! editor shell (feature `eme`). Each surface states its demand in [`EguiLaneDemand`]; with no
//! demand the context goes `run_manually` (bevy_egui skips its pass) and the camera sleeps, so a
//! hidden egui surface costs nothing.
//!
//! Lifted out of `debug_panel` so a player build with `eme` and without `dev` still gets exactly
//! one `EguiPlugin`, one overlay camera and one pointer arbitration (`EguiPointerOver`).

use bevy::camera::visibility::RenderLayers;
use bevy::camera::CameraOutputMode;
use bevy::prelude::*;
use bevy::render::render_resource::BlendState;
use bevy_egui::{
    egui, EguiContext, EguiContextSettings, EguiContexts, EguiFullOutput, EguiGlobalSettings,
    EguiInput, EguiPlugin, EguiPostUpdateSet, EguiPreUpdateSet, EguiPrimaryContextPass,
    PrimaryEguiContext,
};

use crate::ui_script::EguiPointerOver;

/// Who wants the egui lane this frame. Writers run in [`EguiDemandSet`]; the gate after it.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EguiLaneDemand {
    /// The dev panel is open or the inspector is armed.
    pub(crate) dev: bool,
    /// The EME editor is open.
    pub(crate) eme: bool,
}

impl EguiLaneDemand {
    fn any(self) -> bool {
        self.dev || self.eme
    }
}

/// The systems that write [`EguiLaneDemand`] (in `Update`, before the gate).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct EguiDemandSet;

/// Adds `EguiPlugin` in manual-context mode, the overlay camera, the Tab strip, the pointer
/// arbitration and the demand gate. Idempotent: the second surface to add it is a no-op.
pub(crate) struct EguiLanePlugin;

impl Plugin for EguiLanePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin::default());
        // Manual context mode, egui hosted on its own overlay camera: disable auto-creation right
        // after adding the plugin, before its startup system runs.
        {
            let mut egui = app.world_mut().resource_mut::<EguiGlobalSettings>();
            egui.auto_create_primary_context = false;
            // The player hides the OS cursor and draws `Point.blp`, so egui must not drive it.
            egui.enable_cursor_icon_updates = false;
        }
        app.init_resource::<EguiLaneDemand>()
            .add_systems(Startup, spawn_egui_camera)
            // Between bevy_egui filling `EguiInput` and the pass consuming it, the seam bevy_egui
            // documents for input edits.
            .add_systems(
                PreUpdate,
                strip_egui_tab_focus
                    .after(EguiPreUpdateSet::ProcessInput)
                    .before(EguiPreUpdateSet::BeginPass),
            )
            .add_systems(EguiPrimaryContextPass, track_pointer_over_ui)
            .add_systems(Update, gate_egui_lane.after(EguiDemandSet))
            // A gated frame still owes bevy_egui a prepared empty output, between the end pass it
            // skipped and the output consumer that still runs.
            .add_systems(
                PostUpdate,
                feed_gated_egui_output
                    .after(EguiPostUpdateSet::EndPass)
                    .before(EguiPostUpdateSet::ProcessOutput),
            );
    }

    fn is_unique(&self) -> bool {
        true
    }
}

/// Add the lane once, whichever surface asks first.
pub(crate) fn ensure(app: &mut App) {
    if !app.is_plugin_added::<EguiLanePlugin>() {
        app.add_plugins(EguiLanePlugin);
    }
}

/// Strip Tab from egui's input: it is bound to `TargetNearestEnemy` (and closes EME), and egui
/// would take it as focus navigation and grab the keyboard. Bevy's `ButtonInput<KeyCode>` still
/// sees it.
fn strip_egui_tab_focus(mut inputs: Query<&mut EguiInput>) {
    for mut input in &mut inputs {
        input.events.retain(|e| {
            !matches!(
                e,
                egui::Event::Key {
                    key: egui::Key::Tab,
                    ..
                }
            )
        });
    }
}

/// The egui half of `PointerOverUi`, for every surface on the lane.
fn track_pointer_over_ui(mut contexts: EguiContexts, mut over: ResMut<EguiPointerOver>) -> Result {
    let ctx = contexts.ctx_mut()?;
    let now = ctx.is_pointer_over_area() || ctx.wants_pointer_input();
    if over.0 != now {
        over.0 = now;
    }
    Ok(())
}

/// Feed each gated context an empty output: `run_manually` is not "off", and bevy_egui's
/// `process_output_system` logs an ERROR every frame a context has none. The first feed is a real
/// `Context::run`, because tessellating a context that never began a pass panics `No fonts loaded`.
fn feed_gated_egui_output(
    mut contexts: Query<(&mut EguiContext, &mut EguiFullOutput, &EguiContextSettings)>,
    // The empty pass's output minus its one-time texture delta, fed on every later gated frame
    // instead of running a real pass.
    mut cached: Local<Option<egui::FullOutput>>,
) {
    for (mut ctx, mut full_output, settings) in &mut contexts {
        if settings.run_manually && full_output.0.is_none() {
            full_output.0 = Some(match &*cached {
                Some(out) => out.clone(),
                None => {
                    // `get_mut`: `get` needs bevy_egui's `immutable_ctx` feature. The font
                    // texture delta must reach the GPU exactly once, so the cache drops it.
                    let out = ctx.get_mut().run(egui::RawInput::default(), |_| {});
                    let mut keep = out.clone();
                    keep.textures_delta = Default::default();
                    keep.shapes.clear();
                    *cached = Some(keep);
                    out
                }
            });
        }
    }
}

/// The full-window overlay camera hosting the primary egui context, order 2: above the player-UI
/// pass (order 1) and the world camera (order 0), so egui surfaces always sit on top.
fn spawn_egui_camera(mut commands: Commands) {
    commands.spawn((
        PrimaryEguiContext, // `#[require(EguiContext)]` ⇒ this camera carries EguiContext
        // Named for `preflight`'s MSAA check, which reports cameras by name.
        Name::new("egui dev-overlay camera"),
        Camera2d,
        // Set explicitly: `Camera` requires `Msaa`, default `Sample4`, and Bevy keys the 2D depth
        // texture on the target alone, so this must match the player-UI camera's `Msaa::Off` or a
        // pass fails validation. egui feathers its own edges anyway.
        bevy::render::view::Msaa::Off,
        RenderLayers::none(),
        Camera {
            order: 2,
            // An overlay composites only its own pixels: egui paints a cleared transparent canvas
            // in premultiplied colour, so the blit over the swapchain blends premultiplied
            // (`SrcAlpha` would weight it twice). Loading the shared main texture instead would
            // re-present the player-UI camera's un-decoded frame. It never touches the world
            // image, so writeback is off.
            output_mode: CameraOutputMode::Write {
                blend_state: Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                clear_color: ClearColorConfig::None,
            },
            clear_color: ClearColorConfig::Custom(Color::NONE),
            msaa_writeback: bevy::camera::MsaaWriteback::Off,
            ..default()
        },
    ));
}

/// Demand-gate the lane: with no surface open the primary context goes `run_manually` (bevy_egui
/// skips its pass) and the overlay camera sleeps. `EguiPointerOver` clears on the way down: its
/// writer runs inside the gated pass.
fn gate_egui_lane(
    demand: Res<EguiLaneDemand>,
    mut cams: Query<(&mut Camera, &mut EguiContextSettings), With<PrimaryEguiContext>>,
    mut over: ResMut<EguiPointerOver>,
) {
    let open = demand.any();
    for (mut cam, mut settings) in &mut cams {
        if cam.is_active != open {
            cam.is_active = open;
            if !open && over.0 {
                over.0 = false;
            }
        }
        if settings.run_manually == open {
            settings.run_manually = !open;
        }
    }
}

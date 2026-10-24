//! # eme — the Everwood ME (EME) Bevy plugin
//!
//! EME is benilla's in-game, server-authoritative GM map editor (DESIGN-EME.md). This crate is the
//! Bevy side of it, built on the engine-free [`eme_core`] and the voxel seam
//! [`benilla_world::voxel::MonkeyVoxelApi`]; it never touches the network itself (a host plugs a
//! transport into [`EmeWire`], e.g. benilla-app's chat transport `net/eme_relay.rs`).
//!
//! | Module | What |
//! |---|---|
//! | [`wire`] | [`EmeWire`]: the queue-backed [`eme_core::EmeTransport`] a host transport drains / fills |
//! | [`camera`] | the free camera (WASD + RMB look, Shift fast, Ctrl slow, wheel speed, Tab back) |
//! | [`viewport`] | cursor ray → [`EmeAim`] (voxel or ground hit + normal), the brush ring preview |
//! | [`dispatch`] | the frame loop: pump → tool → dispatcher → `MonkeyVoxelApi::submit`; F9 / undo / redo |
//! | [`offline`] | an emulated server for offline runs (local-only undo) |
//! | [`script`] | `WOW_EME_SCRIPT=<json>`: drives the same code paths headlessly (capture mode) |
//! | [`tools`] | the built-in tools (`sculpt`: dig / build / smooth; `flatten`; `pushpull`; `bridge`; `water`; `waterfill`; `area`) |
//! | [`areas`] | the area picker's catalog: the current map's `AreaTable` names |
//! | [`ui`] | the `eme-ui` shell fed from [`EmeHost`] (toolbar, options, history, restore, log, status) |
//!
//! ## Host contract
//! - Shared names live in [`eme_core::host`]: [`EmeActive`] (resource), [`EmeToggleRequested`]
//!   (message), [`EmePointerOverUi`] / [`EmeUiWantsKeyboard`] (written by the UI shell),
//!   [`EmeHostInput`] (written by the host client: typing / pointer over its own UI).
//! - The UI shell ([`ui`], `eme-ui`) draws on the PRIMARY egui context: the host spawns the egui
//!   camera (`PrimaryEguiContext`), keeps that context running while [`EmeActive`] is on, and sets
//!   `eme_ui::EmeLanguage`. A host that configures egui itself adds `EguiPlugin` before this plugin.
//! - The host holds the character still while [`EmeActive`] is on and keeps the world streaming
//!   around the camera (benilla-app: `player.detached`).
//! - A transport fills [`EmeWire::link`] and [`EmeWire::inbound`] and drains
//!   [`EmeWire::take_services`]; voxel edits travel through the voxel layer's own outbox.
//! - UI panels send [`EmeRequest`] messages (undo, redo, select tool, set option) and read
//!   [`EmeHost`] (registry, dispatcher, undo stack) and [`EmeEventMsg`].

pub mod areas;
pub mod camera;
pub mod dispatch;
pub mod offline;
pub mod script;
pub mod tools;
pub mod ui;
pub mod viewport;
pub mod wire;

use bevy::prelude::*;
use eme_core::{
    EmeDispatchError, EmeDispatcher, EmeEvent, EmeOptionValue, EmeStrokeIds, EmeTool,
    EmeToolRegistry, EmeToolReply,
};

pub use camera::EmeFreeCam;
pub use eme_core::{
    EmeActive, EmeHostInput, EmePointerOverUi, EmeToggleRequested, EmeUiWantsKeyboard,
};
pub use eme_ui;
pub use viewport::{
    EmeAim, EmeBrushPreviewState, EmeCursor, EmeGizmos, EmeMarker, EmeRestoreOutline,
};
pub use wire::EmeWire;

/// The EME system sets, in frame order. `Input` → `Frame` run in `Update` before the voxel layer
/// (so a stroke's edits are ingested and meshed the same frame); `Camera` runs in `PostUpdate`
/// before transform propagation, after anything in `Update` that seats the camera.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmeSet {
    /// Toggle, script, cursor ray / aim.
    Input,
    /// Pump the transport, feed the tool, dispatch, submit edits, preview.
    Frame,
    /// Write the free camera's pose (PostUpdate).
    Camera,
}

/// EME's state: the tool registry, the command dispatcher (undo stack + service client) and the
/// shared stroke-id counter. UI panels read it (toolbar = `registry.visible(...)`, history =
/// `dispatcher.undo_stack()`).
#[derive(Resource)]
pub struct EmeHost {
    /// Every registered tool and the active one.
    pub registry: EmeToolRegistry,
    /// Gates, sends, records history, routes replies.
    pub dispatcher: EmeDispatcher,
    /// The character's stroke-id sequence, shared by every stroke-opening tool.
    pub stroke_ids: EmeStrokeIds,
    /// Replies for tools / panels from the last pump (overwritten each frame).
    pub replies: Vec<EmeToolReply>,
    /// Commands the dispatcher refused on the last dispatch.
    pub errors: Vec<EmeDispatchError>,
    /// Messages for the UI's notice line (locale key, args, is-error); drained by `ui`.
    pub(crate) notices: Vec<(String, Vec<String>, bool)>,
    pub(crate) caps_asked: bool,
    /// MONKEY (EME): the `strokes` (mine) request that seeds [`Self::stroke_ids`] after a relog
    /// is in flight / done.
    pub(crate) strokes_seeded: u8,
    /// MONKEY (EME projects): a map edit was refused for want of a project: prompt the user.
    pub(crate) prompt_project: bool,
    /// MONKEY (EME projects): the project list is stale (a `project_changed` push).
    pub(crate) projects_dirty: bool,
}

impl Default for EmeHost {
    fn default() -> Self {
        Self {
            registry: EmeToolRegistry::new(),
            dispatcher: EmeDispatcher::default(),
            stroke_ids: EmeStrokeIds::default(),
            replies: Vec::new(),
            errors: Vec::new(),
            notices: Vec::new(),
            caps_asked: false,
            strokes_seeded: 0,
            prompt_project: false,
            projects_dirty: false,
        }
    }
}

/// A request from a UI panel, a script or a host (honoured only while EME is active and the
/// caller's rank allows it).
#[derive(Message, Debug, Clone, PartialEq)]
pub enum EmeRequest {
    /// Ctrl+Z.
    Undo,
    /// Ctrl+Y.
    Redo,
    /// "Undo anyway" after an `UndoNeedsConfirm` event.
    ConfirmUndo(eme_core::EmeEntryId),
    /// "Cancel" on that confirmation: the entry goes back to applied.
    CancelConfirm(eme_core::EmeEntryId),
    /// A cold-path request from a panel (restore points, log), sent as the host (no tool).
    Service(eme_core::EmeServiceRequest),
    /// Activate a tool by id.
    SelectTool(String),
    /// Set an option of the active tool.
    SetOption {
        /// Option key.
        key: String,
        /// New value.
        value: EmeOptionValue,
    },
}

/// An [`EmeEvent`] re-emitted as a Bevy message (entry pushed / committed / undone / redone /
/// discarded, undo-needs-confirm, …).
#[derive(Message, Debug, Clone, PartialEq)]
pub struct EmeEventMsg(pub EmeEvent);

/// `app.register_eme_tool(tool)`: add a tool (registration order = toolbar order).
pub trait EmeAppExt {
    /// Register `tool` with EME. Panics on a duplicate id (a programming error).
    fn register_eme_tool(&mut self, tool: impl EmeTool) -> &mut Self;
}

impl EmeAppExt for App {
    fn register_eme_tool(&mut self, tool: impl EmeTool) -> &mut Self {
        self.init_resource::<EmeHost>();
        let mut host = self.world_mut().resource_mut::<EmeHost>();
        host.registry
            .register(Box::new(tool))
            .expect("EME tool ids are unique");
        self
    }
}

/// The EME plugin. Inert for anyone whose server rank (`caps`) is below 2: F9 does nothing, no
/// camera, no brush, no preview.
pub struct EmePlugin;

impl Plugin for EmePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EmeHost>()
            .init_resource::<EmeActive>()
            .init_resource::<EmePointerOverUi>()
            .init_resource::<EmeUiWantsKeyboard>()
            .init_resource::<EmeHostInput>()
            .init_resource::<EmeWire>()
            .add_message::<EmeToggleRequested>()
            .add_message::<EmeRequest>()
            .add_message::<EmeEventMsg>();
        let ids = app.world().resource::<EmeHost>().stroke_ids.clone();
        app.register_eme_tool(tools::sculpt::EmeSculptTool::new(ids.clone()));
        app.register_eme_tool(tools::flatten::EmeFlattenTool::new(ids.clone()));
        // MONKEY (EME, CONTRACT v3.6, Planet-Zoo terraforming): sideways growth, bridges / tunnels.
        app.register_eme_tool(tools::pushpull::EmePushPullTool::new(ids.clone()));
        app.register_eme_tool(tools::bridge::EmeBridgeTool::new(ids.clone()));
        // MONKEY (EME, Phase 3 lanes K + L): the overlay tools.
        app.register_eme_tool(tools::water::EmeWaterTool::new(ids.clone()));
        app.register_eme_tool(tools::waterfill::EmeWaterFillTool::new(ids.clone()));
        let catalog = tools::area::EmeAreaCatalog::default();
        app.insert_resource(areas::EmeAreas(catalog.clone()));
        app.register_eme_tool(tools::area::EmeAreaTool::new(ids, catalog));
        app.configure_sets(
            Update,
            (EmeSet::Input, EmeSet::Frame)
                .chain()
                .before(benilla_world::voxel::MonkeyVoxelSet)
                .run_if(world_live),
        );
        app.configure_sets(
            PostUpdate,
            EmeSet::Camera.before(bevy::transform::TransformSystems::Propagate),
        );
        camera::register(app);
        viewport::register(app);
        dispatch::register(app);
        offline::register(app);
        script::register(app);
        areas::register(app);
        ui::register(app);
    }
}

/// The world is live (benilla-world's flag; EME has nothing to do on the glue screens).
fn world_live(live: Option<Res<benilla_world::schedule::WorldLive>>) -> bool {
    live.is_some_and(|l| l.0)
}

/// Milliseconds of game time, EME's clock (frozen-clock captures stay deterministic).
pub(crate) fn now_ms(time: &Time) -> u64 {
    time.elapsed().as_millis() as u64
}

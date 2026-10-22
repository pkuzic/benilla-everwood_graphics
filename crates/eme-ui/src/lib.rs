//! # eme-ui — the UI shell of Everwood ME (EME)
//!
//! egui panels on `bevy_egui`: a tool strip built from [`eme_core::EmeToolRegistry`], undo / redo
//! buttons, a link / rank indicator, a left panel with the active tool's options, a right panel
//! with History / Restore points / Log tabs, a status bar, a "needs confirmation" modal and a
//! read-only banner. No tool logic, no networking, no voxel code: the shell reads resources and
//! writes [`EmeUiCommand`] messages.
//!
//! See the crate README for embedding it and for contributing a tool's options panel.

#![warn(missing_docs)]

pub mod i18n;
pub mod layout;
pub mod shell;
pub mod state;
pub mod theme;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPlugin, EguiPrimaryContextPass};

pub use i18n::{EmeI18n, EmeLang, EmeLanguage, Tr};
pub use layout::{EmeLayout, EmeLayoutPath, EmeRightTab};
pub use shell::{EmeProbeEntry, EmeShell, EmeShellModel, EmeShellOutput};
pub use state::{
    EmeActive, EmeInputArbiter, EmeLogTable, EmeRestorePoints, EmeSession, EmeStatus,
    EmeToggleRequested, EmeTools, EmeUiCommand, EmeUndo,
};
pub use theme::EmeTheme;

/// Adds the shell: resources, the [`EmeUiCommand`] / [`EmeToggleRequested`] messages, layout
/// loading and the drawing system. Adds `bevy_egui`'s plugin itself if the host has not.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmeUiPlugin;

impl Plugin for EmeUiPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<EguiPlugin>() {
            app.add_plugins(EguiPlugin::default());
        }
        app.init_resource::<EmeActive>()
            .init_resource::<EmeTools>()
            .init_resource::<EmeUndo>()
            .init_resource::<EmeSession>()
            .init_resource::<EmeStatus>()
            .init_resource::<EmeRestorePoints>()
            .init_resource::<EmeLogTable>()
            .init_resource::<EmeInputArbiter>()
            .init_resource::<EmeTheme>()
            .init_resource::<EmeI18n>()
            .init_resource::<EmeLanguage>()
            .init_resource::<EmeLayoutPath>()
            .init_resource::<EmeShell>()
            .add_message::<EmeUiCommand>()
            .add_message::<EmeToggleRequested>()
            .add_systems(Startup, load_layout)
            .add_systems(Update, apply_toggle_requests)
            .add_systems(EguiPrimaryContextPass, shell_system);
    }
}

/// Everything the shell reads.
#[derive(SystemParam)]
struct ShellData<'w> {
    tools: Res<'w, EmeTools>,
    undo: Res<'w, EmeUndo>,
    session: Res<'w, EmeSession>,
    status: Res<'w, EmeStatus>,
    restore: Res<'w, EmeRestorePoints>,
    log: Res<'w, EmeLogTable>,
    i18n: Res<'w, EmeI18n>,
    lang: Res<'w, EmeLanguage>,
    theme: Res<'w, EmeTheme>,
}

fn load_layout(path: Res<EmeLayoutPath>, mut shell: ResMut<EmeShell>) {
    if let Some(p) = &path.0 {
        shell.layout = EmeLayout::load(p);
    }
}

/// Turns [`EmeToggleRequested`] into a flip of [`EmeActive`]; opening needs a rank that may open
/// EME (the F9 key does nothing for players).
pub fn apply_toggle_requests(
    mut requests: MessageReader<EmeToggleRequested>,
    mut active: ResMut<EmeActive>,
    session: Res<EmeSession>,
) {
    for _ in requests.read() {
        if active.0 {
            active.0 = false;
        } else if session.perms.can_open() {
            active.0 = true;
        }
    }
}

fn shell_system(
    mut contexts: EguiContexts,
    active: Res<EmeActive>,
    data: ShellData,
    path: Res<EmeLayoutPath>,
    mut shell: ResMut<EmeShell>,
    mut arbiter: ResMut<EmeInputArbiter>,
    mut commands: MessageWriter<EmeUiCommand>,
    mut saved: Local<Option<EmeLayout>>,
) -> Result {
    if !active.0 {
        if *arbiter != EmeInputArbiter::default() {
            *arbiter = EmeInputArbiter::default();
        }
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?.clone();
    let model = EmeShellModel {
        tools: &data.tools.0,
        undo: &data.undo.0,
        session: &data.session,
        status: &data.status,
        restore: &data.restore,
        log: &data.log,
        tr: data.i18n.tr(data.lang.0),
        theme: &data.theme,
    };
    let out = shell.show(&ctx, &model);
    let next = EmeInputArbiter {
        pointer_over_ui: out.pointer_over_ui,
        keyboard_captured: out.keyboard_captured,
    };
    if *arbiter != next {
        *arbiter = next;
    }
    for c in out.commands {
        commands.write(c);
    }
    // Persist the layout once the user lets go of a drag.
    let saved = saved.get_or_insert_with(|| shell.layout.clone());
    if *saved != shell.layout && !ctx.input(|i| i.pointer.any_down()) {
        if let Some(p) = &path.0 {
            if let Err(e) = shell.layout.save(p) {
                warn!("eme-ui: could not save {}: {e}", p.display());
            }
        }
        *saved = shell.layout.clone();
    }
    Ok(())
}

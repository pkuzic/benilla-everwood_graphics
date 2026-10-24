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

use std::marker::PhantomData;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPlugin, EguiPrimaryContextPass};

pub use eme_core::{EmeActive, EmePointerOverUi, EmeToggleRequested, EmeUiWantsKeyboard};
pub use i18n::{EmeI18n, EmeLang, EmeLanguage, Tr};
pub use layout::{EmeLayout, EmeLayoutPath, EmeRightTab};
pub use shell::{
    EmeLogFilterUi, EmePending, EmeProbeEntry, EmeShell, EmeShellModel, EmeShellOutput,
};
pub use state::{
    EmeLogTable, EmeNotice, EmeObjects, EmeOtherStroke, EmeOthersHistory, EmeProjectConflict,
    EmeProjectFilter, EmeProjects, EmeRestorePoints, EmeSession, EmeShellSource,
    EmeStandaloneSource, EmeStatus, EmeUiCommand,
};
pub use theme::EmeTheme;

/// Adds the shell: resources, the [`EmeUiCommand`] message, layout loading and the drawing
/// system. `S` is where the toolbar and the History tab read the tool registry and the undo stack
/// ([`EmeShellSource`]; the `eme` plugin passes its `EmeHost`). Adds `bevy_egui`'s plugin itself
/// if the host has not (a host that wants its own egui camera adds `EguiPlugin` first).
///
/// The shared host names ([`EmeActive`], [`EmeToggleRequested`], [`EmePointerOverUi`],
/// [`EmeUiWantsKeyboard`]) are eme-core's: the shell draws while `EmeActive` is on and writes the
/// two input flags; opening / closing (and its rank gate) belongs to whoever owns `EmeActive`.
pub struct EmeUiPlugin<S: EmeShellSource = EmeStandaloneSource>(PhantomData<fn() -> S>);

impl<S: EmeShellSource> Default for EmeUiPlugin<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<S: EmeShellSource> Plugin for EmeUiPlugin<S> {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<EguiPlugin>() {
            app.add_plugins(EguiPlugin::default());
        }
        app.init_resource::<EmeActive>()
            .init_resource::<EmeSession>()
            .init_resource::<EmeStatus>()
            .init_resource::<EmeRestorePoints>()
            .init_resource::<EmeLogTable>()
            .init_resource::<EmeNotice>()
            .init_resource::<EmeOthersHistory>()
            .init_resource::<EmeProjects>()
            .init_resource::<EmeObjects>()
            .init_resource::<EmePointerOverUi>()
            .init_resource::<EmeUiWantsKeyboard>()
            .init_resource::<EmeTheme>()
            .init_resource::<EmeI18n>()
            .init_resource::<EmeLanguage>()
            .init_resource::<EmeLayoutPath>()
            .init_resource::<EmeShell>()
            .add_message::<EmeUiCommand>()
            .add_systems(Startup, load_layout)
            .add_systems(EguiPrimaryContextPass, shell_system::<S>);
    }
}

/// Everything the shell reads.
#[derive(SystemParam)]
struct ShellData<'w, S: EmeShellSource> {
    source: Option<Res<'w, S>>,
    session: Res<'w, EmeSession>,
    status: Res<'w, EmeStatus>,
    restore: Res<'w, EmeRestorePoints>,
    log: Res<'w, EmeLogTable>,
    notice: Res<'w, EmeNotice>,
    others: Res<'w, EmeOthersHistory>,
    projects: Res<'w, EmeProjects>,
    objects: Res<'w, EmeObjects>,
    i18n: Res<'w, EmeI18n>,
    lang: Res<'w, EmeLanguage>,
    theme: Res<'w, EmeTheme>,
}

fn load_layout(path: Res<EmeLayoutPath>, mut shell: ResMut<EmeShell>) {
    if let Some(p) = &path.0 {
        shell.layout = EmeLayout::load(p);
    }
}

/// The input flags the shell owns.
#[derive(SystemParam)]
struct ShellInput<'w> {
    over: ResMut<'w, EmePointerOverUi>,
    keyboard: ResMut<'w, EmeUiWantsKeyboard>,
}

impl ShellInput<'_> {
    fn set(&mut self, over: bool, keyboard: bool) {
        if self.over.0 != over {
            self.over.0 = over;
        }
        if self.keyboard.0 != keyboard {
            self.keyboard.0 = keyboard;
        }
    }
}

fn shell_system<S: EmeShellSource>(
    mut contexts: EguiContexts,
    active: Res<EmeActive>,
    data: ShellData<S>,
    path: Res<EmeLayoutPath>,
    mut shell: ResMut<EmeShell>,
    mut input: ShellInput,
    mut commands: MessageWriter<EmeUiCommand>,
    mut saved: Local<Option<EmeLayout>>,
) -> Result {
    let Some(source) = data.source.as_deref().filter(|_| active.0) else {
        input.set(false, false);
        return Ok(());
    };
    let ctx = contexts.ctx_mut()?.clone();
    let model = EmeShellModel {
        tools: source.registry(),
        undo: source.undo_stack(),
        session: &data.session,
        status: &data.status,
        restore: &data.restore,
        log: &data.log,
        notice: &data.notice,
        others: &data.others,
        projects: &data.projects,
        objects: &data.objects.0,
        tr: data.i18n.tr(data.lang.0),
        theme: &data.theme,
    };
    let out = shell.show(&ctx, &model);
    input.set(out.pointer_over_ui, out.keyboard_captured);
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

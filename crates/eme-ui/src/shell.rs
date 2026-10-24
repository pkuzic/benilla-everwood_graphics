//! `EmeShell`: the egui layout. Pure drawing: it reads a [`EmeShellModel`] (borrowed resources)
//! and returns the [`EmeUiCommand`]s the user caused. It is independent of bevy systems so a
//! headless test can run it on a bare [`egui::Context`].

use bevy::prelude::Resource;
use bevy_egui::egui::{self, Align, Layout, RichText, Ui};
use eme_core::service::{EmeLayer, EmeLayerFilter, EmeLogQuery};
use eme_core::{
    EmeCapability, EmeEntryState, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeServiceRequest,
    EmeToolId, EmeToolRegistry, EmeUndoEntry, EmeUndoStack,
};

use crate::i18n::Tr;
use crate::layout::{EmeLayout, EmeRightTab, PANEL_MAX, PANEL_MIN};
use crate::state::{
    EmeLogTable, EmeNotice, EmeOthersHistory, EmeProjectFilter, EmeProjects, EmeRestorePoints,
    EmeSession, EmeStatus, EmeUiCommand,
};
use crate::theme::EmeTheme;

/// Read-only inputs of one shell frame.
pub struct EmeShellModel<'a> {
    /// Registered tools.
    pub tools: &'a EmeToolRegistry,
    /// The undo stack.
    pub undo: &'a EmeUndoStack,
    /// Rank, caps, link.
    pub session: &'a EmeSession,
    /// Status bar numbers.
    pub status: &'a EmeStatus,
    /// Restore points.
    pub restore: &'a EmeRestorePoints,
    /// Log page.
    pub log: &'a EmeLogTable,
    /// The message under the top strip (an error or a result).
    pub notice: &'a EmeNotice,
    /// Strokes by other GMs (read-only History rows).
    pub others: &'a EmeOthersHistory,
    /// MONKEY (EME projects): the Projects tab's rows, info and revert conflict.
    pub projects: &'a EmeProjects,
    /// Language-bound strings.
    pub tr: Tr<'a>,
    /// Palette.
    pub theme: &'a EmeTheme,
}

/// One control the shell drew this frame (collected only when probing; used by tests and
/// accessibility tooling).
#[derive(Debug, Clone, PartialEq)]
pub struct EmeProbeEntry {
    /// Stable control id: `tool:<id>`, `undo`, `redo`, `opt:<key>`, `restore:create`,
    /// `restore:revert:<n>`, `log:newer`, `log:older`, `tab:<name>`, `confirm:yes`, `confirm:no`.
    pub id: String,
    /// The control accepts input.
    pub enabled: bool,
    /// Its screen rect.
    pub rect: egui::Rect,
    /// The hover text of the control, if any (why it is disabled, for a disabled one).
    pub tooltip: Option<String>,
}

/// What one shell frame produced.
#[derive(Debug, Default)]
pub struct EmeShellOutput {
    /// Commands for the host, in click order.
    pub commands: Vec<EmeUiCommand>,
    /// Drawn controls (empty unless `EmeShell::probe` is set).
    pub probe: Vec<EmeProbeEntry>,
    /// The pointer is over EME UI.
    pub pointer_over_ui: bool,
    /// A text field of EME has keyboard focus.
    pub keyboard_captured: bool,
    /// The read-only banner was drawn.
    pub read_only_banner: bool,
    /// The confirmation modal was shown.
    pub confirm_modal: bool,
}

/// The log browser's filter fields as typed (text), turned into an [`EmeLogQuery`] by
/// [`EmeLogFilterUi::query`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmeLogFilterUi {
    /// Character name (empty = everyone).
    pub player: String,
    /// Radius around the caller's character, yards (empty = server default).
    pub radius: String,
    /// Look-back, minutes (empty = server default).
    pub minutes: String,
    /// 0 = any tool, 1..=4 = dig / build / smooth / flatten.
    pub mode: usize,
    /// 0 = all layers, 1 = terrain, 2 = water, 3 = area.
    pub layer: usize,
    /// MONKEY (EME projects): only rows of the active project.
    pub project_only: bool,
}

impl EmeLogFilterUi {
    /// The radius field as yards (1..500), if it parses.
    pub fn radius_value(&self) -> Option<f32> {
        self.radius
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|r| r.is_finite())
            .map(|r| r.clamp(1.0, 500.0))
    }

    /// The minutes field (1..10080), if it parses.
    pub fn minutes_value(&self) -> Option<u32> {
        self.minutes
            .trim()
            .parse::<u32>()
            .ok()
            .map(|m| m.clamp(1, 10_080))
    }

    /// The query these fields describe (paging and the row limit are the host's).
    pub fn query(&self) -> EmeLogQuery {
        let player = self.player.trim();
        EmeLogQuery {
            radius: self.radius_value(),
            minutes: self.minutes_value(),
            player: (!player.is_empty()).then(|| player.to_owned()),
            mode: self.mode.checked_sub(1).map(|m| m as u8),
            layer: match self.layer {
                1 => Some(EmeLayerFilter::Voxel),
                2 => Some(EmeLayerFilter::Liquid),
                3 => Some(EmeLayerFilter::Area),
                _ => None,
            },
            limit: None,
            before_seq: None,
            project: None,
        }
    }

    /// MONKEY (EME projects): [`Self::query`] narrowed to `active` when "active project only" is on.
    pub fn query_for(&self, active: Option<u32>) -> EmeLogQuery {
        EmeLogQuery {
            project: if self.project_only { active } else { None },
            ..self.query()
        }
    }
}

/// A destructive admin action waiting for the user's "yes".
#[derive(Debug, Clone, PartialEq)]
pub enum EmePending {
    /// Revert everything in a restore point's circle.
    Revert {
        /// Snapshot id.
        snapshot: u32,
        /// Its label.
        label: String,
        /// Its radius, yards.
        radius: f32,
    },
    /// Undo one player's recent work near the caller's character.
    UndoPlayer {
        /// Character name.
        name: String,
        /// Look-back (server default 30 when `None`).
        minutes: Option<u32>,
        /// Radius (server default 200 when `None`).
        radius: Option<f32>,
    },
    /// MONKEY (EME projects): revert every edit of a project.
    ProjectRevert {
        /// Project id.
        project: u32,
        /// Its name.
        name: String,
    },
    /// MONKEY (EME projects): re-apply a reverted project (rank 4).
    ProjectUnrevert {
        /// Project id.
        project: u32,
        /// Its name.
        name: String,
    },
    /// MONKEY (EME projects): close a project.
    ProjectClose {
        /// Project id.
        project: u32,
        /// Its name.
        name: String,
    },
}

/// The shell: persistent UI state (layout, text fields). Draw it with [`EmeShell::show`].
#[derive(Resource, Debug, Clone, Default)]
pub struct EmeShell {
    /// Panel layout (persisted by the plugin).
    pub layout: EmeLayout,
    /// The restore-point label being typed.
    pub snapshot_label: String,
    /// The log filter fields.
    pub filter: EmeLogFilterUi,
    /// A revert / undo-player waiting for confirmation.
    pub pending: Option<EmePending>,
    /// Collect an [`EmeProbeEntry`] per control (tests).
    pub probe: bool,
    /// MONKEY (EME projects): the new project's name being typed.
    pub project_name: String,
    /// MONKEY (EME projects): the new project's description being typed.
    pub project_desc: String,
    /// MONKEY (EME projects): the History tab shows only the active project's entries.
    pub history_project_only: bool,
}

struct Frame<'a> {
    m: &'a EmeShellModel<'a>,
    can_edit: bool,
    /// MONKEY (EME projects): map-editing tools are locked until a project is active.
    needs_project: bool,
    out: &'a mut EmeShellOutput,
    probe: bool,
}

impl Frame<'_> {
    fn rec(&mut self, id: String, resp: &egui::Response, enabled: bool, tooltip: Option<String>) {
        if self.probe {
            self.out.probe.push(EmeProbeEntry {
                id,
                enabled,
                rect: resp.rect,
                tooltip,
            });
        }
    }

    /// A button; `tip` is shown on hover (and, disabled, explains why).
    fn button(
        &mut self,
        ui: &mut Ui,
        id: String,
        text: String,
        enabled: bool,
        selected: bool,
        tip: Option<String>,
    ) -> bool {
        let mut resp = ui.add_enabled(enabled, egui::Button::new(text).selected(selected));
        if let Some(t) = &tip {
            resp = if enabled {
                resp.on_hover_text(t.clone())
            } else {
                resp.on_disabled_hover_text(t.clone())
            };
        }
        self.rec(id, &resp, enabled, tip);
        enabled && resp.clicked()
    }

    fn push(&mut self, c: EmeUiCommand) {
        self.out.commands.push(c);
    }

    fn tool_name(&self, id: EmeToolId) -> String {
        let key = format!("{}NAME", id.locale_prefix());
        self.m
            .tr
            .try_t(&key)
            .map(str::to_owned)
            .unwrap_or_else(|| id.as_str().to_owned())
    }

    fn label(&self, key: &str) -> String {
        self.m
            .tr
            .try_t(key)
            .map(str::to_owned)
            .unwrap_or_else(|| key.to_owned())
    }
}

impl EmeShell {
    /// A shell with a given starting layout.
    pub fn with_layout(layout: EmeLayout) -> Self {
        Self {
            layout,
            ..Self::default()
        }
    }

    /// Draws the whole shell for one frame.
    pub fn show(&mut self, ctx: &egui::Context, model: &EmeShellModel) -> EmeShellOutput {
        model.theme.apply(ctx);
        let mut out = EmeShellOutput::default();
        let perms = model.session.perms;
        let can_edit = perms.has(EmeCapability::Edit);
        let mut f = Frame {
            m: model,
            can_edit,
            needs_project: can_edit && model.session.needs_project(),
            out: &mut out,
            probe: self.probe,
        };
        let tr = model.tr;

        // Top strip.
        let read_only = perms.can_open() && !can_edit;
        egui::TopBottomPanel::top("eme_top").show(ctx, |ui| {
            top_strip(ui, &mut f, &mut self.layout);
            project_bar(ui, &mut f, &mut self.layout);
            notice_line(ui, &mut f);
            if read_only {
                f.out.read_only_banner = true;
                egui::Frame::new()
                    .stroke(egui::Stroke::new(1.5_f32, model.theme.warn))
                    .inner_margin(4)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("⚠ {}", tr.t("MONKEY_EME_READONLY_BANNER")))
                                .color(model.theme.warn),
                        );
                    });
            }
        });

        // Status bar.
        egui::TopBottomPanel::bottom("eme_status").show(ctx, |ui| {
            status_bar(ui, model);
        });

        // Left: tool options, a floating box as tall as its content (top-left, under the strip),
        // so the host's chat in the bottom-left corner stays in view.
        if self.layout.left_open {
            let top = ctx.available_rect().top();
            let r = egui::Window::new("eme_left")
                .title_bar(false)
                .collapsible(false)
                .movable(false)
                .resizable([true, false])
                .fixed_pos(egui::pos2(6.0, top + 6.0))
                .default_width(self.layout.left_width)
                .min_width(PANEL_MIN)
                .max_width(PANEL_MAX)
                .show(ctx, |ui| options_panel(ui, &mut f));
            if let Some(r) = r {
                self.layout.left_width = r.response.rect.width().clamp(PANEL_MIN, PANEL_MAX);
            }
        }

        // Right: history / restore / log.
        if self.layout.right_open {
            let tab = &mut self.layout.right_tab;
            let mut texts = RightTexts {
                label: &mut self.snapshot_label,
                filter: &mut self.filter,
                pending: &mut self.pending,
                project_name: &mut self.project_name,
                project_desc: &mut self.project_desc,
                history_project_only: &mut self.history_project_only,
            };
            let r = egui::SidePanel::right("eme_right")
                .resizable(true)
                .default_width(self.layout.right_width)
                .width_range(PANEL_MIN..=PANEL_MAX)
                .show(ctx, |ui| right_panel(ui, &mut f, tab, &mut texts));
            self.layout.right_width = r.response.rect.width().clamp(PANEL_MIN, PANEL_MAX);
        }

        // Needs-confirmation modal (Q4: warn, then confirm with force).
        confirm_modal(ctx, &mut f);
        pending_modal(ctx, &mut f, &mut self.pending);
        project_conflict_modal(ctx, &mut f);

        let (over, kb) = (
            ctx.is_pointer_over_area() || ctx.wants_pointer_input(),
            ctx.wants_keyboard_input(),
        );
        out.pointer_over_ui = over;
        out.keyboard_captured = kb;
        out
    }
}

fn top_strip(ui: &mut Ui, f: &mut Frame, layout: &mut EmeLayout) {
    let m = f.m;
    let tr = m.tr;
    let perms = m.session.perms;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(tr.t("MONKEY_EME_TITLE")).strong());
        ui.separator();
        // The toolbar: only the tools this rank may use AND the server offers
        // (`EmeToolRegistry::visible`); nothing to grey out, nothing to explain.
        let active = m.tools.active_id();
        for id in m.tools.visible(&perms, m.session.caps.as_ref()) {
            let is_active = active == Some(id);
            let name = f.tool_name(id);
            let text = if is_active {
                format!("› {name}")
            } else {
                name
            };
            // MONKEY (EME projects): a map-editing tool is locked until a project is active.
            let locked = f.needs_project && m.tools.get(id.as_str()).is_some_and(|t| t.min_rank() >= 3);
            let tip = locked.then(|| tr.t("MONKEY_EME_NEEDS_PROJECT").to_owned());
            if f.button(ui, format!("tool:{id}"), text, !locked, is_active, tip) {
                f.push(EmeUiCommand::ActivateTool(id));
            }
        }
        ui.separator();

        // Undo / redo.
        let (undo_tip, redo_tip) = undo_redo_tips(f, m.undo);
        let undo_ok = f.can_edit && m.undo.can_undo();
        let redo_ok = f.can_edit && m.undo.can_redo();
        if f.button(
            ui,
            "undo".into(),
            tr.t("MONKEY_EME_UNDO").to_owned(),
            undo_ok,
            false,
            Some(if f.can_edit {
                undo_tip
            } else {
                tr.t("MONKEY_EME_NEEDS_EDIT").to_owned()
            }),
        ) {
            f.push(EmeUiCommand::Undo);
        }
        if f.button(
            ui,
            "redo".into(),
            tr.t("MONKEY_EME_REDO").to_owned(),
            redo_ok,
            false,
            Some(if f.can_edit {
                redo_tip
            } else {
                tr.t("MONKEY_EME_NEEDS_EDIT").to_owned()
            }),
        ) {
            f.push(EmeUiCommand::Redo);
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Link and rank: glyph + word, never colour alone.
            let (glyph, key, colour) = match m.session.link {
                eme_core::EmeLink::Up { .. } => ("⚫", "MONKEY_EME_LINK_UP", m.theme.ok),
                eme_core::EmeLink::Handshaking => {
                    ("○", "MONKEY_EME_LINK_HANDSHAKING", m.theme.warn)
                }
                eme_core::EmeLink::Unsupported => {
                    ("✖", "MONKEY_EME_LINK_UNSUPPORTED", m.theme.error)
                }
                eme_core::EmeLink::Down => ("✖", "MONKEY_EME_LINK_DOWN", m.theme.error),
            };
            ui.label(
                RichText::new(format!(
                    "{glyph} {}  |  {}",
                    tr.t(key),
                    tr.f("MONKEY_EME_RANK", &[&perms.rank().0.to_string()])
                ))
                .color(colour),
            );
            ui.separator();
            ui.toggle_value(&mut layout.right_open, tr.t("MONKEY_EME_TOGGLE_RIGHT"));
            ui.toggle_value(&mut layout.left_open, tr.t("MONKEY_EME_TOGGLE_LEFT"));
        });
    });
}

/// MONKEY (EME projects): the active project (or the "no project" warning) under the tool strip.
fn project_bar(ui: &mut Ui, f: &mut Frame, layout: &mut EmeLayout) {
    let m = f.m;
    let tr = m.tr;
    if !f.can_edit || !m.session.projects_required {
        return;
    }
    let mut open_tab = false;
    ui.horizontal_wrapped(|ui| match &m.session.project {
        Some(p) => {
            ui.label(RichText::new(format!("▣ {}", tr.f("MONKEY_EME_PROJECT_ACTIVE", &[&p.name]))).strong().color(m.theme.ok));
            if f.button(ui, "project:info".into(), tr.t("MONKEY_EME_PROJECT_INFO").to_owned(), true, false, None) {
                f.push(EmeUiCommand::Service(EmeServiceRequest::ProjectInfo { project: p.id }));
                open_tab = true;
            }
            if f.button(ui, "project:leave".into(), tr.t("MONKEY_EME_PROJECT_LEAVE").to_owned(), true, false, None) {
                f.push(EmeUiCommand::Service(EmeServiceRequest::ProjectLeave));
            }
            if f.button(ui, "project:browse".into(), tr.t("MONKEY_EME_PROJECT_CHOOSE").to_owned(), true, false, None) {
                open_tab = true;
            }
        }
        None => {
            ui.label(RichText::new(format!("⚠ {}", tr.t("MONKEY_EME_PROJECT_NONE"))).color(m.theme.warn));
            if f.button(ui, "project:browse".into(), tr.t("MONKEY_EME_PROJECT_CHOOSE").to_owned(), true, false, None) {
                open_tab = true;
            }
        }
    });
    if open_tab {
        layout.right_open = true;
        if layout.right_tab != EmeRightTab::Projects {
            layout.right_tab = EmeRightTab::Projects;
            f.push(EmeUiCommand::TabOpened(EmeRightTab::Projects));
        }
    }
}

/// The error / result line under the strip, with a dismiss button.
fn notice_line(ui: &mut Ui, f: &mut Frame) {
    let m = f.m;
    let n = m.notice;
    if n.key.is_empty() {
        return;
    }
    // An argument that is itself a locale key (`MONKEY_EME_*`) is shown translated.
    let args: Vec<&str> = n
        .args
        .iter()
        .map(|a| {
            if a.starts_with("MONKEY_EME_") {
                m.tr.try_t(a).unwrap_or(a.as_str())
            } else {
                a.as_str()
            }
        })
        .collect();
    let text = m.tr.f(&n.key, &args);
    let (glyph, colour) = if n.error {
        ("✖", m.theme.error)
    } else {
        ("✔", m.theme.ok)
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{glyph} {text}")).color(colour));
        if f.button(
            ui,
            "notice:dismiss".into(),
            m.tr.t("MONKEY_EME_DISMISS").to_owned(),
            true,
            false,
            None,
        ) {
            f.push(EmeUiCommand::DismissNotice);
        }
    });
}

fn entry_label(f: &Frame, e: &EmeUndoEntry) -> String {
    let id = e.meta().tool.as_str();
    let key = format!("MONKEY_EME_{}_NAME", id.to_ascii_uppercase());
    f.m.tr
        .try_t(&key)
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_owned())
}

fn undo_redo_tips(f: &Frame, undo: &EmeUndoStack) -> (String, String) {
    let tr = f.m.tr;
    let next_undo = undo.entries().iter().rev().find(|e| {
        matches!(
            e.state(),
            EmeEntryState::Applied | EmeEntryState::Committing
        )
    });
    let next_redo = undo
        .entries()
        .iter()
        .find(|e| matches!(e.state(), EmeEntryState::Undone));
    (
        match next_undo {
            Some(e) if undo.can_undo() => tr.f("MONKEY_EME_UNDO_TIP", &[&entry_label(f, e)]),
            _ => tr.t("MONKEY_EME_UNDO_NONE").to_owned(),
        },
        match next_redo {
            Some(e) if undo.can_redo() => tr.f("MONKEY_EME_REDO_TIP", &[&entry_label(f, e)]),
            _ => tr.t("MONKEY_EME_REDO_NONE").to_owned(),
        },
    )
}

fn status_bar(ui: &mut Ui, m: &EmeShellModel) {
    let tr = m.tr;
    let s = m.status;
    ui.horizontal(|ui| {
        let part = |ui: &mut Ui, text: String| {
            ui.label(RichText::new(text).color(m.theme.text_dim));
            ui.separator();
        };
        part(ui, tr.f("MONKEY_EME_STATUS_MAP", &[&s.map.to_string()]));
        part(ui, tr.f("MONKEY_EME_STATUS_REGION", &[&s.region]));
        part(ui, tr.f("MONKEY_EME_STATUS_EPOCH", &[&s.epoch.to_string()]));
        part(
            ui,
            tr.f("MONKEY_EME_STATUS_PENDING", &[&s.pending.to_string()]),
        );
        part(
            ui,
            tr.f(
                "MONKEY_EME_STATUS_REMESH",
                &[&format!("{:.1}", s.remesh_ms)],
            ),
        );
        part(
            ui,
            tr.f("MONKEY_EME_STATUS_RATE", &[&format!("{:.1}", s.rate)]),
        );
        // MONKEY (EME projects).
        if m.session.projects_required {
            let text = match &m.session.project {
                Some(p) => tr.f("MONKEY_EME_STATUS_PROJECT", &[&p.name]),
                None => tr.t("MONKEY_EME_STATUS_NO_PROJECT").to_owned(),
            };
            let colour = if m.session.project.is_some() { m.theme.ok } else { m.theme.warn };
            ui.label(RichText::new(text).color(colour));
        }
    });
}

fn options_panel(ui: &mut Ui, f: &mut Frame) {
    let m = f.m;
    let tr = m.tr;
    ui.heading(tr.t("MONKEY_EME_OPTIONS_TITLE"));
    ui.separator();
    let Some(id) = m.tools.active_id() else {
        ui.label(tr.t("MONKEY_EME_NO_TOOL"));
        return;
    };
    let Some(tool) = m.tools.get(id.as_str()) else {
        ui.label(tr.t("MONKEY_EME_NO_TOOL"));
        return;
    };
    ui.label(RichText::new(f.tool_name(id)).strong());
    // MONKEY (EME overlay): water / area edits the server does not enforce (CONTRACT §11.5,
    // `caps.enforced = 0`) are visual only: every client draws them, the server ignores them.
    if let Some(layer) = overlay_layer_of(id.as_str()) {
        let enforced = m.session.caps.as_ref().is_some_and(|c| {
            c.enforced && c.enforced_layers.contains(&layer)
        });
        if !enforced {
            ui.label(
                RichText::new(format!("⚠ {}", tr.t("MONKEY_EME_VISUAL_ONLY")))
                    .color(m.theme.warn),
            )
            .on_hover_text(tr.t("MONKEY_EME_VISUAL_ONLY_TIP"));
        }
    }
    let fields = tool.options_ui_model();
    if fields.is_empty() {
        ui.label(tr.t("MONKEY_EME_NO_OPTIONS"));
        return;
    }
    // A read-only rank may still use a read-only tool's controls.
    let mutating = tool.min_rank() >= 3;
    // MONKEY (EME projects): a map-editing tool's controls stay locked without a project.
    let locked = mutating && f.needs_project;
    if locked {
        ui.label(RichText::new(format!("⚠ {}", tr.t("MONKEY_EME_NEEDS_PROJECT"))).color(m.theme.warn));
    }
    let tool_enabled = (f.can_edit || !mutating) && !locked;
    egui::ScrollArea::vertical().show(ui, |ui| {
        for field in &fields {
            option_field(ui, f, id, field, tool_enabled && !field.disabled);
        }
    });
}

/// The overlay layer a tool edits (`water` → liquid, `area` → area).
fn overlay_layer_of(tool: &str) -> Option<eme_core::service::EmeOverlayLayer> {
    match tool {
        "water" => Some(eme_core::service::EmeOverlayLayer::Liquid),
        "area" => Some(eme_core::service::EmeOverlayLayer::Area),
        _ => None,
    }
}

fn option_field(
    ui: &mut Ui,
    f: &mut Frame,
    tool: EmeToolId,
    field: &EmeOptionField,
    enabled: bool,
) {
    let label = f.label(&field.label);
    let pid = format!("opt:{}", field.key);
    let mut emit: Option<EmeOptionValue> = None;
    let mut resp_opt: Option<egui::Response> = None;
    ui.add_enabled_ui(enabled, |ui| match (&field.kind, &field.value) {
        (EmeOptionKind::Slider { min, max, step }, EmeOptionValue::Number(v)) => {
            let mut x = *v;
            let r = ui.add(
                egui::Slider::new(&mut x, *min..=*max)
                    .step_by(f64::from(*step))
                    .text(label.clone()),
            );
            if r.changed() {
                emit = Some(EmeOptionValue::Number(x));
            }
            resp_opt = Some(r);
        }
        (EmeOptionKind::Number, EmeOptionValue::Number(v)) => {
            let mut x = *v;
            ui.horizontal(|ui| {
                let r = ui.add(egui::DragValue::new(&mut x));
                ui.label(label.clone());
                if r.changed() {
                    emit = Some(EmeOptionValue::Number(x));
                }
                resp_opt = Some(r);
            });
        }
        (EmeOptionKind::Toggle, EmeOptionValue::Bool(v)) => {
            let mut x = *v;
            let r = ui.checkbox(&mut x, label.clone());
            if r.changed() {
                emit = Some(EmeOptionValue::Bool(x));
            }
            resp_opt = Some(r);
        }
        (EmeOptionKind::Choice(labels), EmeOptionValue::Choice(i)) => {
            let mut sel = *i;
            let shown = labels.get(sel).map(|k| f.label(k)).unwrap_or_default();
            ui.horizontal(|ui| {
                let combo = egui::ComboBox::from_id_salt(("eme_opt", field.key))
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        for (n, k) in labels.iter().enumerate() {
                            ui.selectable_value(&mut sel, n, f.label(k));
                        }
                    });
                ui.label(label.clone());
                resp_opt = Some(combo.response);
            });
            if sel != *i {
                emit = Some(EmeOptionValue::Choice(sel));
            }
        }
        (EmeOptionKind::Text, EmeOptionValue::Text(t)) => {
            let mut x = t.clone();
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut x).desired_width(120.0));
                ui.label(label.clone());
                if r.changed() {
                    emit = Some(EmeOptionValue::Text(x.clone()));
                }
                resp_opt = Some(r);
            });
        }
        (EmeOptionKind::Button, _) => {
            let r = ui.button(label.clone());
            if r.clicked() {
                emit = Some(EmeOptionValue::Click);
            }
            resp_opt = Some(r);
        }
        _ => {
            // Kind and value disagree (a tool bug): show the label, never panic.
            ui.label(format!("{label} ?"));
        }
    });
    if let Some(r) = resp_opt {
        f.rec(pid, &r, enabled, None);
    }
    if let (true, Some(value)) = (enabled, emit) {
        f.push(EmeUiCommand::SetOption {
            tool,
            key: field.key.to_owned(),
            value,
        });
    }
}

/// The right panel's text fields and toggles (borrowed from [`EmeShell`]).
struct RightTexts<'a> {
    label: &'a mut String,
    filter: &'a mut EmeLogFilterUi,
    pending: &'a mut Option<EmePending>,
    project_name: &'a mut String,
    project_desc: &'a mut String,
    history_project_only: &'a mut bool,
}

fn right_panel(ui: &mut Ui, f: &mut Frame, tab: &mut EmeRightTab, x: &mut RightTexts) {
    let tr = f.m.tr;
    ui.horizontal_wrapped(|ui| {
        for (t, key, name) in [
            (EmeRightTab::History, "MONKEY_EME_TAB_HISTORY", "history"),
            (EmeRightTab::Restore, "MONKEY_EME_TAB_RESTORE", "restore"),
            (EmeRightTab::Log, "MONKEY_EME_TAB_LOG", "log"),
            (EmeRightTab::Projects, "MONKEY_EME_TAB_PROJECTS", "projects"),
        ] {
            let selected = *tab == t;
            let text = if selected {
                format!("› {}", tr.t(key))
            } else {
                tr.t(key).to_owned()
            };
            if f.button(ui, format!("tab:{name}"), text, true, selected, None) && !selected {
                *tab = t;
                f.push(EmeUiCommand::TabOpened(t));
            }
        }
    });
    ui.separator();
    match *tab {
        EmeRightTab::History => history_tab(ui, f, x.history_project_only),
        EmeRightTab::Restore => restore_tab(ui, f, x.label, x.pending),
        EmeRightTab::Log => log_tab(ui, f, x.filter, x.pending),
        EmeRightTab::Projects => projects_tab(ui, f, x.project_name, x.project_desc, x.pending),
    }
}

/// MONKEY (EME projects): the locale key of a project status.
fn status_key(status: &str) -> &'static str {
    match status {
        "open" => "MONKEY_EME_PROJECT_STATUS_OPEN",
        "closed" => "MONKEY_EME_PROJECT_STATUS_CLOSED",
        _ => "MONKEY_EME_PROJECT_STATUS_REVERTED",
    }
}

/// MONKEY (EME projects): create, list (mine / open / all), open, close, revert, un-revert, info.
fn projects_tab(
    ui: &mut Ui,
    f: &mut Frame,
    name: &mut String,
    desc: &mut String,
    pending: &mut Option<EmePending>,
) {
    let m = f.m;
    let tr = m.tr;
    let can_revert_others = m.session.perms.has(EmeCapability::RevertOthers);
    let active = m.session.project.as_ref().map(|p| p.id);
    // Create.
    ui.label(RichText::new(tr.t("MONKEY_EME_PROJECT_NEW_TITLE")).strong());
    ui.horizontal(|ui| {
        ui.label(tr.t("MONKEY_EME_PROJECT_NAME"));
        let r = ui.add_enabled(f.can_edit, egui::TextEdit::singleline(name).char_limit(64).desired_width(150.0));
        f.rec("project:name".into(), &r, f.can_edit, None);
    });
    ui.horizontal(|ui| {
        ui.label(tr.t("MONKEY_EME_PROJECT_DESC"));
        let r = ui.add_enabled(f.can_edit, egui::TextEdit::singleline(desc).char_limit(255).desired_width(150.0));
        f.rec("project:desc".into(), &r, f.can_edit, None);
    });
    let tip = (!f.can_edit).then(|| tr.t("MONKEY_EME_NEEDS_EDIT").to_owned());
    if f.button(
        ui,
        "project:create".into(),
        tr.t("MONKEY_EME_PROJECT_CREATE").to_owned(),
        f.can_edit && !name.trim().is_empty(),
        false,
        tip,
    ) {
        let d = desc.trim();
        f.push(EmeUiCommand::Service(EmeServiceRequest::ProjectCreate {
            name: name.trim().to_owned(),
            description: (!d.is_empty()).then(|| d.to_owned()),
        }));
        name.clear();
        desc.clear();
    }
    ui.separator();
    // Filter.
    ui.horizontal_wrapped(|ui| {
        for (flt, key, id) in [
            (EmeProjectFilter::Mine, "MONKEY_EME_PROJECT_FILTER_MINE", "mine"),
            (EmeProjectFilter::Open, "MONKEY_EME_PROJECT_FILTER_OPEN", "open"),
            (EmeProjectFilter::All, "MONKEY_EME_PROJECT_FILTER_ALL", "all"),
        ] {
            let sel = m.projects.filter == flt;
            if f.button(ui, format!("project:filter:{id}"), tr.t(key).to_owned(), true, sel, None) {
                f.push(EmeUiCommand::ProjectFilter(flt));
            }
        }
        if f.button(ui, "project:refresh".into(), tr.t("MONKEY_EME_PROJECT_REFRESH").to_owned(), true, false, None) {
            f.push(EmeUiCommand::ProjectFilter(m.projects.filter));
        }
    });
    // Info of the selected project.
    if let Some(info) = &m.projects.info {
        project_info_box(ui, f, info);
    }
    ui.separator();
    if m.projects.rows.is_empty() {
        ui.label(tr.t("MONKEY_EME_PROJECT_LIST_EMPTY"));
        return;
    }
    egui::ScrollArea::vertical().id_salt("eme_projects").show(ui, |ui| {
        for (n, row) in m.projects.rows.iter().enumerate() {
            let is_active = active == Some(row.id);
            let mut text = tr.f(
                "MONKEY_EME_PROJECT_ROW",
                &[&row.name, &row.owner, tr.t(status_key(&row.status)), &row.edits.to_string()],
            );
            if is_active {
                text = format!("▣ {text} {}", tr.t("MONKEY_EME_PROJECT_IS_ACTIVE"));
            }
            let colour = match row.status.as_str() {
                "open" => m.theme.text,
                _ => m.theme.text_dim,
            };
            ui.vertical(|ui| {
                let resp = ui.label(RichText::new(text).color(colour));
                if !row.description.is_empty() {
                    resp.on_hover_text(row.description.clone());
                }
                ui.horizontal_wrapped(|ui| {
                    let open = row.status == "open";
                    let own = row.mine || can_revert_others;
                    if open && !is_active {
                        let tip = (!f.can_edit).then(|| tr.t("MONKEY_EME_NEEDS_EDIT").to_owned());
                        if f.button(ui, format!("project:open:{n}"), tr.t("MONKEY_EME_PROJECT_OPEN").to_owned(), f.can_edit, false, tip) {
                            f.push(EmeUiCommand::Service(EmeServiceRequest::ProjectOpen { project: row.id }));
                        }
                    }
                    if f.button(ui, format!("project:info:{n}"), tr.t("MONKEY_EME_PROJECT_INFO").to_owned(), true, false, None) {
                        f.push(EmeUiCommand::Service(EmeServiceRequest::ProjectInfo { project: row.id }));
                    }
                    if open {
                        let ok = f.can_edit && own;
                        let tip = (!ok).then(|| tr.t("MONKEY_EME_PROJECT_OWNER_ONLY").to_owned());
                        if f.button(ui, format!("project:close:{n}"), tr.t("MONKEY_EME_PROJECT_CLOSE").to_owned(), ok, false, tip) {
                            *pending = Some(EmePending::ProjectClose { project: row.id, name: row.name.clone() });
                        }
                    }
                    if row.status != "reverted" {
                        let ok = f.can_edit && own;
                        let tip = (!ok).then(|| tr.t("MONKEY_EME_PROJECT_OWNER_ONLY").to_owned());
                        if f.button(ui, format!("project:revert:{n}"), tr.t("MONKEY_EME_PROJECT_REVERT").to_owned(), ok, false, tip) {
                            *pending = Some(EmePending::ProjectRevert { project: row.id, name: row.name.clone() });
                        }
                    } else {
                        let tip = (!can_revert_others).then(|| tr.t("MONKEY_EME_RESTORE_REVERT_RANK").to_owned());
                        if f.button(ui, format!("project:unrevert:{n}"), tr.t("MONKEY_EME_PROJECT_UNREVERT").to_owned(), can_revert_others, false, tip) {
                            *pending = Some(EmePending::ProjectUnrevert { project: row.id, name: row.name.clone() });
                        }
                    }
                });
            });
            ui.add_space(2.0);
        }
    });
}

/// MONKEY (EME projects): `project_info` — counts per layer, authors, time span, "go to".
fn project_info_box(ui: &mut Ui, f: &mut Frame, info: &eme_core::EmeProjectInfo) {
    let m = f.m;
    let tr = m.tr;
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0_f32, m.theme.text_dim))
        .inner_margin(4)
        .show(ui, |ui| {
            let p = &info.project;
            ui.label(RichText::new(tr.f("MONKEY_EME_PROJECT_INFO_TITLE", &[&p.name])).strong());
            if !p.description.is_empty() {
                ui.label(RichText::new(p.description.clone()).color(m.theme.text_dim));
            }
            ui.label(tr.f("MONKEY_EME_PROJECT_INFO_OWNER", &[&p.owner, tr.t(status_key(&p.status))]));
            let c = |k: &str| info.counts.get(k).copied().unwrap_or_default();
            let (v, l, a) = (c("voxel"), c("liquid"), c("area"));
            ui.label(tr.f(
                "MONKEY_EME_PROJECT_INFO_COUNTS",
                &[
                    &v.live.to_string(),
                    &v.total.to_string(),
                    &l.live.to_string(),
                    &l.total.to_string(),
                    &a.live.to_string(),
                    &a.total.to_string(),
                ],
            ));
            if info.authors.is_empty() {
                ui.label(tr.t("MONKEY_EME_PROJECT_INFO_EMPTY"));
            } else {
                let names: Vec<String> = info
                    .authors
                    .iter()
                    .map(|a| format!("{} ({})", a.name, a.edits))
                    .collect();
                ui.label(tr.f("MONKEY_EME_PROJECT_INFO_AUTHORS", &[&names.join(", ")]));
            }
            if let (Some(t0), Some(t1)) = (info.first_edit, info.last_edit) {
                ui.label(tr.f("MONKEY_EME_PROJECT_INFO_SPAN", &[&hms(t0), &hms(t1)]));
            }
            if let Some(b) = info.bbox {
                if f.button(ui, "project:info:show".into(), tr.t("MONKEY_EME_SHOW").to_owned(), true, false, None) {
                    f.push(EmeUiCommand::ShowInWorld {
                        x: (b[0] + b[2]) * 0.5,
                        y: (b[1] + b[3]) * 0.5,
                        z: None,
                    });
                }
            }
        });
}

/// MONKEY (EME projects): a project revert came back `others_edited`: force it (rank 4) or cancel.
fn project_conflict_modal(ctx: &egui::Context, f: &mut Frame) {
    let Some(c) = f.m.projects.conflict.clone() else { return };
    let tr = f.m.tr;
    let can = f.m.session.perms.has(EmeCapability::RevertOthers);
    egui::Modal::new(egui::Id::new("eme_project_conflict")).show(ctx, |ui| {
        ui.set_max_width(400.0);
        ui.heading(format!("⚠ {}", tr.t("MONKEY_EME_PROJECT_OTHERS_TITLE")));
        ui.add_space(6.0);
        ui.label(tr.f(
            "MONKEY_EME_PROJECT_OTHERS_BODY",
            &[&c.others.join(", "), &c.name, &c.count.to_string()],
        ));
        if !can {
            ui.label(RichText::new(tr.t("MONKEY_EME_PROJECT_FORCE_RANK")).color(f.m.theme.warn));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let tip = (!can).then(|| tr.t("MONKEY_EME_PROJECT_FORCE_RANK").to_owned());
            if f.button(ui, "project:force".into(), tr.t("MONKEY_EME_PROJECT_FORCE").to_owned(), can, false, tip) {
                f.push(EmeUiCommand::Service(EmeServiceRequest::ProjectRevert {
                    project: c.project,
                    force: true,
                }));
                f.push(EmeUiCommand::DismissProjectConflict);
            }
            if f.button(ui, "project:force:no".into(), tr.t("MONKEY_EME_CONFIRM_NO").to_owned(), true, false, None) {
                f.push(EmeUiCommand::DismissProjectConflict);
            }
        });
    });
}

/// Glyph + locale key for an entry state; shape and word, never colour alone.
fn state_view(s: &EmeEntryState) -> (&'static str, &'static str) {
    match s {
        EmeEntryState::Open
        | EmeEntryState::Committing
        | EmeEntryState::UndoPending
        | EmeEntryState::RedoPending => ("○", "MONKEY_EME_STATE_PENDING"),
        EmeEntryState::Applied => ("✔", "MONKEY_EME_STATE_APPLIED"),
        EmeEntryState::Undone => ("⟲", "MONKEY_EME_STATE_UNDONE"),
        EmeEntryState::NeedsConfirm(_) => ("⚠", "MONKEY_EME_STATE_NEEDS_CONFIRM"),
    }
}

/// The localized name of a voxel mode (`MONKEY_EME_MODE_*`), or a dash.
fn mode_name(tr: crate::i18n::Tr, mode: Option<u8>) -> String {
    let key = match mode {
        Some(0) => "MONKEY_EME_MODE_DIG",
        Some(1) => "MONKEY_EME_MODE_BUILD",
        Some(2) => "MONKEY_EME_MODE_SMOOTH",
        Some(3) => "MONKEY_EME_MODE_FLATTEN",
        _ => return "-".to_owned(),
    };
    tr.t(key).to_owned()
}

fn history_tab(ui: &mut Ui, f: &mut Frame, project_only: &mut bool) {
    let m = f.m;
    let tr = m.tr;
    // MONKEY (EME projects): "active project only" narrows my entries and hides the others' rows.
    let active = m.session.project.as_ref().map(|p| p.id);
    if m.session.projects_required {
        let r = ui.checkbox(project_only, tr.t("MONKEY_EME_FILTER_PROJECT"));
        f.rec("history:project_only".into(), &r, true, None);
    }
    let only = *project_only && m.session.projects_required;
    let mine: Vec<&EmeUndoEntry> = m
        .undo
        .entries()
        .iter()
        .filter(|e| !only || (active.is_some() && e.meta().project == active))
        .collect();
    let others_shown = !only;
    if mine.is_empty() && (m.others.rows.is_empty() || !others_shown) {
        ui.label(tr.t("MONKEY_EME_HISTORY_EMPTY"));
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        if mine.is_empty() {
            ui.label(tr.t("MONKEY_EME_HISTORY_EMPTY"));
        }
        for e in mine.iter().rev() {
            let (glyph, key) = state_view(e.state());
            let colour = match e.state() {
                EmeEntryState::Applied => m.theme.ok,
                EmeEntryState::NeedsConfirm(_) => m.theme.warn,
                EmeEntryState::Undone => m.theme.text_dim,
                _ => m.theme.text,
            };
            let mut line = format!("{glyph} {}  ·  {}", entry_label(f, e), tr.t(key));
            if e.pending() > 0 {
                line.push_str(&format!("  ({})", e.pending()));
            }
            ui.label(RichText::new(line).color(colour));
        }
        if !others_shown {
            return;
        }
        // Other GMs' strokes: theirs to undo (or an admin's, from the Log tab); shown read-only.
        ui.separator();
        ui.label(RichText::new(tr.t("MONKEY_EME_OTHERS_TITLE")).strong());
        if m.others.rows.is_empty() {
            ui.label(RichText::new(tr.t("MONKEY_EME_OTHERS_NONE")).color(m.theme.text_dim));
        }
        for (n, r) in m.others.rows.iter().enumerate() {
            let who = if r.name.is_empty() {
                format!("#{}", r.actor)
            } else {
                r.name.clone()
            };
            let ago = m.others.now_ms.saturating_sub(r.seen_ms) / 1000;
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(tr.f(
                        "MONKEY_EME_OTHERS_ROW",
                        &[
                            &who,
                            &mode_name(tr, Some(r.mode)),
                            &r.edits.to_string(),
                            &ago.to_string(),
                        ],
                    ))
                    .color(m.theme.text_dim),
                );
                if f.button(
                    ui,
                    format!("others:show:{n}"),
                    tr.t("MONKEY_EME_SHOW").to_owned(),
                    true,
                    false,
                    None,
                ) {
                    f.push(EmeUiCommand::ShowInWorld {
                        x: r.pos[0],
                        y: r.pos[1],
                        z: Some(r.pos[2]),
                    });
                }
            });
        }
    });
}

fn restore_tab(ui: &mut Ui, f: &mut Frame, label: &mut String, pending: &mut Option<EmePending>) {
    let m = f.m;
    let tr = m.tr;
    ui.horizontal(|ui| {
        ui.label(tr.t("MONKEY_EME_RESTORE_LABEL"));
        ui.add_enabled(
            f.can_edit,
            egui::TextEdit::singleline(label).desired_width(140.0),
        );
    });
    let create_ok = f.can_edit && !label.trim().is_empty();
    let tip = (!f.can_edit).then(|| tr.t("MONKEY_EME_NEEDS_EDIT").to_owned());
    if f.button(
        ui,
        "restore:create".into(),
        tr.t("MONKEY_EME_RESTORE_CREATE").to_owned(),
        create_ok,
        false,
        tip,
    ) {
        f.push(EmeUiCommand::Service(EmeServiceRequest::SnapshotCreate {
            label: label.trim().to_owned(),
            radius: None,
        }));
        label.clear();
    }
    ui.separator();
    if m.restore.rows.is_empty() {
        ui.label(tr.t("MONKEY_EME_RESTORE_EMPTY"));
        return;
    }
    let can_revert = m.session.perms.has(EmeCapability::RevertOthers);
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (n, row) in m.restore.rows.iter().enumerate() {
            let selected = m.restore.selected == Some(row.snapshot);
            // The row's text on its own line, its buttons under it (a narrow panel wraps nothing).
            ui.vertical(|ui| {
                let text = tr.f(
                    "MONKEY_EME_RESTORE_ROW",
                    &[&row.label, &row.name, &format!("{:.0}", row.radius)],
                );
                let resp = ui.selectable_label(selected, text);
                f.rec(format!("restore:select:{n}"), &resp, true, None);
                if resp.clicked() {
                    f.push(EmeUiCommand::SelectRestore(
                        (!selected).then_some(row.snapshot),
                    ));
                }
                ui.horizontal(|ui| {
                    if f.button(
                        ui,
                        format!("restore:show:{n}"),
                        tr.t("MONKEY_EME_SHOW").to_owned(),
                        true,
                        false,
                        None,
                    ) {
                        f.push(EmeUiCommand::SelectRestore(Some(row.snapshot)));
                        f.push(EmeUiCommand::ShowInWorld {
                            x: row.x,
                            y: row.y,
                            z: None,
                        });
                    }
                    let tip =
                        (!can_revert).then(|| tr.t("MONKEY_EME_RESTORE_REVERT_RANK").to_owned());
                    if f.button(
                        ui,
                        format!("restore:revert:{n}"),
                        tr.t("MONKEY_EME_RESTORE_REVERT").to_owned(),
                        can_revert,
                        false,
                        tip,
                    ) {
                        *pending = Some(EmePending::Revert {
                            snapshot: row.snapshot,
                            label: row.label.clone(),
                            radius: row.radius,
                        });
                    }
                });
            });
            ui.add_space(2.0);
        }
    });
}

fn hms(t: u64) -> String {
    let s = t % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// A one-line text field with a label; records `id` for tests. True when Enter was pressed in it.
fn text_field(
    ui: &mut Ui,
    f: &mut Frame,
    id: &str,
    label: &str,
    text: &mut String,
    width: f32,
) -> bool {
    ui.label(label);
    let r = ui.add(egui::TextEdit::singleline(text).desired_width(width));
    f.rec(id.to_owned(), &r, true, None);
    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

fn log_filters(
    ui: &mut Ui,
    f: &mut Frame,
    filter: &mut EmeLogFilterUi,
    pending: &mut Option<EmePending>,
) {
    let tr = f.m.tr;
    let mut apply = false;
    ui.horizontal_wrapped(|ui| {
        apply |= text_field(
            ui,
            f,
            "log:filter:player",
            tr.t("MONKEY_EME_LOG_FILTER_PLAYER"),
            &mut filter.player,
            90.0,
        );
        apply |= text_field(
            ui,
            f,
            "log:filter:radius",
            tr.t("MONKEY_EME_LOG_FILTER_RADIUS"),
            &mut filter.radius,
            40.0,
        );
        apply |= text_field(
            ui,
            f,
            "log:filter:minutes",
            tr.t("MONKEY_EME_LOG_FILTER_MINUTES"),
            &mut filter.minutes,
            40.0,
        );
    });
    ui.horizontal_wrapped(|ui| {
        let modes = [
            "MONKEY_EME_LOG_FILTER_ANY",
            "MONKEY_EME_MODE_DIG",
            "MONKEY_EME_MODE_BUILD",
            "MONKEY_EME_MODE_SMOOTH",
            "MONKEY_EME_MODE_FLATTEN",
        ];
        ui.label(tr.t("MONKEY_EME_LOG_FILTER_TOOL"));
        let c = egui::ComboBox::from_id_salt("eme_log_mode")
            .selected_text(tr.t(modes[filter.mode.min(4)]))
            .show_ui(ui, |ui| {
                for (n, k) in modes.iter().enumerate() {
                    ui.selectable_value(&mut filter.mode, n, tr.t(k));
                }
            });
        f.rec("log:filter:tool".into(), &c.response, true, None);
        let layers = [
            "MONKEY_EME_LOG_FILTER_ALL",
            "MONKEY_EME_LAYER_VOXEL",
            "MONKEY_EME_LAYER_LIQUID",
            "MONKEY_EME_LAYER_AREA",
        ];
        ui.label(tr.t("MONKEY_EME_LOG_FILTER_LAYER"));
        let c = egui::ComboBox::from_id_salt("eme_log_layer")
            .selected_text(tr.t(layers[filter.layer.min(3)]))
            .show_ui(ui, |ui| {
                for (n, k) in layers.iter().enumerate() {
                    ui.selectable_value(&mut filter.layer, n, tr.t(k));
                }
            });
        f.rec("log:filter:layer".into(), &c.response, true, None);
    });
    // MONKEY (EME projects): only the active project's rows.
    let active = f.m.session.project.as_ref().map(|p| p.id);
    if f.m.session.projects_required {
        let r = ui.add_enabled(active.is_some(), egui::Checkbox::new(&mut filter.project_only, tr.t("MONKEY_EME_FILTER_PROJECT")));
        f.rec("log:filter:project".into(), &r, active.is_some(), None);
        if r.changed() {
            apply = true;
        }
    }
    ui.horizontal_wrapped(|ui| {
        if f.button(
            ui,
            "log:apply".into(),
            tr.t("MONKEY_EME_LOG_APPLY").to_owned(),
            true,
            false,
            None,
        ) {
            apply = true;
        }
        if f.button(
            ui,
            "log:clear".into(),
            tr.t("MONKEY_EME_LOG_CLEAR").to_owned(),
            true,
            false,
            None,
        ) {
            *filter = EmeLogFilterUi::default();
            apply = true;
        }
        let can_revert = f.m.session.perms.has(EmeCapability::RevertOthers);
        let name = filter.player.trim().to_owned();
        let tip = if !can_revert {
            Some(tr.t("MONKEY_EME_RESTORE_REVERT_RANK").to_owned())
        } else if name.is_empty() {
            Some(tr.t("MONKEY_EME_LOG_UNDO_PLAYER_HINT").to_owned())
        } else {
            None
        };
        if f.button(
            ui,
            "log:undo_player".into(),
            tr.t("MONKEY_EME_LOG_UNDO_PLAYER").to_owned(),
            can_revert && !name.is_empty(),
            false,
            tip,
        ) {
            *pending = Some(EmePending::UndoPlayer {
                name,
                minutes: filter.minutes_value(),
                radius: filter.radius_value(),
            });
        }
    });
    if apply {
        f.push(EmeUiCommand::LogQuery(filter.query_for(active)));
    }
}

fn log_tab(
    ui: &mut Ui,
    f: &mut Frame,
    filter: &mut EmeLogFilterUi,
    pending: &mut Option<EmePending>,
) {
    let m = f.m;
    let tr = m.tr;
    let log = m.log;
    log_filters(ui, f, filter, pending);
    ui.separator();
    if log.rows.is_empty() {
        ui.label(tr.t("MONKEY_EME_LOG_EMPTY"));
    } else {
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 40.0)
            .show(ui, |ui| {
                egui::Grid::new("eme_log_grid")
                    .striped(true)
                    .show(ui, |ui| {
                        for k in [
                            "MONKEY_EME_LOG_COL_TIME",
                            "MONKEY_EME_LOG_COL_PLAYER",
                            "MONKEY_EME_LOG_COL_TOOL",
                            "MONKEY_EME_LOG_COL_LAYER",
                            "MONKEY_EME_LOG_COL_WHERE",
                        ] {
                            ui.label(RichText::new(tr.t(k)).strong());
                        }
                        ui.label("");
                        ui.end_row();
                        for (n, r) in log.rows.iter().enumerate() {
                            let dim = r.reverted;
                            let col = if dim { m.theme.text_dim } else { m.theme.text };
                            ui.label(RichText::new(hms(r.t)).color(col));
                            let mut who = r.name.clone();
                            if dim {
                                who.push_str(&format!(" ({})", tr.t("MONKEY_EME_LOG_REVERTED")));
                            }
                            // Clicking a name fills the player filter (then "Undo this player").
                            let resp = ui
                                .add(
                                    egui::Label::new(RichText::new(who).color(col))
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_text(tr.t("MONKEY_EME_LOG_PICK_PLAYER"));
                            f.rec(format!("log:who:{n}"), &resp, true, None);
                            if resp.clicked() {
                                filter.player = r.name.clone();
                            }
                            ui.label(RichText::new(mode_name(tr, r.mode)).color(col));
                            let lk = match r.layer {
                                EmeLayer::Voxel => "MONKEY_EME_LAYER_VOXEL",
                                EmeLayer::Liquid => "MONKEY_EME_LAYER_LIQUID",
                                EmeLayer::Area => "MONKEY_EME_LAYER_AREA",
                            };
                            ui.label(RichText::new(tr.t(lk)).color(col));
                            ui.label(
                                RichText::new(format!("{:.0}, {:.0}, {:.0}", r.x, r.y, r.z))
                                    .color(col),
                            );
                            if f.button(
                                ui,
                                format!("log:show:{n}"),
                                tr.t("MONKEY_EME_SHOW").to_owned(),
                                true,
                                false,
                                None,
                            ) {
                                f.push(EmeUiCommand::ShowInWorld {
                                    x: r.x,
                                    y: r.y,
                                    z: Some(r.z),
                                });
                            }
                            ui.end_row();
                        }
                    });
            });
    }
    ui.separator();
    ui.horizontal(|ui| {
        if f.button(
            ui,
            "log:newer".into(),
            tr.t("MONKEY_EME_LOG_NEWER").to_owned(),
            log.page > 0,
            false,
            None,
        ) {
            f.push(EmeUiCommand::LogPage { older: false });
        }
        if f.button(
            ui,
            "log:older".into(),
            tr.t("MONKEY_EME_LOG_OLDER").to_owned(),
            log.more,
            false,
            None,
        ) {
            f.push(EmeUiCommand::LogPage { older: true });
        }
        ui.label(tr.f(
            "MONKEY_EME_LOG_PAGE",
            &[&(log.page + 1).to_string(), &log.count.to_string()],
        ));
    });
}

/// The yes/no for a revert / undo-player (destructive, rank 4): says exactly what will happen.
fn pending_modal(ctx: &egui::Context, f: &mut Frame, pending: &mut Option<EmePending>) {
    let Some(p) = pending.clone() else { return };
    let tr = f.m.tr;
    let (body, yes) = match &p {
        EmePending::Revert { label, radius, .. } => (
            tr.f("MONKEY_EME_REVERT_BODY", &[label, &format!("{radius:.0}")]),
            tr.t("MONKEY_EME_REVERT_YES").to_owned(),
        ),
        EmePending::UndoPlayer {
            name,
            minutes,
            radius,
        } => (
            tr.f(
                "MONKEY_EME_UNDOPLAYER_BODY",
                &[
                    name,
                    &minutes.unwrap_or(30).to_string(),
                    &format!("{:.0}", radius.unwrap_or(200.0)),
                ],
            ),
            tr.t("MONKEY_EME_UNDOPLAYER_YES").to_owned(),
        ),
        EmePending::ProjectRevert { name, .. } => (
            tr.f("MONKEY_EME_PROJECT_REVERT_BODY", &[name]),
            tr.t("MONKEY_EME_PROJECT_REVERT_YES").to_owned(),
        ),
        EmePending::ProjectUnrevert { name, .. } => (
            tr.f("MONKEY_EME_PROJECT_UNREVERT_BODY", &[name]),
            tr.t("MONKEY_EME_PROJECT_UNREVERT_YES").to_owned(),
        ),
        EmePending::ProjectClose { name, .. } => (
            tr.f("MONKEY_EME_PROJECT_CLOSE_BODY", &[name]),
            tr.t("MONKEY_EME_PROJECT_CLOSE_YES").to_owned(),
        ),
    };
    // Project revert / close: the owner (rank 3) may; the server applies the ownership rule.
    let can = match &p {
        EmePending::ProjectRevert { .. } | EmePending::ProjectClose { .. } => f.can_edit,
        _ => f.m.session.perms.has(EmeCapability::RevertOthers),
    };
    let mut done = false;
    egui::Modal::new(egui::Id::new("eme_pending")).show(ctx, |ui| {
        ui.set_max_width(400.0);
        ui.heading(format!("⚠ {}", tr.t("MONKEY_EME_PENDING_TITLE")));
        ui.add_space(6.0);
        ui.label(body);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if f.button(ui, "pending:yes".into(), yes, can, false, None) {
                let req = match &p {
                    EmePending::Revert { snapshot, .. } => EmeServiceRequest::Revert {
                        snapshot: *snapshot,
                    },
                    EmePending::UndoPlayer {
                        name,
                        minutes,
                        radius,
                    } => EmeServiceRequest::UndoPlayer {
                        name: name.clone(),
                        minutes: *minutes,
                        radius: *radius,
                    },
                    EmePending::ProjectRevert { project, .. } => EmeServiceRequest::ProjectRevert {
                        project: *project,
                        force: false,
                    },
                    EmePending::ProjectUnrevert { project, .. } => {
                        EmeServiceRequest::ProjectUnrevert { project: *project }
                    }
                    EmePending::ProjectClose { project, .. } => {
                        EmeServiceRequest::ProjectClose { project: *project }
                    }
                };
                f.push(EmeUiCommand::Service(req));
                done = true;
            }
            if f.button(
                ui,
                "pending:no".into(),
                tr.t("MONKEY_EME_CONFIRM_NO").to_owned(),
                true,
                false,
                None,
            ) {
                done = true;
            }
        });
    });
    if done {
        *pending = None;
    }
}

fn confirm_modal(ctx: &egui::Context, f: &mut Frame) {
    let m = f.m;
    let tr = m.tr;
    let Some((entry, others)) = m.undo.entries().iter().find_map(|e| match e.state() {
        EmeEntryState::NeedsConfirm(o) => Some((e.meta().id, o.clone())),
        _ => None,
    }) else {
        return;
    };
    f.out.confirm_modal = true;
    egui::Modal::new(egui::Id::new("eme_confirm")).show(ctx, |ui| {
        ui.set_max_width(380.0);
        ui.heading(format!("⚠ {}", tr.t("MONKEY_EME_CONFIRM_TITLE")));
        ui.add_space(6.0);
        ui.label(tr.f(
            "MONKEY_EME_CONFIRM_BODY",
            &[&others.others.join(", "), &others.count.to_string()],
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if f.button(
                ui,
                "confirm:yes".into(),
                tr.t("MONKEY_EME_CONFIRM_YES").to_owned(),
                f.can_edit,
                false,
                None,
            ) {
                f.push(EmeUiCommand::ConfirmUndo(entry));
            }
            if f.button(
                ui,
                "confirm:no".into(),
                tr.t("MONKEY_EME_CONFIRM_NO").to_owned(),
                true,
                false,
                None,
            ) {
                f.push(EmeUiCommand::CancelConfirm(entry));
            }
        });
    });
}

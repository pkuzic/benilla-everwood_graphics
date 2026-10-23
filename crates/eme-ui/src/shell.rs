//! `EmeShell`: the egui layout. Pure drawing: it reads a [`EmeShellModel`] (borrowed resources)
//! and returns the [`EmeUiCommand`]s the user caused. It is independent of bevy systems so a
//! headless test can run it on a bare [`egui::Context`].

use bevy::prelude::Resource;
use bevy_egui::egui::{self, Align, Layout, RichText, Ui};
use eme_core::service::EmeLayer;
use eme_core::{
    EmeCapability, EmeEntryState, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeServiceRequest,
    EmeToolId, EmeToolRegistry, EmeUndoEntry, EmeUndoStack,
};

use crate::i18n::Tr;
use crate::layout::{EmeLayout, EmeRightTab, PANEL_MAX, PANEL_MIN};
use crate::state::{EmeLogTable, EmeRestorePoints, EmeSession, EmeStatus, EmeUiCommand};
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

/// The shell: persistent UI state (layout, text fields). Draw it with [`EmeShell::show`].
#[derive(Resource, Debug, Clone, Default)]
pub struct EmeShell {
    /// Panel layout (persisted by the plugin).
    pub layout: EmeLayout,
    /// The restore-point label being typed.
    pub snapshot_label: String,
    /// Collect an [`EmeProbeEntry`] per control (tests).
    pub probe: bool,
}

struct Frame<'a> {
    m: &'a EmeShellModel<'a>,
    can_edit: bool,
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
            out: &mut out,
            probe: self.probe,
        };
        let tr = model.tr;

        // Top strip.
        let read_only = perms.can_open() && !can_edit;
        egui::TopBottomPanel::top("eme_top").show(ctx, |ui| {
            top_strip(ui, &mut f, &mut self.layout);
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
            let label = &mut self.snapshot_label;
            let r = egui::SidePanel::right("eme_right")
                .resizable(true)
                .default_width(self.layout.right_width)
                .width_range(PANEL_MIN..=PANEL_MAX)
                .show(ctx, |ui| right_panel(ui, &mut f, tab, label));
            self.layout.right_width = r.response.rect.width().clamp(PANEL_MIN, PANEL_MAX);
        }

        // Needs-confirmation modal (Q4: warn, then confirm with force).
        confirm_modal(ctx, &mut f);

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
            if f.button(ui, format!("tool:{id}"), text, true, is_active, None) {
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
    let fields = tool.options_ui_model();
    if fields.is_empty() {
        ui.label(tr.t("MONKEY_EME_NO_OPTIONS"));
        return;
    }
    // A read-only rank may still use a read-only tool's controls.
    let mutating = tool.min_rank() >= 3;
    let tool_enabled = f.can_edit || !mutating;
    egui::ScrollArea::vertical().show(ui, |ui| {
        for field in &fields {
            option_field(ui, f, id, field, tool_enabled && !field.disabled);
        }
    });
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

fn right_panel(ui: &mut Ui, f: &mut Frame, tab: &mut EmeRightTab, label: &mut String) {
    let tr = f.m.tr;
    ui.horizontal(|ui| {
        for (t, key, name) in [
            (EmeRightTab::History, "MONKEY_EME_TAB_HISTORY", "history"),
            (EmeRightTab::Restore, "MONKEY_EME_TAB_RESTORE", "restore"),
            (EmeRightTab::Log, "MONKEY_EME_TAB_LOG", "log"),
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
        EmeRightTab::History => history_tab(ui, f),
        EmeRightTab::Restore => restore_tab(ui, f, label),
        EmeRightTab::Log => log_tab(ui, f),
    }
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

fn history_tab(ui: &mut Ui, f: &mut Frame) {
    let m = f.m;
    let tr = m.tr;
    if m.undo.entries().is_empty() {
        ui.label(tr.t("MONKEY_EME_HISTORY_EMPTY"));
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for e in m.undo.entries().iter().rev() {
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
    });
}

fn restore_tab(ui: &mut Ui, f: &mut Frame, label: &mut String) {
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
            ui.horizontal_wrapped(|ui| {
                ui.label(tr.f(
                    "MONKEY_EME_RESTORE_ROW",
                    &[&row.label, &row.name, &format!("{:.0}", row.radius)],
                ));
                let tip = (!can_revert).then(|| tr.t("MONKEY_EME_RESTORE_REVERT_RANK").to_owned());
                if f.button(
                    ui,
                    format!("restore:revert:{n}"),
                    tr.t("MONKEY_EME_RESTORE_REVERT").to_owned(),
                    can_revert,
                    false,
                    tip,
                ) {
                    f.push(EmeUiCommand::Service(EmeServiceRequest::Revert {
                        snapshot: row.snapshot,
                    }));
                }
            });
        }
    });
}

fn hms(t: u64) -> String {
    let s = t % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn log_tab(ui: &mut Ui, f: &mut Frame) {
    let m = f.m;
    let tr = m.tr;
    let log = m.log;
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
                            "MONKEY_EME_LOG_COL_LAYER",
                            "MONKEY_EME_LOG_COL_WHERE",
                        ] {
                            ui.label(RichText::new(tr.t(k)).strong());
                        }
                        ui.end_row();
                        for r in &log.rows {
                            let dim = r.reverted;
                            let col = if dim { m.theme.text_dim } else { m.theme.text };
                            ui.label(RichText::new(hms(r.t)).color(col));
                            let mut who = r.name.clone();
                            if dim {
                                who.push_str(&format!(" ({})", tr.t("MONKEY_EME_LOG_REVERTED")));
                            }
                            ui.label(RichText::new(who).color(col));
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

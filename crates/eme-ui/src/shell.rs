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
    EmeLogTable, EmeNotice, EmeOthersHistory, EmeRestorePoints, EmeSession, EmeStatus, EmeUiCommand,
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
            let label = &mut self.snapshot_label;
            let filter = &mut self.filter;
            let pending = &mut self.pending;
            let r = egui::SidePanel::right("eme_right")
                .resizable(true)
                .default_width(self.layout.right_width)
                .width_range(PANEL_MIN..=PANEL_MAX)
                .show(ctx, |ui| {
                    right_panel(ui, &mut f, tab, label, filter, pending)
                });
            self.layout.right_width = r.response.rect.width().clamp(PANEL_MIN, PANEL_MAX);
        }

        // Needs-confirmation modal (Q4: warn, then confirm with force).
        confirm_modal(ctx, &mut f);
        pending_modal(ctx, &mut f, &mut self.pending);

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

/// The error / result line under the strip, with a dismiss button.
fn notice_line(ui: &mut Ui, f: &mut Frame) {
    let m = f.m;
    let n = m.notice;
    if n.key.is_empty() {
        return;
    }
    let args: Vec<&str> = n.args.iter().map(String::as_str).collect();
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
    let tool_enabled = f.can_edit || !mutating;
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

fn right_panel(
    ui: &mut Ui,
    f: &mut Frame,
    tab: &mut EmeRightTab,
    label: &mut String,
    filter: &mut EmeLogFilterUi,
    pending: &mut Option<EmePending>,
) {
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
        EmeRightTab::Restore => restore_tab(ui, f, label, pending),
        EmeRightTab::Log => log_tab(ui, f, filter, pending),
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

fn history_tab(ui: &mut Ui, f: &mut Frame) {
    let m = f.m;
    let tr = m.tr;
    if m.undo.entries().is_empty() && m.others.rows.is_empty() {
        ui.label(tr.t("MONKEY_EME_HISTORY_EMPTY"));
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        if m.undo.entries().is_empty() {
            ui.label(tr.t("MONKEY_EME_HISTORY_EMPTY"));
        }
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
        f.push(EmeUiCommand::LogQuery(filter.query()));
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
    };
    let can = f.m.session.perms.has(EmeCapability::RevertOthers);
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

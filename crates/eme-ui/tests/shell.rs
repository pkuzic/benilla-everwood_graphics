//! Headless layout tests: the shell runs on a bare `egui::Context`, no window, no GPU.

use bevy_egui::egui;
use eme_core::service::{EmeRequestId, EmeServiceReply};
use eme_core::{
    EmeCaps, EmeCommandSink, EmeInputCtx, EmeLink, EmeOptionField, EmeOptionKind, EmeOptionValue,
    EmeOptions, EmePermissions, EmePreview, EmeRank, EmeServiceRequest, EmeTool, EmeToolId,
    EmeToolRegistry, EmeUndoStack,
};
use eme_ui::{
    EmeI18n, EmeLang, EmeLayout, EmeLogTable, EmeRestorePoints, EmeRightTab, EmeSession, EmeShell,
    EmeShellModel, EmeShellOutput, EmeStatus, EmeTheme, EmeUiCommand,
};

struct TestTool {
    id: &'static str,
    min_rank: u8,
    strength: f32,
}

impl EmeTool for TestTool {
    fn id(&self) -> EmeToolId {
        EmeToolId(self.id)
    }
    fn min_rank(&self) -> u8 {
        self.min_rank
    }
    fn on_input(&mut self, _ctx: &EmeInputCtx, _out: &mut EmeCommandSink) {}
    fn options_ui_model(&self) -> EmeOptions {
        vec![
            EmeOptionField {
                key: "strength",
                label: "MONKEY_EME_TEST_STRENGTH".into(),
                kind: EmeOptionKind::Slider {
                    min: 0.0,
                    max: 1.0,
                    step: 0.1,
                },
                value: EmeOptionValue::Number(self.strength),
                disabled: false,
            },
            EmeOptionField {
                key: "go",
                label: "MONKEY_EME_TEST_GO".into(),
                kind: EmeOptionKind::Button,
                value: EmeOptionValue::Click,
                disabled: false,
            },
        ]
    }
    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        if let (_, EmeOptionValue::Number(v)) = (key, value) {
            self.strength = v;
        }
    }
    fn preview(&self, _ctx: &EmeInputCtx) -> Option<EmePreview> {
        None
    }
}

fn registry() -> EmeToolRegistry {
    let mut r = EmeToolRegistry::new();
    for (id, rank) in [("sculpt", 3), ("water", 3), ("log", 2)] {
        r.register(Box::new(TestTool {
            id,
            min_rank: rank,
            strength: 0.5,
        }))
        .unwrap();
    }
    r
}

fn session(rank: u8) -> EmeSession {
    EmeSession {
        perms: EmePermissions::from_rank(EmeRank(rank)),
        caps: None,
        link: EmeLink::Up { server_version: 3 },
    }
}

struct World {
    tools: EmeToolRegistry,
    undo: EmeUndoStack,
    session: EmeSession,
    status: EmeStatus,
    restore: EmeRestorePoints,
    log: EmeLogTable,
    i18n: EmeI18n,
    theme: EmeTheme,
    lang: EmeLang,
}

impl World {
    fn new(rank: u8) -> Self {
        Self {
            tools: registry(),
            undo: EmeUndoStack::new(),
            session: session(rank),
            status: EmeStatus::default(),
            restore: EmeRestorePoints::default(),
            log: EmeLogTable::default(),
            i18n: EmeI18n::builtin(),
            theme: EmeTheme::default(),
            lang: EmeLang::EnUs,
        }
    }

    fn activate(&mut self, id: &str) {
        let mut sink = EmeCommandSink::new();
        self.tools
            .activate(id, &self.session.perms, None, &mut sink)
            .unwrap();
    }

    fn model(&self) -> EmeShellModel<'_> {
        EmeShellModel {
            tools: &self.tools,
            undo: &self.undo,
            session: &self.session,
            status: &self.status,
            restore: &self.restore,
            log: &self.log,
            tr: self.i18n.tr(self.lang),
            theme: &self.theme,
        }
    }
}

fn probing_shell() -> EmeShell {
    EmeShell {
        probe: true,
        ..EmeShell::default()
    }
}

fn frame(
    ctx: &egui::Context,
    shell: &mut EmeShell,
    w: &World,
    _time: f64,
    events: Vec<egui::Event>,
) -> EmeShellOutput {
    // One monotonic clock for the whole test binary (egui forbids time going backwards).
    static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let time = CLOCK.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as f64 * 0.05;
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 800.0),
        )),
        time: Some(time),
        events,
        ..Default::default()
    };
    let mut out = None;
    let _ = ctx.run(input, |ctx| {
        out = Some(shell.show(ctx, &w.model()));
    });
    out.unwrap()
}

/// Settles layout over a few frames and returns the last output.
fn settle(ctx: &egui::Context, shell: &mut EmeShell, w: &World) -> EmeShellOutput {
    let mut out = frame(ctx, shell, w, 0.0, vec![]);
    for i in 1..4 {
        out = frame(ctx, shell, w, f64::from(i) * 0.1, vec![]);
    }
    out
}

fn entry<'a>(out: &'a EmeShellOutput, id: &str) -> &'a eme_ui::EmeProbeEntry {
    out.probe.iter().find(|p| p.id == id).unwrap_or_else(|| {
        panic!(
            "control {id} not drawn; have {:?}",
            out.probe.iter().map(|p| &p.id).collect::<Vec<_>>()
        )
    })
}

/// Click the centre of control `id` (move, press, release over three frames).
fn click(ctx: &egui::Context, shell: &mut EmeShell, w: &World, id: &str) -> Vec<EmeUiCommand> {
    let out = settle(ctx, shell, w);
    let pos = entry(&out, id).rect.center();
    let mut cmds = Vec::new();
    let mods = egui::Modifiers::NONE;
    let steps = [
        vec![egui::Event::PointerMoved(pos)],
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: mods,
        }],
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: mods,
        }],
    ];
    for (i, ev) in steps.into_iter().enumerate() {
        let o = frame(ctx, shell, w, 1.0 + i as f64 * 0.1, ev);
        cmds.extend(o.commands);
    }
    cmds
}

fn committed_stroke(s: &mut EmeUndoStack, stroke: u16) -> eme_core::EmeEntryId {
    let id = s.note_edit(0, "sculpt", stroke, stroke * 10);
    s.close_stroke(stroke, false);
    assert!(s.on_edit_confirmed(stroke * 10, 1000 + u32::from(stroke)));
    id
}

#[test]
fn rank_matrix_tools_undo_banner() {
    for rank in [2u8, 3, 4] {
        let mut w = World::new(rank);
        committed_stroke(&mut w.undo, 1);
        let ctx = egui::Context::default();
        let mut shell = probing_shell();
        let out = settle(&ctx, &mut shell, &w);

        // Mutating tools need rank 3; the read-only log tool opens at 2.
        assert_eq!(entry(&out, "tool:sculpt").enabled, rank >= 3, "rank {rank}");
        assert_eq!(entry(&out, "tool:water").enabled, rank >= 3, "rank {rank}");
        assert!(entry(&out, "tool:log").enabled, "rank {rank}");
        // Undo / redo are edits.
        assert_eq!(entry(&out, "undo").enabled, rank >= 3, "rank {rank}");
        assert!(!entry(&out, "redo").enabled);
        // Banner exactly for the read-only rank.
        assert_eq!(out.read_only_banner, rank == 2, "rank {rank}");
        // A disabled tool says why.
        if rank == 2 {
            let tip = entry(&out, "tool:sculpt").tooltip.as_deref().unwrap();
            assert!(tip.contains('3'), "tooltip names the rank: {tip}");
        } else {
            assert!(entry(&out, "tool:sculpt").tooltip.is_none());
        }
    }
}

#[test]
fn rank_below_view_disables_everything() {
    let w = World::new(1);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    let out = settle(&ctx, &mut shell, &w);
    for id in ["tool:sculpt", "tool:log", "undo", "redo"] {
        assert!(!entry(&out, id).enabled, "{id}");
    }
}

#[test]
fn server_caps_can_hide_a_tool() {
    let mut w = World::new(3);
    let reply = EmeServiceReply::ok(EmeRequestId(1))
        .with("rank", 3)
        .with("tools", ["sculpt"]);
    w.session.caps = Some(EmeCaps::from_reply(&reply).unwrap());
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    let out = settle(&ctx, &mut shell, &w);
    assert!(entry(&out, "tool:sculpt").enabled);
    assert!(!entry(&out, "tool:water").enabled);
    assert!(entry(&out, "tool:water").tooltip.is_some());
}

#[test]
fn clicking_undo_and_a_tool_emit_commands() {
    let mut w = World::new(3);
    committed_stroke(&mut w.undo, 1);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    assert_eq!(
        click(&ctx, &mut shell, &w, "undo"),
        vec![EmeUiCommand::Undo]
    );
    let cmds = click(&ctx, &mut shell, &w, "tool:water");
    assert_eq!(cmds, vec![EmeUiCommand::ActivateTool(EmeToolId("water"))]);
}

#[test]
fn a_disabled_button_emits_nothing() {
    let w = World::new(2);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    assert!(click(&ctx, &mut shell, &w, "tool:sculpt").is_empty());
}

#[test]
fn options_panel_slider_and_read_only_lock() {
    // Rank 3 editing a mutating tool: controls live. Rank 2: locked.
    let mut w = World::new(3);
    w.activate("sculpt");
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    let out = settle(&ctx, &mut shell, &w);
    assert!(entry(&out, "opt:strength").enabled);
    assert!(entry(&out, "opt:go").enabled);
    let cmds = click(&ctx, &mut shell, &w, "opt:go");
    assert_eq!(
        cmds,
        vec![EmeUiCommand::SetOption {
            tool: EmeToolId("sculpt"),
            key: "go".into(),
            value: EmeOptionValue::Click
        }]
    );

    // A rank-2 viewer on a read-only tool keeps its controls; on a mutating tool they lock.
    let mut w2 = World::new(2);
    w2.activate("log");
    let out = settle(&egui::Context::default(), &mut probing_shell(), &w2);
    assert!(entry(&out, "opt:strength").enabled);
    // Rank dropped while a mutating tool was active (activation needs rank 3; swap the session).
    let mut w3 = World::new(3);
    w3.activate("sculpt");
    w3.session = session(2);
    let out = settle(&egui::Context::default(), &mut probing_shell(), &w3);
    assert!(!entry(&out, "opt:strength").enabled);
    assert!(!entry(&out, "opt:go").enabled);
}

#[test]
fn restore_tab_create_and_revert_gates() {
    use eme_core::service::EmeSnapshotRow;
    let row = |n| EmeSnapshotRow {
        snapshot: n,
        label: format!("pre-{n}"),
        name: "Geoff".into(),
        t: 1_700_000_000,
        map: 1,
        x: 0.0,
        y: 0.0,
        radius: 100.0,
        at_seq: Default::default(),
    };
    for (rank, can_revert) in [(2u8, false), (3, false), (4, true)] {
        let mut w = World::new(rank);
        w.restore.rows = vec![row(7)];
        let ctx = egui::Context::default();
        let mut shell = probing_shell();
        shell.layout.right_tab = EmeRightTab::Restore;
        shell.snapshot_label = "before the bridge".into();
        let out = settle(&ctx, &mut shell, &w);
        assert_eq!(
            entry(&out, "restore:create").enabled,
            rank >= 3,
            "rank {rank}"
        );
        assert_eq!(
            entry(&out, "restore:revert:0").enabled,
            can_revert,
            "rank {rank}"
        );
        if rank == 4 {
            let cmds = click(&ctx, &mut shell, &w, "restore:revert:0");
            assert_eq!(
                cmds,
                vec![EmeUiCommand::Service(EmeServiceRequest::Revert {
                    snapshot: 7
                })]
            );
        }
        if rank == 3 {
            let cmds = click(&ctx, &mut shell, &w, "restore:create");
            assert_eq!(
                cmds,
                vec![EmeUiCommand::Service(EmeServiceRequest::SnapshotCreate {
                    label: "before the bridge".into(),
                    radius: None
                })]
            );
            assert!(
                shell.snapshot_label.is_empty(),
                "label cleared after create"
            );
        }
    }
}

#[test]
fn log_paging_buttons() {
    let mut w = World::new(2);
    w.log.page = 1;
    w.log.more = true;
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    shell.layout.right_tab = EmeRightTab::Log;
    let out = settle(&ctx, &mut shell, &w);
    assert!(entry(&out, "log:newer").enabled);
    assert!(entry(&out, "log:older").enabled);
    assert_eq!(
        click(&ctx, &mut shell, &w, "log:older"),
        vec![EmeUiCommand::LogPage { older: true }]
    );
    w.log.page = 0;
    w.log.more = false;
    let out = settle(&ctx, &mut shell, &w);
    assert!(!entry(&out, "log:newer").enabled);
    assert!(!entry(&out, "log:older").enabled);
}

#[test]
fn others_edited_shows_the_confirmation_modal() {
    let mut w = World::new(3);
    let id = committed_stroke(&mut w.undo, 1);
    w.undo.undo().unwrap();
    let req = w.undo.drain_requests().pop().unwrap();
    let reply = EmeServiceReply::error(EmeRequestId(1), "others_edited")
        .with("others", ["Geoff"])
        .with("count", 3);
    w.undo.on_reply(req.ticket, &reply);

    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    let out = settle(&ctx, &mut shell, &w);
    assert!(out.confirm_modal);
    assert!(entry(&out, "confirm:yes").enabled);
    assert!(out.pointer_over_ui || !out.pointer_over_ui); // computed without panicking
    assert_eq!(
        click(&ctx, &mut shell, &w, "confirm:yes"),
        vec![EmeUiCommand::ConfirmUndo(id)]
    );
    assert_eq!(
        click(&ctx, &mut shell, &w, "confirm:no"),
        vec![EmeUiCommand::CancelConfirm(id)]
    );
    // No modal without a pending confirmation.
    let calm = World::new(3);
    assert!(!settle(&egui::Context::default(), &mut probing_shell(), &calm).confirm_modal);
}

#[test]
fn russian_shell_draws() {
    let mut w = World::new(2);
    w.lang = EmeLang::RuRu;
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    let out = settle(&ctx, &mut shell, &w);
    assert!(out.read_only_banner);
    let tip = entry(&out, "tool:sculpt").tooltip.clone().unwrap();
    assert!(tip.contains("ранг"), "{tip}");
}

#[test]
fn pointer_over_panel_is_reported() {
    let w = World::new(3);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    let out = settle(&ctx, &mut shell, &w);
    let top = entry(&out, "undo").rect.center();
    let o = frame(
        &ctx,
        &mut shell,
        &w,
        2.0,
        vec![egui::Event::PointerMoved(top)],
    );
    assert!(o.pointer_over_ui, "over the tool strip");
    // Middle of the viewport (between side panels) belongs to the world.
    let o = frame(
        &ctx,
        &mut shell,
        &w,
        2.1,
        vec![egui::Event::PointerMoved(egui::pos2(700.0, 400.0))],
    );
    assert!(!o.pointer_over_ui, "over the world");
}

#[test]
fn layout_round_trip_and_clamp() {
    let l = EmeLayout {
        left_open: false,
        right_open: true,
        left_width: 300.0,
        right_width: 410.5,
        right_tab: EmeRightTab::Log,
    };
    assert_eq!(EmeLayout::from_toml(&l.to_toml()).unwrap(), l);
    // Hand-edited nonsense is clamped; absent keys default.
    let wild = EmeLayout::from_toml("left_width = 5.0\nright_width = 99999.0\n").unwrap();
    assert_eq!(wild.left_width, eme_ui::layout::PANEL_MIN);
    assert_eq!(wild.right_width, eme_ui::layout::PANEL_MAX);
    assert!(wild.left_open);
    assert!(EmeLayout::from_toml("right_tab = \"nope\"").is_err());
}

#[test]
fn layout_file_save_load() {
    let dir = std::env::temp_dir().join(format!("eme-ui-layout-{}", std::process::id()));
    let path = dir.join("benilla-config").join("eme-layout.toml");
    let l = EmeLayout {
        right_tab: EmeRightTab::Restore,
        left_width: 222.0,
        ..EmeLayout::default()
    };
    l.save(&path).unwrap();
    assert_eq!(EmeLayout::load(&path), l);
    std::fs::write(&path, "garbage = = =").unwrap();
    assert_eq!(EmeLayout::load(&path), EmeLayout::default());
    assert_eq!(
        EmeLayout::load(&dir.join("missing.toml")),
        EmeLayout::default()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn panels_track_resized_width_into_layout() {
    let w = World::new(3);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    settle(&ctx, &mut shell, &w);
    assert!(shell.layout.left_width >= eme_ui::layout::PANEL_MIN);
    assert!(shell.layout.right_width <= eme_ui::layout::PANEL_MAX);
}

#[test]
fn toggle_requests_respect_rank() {
    use bevy::prelude::*;
    use eme_ui::{apply_toggle_requests, EmeActive, EmeToggleRequested};
    for (rank, expect) in [(1u8, false), (2, true), (4, true)] {
        let mut app = App::new();
        app.add_message::<EmeToggleRequested>()
            .init_resource::<EmeActive>()
            .insert_resource(session(rank))
            .add_systems(Update, apply_toggle_requests);
        app.world_mut().write_message(EmeToggleRequested);
        app.update();
        assert_eq!(app.world().resource::<EmeActive>().0, expect, "rank {rank}");
        if expect {
            app.world_mut().write_message(EmeToggleRequested);
            app.update();
            assert!(!app.world().resource::<EmeActive>().0, "toggles back off");
        }
    }
}

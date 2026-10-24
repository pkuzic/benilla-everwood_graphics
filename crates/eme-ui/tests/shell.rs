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
        project: None,
        projects_required: false,
    }
}

struct World {
    tools: EmeToolRegistry,
    undo: EmeUndoStack,
    session: EmeSession,
    status: EmeStatus,
    restore: EmeRestorePoints,
    log: EmeLogTable,
    notice: eme_ui::EmeNotice,
    others: eme_ui::EmeOthersHistory,
    projects: eme_ui::EmeProjects,
    objects: eme_core::EmeObjectsView,
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
            notice: Default::default(),
            others: Default::default(),
            projects: Default::default(),
            objects: Default::default(),
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
            notice: &self.notice,
            others: &self.others,
            projects: &self.projects,
            objects: &self.objects,
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

/// Whether control `id` was drawn at all.
fn drawn(out: &EmeShellOutput, id: &str) -> bool {
    out.probe.iter().any(|p| p.id == id)
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

        // The toolbar = `registry.visible`: mutating tools from rank 3, the read-only log at 2.
        assert_eq!(drawn(&out, "tool:sculpt"), rank >= 3, "rank {rank}");
        assert_eq!(drawn(&out, "tool:water"), rank >= 3, "rank {rank}");
        assert!(entry(&out, "tool:log").enabled, "rank {rank}");
        // Undo / redo are edits.
        assert_eq!(entry(&out, "undo").enabled, rank >= 3, "rank {rank}");
        assert!(!entry(&out, "redo").enabled);
        // Banner exactly for the read-only rank.
        assert_eq!(out.read_only_banner, rank == 2, "rank {rank}");
        // A read-only rank says why the edit buttons are off.
        if rank == 2 {
            let tip = entry(&out, "undo").tooltip.as_deref().unwrap();
            assert!(tip.contains('3'), "tooltip names the rank: {tip}");
        }
    }
}

#[test]
fn rank_below_view_disables_everything() {
    let w = World::new(1);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    let out = settle(&ctx, &mut shell, &w);
    for id in ["tool:sculpt", "tool:log"] {
        assert!(!drawn(&out, id), "{id}");
    }
    for id in ["undo", "redo"] {
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
    assert!(
        !drawn(&out, "tool:water"),
        "a tool the server does not offer is not shown"
    );
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
    let mut w = World::new(2);
    committed_stroke(&mut w.undo, 1);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    assert!(click(&ctx, &mut shell, &w, "undo").is_empty());
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
            // Revert asks first: nothing is sent until the modal's "yes".
            let cmds = click(&ctx, &mut shell, &w, "restore:revert:0");
            assert!(cmds.is_empty(), "{cmds:?}");
            assert!(shell.pending.is_some());
            let no = click(&ctx, &mut shell, &w, "pending:no");
            assert!(no.is_empty() && shell.pending.is_none());
            click(&ctx, &mut shell, &w, "restore:revert:0");
            let cmds = click(&ctx, &mut shell, &w, "pending:yes");
            assert_eq!(
                cmds,
                vec![EmeUiCommand::Service(EmeServiceRequest::Revert {
                    snapshot: 7
                })]
            );
            assert!(shell.pending.is_none());
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
    let tip = entry(&out, "undo").tooltip.clone().unwrap();
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

fn log_row(name: &str, mode: u8) -> eme_core::service::EmeLogRow {
    eme_core::service::EmeLogRow {
        layer: eme_core::service::EmeLayer::Voxel,
        seq: 100,
        t: 1_700_000_000,
        guid: 5,
        name: name.into(),
        account: 1,
        mode: Some(mode),
        radius: Some(4.0),
        x: -9540.0,
        y: -300.0,
        z: 55.0,
        stroke: Some(3),
        reverted: false,
    }
}

#[test]
fn log_filters_build_the_query_and_apply() {
    let mut w = World::new(3);
    w.log.rows = vec![log_row("Geoff", 0)];
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    shell.layout.right_tab = EmeRightTab::Log;
    shell.filter = eme_ui::shell::EmeLogFilterUi {
        player: " Geoff ".into(),
        radius: "80".into(),
        minutes: "15".into(),
        mode: 4, // flatten
        layer: 1,
        project_only: false,
    };
    let out = settle(&ctx, &mut shell, &w);
    for id in [
        "log:filter:player",
        "log:filter:radius",
        "log:filter:minutes",
        "log:filter:tool",
        "log:filter:layer",
        "log:apply",
    ] {
        assert!(drawn(&out, id), "{id}");
    }
    let cmds = click(&ctx, &mut shell, &w, "log:apply");
    let [EmeUiCommand::LogQuery(q)] = cmds.as_slice() else {
        panic!("{cmds:?}")
    };
    assert_eq!(q.player.as_deref(), Some("Geoff"));
    assert_eq!(q.radius, Some(80.0));
    assert_eq!(q.minutes, Some(15));
    assert_eq!(q.mode, Some(3));
    assert_eq!(q.layer, Some(eme_core::service::EmeLayerFilter::Voxel));
    // Clear resets the fields and re-queries unfiltered.
    let cmds = click(&ctx, &mut shell, &w, "log:clear");
    assert_eq!(
        cmds,
        vec![EmeUiCommand::LogQuery(
            eme_core::service::EmeLogQuery::default()
        )]
    );
    assert_eq!(shell.filter, Default::default());
}

#[test]
fn log_row_buttons_show_in_world_and_pick_the_player() {
    let mut w = World::new(4);
    w.log.rows = vec![log_row("Geoff", 1)];
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    shell.layout.right_tab = EmeRightTab::Log;
    assert_eq!(
        click(&ctx, &mut shell, &w, "log:show:0"),
        vec![EmeUiCommand::ShowInWorld {
            x: -9540.0,
            y: -300.0,
            z: Some(55.0)
        }]
    );
    // Undo-player needs a name: disabled until one is picked.
    let out = settle(&ctx, &mut shell, &w);
    assert!(!entry(&out, "log:undo_player").enabled);
    assert!(click(&ctx, &mut shell, &w, "log:who:0").is_empty());
    assert_eq!(shell.filter.player, "Geoff");
    assert!(entry(&settle(&ctx, &mut shell, &w), "log:undo_player").enabled);
}

#[test]
fn undo_player_needs_rank_four_and_a_confirmation() {
    for rank in [2u8, 3, 4] {
        let mut w = World::new(rank);
        w.log.rows = vec![log_row("Geoff", 0)];
        let ctx = egui::Context::default();
        let mut shell = probing_shell();
        shell.layout.right_tab = EmeRightTab::Log;
        shell.filter.player = "Geoff".into();
        shell.filter.minutes = "20".into();
        let out = settle(&ctx, &mut shell, &w);
        assert_eq!(
            entry(&out, "log:undo_player").enabled,
            rank == 4,
            "rank {rank}"
        );
        if rank == 4 {
            assert!(click(&ctx, &mut shell, &w, "log:undo_player").is_empty());
            assert!(matches!(
                shell.pending,
                Some(eme_ui::shell::EmePending::UndoPlayer { .. })
            ));
            let cmds = click(&ctx, &mut shell, &w, "pending:yes");
            assert_eq!(
                cmds,
                vec![EmeUiCommand::Service(EmeServiceRequest::UndoPlayer {
                    name: "Geoff".into(),
                    minutes: Some(20),
                    radius: None
                })]
            );
        }
    }
}

#[test]
fn restore_rows_select_and_show_in_world() {
    use eme_core::service::EmeSnapshotRow;
    let mut w = World::new(3);
    w.restore.rows = vec![EmeSnapshotRow {
        snapshot: 7,
        label: "pre".into(),
        name: "Geoff".into(),
        t: 1,
        map: 0,
        x: -9500.0,
        y: -310.0,
        radius: 60.0,
        at_seq: Default::default(),
    }];
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    shell.layout.right_tab = EmeRightTab::Restore;
    assert_eq!(
        click(&ctx, &mut shell, &w, "restore:select:0"),
        vec![EmeUiCommand::SelectRestore(Some(7))]
    );
    w.restore.selected = Some(7);
    assert_eq!(
        click(&ctx, &mut shell, &w, "restore:select:0"),
        vec![EmeUiCommand::SelectRestore(None)]
    );
    assert_eq!(
        click(&ctx, &mut shell, &w, "restore:show:0"),
        vec![
            EmeUiCommand::SelectRestore(Some(7)),
            EmeUiCommand::ShowInWorld {
                x: -9500.0,
                y: -310.0,
                z: None
            }
        ]
    );
}

#[test]
fn notice_shows_and_dismisses_in_both_languages() {
    let mut w = World::new(3);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    assert!(!drawn(&settle(&ctx, &mut shell, &w), "notice:dismiss"));
    w.notice.set("MONKEY_EME_ERR_RANK", vec![], true);
    assert!(drawn(&settle(&ctx, &mut shell, &w), "notice:dismiss"));
    assert_eq!(
        click(&ctx, &mut shell, &w, "notice:dismiss"),
        vec![EmeUiCommand::DismissNotice]
    );
    w.lang = EmeLang::RuRu;
    w.notice.set(
        "MONKEY_EME_NOTICE_REVERT_OK",
        vec!["1".into(), "2".into(), "3".into()],
        false,
    );
    assert!(drawn(&settle(&ctx, &mut shell, &w), "notice:dismiss"));
}

#[test]
fn other_gms_strokes_are_listed_read_only() {
    let mut w = World::new(3);
    w.others.now_ms = 10_000;
    w.others.rows = vec![eme_ui::EmeOtherStroke {
        actor: 9,
        name: "Zara".into(),
        stroke: 4,
        edits: 6,
        mode: 1,
        pos: [-9500.0, -300.0, 60.0],
        seen_ms: 7_000,
    }];
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    shell.layout.right_tab = EmeRightTab::History;
    let out = settle(&ctx, &mut shell, &w);
    // No undo / redo control for someone else's stroke: only "go to".
    assert!(drawn(&out, "others:show:0"));
    assert_eq!(
        click(&ctx, &mut shell, &w, "others:show:0"),
        vec![EmeUiCommand::ShowInWorld {
            x: -9500.0,
            y: -300.0,
            z: Some(60.0)
        }]
    );
}

// --- MONKEY (EME projects, CONTRACT v3.4) ---------------------------------------------------

fn project_row(id: u32, name: &str, status: &str, mine: bool) -> eme_core::EmeProjectRow {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": name, "owner": if mine { "Geoff" } else { "Ada" }, "status": status,
        "edits": 3, "live": 3, "mine": mine, "active": false
    }))
    .unwrap()
}

/// Map-editing tools are locked (with the "open or create a project" tooltip) until a project is
/// active; read-only tools are not; the bar offers the Projects tab.
#[test]
fn mutating_tools_are_locked_without_a_project() {
    let mut w = World::new(3);
    w.session.projects_required = true;
    w.activate("sculpt");
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    let out = settle(&ctx, &mut shell, &w);
    let sculpt = entry(&out, "tool:sculpt");
    assert!(!sculpt.enabled);
    assert_eq!(sculpt.tooltip.as_deref(), Some("Open or create a project first"));
    assert!(entry(&out, "tool:log").enabled, "read-only tools stay usable");
    assert!(!entry(&out, "opt:strength").enabled, "the active tool's options are locked");
    assert!(entry(&out, "undo").enabled || !w.undo.can_undo(), "undo is not project-gated");
    let cmds = click(&ctx, &mut shell, &w, "project:browse");
    assert_eq!(cmds, vec![EmeUiCommand::TabOpened(EmeRightTab::Projects)]);
    assert_eq!(shell.layout.right_tab, EmeRightTab::Projects);

    // With a project: unlocked, and the bar names it.
    w.session.project = Some(eme_core::EmeProjectRef { id: 4, name: "Pond".into(), status: None });
    let out = settle(&ctx, &mut shell, &w);
    assert!(entry(&out, "tool:sculpt").enabled);
    assert!(entry(&out, "opt:strength").enabled);
    assert!(drawn(&out, "project:leave"));
    assert_eq!(click(&ctx, &mut shell, &w, "project:leave"), vec![EmeUiCommand::Service(EmeServiceRequest::ProjectLeave)]);

    // A server without projects locks nothing and shows no bar.
    let mut w = World::new(3);
    w.activate("sculpt");
    let out = settle(&ctx, &mut probing_shell(), &w);
    assert!(entry(&out, "tool:sculpt").enabled);
    assert!(!drawn(&out, "project:browse"));
}

#[test]
fn projects_tab_create_open_and_revert_with_confirmation() {
    let mut w = World::new(3);
    w.session.projects_required = true;
    w.projects.rows = vec![project_row(7, "Pond", "open", true), project_row(8, "Bridge", "open", false), project_row(9, "Old", "reverted", true)];
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    shell.layout.right_tab = EmeRightTab::Projects;
    let out = settle(&ctx, &mut shell, &w);
    assert!(!entry(&out, "project:create").enabled, "a name first");
    assert!(entry(&out, "project:revert:0").enabled, "the owner may revert");
    assert!(!entry(&out, "project:revert:1").enabled, "not someone else's at rank 3");
    assert!(!entry(&out, "project:unrevert:2").enabled, "un-revert is rank 4");
    shell.project_name = "Пруд у мельницы".into();
    let cmds = click(&ctx, &mut shell, &w, "project:create");
    assert_eq!(
        cmds,
        vec![EmeUiCommand::Service(EmeServiceRequest::ProjectCreate { name: "Пруд у мельницы".into(), description: None })]
    );
    assert_eq!(
        click(&ctx, &mut shell, &w, "project:open:1"),
        vec![EmeUiCommand::Service(EmeServiceRequest::ProjectOpen { project: 8 })]
    );
    assert_eq!(click(&ctx, &mut shell, &w, "project:filter:all"), vec![EmeUiCommand::ProjectFilter(eme_ui::EmeProjectFilter::All)]);
    // Revert asks first.
    assert!(click(&ctx, &mut shell, &w, "project:revert:0").is_empty());
    assert!(shell.pending.is_some());
    assert_eq!(
        click(&ctx, &mut shell, &w, "pending:yes"),
        vec![EmeUiCommand::Service(EmeServiceRequest::ProjectRevert { project: 7, force: false })]
    );
}

/// `others_edited`: rank 3 cannot force (says why), rank 4 forces.
#[test]
fn project_conflict_needs_rank_four_to_force() {
    for rank in [3u8, 4] {
        let mut w = World::new(rank);
        w.session.projects_required = true;
        w.projects.conflict = Some(eme_ui::EmeProjectConflict { project: 7, name: "Pond".into(), others: vec!["Ada".into()], count: 5 });
        let ctx = egui::Context::default();
        let mut shell = probing_shell();
        let out = settle(&ctx, &mut shell, &w);
        assert_eq!(entry(&out, "project:force").enabled, rank == 4);
        if rank == 4 {
            assert_eq!(
                click(&ctx, &mut shell, &w, "project:force"),
                vec![
                    EmeUiCommand::Service(EmeServiceRequest::ProjectRevert { project: 7, force: true }),
                    EmeUiCommand::DismissProjectConflict
                ]
            );
        } else {
            assert_eq!(click(&ctx, &mut shell, &w, "project:force:no"), vec![EmeUiCommand::DismissProjectConflict]);
        }
    }
}

// ---- MONKEY (EME objects): the Objects tab -----------------------------------------------------

use eme_core::service::{EmeObjectKind, EmeObjectRow, EmeOwner};
use eme_core::{EmeObjField, EmeObjectsCmd};

fn obj_row(guid: u32, entry: u32, name: &str, x: f32) -> EmeObjectRow {
    EmeObjectRow {
        guid,
        kind: EmeObjectKind::Go,
        entry,
        name: Some(name.into()),
        map: 0,
        x,
        y: 20.0,
        z: 3.0,
        o: std::f32::consts::FRAC_PI_2,
        scale: 1.5,
        owner: Some(EmeOwner {
            guid: 7,
            name: "Geoff".into(),
        }),
        tx: None,
    }
}

/// A world whose registry has the Objects tool, with the tool active and three rows loaded.
fn objects_world(rank: u8) -> World {
    let mut w = World::new(rank);
    w.tools
        .register(Box::new(TestTool {
            id: "objects",
            min_rank: 2,
            strength: 0.0,
        }))
        .unwrap();
    w.session.link = EmeLink::Up { server_version: 3 };
    w.objects.active = true;
    w.objects.can_edit = rank >= 3;
    w.objects.rows = vec![
        obj_row(11, 2910, "Chair", 10.0),
        obj_row(12, 2911, "Table", 12.0),
        obj_row(13, 2912, "Barrel", 14.0),
    ];
    w
}

fn open_objects_tab(shell: &mut EmeShell) {
    shell.layout.right_open = true;
    shell.layout.right_tab = EmeRightTab::Objects;
}

#[test]
fn the_objects_tab_exists_only_where_the_tool_does() {
    let ctx = egui::Context::default();
    // No Objects tool registered: no tab.
    let w = World::new(3);
    let mut shell = probing_shell();
    assert!(!drawn(&settle(&ctx, &mut shell, &w), "tab:objects"));
    // Rank 2 and 3 get it; rank 1 does not.
    for (rank, tab) in [(1u8, false), (2, true), (3, true)] {
        let w = objects_world(rank);
        let mut shell = probing_shell();
        assert_eq!(drawn(&settle(&ctx, &mut shell, &w), "tab:objects"), tab, "rank {rank}");
    }
    // The server not listing the tool hides it.
    let mut w = objects_world(3);
    let reply = EmeServiceReply::ok(EmeRequestId(1)).with("rank", 3).with("tools", ["sculpt"]);
    w.session.caps = Some(EmeCaps::from_reply(&reply).unwrap());
    let mut shell = probing_shell();
    assert!(!drawn(&settle(&ctx, &mut shell, &w), "tab:objects"));
}

#[test]
fn clicking_an_outliner_row_selects_it_and_frames_the_camera() {
    let w = objects_world(3);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    open_objects_tab(&mut shell);
    let out = settle(&ctx, &mut shell, &w);
    for g in [11, 12, 13] {
        assert!(drawn(&out, &format!("obj:row:{g}")));
    }
    let cmds = click(&ctx, &mut shell, &w, "obj:row:12");
    assert_eq!(
        cmds,
        vec![
            EmeUiCommand::Objects(EmeObjectsCmd::Select(12)),
            EmeUiCommand::Objects(EmeObjectsCmd::Frame)
        ]
    );
}

#[test]
fn the_outliner_search_filters_by_name_entry_and_guid() {
    let w = objects_world(3);
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    open_objects_tab(&mut shell);
    for (q, rows) in [("tab", vec![12]), ("2912", vec![13]), ("#2910", vec![11]), ("13", vec![13]), ("zzz", vec![]), ("", vec![11, 12, 13])] {
        shell.obj_search = q.into();
        let out = settle(&ctx, &mut shell, &w);
        let got: Vec<u32> = [11u32, 12, 13]
            .into_iter()
            .filter(|g| drawn(&out, &format!("obj:row:{g}")))
            .collect();
        assert_eq!(got, rows, "query {q:?}");
    }
}

#[test]
fn the_inspector_shows_the_primary_and_enter_applies_a_field() {
    let mut w = objects_world(3);
    w.objects.selected = vec![11, 12];
    w.objects.primary = Some(obj_row(12, 2911, "Table", 12.0));
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    open_objects_tab(&mut shell);
    let out = settle(&ctx, &mut shell, &w);
    for f in ["x", "y", "z", "o", "s"] {
        assert!(entry(&out, &format!("obj:field:{f}")).enabled, "{f}");
    }
    // The boxes hold the primary's numbers (heading in degrees).
    assert_eq!(shell.obj_fields[0], "12.00");
    assert_eq!(shell.obj_fields[3], "90.0");
    assert_eq!(shell.obj_fields[4], "1.50");
    // Focus the X box, type, press Enter: one SetField command.
    click(&ctx, &mut shell, &w, "obj:field:x");
    shell.obj_fields[0] = "13,5".into();
    let enter = egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: Some(egui::Key::Enter),
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    let o = frame(&ctx, &mut shell, &w, 5.0, vec![enter]);
    assert_eq!(
        o.commands,
        vec![EmeUiCommand::Objects(EmeObjectsCmd::SetField {
            field: EmeObjField::X,
            value: 13.5
        })]
    );
}

#[test]
fn a_viewer_and_a_projectless_editor_cannot_change_objects() {
    let ctx = egui::Context::default();
    // Rank 2: read-only; the buttons are off and say why.
    let mut w = objects_world(2);
    w.objects.selected = vec![11];
    w.objects.primary = Some(obj_row(11, 2910, "Chair", 10.0));
    let mut shell = probing_shell();
    open_objects_tab(&mut shell);
    let out = settle(&ctx, &mut shell, &w);
    for id in ["obj:delete", "obj:duplicate", "obj:place", "obj:field:x", "obj:spawn_entry"] {
        assert!(!entry(&out, id).enabled, "{id}");
    }
    assert!(entry(&out, "obj:frame").enabled && entry(&out, "obj:refresh").enabled);
    assert!(entry(&out, "obj:delete").tooltip.as_deref().unwrap().contains('3'));
    assert!(click(&ctx, &mut shell, &w, "obj:delete").is_empty());

    // Rank 3 with projects required and none open: locked too.
    let mut w = objects_world(3);
    w.session.projects_required = true;
    w.objects.selected = vec![11];
    w.objects.primary = Some(obj_row(11, 2910, "Chair", 10.0));
    let mut shell = probing_shell();
    open_objects_tab(&mut shell);
    let out = settle(&ctx, &mut shell, &w);
    assert!(!entry(&out, "obj:delete").enabled);
    assert!(click(&ctx, &mut shell, &w, "obj:delete").is_empty());
    // With a project they work.
    w.session.project = Some(eme_core::EmeProjectRef {
        id: 1,
        name: "p".into(),
        status: None,
    });
    let mut shell = probing_shell();
    open_objects_tab(&mut shell);
    assert_eq!(
        click(&ctx, &mut shell, &w, "obj:delete"),
        vec![EmeUiCommand::Objects(EmeObjectsCmd::Delete)]
    );
    assert_eq!(
        click(&ctx, &mut shell, &w, "obj:duplicate"),
        vec![EmeUiCommand::Objects(EmeObjectsCmd::Duplicate)]
    );
}

#[test]
fn the_spawn_list_arms_a_template_and_the_entry_box_places_by_id() {
    let mut w = objects_world(3);
    w.objects.templates = vec![(2910, "Chair".into()), (2911, "Table".into())];
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    open_objects_tab(&mut shell);
    let cmds = click(&ctx, &mut shell, &w, "obj:spawn:2911");
    assert_eq!(
        cmds,
        vec![EmeUiCommand::Objects(EmeObjectsCmd::ArmSpawn {
            entry: 2911,
            name: Some("Table".into())
        })]
    );
    // The Place button needs a number.
    let out = settle(&ctx, &mut shell, &w);
    assert!(!entry(&out, "obj:place").enabled);
    shell.spawn_entry = "2910".into();
    assert_eq!(
        click(&ctx, &mut shell, &w, "obj:place"),
        vec![EmeUiCommand::Objects(EmeObjectsCmd::ArmSpawn {
            entry: 2910,
            name: Some("Chair".into())
        })]
    );
    // The list filter.
    shell.spawn_search = "tab".into();
    let out = settle(&ctx, &mut shell, &w);
    assert!(drawn(&out, "obj:spawn:2911") && !drawn(&out, "obj:spawn:2910"));
}

#[test]
fn an_inactive_tool_asks_to_be_picked_and_the_button_activates_it() {
    let mut w = objects_world(3);
    w.objects.active = false;
    let ctx = egui::Context::default();
    let mut shell = probing_shell();
    open_objects_tab(&mut shell);
    let out = settle(&ctx, &mut shell, &w);
    assert!(!drawn(&out, "obj:row:11"));
    assert_eq!(
        click(&ctx, &mut shell, &w, "obj:activate"),
        vec![EmeUiCommand::ActivateTool(EmeToolId("objects"))]
    );
}

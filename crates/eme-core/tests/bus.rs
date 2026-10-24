//! Tools, reg        .map(|i| EmeAreaCell { ax: i, ay: i, area_id: 1519 })stry, permission gating and the dispatcher end to end over the loopback transport.

use eme_core::api::{EmeEvent, EmeSendError, EmeServiceClient};
use eme_core::bus::{EmeDispatcher, EmeRoute};
use eme_core::command::{EmeCommand, EmeCommandSink, EmeObjectRequest, EmeStrokeMark};
use eme_core::perm::{EmeCapability, EmePermissions, EmeRank};
use eme_core::service::*;
use eme_core::stroke::{EmeBrushSample, EmeStrokeBuilder};
use eme_core::tool::*;
use eme_core::transport::{
    mode, EmeInbound, EmeLink, EmeLoopbackTransport, EmeRejectReason, EmeTransport,
};
use eme_core::undo::{EmeEntryState, EmeUndoStack, EmeUndoStarted};

// --- a minimal sculpt tool, written the way a real tool file would be -----------------------

struct Sculpt {
    builder: EmeStrokeBuilder,
    radius: f32,
}

impl Sculpt {
    fn new() -> Self {
        Self {
            builder: EmeStrokeBuilder::default(),
            radius: 4.0,
        }
    }
    fn sample(&self, ctx: &EmeInputCtx, hit: &EmeHit) -> EmeBrushSample {
        EmeBrushSample {
            map: ctx.map,
            pos: hit.pos,
            radius: self.radius,
            mode: mode::DIG,
            strength: 255,
            param: 0,
        }
    }
}

impl EmeTool for Sculpt {
    fn id(&self) -> EmeToolId {
        EmeToolId("sculpt")
    }
    fn min_rank(&self) -> u8 {
        3
    }
    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        let now = ctx.now_ms;
        if let (Some(hit), false) = (ctx.hit, ctx.pointer_over_ui) {
            if ctx.primary.pressed {
                let (stroke, e) = self.builder.begin(now, self.sample(ctx, &hit));
                out.push(EmeCommand::VoxelStroke {
                    stroke,
                    ops: e.into_iter().collect(),
                    mark: EmeStrokeMark::Continue,
                });
                return;
            }
            if ctx.primary.held {
                let s = self.sample(ctx, &hit);
                if let Some(e) = self.builder.update(now, s) {
                    out.push(EmeCommand::VoxelStroke {
                        stroke: e.stroke,
                        ops: vec![e],
                        mark: EmeStrokeMark::Continue,
                    });
                }
                return;
            }
        }
        if ctx.primary.released {
            let sample = ctx.hit.map(|h| self.sample(ctx, &h));
            if let Some(end) = self.builder.end(now, sample) {
                out.push(EmeCommand::VoxelStroke {
                    stroke: end.stroke,
                    ops: end.edit.into_iter().collect(),
                    mark: EmeStrokeMark::End {
                        trailing: end.trailing,
                    },
                });
            }
            return;
        }
        if let Some(e) = self.builder.tick(now) {
            out.push(EmeCommand::VoxelStroke {
                stroke: e.stroke,
                ops: vec![e],
                mark: EmeStrokeMark::Continue,
            });
        }
    }
    fn options_ui_model(&self) -> EmeOptions {
        vec![EmeOptionField {
            key: "radius",
            label: format!("{}RADIUS", self.id().locale_prefix()),
            kind: EmeOptionKind::Slider {
                min: 0.5,
                max: 16.0,
                step: 0.5,
            },
            value: EmeOptionValue::Number(self.radius),
            disabled: false,
        }]
    }
    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        if let ("radius", EmeOptionValue::Number(r)) = (key, value) {
            self.radius = r.clamp(0.5, 16.0);
        }
    }
    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview> {
        ctx.hit.map(|h| EmePreview::Ring {
            center: h.pos,
            radius: self.radius,
            falloff: 0.25,
        })
    }
}

struct LogBrowser;

impl EmeTool for LogBrowser {
    fn id(&self) -> EmeToolId {
        EmeToolId("log")
    }
    fn min_rank(&self) -> u8 {
        2
    }
    fn on_input(&mut self, _ctx: &EmeInputCtx, _out: &mut EmeCommandSink) {}
    fn options_ui_model(&self) -> EmeOptions {
        Vec::new()
    }
    fn preview(&self, _ctx: &EmeInputCtx) -> Option<EmePreview> {
        None
    }
}

fn registry() -> EmeToolRegistry {
    let mut r = EmeToolRegistry::new();
    r.register(Box::new(Sculpt::new())).unwrap();
    r.register(Box::new(LogBrowser)).unwrap();
    r
}

fn caps(rank: u8, tools: &[&str]) -> EmeCaps {
    EmeCaps {
        rank: EmeRank(rank),
        tools: tools.iter().map(|s| s.to_string()).collect(),
        wire: 3,
        perms: Vec::new(),
        enforced: false,
        enforced_layers: Vec::new(),
        limits: EmeLimits::default(),
        project: None,
        projects_ready: None,
    }
}

fn perms(rank: u8) -> EmePermissions {
    EmePermissions::from_rank(EmeRank(rank))
}

// --- permissions ----------------------------------------------------------------------------

#[test]
fn rank_table() {
    for rank in 0..=1 {
        let p = perms(rank);
        assert!(!p.can_open() && !p.has(EmeCapability::View) && !p.has(EmeCapability::Edit));
    }
    let p = perms(2);
    assert!(p.can_open() && !p.has(EmeCapability::Edit));
    let p = perms(3);
    assert!(p.has(EmeCapability::Edit) && !p.has(EmeCapability::RevertOthers));
    let p = perms(4);
    for c in [
        EmeCapability::View,
        EmeCapability::Edit,
        EmeCapability::RevertOthers,
        EmeCapability::DeleteOthersObjects,
        EmeCapability::AreaFlags,
    ] {
        assert!(p.has(c));
    }
    assert!(!EmePermissions::none().can_open());
    assert!(
        !perms(1).allows_rank(0),
        "below rank 2 nothing is usable, even a rank-0 tool"
    );
}

#[test]
fn toolbar_is_filtered_by_rank_and_server_tool_list() {
    let r = registry();
    assert!(r.visible(&perms(1), None).is_empty());
    assert_eq!(r.visible(&perms(2), None), vec![EmeToolId("log")]);
    assert_eq!(
        r.visible(&perms(3), None),
        vec![EmeToolId("sculpt"), EmeToolId("log")]
    );
    let c = caps(3, &["log"]);
    assert_eq!(r.visible(&perms(3), Some(&c)), vec![EmeToolId("log")]);
}

#[test]
fn registry_rejects_duplicates_and_gates_activation() {
    let mut r = registry();
    assert!(r.register(Box::new(LogBrowser)).is_err());
    let mut sink = EmeCommandSink::new();
    assert_eq!(
        r.activate("sculpt", &perms(2), None, &mut sink),
        Err(eme_core::tool::EmeRegistryError::NotPermitted)
    );
    assert_eq!(
        r.activate("nope", &perms(4), None, &mut sink),
        Err(eme_core::tool::EmeRegistryError::Unknown)
    );
    r.activate("sculpt", &perms(3), None, &mut sink).unwrap();
    assert_eq!(r.active_id(), Some(EmeToolId("sculpt")));
    // A rank drop while active: feeding deactivates.
    let ctx = EmeInputCtx::idle(0, 0, perms(2));
    assert!(!r.feed(&ctx, None, &mut sink));
    assert_eq!(r.active_id(), None);
}

#[test]
fn options_round_trip_through_the_tool() {
    let mut r = registry();
    let t = r.get_mut("sculpt").unwrap();
    t.set_option("radius", EmeOptionValue::Number(40.0));
    let model = t.options_ui_model();
    assert_eq!(model[0].value, EmeOptionValue::Number(16.0));
    assert_eq!(model[0].label, "MONKEY_EME_SCULPT_RADIUS");
}

#[test]
fn service_client_gates_sizes_and_times_out() {
    let mut t = EmeLoopbackTransport::new(EmeLink::Up { server_version: 3 });
    let mut c: EmeServiceClient<u8> = EmeServiceClient::new(1000);
    // Before caps: only caps.
    assert_eq!(
        c.send(&mut t, 0, EmeServiceRequest::Log(EmeLogQuery::default()), 1),
        Err(EmeSendError::NotPermitted(EmeCapability::View))
    );
    let id = c.send(&mut t, 0, EmeServiceRequest::Caps, 2).unwrap();
    c.set_permissions(perms(2));
    assert_eq!(
        c.send(
            &mut t,
            0,
            EmeServiceRequest::Undo(EmeUndoArgs {
                stroke: Some(1),
                ..Default::default()
            }),
            3
        ),
        Err(EmeSendError::NotPermitted(EmeCapability::Edit))
    );
    let id2 = c
        .send(
            &mut t,
            500,
            EmeServiceRequest::Log(EmeLogQuery::default()),
            4,
        )
        .unwrap();
    // Oversized frame.
    c.set_permissions(perms(3));
    let cells = (0..200)
        .map(|i| EmeAreaCell {
            ax: i,
            ay: i,
            area_id: 1519,
        })
        .collect();
    let big = EmeServiceRequest::OvEdit(EmeOverlayEdit::Area {
        map: 0,
        stroke: 1,
        cells,
    });
    assert!(matches!(
        c.send(&mut t, 0, big, 5),
        Err(EmeSendError::TooLarge(_))
    ));
    // Replies route; a late reply after a timeout is dropped.
    let done = c.on_reply(EmeServiceReply::ok(id)).unwrap();
    assert_eq!((done.route, done.op), (2, "caps"));
    let expired = c.expire(1500);
    assert_eq!(expired.len(), 1);
    assert_eq!((expired[0].id, expired[0].route), (id2, 4));
    assert_eq!(
        expired[0].reply.error_code(),
        Some(EmeServiceError::Timeout)
    );
    assert!(c.on_reply(EmeServiceReply::ok(id2)).is_none());
    // Link down.
    let mut down = EmeLoopbackTransport::new(EmeLink::Unsupported);
    assert_eq!(
        c.send(&mut down, 0, EmeServiceRequest::Caps, 6),
        Err(EmeSendError::LinkDown)
    );
}

// --- dispatcher end to end ------------------------------------------------------------------

struct World {
    reg: EmeToolRegistry,
    disp: EmeDispatcher,
    t: EmeLoopbackTransport,
    seq: u32,
}

impl World {
    fn new(rank: u8) -> Self {
        let mut w = Self {
            reg: registry(),
            disp: EmeDispatcher::new(EmeUndoStack::new(), 5000),
            t: EmeLoopbackTransport::new(EmeLink::Up { server_version: 3 }),
            seq: 1000,
        };
        w.disp.request_caps(0, &mut w.t).unwrap();
        let (id, _) = w.t.sent_services.pop().unwrap();
        let mut reply = EmeServiceReply::ok(id);
        let c = serde_json::to_value(caps(rank, &["sculpt", "log", "objects"])).unwrap();
        reply.extra = c.as_object().unwrap().clone();
        w.t.deliver(EmeInbound::Reply(reply));
        let out = w.disp.pump(0, &mut w.t);
        assert_eq!(out.replies[0].op, "caps");
        assert_eq!(w.disp.permissions().rank(), EmeRank(rank));
        w
    }

    fn frame(&mut self, now: u64, x: Option<f32>, pressed: bool, held: bool, released: bool) {
        let mut ctx = EmeInputCtx::idle(now, 0, self.disp.permissions());
        ctx.hit = x.map(|x| EmeHit {
            pos: [x, 0.0, 10.0],
            normal: [0.0, 0.0, 1.0],
            object: None,
        });
        ctx.primary.pressed = pressed;
        ctx.primary.held = held || pressed;
        ctx.primary.released = released;
        let mut sink = EmeCommandSink::new();
        self.reg.feed(&ctx, self.disp.caps(), &mut sink);
        let errs = self.disp.dispatch(now, &mut self.t, &mut sink);
        assert!(errs.is_empty(), "{errs:?}");
    }

    /// The server echoes every sent edit.
    fn server_confirms(&mut self) {
        for e in std::mem::take(&mut self.t.sent_edits) {
            self.seq += 1;
            self.t.deliver(EmeInbound::EditConfirmed {
                nonce: e.nonce,
                seq: self.seq,
                stroke: e.stroke,
            });
        }
    }

    fn answer_services(&mut self, f: impl Fn(&EmeServiceRequest) -> Option<&'static str>) {
        for (id, req) in std::mem::take(&mut self.t.sent_services) {
            let reply = match f(&req) {
                None => EmeServiceReply::ok(id),
                Some(code) => EmeServiceReply::error(id, code),
            };
            self.t.deliver(EmeInbound::Reply(reply));
        }
    }
}

#[test]
fn a_held_brush_becomes_one_undoable_stroke() {
    let mut w = World::new(3);
    let mut sink = EmeCommandSink::new();
    w.reg
        .activate("sculpt", &w.disp.permissions(), w.disp.caps(), &mut sink)
        .unwrap();
    w.frame(1000, Some(0.0), true, true, false);
    let mut t = 1000;
    for i in 1..=30 {
        t += 16;
        w.frame(t, Some(i as f32), false, true, false);
    }
    w.frame(t + 16, Some(31.0), false, false, true);
    let sent = w.t.sent_edits.len();
    assert!((3..=5).contains(&sent), "{sent} edits for ~0.5 s");
    let stroke = w.t.sent_edits[0].stroke;
    assert!(w.t.sent_edits.iter().all(|e| e.stroke == stroke));

    w.server_confirms();
    w.disp.pump(t + 100, &mut w.t);
    // A trailing release edit may still be pending; let ticks flush it.
    for k in 1..10 {
        w.frame(t + 100 + k * 16, None, false, false, false);
    }
    w.server_confirms();
    w.disp.pump(t + 400, &mut w.t);
    let entries = w.disp.undo_stack().entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state(), &EmeEntryState::Applied);

    assert!(matches!(
        w.disp.undo(t + 500, &mut w.t),
        Ok(EmeUndoStarted::Sent(_))
    ));
    assert_eq!(
        w.t.sent_services.last().unwrap().1,
        EmeServiceRequest::Undo(EmeUndoArgs {
            stroke: Some(stroke),
            layer: Some(EmeLayer::Voxel),
            ..Default::default()
        })
    );
    w.answer_services(|_| None);
    w.disp.pump(t + 600, &mut w.t);
    assert_eq!(
        w.disp.undo_stack().entries()[0].state(),
        &EmeEntryState::Undone
    );
    let events = w.disp.drain_events();
    assert!(events
        .iter()
        .any(|e| matches!(e, EmeEvent::EntryCommitted(_))));
    assert!(events.iter().any(|e| matches!(e, EmeEvent::EntryUndone(_))));
}

#[test]
fn rank_2_is_read_only_at_the_bus() {
    let mut w = World::new(2);
    let mut sink = EmeCommandSink::new();
    let e = EmeStrokeBuilder::default().begin(
        0,
        EmeBrushSample {
            map: 0,
            pos: [0.0; 3],
            radius: 2.0,
            mode: mode::DIG,
            strength: 255,
            param: 0,
        },
    );
    sink.push_from(
        EmeToolId("sculpt"),
        EmeCommand::VoxelStroke {
            stroke: e.0,
            ops: e.1.into_iter().collect(),
            mark: EmeStrokeMark::End { trailing: false },
        },
    );
    sink.push(EmeCommand::Object(EmeObjectRequest::Delete {
        kind: EmeObjectKind::Npc,
        guid: 5,
    }));
    sink.push(EmeCommand::Service(EmeServiceRequest::Log(
        EmeLogQuery::default(),
    )));
    let errs = w.disp.dispatch(0, &mut w.t, &mut sink);
    assert_eq!(errs.len(), 2);
    assert!(errs
        .iter()
        .all(|e| e.error == EmeSendError::NotPermitted(EmeCapability::Edit)));
    assert_eq!(errs[0].origin, Some(EmeToolId("sculpt")));
    assert!(w.t.sent_edits.is_empty());
    assert_eq!(w.t.sent_services.len(), 1, "only the log query left");
    assert!(w.disp.undo_stack().entries().is_empty());
    assert!(w.disp.undo(0, &mut w.t).is_err());
}

#[test]
fn rejected_edits_never_reach_history() {
    let mut w = World::new(3);
    let mut b = EmeStrokeBuilder::default();
    let (stroke, e) = b.begin(
        0,
        EmeBrushSample {
            map: 0,
            pos: [0.0; 3],
            radius: 2.0,
            mode: mode::DIG,
            strength: 255,
            param: 0,
        },
    );
    let mut sink = EmeCommandSink::new();
    sink.push(EmeCommand::VoxelStroke {
        stroke,
        ops: e.into_iter().collect(),
        mark: EmeStrokeMark::End { trailing: false },
    });
    w.disp.dispatch(0, &mut w.t, &mut sink);
    let nonce = w.t.sent_edits[0].nonce;
    w.t.deliver(EmeInbound::EditRejected {
        nonce,
        reason: EmeRejectReason::Range,
    });
    w.disp.pump(10, &mut w.t);
    assert!(w.disp.undo_stack().entries().is_empty());
}

#[test]
fn objects_get_transaction_ids_and_one_shared_ctrl_z() {
    let mut w = World::new(3);
    let mut sink = EmeCommandSink::new();
    let spawn = EmeObjSpawn {
        kind: EmeObjectKind::Go,
        entry: 180000,
        map: 0,
        x: 1.0,
        y: 2.0,
        z: 3.0,
        o: 0.0,
        scale: 1.0,
        tx: EmeTxId(0),
    };
    sink.push(EmeCommand::Object(EmeObjectRequest::Spawn(spawn)));
    sink.push(EmeCommand::Object(EmeObjectRequest::Delete {
        kind: EmeObjectKind::Go,
        guid: 77,
    }));
    w.disp.dispatch(0, &mut w.t, &mut sink);
    let txs: Vec<EmeTxId> =
        w.t.sent_services
            .iter()
            .map(|(_, r)| match r {
                EmeServiceRequest::ObjSpawn(s) => s.tx,
                EmeServiceRequest::ObjDelete { tx, .. } => *tx,
                other => panic!("{other:?}"),
            })
            .collect();
    assert_eq!(txs, vec![EmeTxId(1), EmeTxId(2)]);
    // The delete is refused by the placement policy; the spawn lands.
    w.answer_services(|r| matches!(r, EmeServiceRequest::ObjDelete { .. }).then_some("rank"));
    let out = w.disp.pump(10, &mut w.t);
    assert_eq!(out.replies.len(), 2, "tools see their commit replies");
    assert_eq!(w.disp.undo_stack().entries().len(), 1);
    w.disp.undo(20, &mut w.t).unwrap();
    assert_eq!(
        w.t.sent_services.last().unwrap().1,
        EmeServiceRequest::ObjUndo {
            tx: EmeTxId(1),
            actor: None
        }
    );
}

#[test]
fn link_loss_fails_requests_and_settles_history() {
    let mut w = World::new(3);
    let mut sink = EmeCommandSink::new();
    sink.push(EmeCommand::Overlay(EmeOverlayEdit::Liquid {
        map: 0,
        stroke: 4,
        cells: vec![EmeLiquidCell {
            cx: 1,
            cy: 1,
            kind: EmeLiquidKind::Water,
            level: 5.0,
            flags: 0,
            fade: 0,
        }],
    }));
    sink.push(EmeCommand::Service(EmeServiceRequest::SnapshotList {
        radius: None,
        limit: None,
    }));
    w.disp.dispatch(0, &mut w.t, &mut sink);
    assert_eq!(w.disp.undo_stack().entries().len(), 1);
    w.t.deliver(EmeInbound::Link(EmeLink::Down));
    let out = w.disp.pump(10, &mut w.t);
    assert_eq!(out.link, Some(EmeLink::Down));
    assert!(out
        .replies
        .iter()
        .all(|r| r.reply.error_code() == Some(EmeServiceError::LinkLost)));
    assert_eq!(out.replies.len(), 2);
    assert!(
        w.disp.undo_stack().entries().is_empty(),
        "unconfirmed overlay batch discarded"
    );
    // Further commands report the link.
    let mut sink = EmeCommandSink::new();
    sink.push(EmeCommand::Service(EmeServiceRequest::SnapshotList {
        radius: None,
        limit: None,
    }));
    let errs = w.disp.dispatch(20, &mut w.t, &mut sink);
    assert_eq!(errs[0].error, EmeSendError::LinkDown);
}

#[test]
fn queued_undo_goes_out_when_the_commit_reply_arrives() {
    let mut w = World::new(3);
    let mut sink = EmeCommandSink::new();
    sink.push(EmeCommand::Overlay(EmeOverlayEdit::Area {
        map: 1,
        stroke: 2,
        cells: vec![EmeAreaCell {
            ax: 0,
            ay: 0,
            area_id: 12,
        }],
    }));
    w.disp.dispatch(0, &mut w.t, &mut sink);
    assert!(matches!(
        w.disp.undo(5, &mut w.t),
        Ok(EmeUndoStarted::Queued(_))
    ));
    w.answer_services(|_| None);
    w.disp.pump(10, &mut w.t);
    assert_eq!(
        w.t.sent_services.last().unwrap().1,
        EmeServiceRequest::Undo(EmeUndoArgs {
            stroke: Some(2),
            layer: Some(EmeLayer::Area),
            ..Default::default()
        })
    );
}

#[test]
fn pushes_reach_the_host_and_caps_pushes_update_permissions() {
    let mut w = World::new(3);
    let sent = w.t.sent_services.len();
    w.t.deliver(EmeInbound::Push(EmeServicePush::CapsChanged {
        rank: EmeRank(2),
    }));
    let out = w.disp.pump(0, &mut w.t);
    assert_eq!(out.pushes.len(), 1);
    assert_eq!(w.t.sent_services.len(), sent + 1, "caps re-requested");
    assert_eq!(w.t.sent_services.last().unwrap().1, EmeServiceRequest::Caps);
    assert_eq!(w.disp.caps().unwrap().rank, EmeRank(2));
    w.t.deliver(EmeInbound::Push(EmeServicePush::Enforce { enforced: true }));
    w.disp.pump(1, &mut w.t);
    assert!(w.disp.caps().unwrap().enforced);
    assert_eq!(w.disp.permissions().rank(), EmeRank(2));
    assert!(!w.disp.permissions().has(EmeCapability::Edit));
}

#[test]
fn route_type_is_public_for_hosts() {
    // Hosts that run their own client may reuse the dispatcher's routing tag.
    let r = EmeRoute::Tool(Some(EmeToolId("log")));
    assert_eq!(r.clone(), r);
    let _ = EmeLoopbackTransport::default().state();
}

//! MONKEY (EME objects, CONTRACT v3.7): object mutations carry a `tx` and are resent with the SAME
//! frame id and `tx` when the server does not answer; undo / redo map onto `obj_undo` and a
//! re-issued op; the Objects editor drives the dispatcher end to end.

use eme_core::api::EmeDiscardReason;
use eme_core::bus::EmeDispatcher;
use eme_core::command::{EmeCommand, EmeCommandSink, EmeObjectRequest};
use eme_core::gizmo::{EmeObb, EmeWorldObject};
use eme_core::objects::{EmeObjectsCmd, EmeObjectsEditor};
use eme_core::perm::{EmePermissions, EmeRank};
use eme_core::service::*;
use eme_core::tool::{EmeButton, EmeHit, EmeInputCtx};
use eme_core::transport::{EmeInbound, EmeLink, EmeLoopbackTransport};
use eme_core::undo::{EmeEntryState, EmeUndoStack};
use serde_json::json;

struct W {
    d: EmeDispatcher,
    t: EmeLoopbackTransport,
}

impl W {
    fn new(rank: u8) -> Self {
        let mut w = Self {
            d: EmeDispatcher::new(EmeUndoStack::new(), 10_000),
            t: EmeLoopbackTransport::new(EmeLink::Up { server_version: 3 }),
        };
        w.d.request_caps(0, &mut w.t).unwrap();
        let (id, _) = w.t.sent_services.pop().unwrap();
        let mut r = EmeServiceReply::ok(id);
        r.extra = json!({
            "rank": rank, "wire": 3, "tools": ["objects"], "perms": {}, "enforced": 0,
            "enforced_layers": {}, "limits": {"obj_scale": [0.1, 20.0]}
        })
        .as_object()
        .unwrap()
        .clone();
        w.t.deliver(EmeInbound::Reply(r));
        w.d.pump(0, &mut w.t);
        w
    }

    fn send(&mut self, now: u64, obj: EmeObjectRequest) {
        let mut sink = EmeCommandSink::new();
        sink.push_from(eme_core::EmeToolId("objects"), EmeCommand::Object(obj));
        let errs = self.d.dispatch(now, &mut self.t, &mut sink);
        assert!(errs.is_empty(), "{errs:?}");
    }

    fn reply(&mut self, id: EmeRequestId, row: Option<EmeObjectRow>, tx: EmeTxId, extra: serde_json::Value) {
        let mut r = EmeServiceReply::ok(id).with("tx", tx).with("row", row);
        for (k, v) in extra.as_object().unwrap() {
            r.extra.insert(k.clone(), v.clone());
        }
        self.t.deliver(EmeInbound::Reply(r));
    }
}

fn row(guid: u32) -> EmeObjectRow {
    EmeObjectRow {
        guid,
        kind: EmeObjectKind::Go,
        entry: 2910,
        name: Some("Chair".into()),
        map: 0,
        x: 1.0,
        y: 2.0,
        z: 3.0,
        o: 0.5,
        scale: 1.0,
        owner: None,
        tx: None,
    }
}

fn mv(guid: u32, x: f32) -> EmeObjectRequest {
    EmeObjectRequest::Move(EmeObjMove {
        kind: EmeObjectKind::Go,
        guid,
        x,
        y: 2.0,
        z: 3.0,
        o: 0.5,
        scale: 1.0,
        tx: EmeTxId(0),
    })
}

fn tx_of(r: &EmeServiceRequest) -> EmeTxId {
    match r {
        EmeServiceRequest::ObjMove(m) => m.tx,
        EmeServiceRequest::ObjSpawn(s) => s.tx,
        EmeServiceRequest::ObjDelete { tx, .. } => *tx,
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unanswered_mutation_is_resent_with_the_same_id_and_tx_then_fails() {
    let mut w = W::new(3);
    w.send(1_000, mv(123, 5.0));
    assert_eq!(w.t.sent_services.len(), 1);
    let (id, first) = w.t.sent_services[0].clone();
    let tx = tx_of(&first);
    assert!(tx.0 > 0, "the dispatcher assigned a tx");

    // Before the wait is over: nothing.
    w.d.pump(1_000 + 3_999, &mut w.t);
    assert_eq!(w.t.sent_services.len(), 1);
    // After it: the identical frame (id AND tx), again after each further wait.
    w.d.pump(1_000 + 4_000, &mut w.t);
    w.d.pump(1_000 + 8_000, &mut w.t);
    assert_eq!(w.t.sent_services.len(), 3);
    for (rid, req) in &w.t.sent_services {
        assert_eq!(*rid, id);
        assert_eq!(*req, first);
    }
    // Out of resends: one more wait, then it fails with a timeout and the entry is dropped.
    let out = w.d.pump(1_000 + 12_000, &mut w.t);
    assert_eq!(w.t.sent_services.len(), 3, "no fourth frame");
    let r = out.replies.iter().find(|r| r.op == "obj_move").expect("the failed reply");
    assert_eq!(r.reply.error_code(), Some(EmeServiceError::Timeout));
    assert!(w.d.undo_stack().entries().is_empty());
    let ev = w.d.drain_events();
    assert!(ev.iter().any(|e| matches!(
        e,
        eme_core::EmeEvent::EntryDiscarded { reason: EmeDiscardReason::CommitFailed(EmeServiceError::Timeout), .. }
    )));
}

#[test]
fn a_reply_after_a_resend_commits_once_and_the_replayed_duplicate_is_ignored() {
    let mut w = W::new(3);
    w.send(0, mv(123, 5.0));
    let (id, first) = w.t.sent_services[0].clone();
    let tx = tx_of(&first);
    w.d.pump(4_000, &mut w.t); // resend
    assert_eq!(w.t.sent_services.len(), 2);
    // The server answers the resend (the original was lost), then the original's late reply.
    w.reply(id, Some(row(123)), tx, json!({"replayed": true}));
    let out = w.d.pump(4_100, &mut w.t);
    assert_eq!(out.replies.iter().filter(|r| r.op == "obj_move").count(), 1);
    let e = &w.d.undo_stack().entries()[0];
    assert_eq!(*e.state(), EmeEntryState::Applied);
    w.reply(id, Some(row(123)), tx, json!({}));
    let out = w.d.pump(4_200, &mut w.t);
    assert!(out.replies.is_empty(), "the late duplicate matches nothing");
    // Nothing is resent for a settled request.
    w.d.pump(60_000, &mut w.t);
    assert_eq!(w.t.sent_services.len(), 2);
}

#[test]
fn a_new_mutation_gets_a_fresh_tx_and_a_plain_request_is_not_resent() {
    let mut w = W::new(3);
    w.send(0, mv(1, 1.0));
    w.send(0, mv(1, 2.0));
    let txs: Vec<_> = w.t.sent_services.iter().map(|(_, r)| tx_of(r)).collect();
    assert_eq!(txs.len(), 2);
    assert_ne!(txs[0], txs[1]);
    // A read (`obj_nearby`) is not idempotent-tracked: it is never resent, it just times out.
    let mut sink = EmeCommandSink::new();
    sink.push(EmeCommand::Service(EmeServiceRequest::ObjNearby {
        radius: 60.0,
        kind: None,
        limit: None,
        after_guid: None,
    }));
    w.d.dispatch(0, &mut w.t, &mut sink);
    assert_eq!(w.t.sent_services.len(), 3);
    w.d.pump(9_999, &mut w.t);
    let resent_nearby = w
        .t
        .sent_services
        .iter()
        .filter(|(_, r)| matches!(r, EmeServiceRequest::ObjNearby { .. }))
        .count();
    assert_eq!(resent_nearby, 1);
}

#[test]
fn delete_undo_respawns_under_a_new_guid_and_redo_names_the_live_one() {
    let mut w = W::new(3);
    w.send(0, EmeObjectRequest::Delete { kind: EmeObjectKind::Go, guid: 123 });
    let (id, req) = w.t.sent_services.pop().unwrap();
    let tx = tx_of(&req);
    w.reply(id, Some(row(123)), tx, json!({}));
    w.d.pump(10, &mut w.t);

    // Ctrl+Z: obj_undo of that tx.
    w.d.undo(20, &mut w.t).unwrap();
    let (uid, ureq) = w.t.sent_services.pop().unwrap();
    assert_eq!(ureq, EmeServiceRequest::ObjUndo { tx, actor: None });
    // The server re-creates it as G200 (replaces 123).
    w.reply(uid, Some(row(200)), tx, json!({"replaces": 123}));
    w.d.pump(30, &mut w.t);
    assert_eq!(*w.d.undo_stack().entries()[0].state(), EmeEntryState::Undone);

    // Ctrl+Y re-issues the delete against the LIVE guid with a fresh tx.
    w.d.redo(40, &mut w.t).unwrap();
    let (_, rreq) = w.t.sent_services.pop().unwrap();
    match rreq {
        EmeServiceRequest::ObjDelete { guid, tx: tx2, kind } => {
            assert_eq!((guid, kind), (200, EmeObjectKind::Go));
            assert_ne!(tx2, tx);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unanswered_undo_is_not_resent_but_times_out_and_the_entry_stays_applied() {
    let mut w = W::new(3);
    w.send(0, mv(123, 5.0));
    let (id, req) = w.t.sent_services.pop().unwrap();
    let tx = tx_of(&req);
    w.reply(id, Some(row(123)), tx, json!({}));
    w.d.pump(10, &mut w.t);
    w.d.undo(20, &mut w.t).unwrap();
    assert_eq!(w.t.sent_services.len(), 1);
    w.d.pump(20 + 5_000, &mut w.t);
    assert_eq!(w.t.sent_services.len(), 1, "obj_undo is not idempotent: never resent");
    w.d.pump(20 + 10_000, &mut w.t);
    assert_eq!(*w.d.undo_stack().entries()[0].state(), EmeEntryState::Applied);
}

#[test]
fn a_redo_of_a_move_is_resendable_like_any_object_mutation() {
    let mut w = W::new(3);
    w.send(0, mv(123, 5.0));
    let (id, req) = w.t.sent_services.pop().unwrap();
    let tx = tx_of(&req);
    w.reply(id, Some(row(123)), tx, json!({}));
    w.d.pump(10, &mut w.t);
    w.d.undo(20, &mut w.t).unwrap();
    let (uid, _) = w.t.sent_services.pop().unwrap();
    w.reply(uid, Some(row(123)), tx, json!({}));
    w.d.pump(30, &mut w.t);
    w.d.redo(40, &mut w.t).unwrap();
    assert_eq!(w.t.sent_services.len(), 1);
    let first = w.t.sent_services[0].clone();
    w.d.pump(40 + 4_000, &mut w.t);
    assert_eq!(w.t.sent_services.len(), 2);
    assert_eq!(w.t.sent_services[1], first);
}

// ---- the editor and the dispatcher together -----------------------------------------------------

#[test]
fn the_editor_moves_an_object_through_the_dispatcher_and_follows_the_reply() {
    let mut w = W::new(3);
    let mut ed = EmeObjectsEditor::new();
    ed.activate();
    ed.set_world(vec![EmeWorldObject {
        guid: 123,
        entry: 2910,
        name: None,
        pos: [1.0, 2.0, 3.0],
        obb: EmeObb::axis_aligned([1.0, 2.0, 4.0], [1.0; 3]),
    }]);
    let perms = EmePermissions::from_rank(EmeRank(3));
    let mk = |now: u64| {
        let mut ctx = EmeInputCtx::idle(now, 0, perms);
        ctx.primary = EmeButton::default();
        ctx.hit = Some(EmeHit { pos: [1.0, 2.0, 3.0], normal: [0.0, 0.0, 1.0], object: None });
        ctx
    };
    // Select 123 (its row arrives by obj_get), then set X in the inspector.
    ed.select_only(123);
    let mut sink = EmeCommandSink::new();
    ed.step(&mk(1_000), None, &mut sink);
    w.d.dispatch(1_000, &mut w.t, &mut sink);
    let (gid, greq) = w.t.sent_services.pop().expect("obj_nearby or obj_get");
    // The first request may be the list; answer whichever it is.
    let answer = |w: &mut W, ed: &mut EmeObjectsEditor, id: EmeRequestId, req: &EmeServiceRequest, now| {
        match req {
            EmeServiceRequest::ObjGet { .. } => w.reply(id, Some(row(123)), EmeTxId(0), json!({})),
            EmeServiceRequest::ObjNearby { .. } => {
                let mut r = EmeServiceReply::ok(id).with("more", false);
                r.rows = Some(vec![serde_json::to_value(row(123)).unwrap()]);
                w.t.deliver(EmeInbound::Reply(r));
            }
            other => panic!("{other:?}"),
        }
        for r in w.d.pump(now, &mut w.t).replies {
            ed.on_reply(r.op, &r.reply);
        }
    };
    answer(&mut w, &mut ed, gid, &greq, 1_050);
    // Drain remaining reads (list and row), answering each.
    for step in 0..6u64 {
        let now = 1_200 + step * 200;
        let mut sink = EmeCommandSink::new();
        ed.step(&mk(now), None, &mut sink);
        w.d.dispatch(now, &mut w.t, &mut sink);
        while let Some((id, req)) = w.t.sent_services.pop() {
            answer(&mut w, &mut ed, id, &req, now + 10);
        }
    }
    assert_eq!(ed.row(123).map(|r| r.x), Some(1.0));

    // The inspector edit: one obj_move through the dispatcher, with a tx.
    let mut sink = EmeCommandSink::new();
    ed.command(EmeObjectsCmd::SetField { field: eme_core::objects::EmeObjField::X, value: 9.0 });
    ed.step(&mk(3_000), None, &mut sink);
    assert!(w.d.dispatch(3_000, &mut w.t, &mut sink).is_empty());
    let (mid, mreq) = w.t.sent_services.pop().expect("obj_move");
    let EmeServiceRequest::ObjMove(m) = &mreq else { panic!("{mreq:?}") };
    assert_eq!((m.guid, m.x, m.y, m.z, m.scale), (123, 9.0, 2.0, 3.0, 1.0));
    assert!(m.tx.0 > 0);
    assert_eq!(w.d.undo_stack().entries().len(), 1, "one undo entry for the move");
    // The server answers with the authoritative row.
    let mut moved = row(123);
    moved.x = 9.0;
    w.reply(mid, Some(moved), m.tx, json!({}));
    for r in w.d.pump(3_100, &mut w.t).replies {
        ed.on_reply(r.op, &r.reply);
    }
    assert_eq!(ed.row(123).unwrap().x, 9.0);
    assert!(ed.ghost_of(123).is_none());
    // Ctrl+Z maps to obj_undo of that very tx.
    w.d.undo(3_200, &mut w.t).unwrap();
    let (_, ureq) = w.t.sent_services.pop().unwrap();
    assert_eq!(ureq, EmeServiceRequest::ObjUndo { tx: m.tx, actor: None });
}

#[test]
fn a_rank_two_caller_cannot_dispatch_object_mutations() {
    let mut w = W::new(2);
    let mut sink = EmeCommandSink::new();
    sink.push(EmeCommand::Object(mv(1, 1.0)));
    let errs = w.d.dispatch(0, &mut w.t, &mut sink);
    assert_eq!(errs.len(), 1);
    assert!(w.t.sent_services.is_empty());
}

#[test]
fn tx_ids_of_two_sessions_do_not_overlap() {
    let a = EmeUndoStack::tx_seed(1_800_000_000);
    let b = EmeUndoStack::tx_seed(1_800_000_000 + 60);
    assert!(b - a >= 960, "a minute apart = 960 ids");
    // A later session starts above everything an earlier one could have used in that minute.
    let mut s = EmeUndoStack::new().with_tx_start(a);
    let last = (0..500).map(|_| s.alloc_tx().0).last().unwrap();
    assert!(last < b);
    // Never 0, never past the u32 range even for an absurd clock.
    assert!(EmeUndoStack::tx_seed(0) >= 1);
    assert!(EmeUndoStack::tx_seed(u64::MAX) < u32::MAX);
}

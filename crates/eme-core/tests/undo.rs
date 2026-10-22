//! The server-confirmed undo/redo state machine (CONTRACT v3 §10, §13).

use eme_core::api::{EmeDiscardReason, EmeEvent, EmeHistoryHook};
use eme_core::service::*;
use eme_core::transport::EmeRejectReason;
use eme_core::undo::*;
use std::sync::{Arc, Mutex};

fn ok() -> EmeServiceReply {
    EmeServiceReply::ok(EmeRequestId(1))
}

fn err(code: &str) -> EmeServiceReply {
    EmeServiceReply::error(EmeRequestId(1), code)
}

/// A committed voxel stroke `stroke` with one edit (nonce = stroke * 10).
fn committed_stroke(s: &mut EmeUndoStack, stroke: u16) -> EmeEntryId {
    let id = s.note_edit(0, "sculpt", stroke, stroke * 10);
    s.close_stroke(stroke, false);
    assert!(s.on_edit_confirmed(stroke * 10, 1000 + stroke as u32));
    assert_eq!(s.get(id).unwrap().state(), &EmeEntryState::Applied);
    id
}

fn state(s: &EmeUndoStack, id: EmeEntryId) -> EmeEntryState {
    s.get(id).unwrap().state().clone()
}

fn one_request(s: &mut EmeUndoStack) -> EmeHistoryRequest {
    let mut r = s.drain_requests();
    assert_eq!(r.len(), 1, "{r:?}");
    r.pop().unwrap()
}

fn voxel_undo(stroke: u16, force: bool) -> EmeServiceRequest {
    EmeServiceRequest::Undo(EmeUndoArgs {
        stroke: Some(stroke),
        layer: Some(EmeLayer::Voxel),
        force,
        sid: None,
    })
}

#[test]
fn stroke_commits_after_every_edit_answers_in_any_order() {
    let mut s = EmeUndoStack::new();
    let id = s.note_edit(0, "sculpt", 1, 100);
    s.note_edit(10, "sculpt", 1, 101);
    s.note_edit(20, "sculpt", 1, 102);
    assert_eq!(state(&s, id), EmeEntryState::Open);
    // Confirms out of order, one before the brush is released.
    assert!(s.on_edit_confirmed(102, 503));
    s.close_stroke(1, false);
    assert_eq!(state(&s, id), EmeEntryState::Committing);
    assert!(s.on_edit_rejected(100, EmeRejectReason::Rate));
    assert_eq!(state(&s, id), EmeEntryState::Committing);
    assert!(s.on_edit_confirmed(101, 501));
    let e = s.get(id).unwrap();
    assert_eq!(e.state(), &EmeEntryState::Applied);
    assert_eq!((e.confirmed(), e.rejected(), e.pending()), (2, 1, 0));
    assert_eq!(e.seq_range(), Some((501, 503)));
    assert!(
        !s.on_edit_confirmed(101, 501),
        "a duplicate confirm is ignored"
    );
    assert!(!s.on_edit_confirmed(999, 1), "an unknown nonce is ignored");
}

#[test]
fn a_fully_rejected_stroke_is_discarded() {
    let mut s = EmeUndoStack::new();
    let id = s.note_edit(0, "sculpt", 1, 7);
    s.close_stroke(1, false);
    s.drain_events();
    s.on_edit_rejected(7, EmeRejectReason::from_code(6));
    assert!(s.get(id).is_none());
    assert!(!s.can_undo());
    let ev = s.drain_events();
    assert!(matches!(
        &ev[..],
        [EmeEvent::EntryDiscarded {
            reason: EmeDiscardReason::CommitFailed(EmeServiceError::Stroke),
            ..
        }]
    ));
}

#[test]
fn undo_and_redo_are_server_confirmed_and_learn_the_sid() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    assert_eq!(s.undo(), Ok(EmeUndoStarted::Sent(a)));
    let r = one_request(&mut s);
    assert_eq!(r.request, voxel_undo(1, false));
    assert_eq!(state(&s, a), EmeEntryState::UndoPending);
    assert_eq!(
        s.redo(),
        Err(EmeUndoError::Busy(a)),
        "redo of an in-flight undo"
    );
    let reply = ok()
        .with("sid", 412)
        .with("layer", "voxel")
        .with("reverted", 1)
        .with("revert_id", 57);
    assert!(s.on_reply(r.ticket, &reply));
    assert_eq!(state(&s, a), EmeEntryState::Undone);
    assert_eq!(s.get(a).unwrap().sid(), Some(412));
    assert!(!s.can_undo());

    assert_eq!(s.redo(), Ok(a));
    let r = one_request(&mut s);
    assert_eq!(
        r.request,
        EmeServiceRequest::Redo(EmeUndoArgs {
            sid: Some(412),
            ..Default::default()
        }),
        "redo names the exact stroke row"
    );
    assert_eq!(state(&s, a), EmeEntryState::RedoPending);
    s.on_reply(r.ticket, &ok().with("sid", 412).with("restored", 1));
    assert_eq!(state(&s, a), EmeEntryState::Applied);
    assert!(s.can_undo() && !s.can_redo());
}

#[test]
fn server_rejection_restores_the_previous_state() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    s.undo().unwrap();
    let r = one_request(&mut s);
    s.drain_events();
    s.on_reply(r.ticket, &err("rank"));
    assert_eq!(state(&s, a), EmeEntryState::Applied);
    assert_eq!(s.get(a).unwrap().last_error(), Some(&EmeServiceError::Rank));
    assert!(matches!(
        &s.drain_events()[..],
        [EmeEvent::RequestFailed { op: "undo", .. }]
    ));

    s.undo().unwrap();
    let r = one_request(&mut s);
    s.on_reply(r.ticket, &ok());
    s.redo().unwrap();
    let r = one_request(&mut s);
    s.on_reply(r.ticket, &err("rate"));
    assert_eq!(
        state(&s, a),
        EmeEntryState::Undone,
        "a failed redo stays undone"
    );
}

#[test]
fn redo_conflict_discards_the_entry() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    s.undo().unwrap();
    let r = one_request(&mut s);
    s.on_reply(r.ticket, &ok());
    s.redo().unwrap();
    let r = one_request(&mut s);
    s.drain_events();
    s.on_reply(r.ticket, &err("conflict"));
    assert!(s.get(a).is_none());
    assert!(matches!(
        &s.drain_events()[..],
        [EmeEvent::EntryDiscarded {
            reason: EmeDiscardReason::RedoConflict,
            ..
        }]
    ));
}

#[test]
fn rapid_undos_target_successive_entries_and_settle_out_of_order() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    let b = committed_stroke(&mut s, 2);
    let c = committed_stroke(&mut s, 3);
    assert_eq!(s.undo(), Ok(EmeUndoStarted::Sent(c)));
    assert_eq!(s.undo(), Ok(EmeUndoStarted::Sent(b)));
    let reqs = s.drain_requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(s.cursor(), 1);
    // B's reply first, then C fails.
    s.on_reply(reqs[1].ticket, &ok());
    s.on_reply(reqs[0].ticket, &err("internal"));
    assert_eq!(state(&s, b), EmeEntryState::Undone);
    assert_eq!(state(&s, c), EmeEntryState::Applied);
    assert_eq!(state(&s, a), EmeEntryState::Applied);
    // Next Ctrl+Z goes to the newest applied (C); Ctrl+Y to the oldest undone (B).
    assert_eq!(s.undo(), Ok(EmeUndoStarted::Sent(c)));
    s.drain_requests();
    assert_eq!(s.redo(), Ok(b));
}

#[test]
fn redo_order_is_oldest_undone_first_and_waits_for_in_flight_undos() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    let b = committed_stroke(&mut s, 2);
    s.undo().unwrap();
    let rb = one_request(&mut s);
    s.on_reply(rb.ticket, &ok());
    s.undo().unwrap();
    let ra = one_request(&mut s);
    // A's undo is in flight: redo must not jump ahead to B.
    assert_eq!(s.redo(), Err(EmeUndoError::Busy(a)));
    s.on_reply(ra.ticket, &ok());
    assert_eq!(s.redo(), Ok(a));
    assert_eq!(s.redo(), Ok(b), "a second rapid redo goes to the next one");
}

#[test]
fn stale_and_duplicate_replies_are_ignored() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    s.undo().unwrap();
    let r1 = one_request(&mut s);
    s.on_link_lost();
    assert_eq!(state(&s, a), EmeEntryState::Applied);
    assert!(
        !s.on_reply(r1.ticket, &ok()),
        "pre-link-loss reply is stale"
    );
    s.undo().unwrap();
    let r2 = one_request(&mut s);
    assert!(s.on_reply(r2.ticket, &ok()));
    assert!(!s.on_reply(r2.ticket, &ok()), "duplicate");
    assert_eq!(state(&s, a), EmeEntryState::Undone);
}

#[test]
fn undo_of_an_unconfirmed_stroke_is_queued_until_commit() {
    let mut s = EmeUndoStack::new();
    let a = s.note_edit(0, "sculpt", 1, 5);
    assert_eq!(s.undo(), Err(EmeUndoError::Busy(a)), "brush still held");
    s.close_stroke(1, false);
    assert_eq!(s.undo(), Ok(EmeUndoStarted::Queued(a)));
    assert!(s.drain_requests().is_empty());
    assert_eq!(s.undo(), Err(EmeUndoError::Nothing), "already queued");
    s.on_edit_confirmed(5, 77);
    assert_eq!(one_request(&mut s).request, voxel_undo(1, false));
    assert_eq!(state(&s, a), EmeEntryState::UndoPending);
}

#[test]
fn trailing_edit_keeps_the_stroke_committing() {
    let mut s = EmeUndoStack::new();
    let a = s.note_edit(0, "sculpt", 1, 1);
    s.close_stroke(1, true);
    s.on_edit_confirmed(1, 10);
    assert_eq!(
        state(&s, a),
        EmeEntryState::Committing,
        "trailing edit not sent yet"
    );
    assert_eq!(s.note_edit(130, "sculpt", 1, 2), a);
    s.on_edit_confirmed(2, 11);
    assert_eq!(state(&s, a), EmeEntryState::Applied);
    assert_eq!(s.get(a).unwrap().confirmed(), 2);
}

#[test]
fn a_new_stroke_closes_a_split_one() {
    let mut s = EmeUndoStack::new();
    let a = s.note_edit(0, "sculpt", 1, 1);
    let b = s.note_edit(10, "sculpt", 2, 2);
    assert_ne!(a, b);
    assert_eq!(state(&s, a), EmeEntryState::Committing);
    assert_eq!(state(&s, b), EmeEntryState::Open);
}

#[test]
fn others_edited_asks_for_confirmation() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    s.undo().unwrap();
    let r = one_request(&mut s);
    s.drain_events();
    s.on_reply(
        r.ticket,
        &err("others_edited")
            .with("others", ["Geoff"])
            .with("count", 3),
    );
    let EmeEntryState::NeedsConfirm(others) = state(&s, a) else {
        panic!()
    };
    assert_eq!(
        (others.others.clone(), others.count),
        (vec!["Geoff".to_owned()], 3)
    );
    assert!(matches!(
        &s.drain_events()[..],
        [EmeEvent::UndoNeedsConfirm { .. }]
    ));
    // Ctrl+Z again just re-asks.
    assert!(matches!(s.undo(), Ok(EmeUndoStarted::NeedsConfirmation(id, _)) if id == a));
    assert!(s.drain_requests().is_empty());
    // Undo anyway → force.
    s.confirm_undo(a).unwrap();
    let r = one_request(&mut s);
    assert_eq!(r.request, voxel_undo(1, true));
    s.on_reply(r.ticket, &ok());
    assert_eq!(state(&s, a), EmeEntryState::Undone);
}

#[test]
fn declining_the_confirmation_keeps_the_stroke() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    s.undo().unwrap();
    let r = one_request(&mut s);
    s.on_reply(r.ticket, &err("others_edited"));
    s.cancel_confirm(a).unwrap();
    assert_eq!(state(&s, a), EmeEntryState::Applied);
    assert_eq!(s.confirm_undo(a), Err(EmeUndoError::InvalidEntry));
}

#[test]
fn new_work_clears_the_redo_branch() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    let b = committed_stroke(&mut s, 2);
    s.undo().unwrap();
    let rb = one_request(&mut s);
    s.on_reply(rb.ticket, &ok());
    s.undo().unwrap();
    let ra = one_request(&mut s); // A's undo in flight
    s.drain_events();
    let c = committed_stroke(&mut s, 3);
    assert!(s.get(b).is_none(), "undone B dropped at once");
    assert_eq!(state(&s, a), EmeEntryState::UndoPending);
    assert!(
        !s.can_redo(),
        "A will be dropped, so it is not a redo target"
    );
    s.on_reply(ra.ticket, &ok());
    assert!(s.get(a).is_none(), "A dropped when its undo settled");
    assert_eq!(s.entries().len(), 1);
    assert_eq!(s.entries()[0].meta().id, c);
    let discards = s
        .drain_events()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                EmeEvent::EntryDiscarded {
                    reason: EmeDiscardReason::RedoBranchCleared,
                    ..
                }
            )
        })
        .count();
    assert_eq!(discards, 2);
}

#[test]
fn a_failed_undo_of_a_doomed_entry_keeps_it() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    s.undo().unwrap();
    let ra = one_request(&mut s);
    committed_stroke(&mut s, 2);
    s.on_reply(ra.ticket, &err("rate"));
    assert_eq!(
        state(&s, a),
        EmeEntryState::Applied,
        "still live on the server, so it stays"
    );
}

#[test]
fn overlay_batches_join_one_entry_and_undo_by_layer() {
    let mut s = EmeUndoStack::new();
    let a = s.push_overlay(0, "water", EmeOverlayLayer::Liquid, 9);
    assert_eq!(s.push_overlay(5, "water", EmeOverlayLayer::Liquid, 9), a);
    assert!(s.on_commit_reply(a, &ok()));
    assert_eq!(state(&s, a), EmeEntryState::Committing);
    assert!(s.on_commit_reply(a, &err("range")));
    assert_eq!(state(&s, a), EmeEntryState::Applied, "one batch landed");
    assert!(!s.on_commit_reply(a, &ok()), "no batch outstanding");
    s.undo().unwrap();
    let r = one_request(&mut s);
    assert_eq!(
        r.request,
        EmeServiceRequest::Undo(EmeUndoArgs {
            stroke: Some(9),
            layer: Some(EmeLayer::Liquid),
            ..Default::default()
        })
    );
    s.on_reply(r.ticket, &ok());
    s.redo().unwrap();
    assert_eq!(
        one_request(&mut s).request,
        EmeServiceRequest::Redo(EmeUndoArgs {
            stroke: Some(9),
            layer: Some(EmeLayer::Liquid),
            ..Default::default()
        })
    );
}

fn spawn(tx: u32) -> EmeServiceRequest {
    EmeServiceRequest::ObjSpawn(EmeObjSpawn {
        kind: EmeObjectKind::Go,
        entry: 2910,
        map: 0,
        x: 1.0,
        y: 2.0,
        z: 3.0,
        o: 0.5,
        scale: 1.0,
        tx: EmeTxId(tx),
    })
}

fn row(guid: u32) -> EmeObjectRow {
    EmeObjectRow {
        guid,
        kind: EmeObjectKind::Go,
        entry: 2910,
        name: None,
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

#[test]
fn one_shared_ctrl_z_across_terrain_and_objects() {
    let mut s = EmeUndoStack::new();
    let terrain = committed_stroke(&mut s, 1);
    let tx = s.alloc_tx();
    let obj = s.push_object(0, "objects", spawn(tx.0)).unwrap();
    s.on_commit_reply(obj, &ok().with("row", row(123)).with("tx", tx));
    assert_eq!(s.get(obj).unwrap().object_guid(), Some(123));
    s.undo().unwrap();
    assert_eq!(
        one_request(&mut s).request,
        EmeServiceRequest::ObjUndo { tx, actor: None }
    );
    s.undo().unwrap();
    assert_eq!(one_request(&mut s).request, voxel_undo(1, false));
    assert_eq!(state(&s, terrain), EmeEntryState::UndoPending);
}

#[test]
fn object_redo_reissues_the_op_and_follows_new_guids() {
    let mut s = EmeUndoStack::new();
    // Spawn G123, then move it.
    let tx1 = s.alloc_tx();
    let sp = s.push_object(0, "objects", spawn(tx1.0)).unwrap();
    s.on_commit_reply(sp, &ok().with("row", row(123)));
    let tx2 = s.alloc_tx();
    let mv_req = EmeServiceRequest::ObjMove(EmeObjMove {
        kind: EmeObjectKind::Go,
        guid: 123,
        x: 5.0,
        y: 5.0,
        z: 5.0,
        o: 1.0,
        scale: 2.0,
        tx: tx2,
    });
    let mv = s.push_object(0, "objects", mv_req).unwrap();
    s.on_commit_reply(mv, &ok().with("row", row(123)));

    // Undo both (move back, then the spawn deletes it).
    for _ in 0..2 {
        s.undo().unwrap();
        let r = one_request(&mut s);
        s.on_reply(r.ticket, &ok().with("row", Option::<EmeObjectRow>::None));
    }
    // Redo the spawn: the original op again with a fresh tx; the server makes G200.
    s.redo().unwrap();
    let r = one_request(&mut s);
    let EmeServiceRequest::ObjSpawn(again) = &r.request else {
        panic!("{r:?}")
    };
    assert_ne!(again.tx, tx1, "fresh tx");
    let fresh = again.tx;
    s.on_reply(r.ticket, &ok().with("row", row(200)).with("tx", fresh));
    assert_eq!(
        s.get(sp).unwrap().meta().backend,
        EmeUndoBackend::Object {
            tx: fresh,
            action: EmeObjectAction::Spawn
        }
    );
    // Redo the move: it now names G200, not the dead G123.
    s.redo().unwrap();
    let r = one_request(&mut s);
    let EmeServiceRequest::ObjMove(m) = &r.request else {
        panic!("{r:?}")
    };
    assert_eq!((m.guid, m.scale), (200, 2.0));
    assert_ne!(m.tx, tx2);
    // And a later undo of the spawn uses the fresh tx.
    s.on_reply(r.ticket, &ok().with("row", row(200)));
    s.undo().unwrap();
    s.drain_requests();
    s.undo().unwrap();
    assert_eq!(
        one_request(&mut s).request,
        EmeServiceRequest::ObjUndo {
            tx: fresh,
            actor: None
        }
    );
}

#[test]
fn undo_of_a_delete_aliases_the_respawned_guid() {
    let mut s = EmeUndoStack::new();
    let tx = s.alloc_tx();
    let del = s
        .push_object(
            0,
            "objects",
            EmeServiceRequest::ObjDelete {
                kind: EmeObjectKind::Go,
                guid: 123,
                tx,
            },
        )
        .unwrap();
    s.on_commit_reply(del, &ok().with("row", row(123)));
    s.undo().unwrap();
    let r = one_request(&mut s);
    s.on_reply(r.ticket, &ok().with("row", row(124)).with("replaces", 123));
    assert_eq!(s.resolve_guid(EmeObjectKind::Go, 123), 124);
    assert_eq!(
        s.resolve_guid(EmeObjectKind::Npc, 123),
        123,
        "kinds do not share guids"
    );
    s.redo().unwrap();
    let r = one_request(&mut s);
    assert!(
        matches!(r.request, EmeServiceRequest::ObjDelete { guid: 124, .. }),
        "{r:?}"
    );
}

#[test]
fn refused_object_commits_are_discarded() {
    let mut s = EmeUndoStack::new();
    let obj = s.push_object(0, "objects", spawn(1)).unwrap();
    s.on_commit_reply(obj, &err("fief"));
    assert!(s.get(obj).is_none());
    assert!(s
        .push_object(0, "objects", EmeServiceRequest::Caps)
        .is_none());
}

#[test]
fn link_loss_resolves_everything_in_flight() {
    let mut s = EmeUndoStack::new();
    let a = committed_stroke(&mut s, 1);
    let b = committed_stroke(&mut s, 2);
    s.undo().unwrap(); // b undo pending
    s.drain_requests();
    let partial = s.note_edit(0, "sculpt", 3, 30);
    s.note_edit(0, "sculpt", 3, 31);
    s.on_edit_confirmed(30, 900);
    let unconfirmed = s.note_edit(0, "sculpt", 4, 40);
    s.on_link_lost();
    assert_eq!(state(&s, a), EmeEntryState::Applied);
    assert_eq!(state(&s, b), EmeEntryState::Applied);
    assert_eq!(
        s.get(b).unwrap().last_error(),
        Some(&EmeServiceError::LinkLost)
    );
    assert_eq!(
        state(&s, partial),
        EmeEntryState::Applied,
        "a confirmed row exists server-side"
    );
    assert!(s.get(unconfirmed).is_none());
    assert!(!s.on_edit_confirmed(31, 901), "nonce forgotten");
}

#[test]
fn capacity_evicts_the_oldest_settled_entries() {
    let mut s = EmeUndoStack::with_capacity(3);
    let first = committed_stroke(&mut s, 1);
    for st in 2..=5 {
        committed_stroke(&mut s, st);
    }
    assert_eq!(s.entries().len(), 3);
    assert!(s.get(first).is_none());
}

#[test]
fn tx_ids_are_unique_and_resumable() {
    let mut s = EmeUndoStack::new().with_tx_start(8812);
    assert_eq!(s.alloc_tx(), EmeTxId(8812));
    assert_eq!(s.alloc_tx(), EmeTxId(8813));
}

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

struct Hook(Arc<Recorder>);

impl Hook {
    fn log(&self, s: String) {
        self.0 .0.lock().unwrap().push(s);
    }
}

impl EmeHistoryHook for Hook {
    fn on_push(&self, m: &EmeUndoEntryMeta) {
        self.log(format!("push {}", m.id.0));
    }
    fn on_commit(&self, m: &EmeUndoEntryMeta) {
        self.log(format!("commit {}", m.id.0));
    }
    fn on_undo(&self, m: &EmeUndoEntryMeta) {
        self.log(format!("undo {}", m.id.0));
    }
    fn on_redo(&self, m: &EmeUndoEntryMeta) {
        self.log(format!("redo {}", m.id.0));
    }
    fn on_discard(&self, m: &EmeUndoEntryMeta, r: &EmeDiscardReason) {
        self.log(format!("discard {} {r:?}", m.id.0));
    }
}

#[test]
fn history_hooks_see_server_confirmed_transitions_only() {
    let rec = Arc::new(Recorder::default());
    let mut s = EmeUndoStack::new();
    s.add_hook(Box::new(Hook(rec.clone())));
    let a = committed_stroke(&mut s, 1);
    s.undo().unwrap();
    assert_eq!(
        rec.0.lock().unwrap().len(),
        2,
        "no undo call before the reply"
    );
    let r = one_request(&mut s);
    s.on_reply(r.ticket, &ok());
    s.redo().unwrap();
    let r = one_request(&mut s);
    s.on_reply(r.ticket, &ok());
    s.clear();
    let id = a.0;
    assert_eq!(
        *rec.0.lock().unwrap(),
        vec![
            format!("push {id}"),
            format!("commit {id}"),
            format!("undo {id}"),
            format!("redo {id}"),
            format!("discard {id} Cleared"),
        ]
    );
}

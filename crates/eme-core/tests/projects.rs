//! MONKEY (EME projects, CONTRACT v3.4 / v3.4.1): no map edit without an active server-side
//! project; the dispatcher tracks the active project from `caps`, the project ops' replies, `R 7`
//! and the `project_changed` push, and re-opens the user's project after a fresh `caps` (HELLO,
//! map change, reconnect).

use eme_core::api::{EmeDiscardReason, EmeEvent, EmeSendError};
use eme_core::bus::EmeDispatcher;
use eme_core::command::{EmeCommand, EmeCommandSink, EmeStrokeMark};
use eme_core::service::*;
use eme_core::transport::{EmeInbound, EmeLink, EmeLoopbackTransport, EmeRejectReason, EmeVoxelEdit};
use eme_core::undo::EmeUndoStack;
use serde_json::json;

fn caps_json(rank: u8, project: serde_json::Value, ready: Option<bool>) -> serde_json::Value {
    let mut v = json!({
        "rank": rank, "wire": 3, "tools": ["sculpt"], "perms": {}, "enforced": 0,
        "enforced_layers": {}, "project": project,
        "limits": {"edit_range": 120, "modes": [0, 1, 2, 3]}
    });
    if let Some(r) = ready {
        v["projects_ready"] = json!(r);
    }
    v
}

struct W {
    d: EmeDispatcher,
    t: EmeLoopbackTransport,
}

impl W {
    fn new(rank: u8, project: serde_json::Value, ready: Option<bool>) -> Self {
        let mut w = Self {
            d: EmeDispatcher::new(EmeUndoStack::new(), 5000),
            t: EmeLoopbackTransport::new(EmeLink::Up { server_version: 3 }),
        };
        w.caps(rank, project, ready);
        w
    }

    /// Ask caps and answer it.
    fn caps(&mut self, rank: u8, project: serde_json::Value, ready: Option<bool>) {
        self.d.request_caps(0, &mut self.t).unwrap();
        let (id, _) = self.t.sent_services.pop().unwrap();
        let mut r = EmeServiceReply::ok(id);
        r.extra = caps_json(rank, project, ready).as_object().unwrap().clone();
        self.t.deliver(EmeInbound::Reply(r));
        self.d.pump(0, &mut self.t);
    }

    fn edit(&mut self, nonce: u16) -> Vec<eme_core::EmeDispatchError> {
        let mut sink = EmeCommandSink::new();
        sink.push(EmeCommand::VoxelStroke {
            stroke: 1,
            ops: vec![EmeVoxelEdit {
                nonce,
                stroke: 1,
                map: 0,
                pos: [1.0, 2.0, 3.0],
                radius: 3.0,
                mode: 0,
                strength: 255,
                param: 0,
            }],
            mark: EmeStrokeMark::End { trailing: false },
        });
        self.d.dispatch(1, &mut self.t, &mut sink)
    }

    /// Answer the newest request with `extra` fields (ok) or an error code.
    fn answer(&mut self, ok: bool, extra: serde_json::Value) -> EmeServiceRequest {
        let (id, req) = self.t.sent_services.pop().expect("a request");
        let mut r = if ok {
            EmeServiceReply::ok(id)
        } else {
            EmeServiceReply::error(id, extra["err"].as_str().unwrap())
        };
        if ok {
            r.extra = extra.as_object().unwrap().clone();
        }
        self.t.deliver(EmeInbound::Reply(r));
        req
    }
}

#[test]
fn requests_use_the_project_field_not_id() {
    let j = |r: EmeServiceRequest| EmeServiceEnvelope::new(EmeRequestId(9), r).to_json();
    assert_eq!(
        j(EmeServiceRequest::ProjectOpen { project: 5 }),
        r#"{"v":1,"id":9,"op":"project_open","project":5}"#
    );
    assert_eq!(j(EmeServiceRequest::ProjectLeave), r#"{"v":1,"id":9,"op":"project_leave"}"#);
    assert_eq!(
        j(EmeServiceRequest::ProjectRevert { project: 5, force: true }),
        r#"{"v":1,"id":9,"op":"project_revert","project":5,"force":true}"#
    );
    assert_eq!(
        j(EmeServiceRequest::ProjectRevert { project: 5, force: false }),
        r#"{"v":1,"id":9,"op":"project_revert","project":5}"#
    );
    assert_eq!(
        j(EmeServiceRequest::ProjectCreate { name: "Пруд".into(), description: None }),
        r#"{"v":1,"id":9,"op":"project_create","name":"Пруд"}"#
    );
    assert_eq!(
        j(EmeServiceRequest::ProjectList { status: Some("all".into()), mine: true, limit: Some(15), before_id: None }),
        r#"{"v":1,"id":9,"op":"project_list","status":"all","mine":true,"limit":15}"#
    );
    let back = EmeServiceEnvelope::from_json(r#"{"v":1,"id":3,"op":"project_info","project":7}"#).unwrap();
    assert_eq!(back.request, EmeServiceRequest::ProjectInfo { project: 7 });
    assert!(EmeServiceRequest::ProjectUnrevert { project: 1 }.is_mutating());
    assert!(!EmeServiceRequest::ProjectList { status: None, mine: false, limit: None, before_id: None }.is_mutating());
}

#[test]
fn lua_shaped_project_replies_decode() {
    // project_list as MonkeyEmeProjects.lua emits it (null last_edit, flags as booleans).
    let r: EmeServiceReply = serde_json::from_str(
        r#"{"v":1,"id":4,"ok":true,"more":false,"rows":[{"id":3,"name":"Pond","description":"","owner":"Geoff","owner_guid":201,"status":"open","map":0,"created":1761300000,"edits":12,"live":12,"last_edit":null,"mine":true,"active":false}]}"#,
    )
    .unwrap();
    let rows = r.rows_as::<EmeProjectRow>().unwrap();
    assert_eq!(rows[0].name, "Pond");
    assert_eq!(rows[0].last_edit, None);
    assert!(rows[0].mine && !rows[0].active);
    // An empty list is `{}` from Lua.
    let r: EmeServiceReply = serde_json::from_str(r#"{"v":1,"id":4,"ok":true,"more":false,"rows":{}}"#).unwrap();
    assert!(r.rows_as::<EmeProjectRow>().unwrap().is_empty());
    // project_info.
    let r: EmeServiceReply = serde_json::from_str(
        r#"{"v":1,"id":5,"ok":true,"project_id":3,"project":{"id":3,"name":"Pond","description":"d","owner":"Geoff","owner_guid":201,"status":"open","map":0,"created":1,"closed_at":null,"reverted_at":null,"reverted_by":null},"counts":{"voxel":{"total":10,"live":9},"liquid":{"total":2,"live":2},"area":{"total":0,"live":0}},"authors":[{"guid":201,"name":"Geoff","edits":8,"live":8},{"guid":202,"name":"Ada","edits":4,"live":3}],"bbox":[-1.5,2,30,40.25],"first_edit":10,"last_edit":20}"#,
    )
    .unwrap();
    let info: EmeProjectInfo = r.extra_as().unwrap();
    assert_eq!(info.counts["voxel"], EmeProjectCount { total: 10, live: 9 });
    assert_eq!(info.authors.len(), 2);
    assert_eq!(info.bbox, Some([-1.5, 2.0, 30.0, 40.25]));
    // An empty project: no authors (`{}`), bbox null.
    let r: EmeServiceReply = serde_json::from_str(
        r#"{"v":1,"id":5,"ok":true,"project_id":3,"project":{"id":3,"name":"P","status":"open"},"counts":{"voxel":{"total":0,"live":0}},"authors":{},"bbox":null,"first_edit":null,"last_edit":null}"#,
    )
    .unwrap();
    let info: EmeProjectInfo = r.extra_as().unwrap();
    assert!(info.authors.is_empty() && info.bbox.is_none());
    // The push.
    match EmeDownlink::from_json(r#"{"v":1,"id":0,"ev":"project_changed","project":3,"status":"reverted","name":"Pond"}"#).unwrap() {
        EmeDownlink::Push(EmeServicePush::ProjectChanged { project, status, name }) => {
            assert_eq!((project, status.as_str(), name.as_deref()), (3, "reverted", Some("Pond")));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(EmeServiceError::parse("no_project"), EmeServiceError::NoProject);
    assert_eq!(EmeServiceError::NoProject.locale_key(), "MONKEY_EME_ERR_NO_PROJECT");
    assert_eq!(EmeRejectReason::from_code(7), EmeRejectReason::NoProject);
}

#[test]
fn caps_without_project_support_gates_nothing() {
    // A v3.3 server: no `projects_ready` in caps.
    let mut w = W::new(3, json!(null), None);
    assert!(!w.d.projects_required());
    assert!(w.edit(1).is_empty());
    assert_eq!(w.t.sent_edits.len(), 1);
}

#[test]
fn no_map_edit_without_an_active_project() {
    let mut w = W::new(3, json!(null), Some(true));
    assert!(w.d.projects_required() && !w.d.may_edit_map());
    let errs = w.edit(1);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].error, EmeSendError::NoProject);
    assert!(w.t.sent_edits.is_empty(), "nothing uplinked");
    assert!(w.d.undo_stack().entries().is_empty(), "nothing in history");

    // Create a project → active, and edits flow.
    w.d.request(2, &mut w.t, None, EmeServiceRequest::ProjectCreate { name: "Pond".into(), description: None })
        .unwrap();
    w.answer(true, json!({"project_id": 7, "project": {"id": 7, "name": "Pond", "status": "open"}}));
    let out = w.d.pump(3, &mut w.t);
    assert_eq!(out.project, Some(Some(EmeProjectRef { id: 7, name: "Pond".into(), status: Some("open".into()) })));
    assert_eq!(w.d.project().map(|p| p.id), Some(7));
    assert!(w.edit(2).is_empty());
    assert_eq!(w.t.sent_edits.len(), 1);
    assert_eq!(w.d.undo_stack().entries()[0].meta().project, Some(7), "entries carry the project");
}

#[test]
fn caps_names_the_active_project_and_a_fresh_selection_is_reopened() {
    let mut w = W::new(3, json!({"id": 4, "name": "Bridge"}), Some(true));
    assert_eq!(w.d.project().map(|p| p.name.as_str()), Some("Bridge"));
    // The user opens project 9.
    w.d.request(1, &mut w.t, None, EmeServiceRequest::ProjectOpen { project: 9 }).unwrap();
    w.answer(true, json!({"project_id": 9, "project": {"id": 9, "name": "Pond", "status": "open"}}));
    w.d.pump(2, &mut w.t);
    assert_eq!(w.d.wanted_project(), Some(9));
    // A map change: the server's new Lua state has no selection; caps says none → re-open 9.
    w.t.deliver(EmeInbound::Link(EmeLink::Handshaking));
    w.d.pump(3, &mut w.t);
    w.t.deliver(EmeInbound::Link(EmeLink::Up { server_version: 3 }));
    w.d.pump(4, &mut w.t);
    w.caps(3, json!(null), Some(true));
    assert!(w.d.project().is_none());
    let (_, req) = w.t.sent_services.last().cloned().expect("project_open re-sent");
    assert_eq!(req, EmeServiceRequest::ProjectOpen { project: 9 });
    // Sent once only.
    w.d.pump(5, &mut w.t);
    assert_eq!(
        w.t.sent_services.iter().filter(|(_, r)| matches!(r, EmeServiceRequest::ProjectOpen { .. })).count(),
        1
    );
    w.answer(true, json!({"project_id": 9, "project": {"id": 9, "name": "Pond", "status": "open"}}));
    w.d.pump(6, &mut w.t);
    assert_eq!(w.d.project().map(|p| p.id), Some(9));
}

#[test]
fn a_closed_project_is_not_reopened() {
    let mut w = W::new(3, json!({"id": 9, "name": "Pond"}), Some(true));
    w.d.request(1, &mut w.t, None, EmeServiceRequest::ProjectOpen { project: 9 }).unwrap();
    w.answer(true, json!({"project_id": 9, "project": {"id": 9, "name": "Pond", "status": "open"}}));
    w.d.pump(2, &mut w.t);
    w.caps(3, json!(null), Some(true));
    // Re-open refused: closed meanwhile.
    w.answer(false, json!({"err": "conflict"}));
    w.d.pump(3, &mut w.t);
    assert_eq!(w.d.wanted_project(), None);
    w.caps(3, json!(null), Some(true));
    assert!(w.t.sent_services.is_empty(), "no further project_open");
}

#[test]
fn r7_drops_the_project_and_asks_caps_again() {
    let mut w = W::new(3, json!({"id": 9, "name": "Pond"}), Some(true));
    assert!(w.edit(5).is_empty());
    w.t.deliver(EmeInbound::EditRejected { nonce: 5, reason: EmeRejectReason::NoProject });
    let out = w.d.pump(2, &mut w.t);
    assert!(out.no_project);
    assert!(w.d.project().is_none());
    assert_eq!(w.t.sent_services.last().unwrap().1, EmeServiceRequest::Caps);
}

#[test]
fn leave_close_and_revert_clear_the_active_project() {
    let mut w = W::new(3, json!({"id": 9, "name": "Pond"}), Some(true));
    w.d.request(1, &mut w.t, None, EmeServiceRequest::ProjectLeave).unwrap();
    w.answer(true, json!({"project": null}));
    w.d.pump(2, &mut w.t);
    assert!(w.d.project().is_none() && w.d.wanted_project().is_none());

    let mut w = W::new(3, json!({"id": 9, "name": "Pond"}), Some(true));
    assert!(w.edit(5).is_empty());
    w.t.deliver(EmeInbound::EditConfirmed { nonce: 5, seq: 100, stroke: 1 });
    w.d.pump(2, &mut w.t);
    assert_eq!(w.d.undo_stack().entries().len(), 1);
    w.d.request(3, &mut w.t, None, EmeServiceRequest::ProjectRevert { project: 9, force: false }).unwrap();
    w.answer(true, json!({"project_id": 9, "status": "reverted", "voxel": 1, "liquid": 0, "area": 0, "object": 0, "revert_id": 4}));
    w.d.pump(4, &mut w.t);
    assert!(w.d.project().is_none());
    assert!(w.d.undo_stack().entries().is_empty(), "a reverted project's entries leave the stack");
    let ev = w.d.drain_events();
    assert!(ev.iter().any(|e| matches!(e, EmeEvent::EntryDiscarded { reason: EmeDiscardReason::ProjectReverted, .. })));
}

#[test]
fn a_project_changed_push_drops_the_active_project() {
    let mut w = W::new(3, json!({"id": 9, "name": "Pond"}), Some(true));
    w.t.deliver(EmeInbound::Push(EmeServicePush::ProjectChanged { project: 8, status: "closed".into(), name: None }));
    w.d.pump(1, &mut w.t);
    assert_eq!(w.d.project().map(|p| p.id), Some(9), "another project");
    w.t.deliver(EmeInbound::Push(EmeServicePush::ProjectChanged { project: 9, status: "reverted".into(), name: None }));
    let out = w.d.pump(2, &mut w.t);
    assert_eq!(out.project, Some(None));
    assert_eq!(out.pushes.len(), 1);
    assert!(!w.d.may_edit_map());
}

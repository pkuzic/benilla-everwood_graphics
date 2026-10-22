//! `.mveme` JSON: round-trips against the example frames in `tests/fixtures/`, typed decoding,
//! and Lua-encoder leniency.

use eme_core::service::*;
use eme_core::{EmeCapability, EmeRank};
use serde_json::Value;

fn fixture(name: &str) -> Vec<Value> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn every_request_fixture_round_trips_canonically() {
    let frames = fixture("requests.json");
    assert_eq!(frames.len(), 24);
    for frame in frames {
        let text = frame.to_string();
        let env = EmeServiceEnvelope::from_json(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(env.v, EME_SERVICE_VERSION);
        assert_eq!(frame["op"], env.request.op(), "op name of {text}");
        let back: Value = serde_json::from_str(&env.to_json()).unwrap();
        assert_eq!(back, frame, "canonical re-encode of {text}");
        assert_eq!(EmeServiceEnvelope::from_json(&env.to_json()).unwrap(), env);
    }
}

#[test]
fn every_op_has_a_fixture() {
    let ops: std::collections::HashSet<String> = fixture("requests.json")
        .iter()
        .map(|f| f["op"].as_str().unwrap().to_owned())
        .collect();
    for op in [
        "caps",
        "strokes",
        "undo",
        "redo",
        "log",
        "snapshot_create",
        "snapshot_list",
        "revert",
        "undo_player",
        "ov_get",
        "ov_edit",
        "ov_undo",
        "ov_redo",
        "obj_nearby",
        "obj_get",
        "obj_spawn",
        "obj_move",
        "obj_delete",
        "obj_undo",
        "obj_redo",
    ] {
        assert!(ops.contains(op), "no fixture for {op}");
    }
}

#[test]
fn request_shapes_are_exact() {
    let env = EmeServiceEnvelope::new(
        EmeRequestId(7),
        EmeServiceRequest::Undo {
            stroke: 12,
            force: false,
        },
    );
    assert_eq!(env.to_json(), r#"{"v":1,"id":7,"op":"undo","stroke":12}"#);
    let env = EmeServiceEnvelope::new(EmeRequestId(1), EmeServiceRequest::Caps);
    assert_eq!(env.to_json(), r#"{"v":1,"id":1,"op":"caps"}"#);
    let env = EmeServiceEnvelope::new(
        EmeRequestId(2),
        EmeServiceRequest::OvEdit(EmeOverlayEdit::Area {
            map: 1,
            stroke: 3,
            cells: vec![EmeAreaCell {
                cx: 1,
                cy: -2,
                area_id: 12,
            }],
        }),
    );
    assert_eq!(
        env.to_json(),
        r#"{"v":1,"id":2,"op":"ov_edit","layer":"area","map":1,"stroke":3,"cells":[{"cx":1,"cy":-2,"area_id":12}]}"#
    );
}

#[test]
fn russian_labels_survive_as_utf8() {
    let frames = fixture("requests.json");
    let env = EmeServiceEnvelope::from_json(&frames[8].to_string()).unwrap();
    match &env.request {
        EmeServiceRequest::SnapshotCreate { label, .. } => assert!(label.starts_with("Перед")),
        other => panic!("{other:?}"),
    }
    assert!(
        env.to_json().contains("Перед карьером"),
        "no \\u escaping needed"
    );
}

#[test]
fn unknown_op_is_an_error() {
    assert!(EmeServiceEnvelope::from_json(r#"{"v":1,"id":1,"op":"format_c"}"#).is_err());
    assert!(
        EmeServiceEnvelope::from_json(r#"{"v":1,"id":1,"op":"undo"}"#).is_err(),
        "missing stroke"
    );
}

#[test]
fn every_reply_fixture_round_trips_canonically() {
    for frame in fixture("replies.json") {
        let text = frame.to_string();
        let parsed = EmeDownlink::from_json(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        let EmeDownlink::Reply(r) = &parsed else {
            panic!("reply expected: {text}")
        };
        let back: Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(back, frame, "canonical re-encode of {text}");
    }
}

#[test]
fn every_push_fixture_round_trips_canonically() {
    for frame in fixture("pushes.json") {
        let text = frame.to_string();
        let parsed = EmeDownlink::from_json(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert!(
            matches!(parsed, EmeDownlink::Push(_)),
            "push expected: {text}"
        );
        let back: Value = serde_json::from_str(&parsed.to_json()).unwrap();
        assert_eq!(back, frame, "canonical re-encode of {text}");
    }
}

fn reply(i: usize) -> EmeServiceReply {
    match EmeDownlink::from_json(&fixture("replies.json")[i].to_string()).unwrap() {
        EmeDownlink::Reply(r) => r,
        p => panic!("{p:?}"),
    }
}

#[test]
fn caps_reply_decodes() {
    let caps = EmeCaps::from_reply(&reply(0)).unwrap();
    assert_eq!(caps.rank, EmeRank(3));
    assert!(caps.offers_tool("water") && !caps.offers_tool("paint"));
    assert_eq!(caps.limits.edit_rate, Some(8.0));
    assert_eq!(caps.limits.stroke_edits, Some(400));
    assert!(!caps.enforced.area && !caps.enforced.liquid);
    assert_eq!(
        caps.enforced.notes,
        vec!["MONKEY_EME_ENFORCED_VISUAL_ONLY".to_owned()]
    );
}

#[test]
fn row_replies_decode() {
    let strokes: Vec<EmeStrokeRow> = reply(1).rows_as().unwrap();
    assert_eq!(strokes[0].stroke, 12);
    assert_eq!(strokes[0].edits, 19);
    assert!(!reply(1).more());

    let log = reply(2);
    assert!(log.more());
    let rows: Vec<EmeLogRow> = log.rows_as().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].stroke, Some(12));
    assert_eq!(rows[1].stroke, None);
    assert_eq!(rows[1].result, "rank");
    assert_eq!(rows[1].rank, EmeRank::VIEW);

    let snaps: Vec<EmeSnapshotRow> = reply(3).rows_as().unwrap();
    assert_eq!(
        (snaps[0].id, snaps[0].at_seq, snaps[0].at_seq_area),
        (42, 1000, 3)
    );

    let rev: EmeRevertResult = reply(4).extra_as().unwrap();
    assert_eq!(
        rev,
        EmeRevertResult {
            reverted: 57,
            regions: 4
        }
    );

    let warn = reply(5);
    assert_eq!(warn.error_code(), Some(EmeServiceError::OthersAfter));
    let others: Vec<EmeActorRow> = warn.rows_as().unwrap();
    assert_eq!(others[0].name, "Zara");

    let objs: Vec<EmeObjectRow> = reply(6).rows_as().unwrap();
    assert_eq!(objs[0].kind, EmeObjectKind::Go);
    assert_eq!(objs[0].placed_by.as_deref(), Some("Geoff"));

    assert_eq!(reply(7).error_code(), Some(EmeServiceError::Denied));
    assert_eq!(
        reply(8).error_code(),
        Some(EmeServiceError::Other("some_new_code".into()))
    );
    assert_eq!(
        reply(8).error_code().unwrap().locale_key(),
        "MONKEY_EME_ERR_OTHER"
    );
    assert_eq!(
        EmeServiceError::OthersAfter.locale_key(),
        "MONKEY_EME_ERR_OTHERS_AFTER"
    );
}

#[test]
fn lua_encoder_quirks_are_accepted() {
    let frames = fixture("replies_lua.json");
    let parse = |i: usize| match EmeDownlink::from_json(&frames[i].to_string()).unwrap() {
        EmeDownlink::Reply(r) => r,
        p => panic!("{p:?}"),
    };
    // ok:1, tools:{} (empty Lua table), limits:{}, enforced:0.
    let r = parse(0);
    assert!(r.ok);
    let caps = EmeCaps::from_reply(&r).unwrap();
    assert_eq!(caps.rank, EmeRank(2));
    assert!(caps.tools.is_empty());
    assert_eq!(caps.limits, EmeLimits::default());
    assert!(!caps.enforced.area && !caps.enforced.liquid);
    // rows:{} = no rows; it re-encodes as [].
    let r = parse(1);
    assert_eq!(r.rows, Some(vec![]));
    assert!(r.to_json().contains(r#""rows":[]"#));
    // ok:0.
    assert_eq!(parse(2).error_code(), Some(EmeServiceError::Rank));
    // enforced:1 = both; integer rate; unknown limit kept.
    let caps = EmeCaps::from_reply(&parse(3)).unwrap();
    assert!(caps.enforced.area && caps.enforced.liquid);
    assert_eq!(caps.limits.edit_rate, Some(4.0));
    assert_eq!(caps.limits.other.get("future_limit"), Some(&Value::from(7)));
    // reverted:1.
    let rows: Vec<EmeStrokeRow> = parse(4).rows_as().unwrap();
    assert!(rows[0].reverted);
}

#[test]
fn a_non_empty_object_is_not_a_list() {
    let text = r#"{"v":1,"id":1,"ok":true,"rows":{"a":1}}"#;
    assert!(EmeDownlink::from_json(text).is_err());
}

#[test]
fn request_capabilities_match_the_rank_table() {
    use EmeServiceRequest as R;
    assert_eq!(R::Caps.required_capability(), None);
    assert_eq!(
        R::Log(EmeLogQuery::default()).required_capability(),
        Some(EmeCapability::View)
    );
    assert_eq!(
        R::SnapshotList { radius: None }.required_capability(),
        Some(EmeCapability::View)
    );
    assert_eq!(
        R::Undo {
            stroke: 1,
            force: false
        }
        .required_capability(),
        Some(EmeCapability::Edit)
    );
    assert_eq!(
        R::SnapshotCreate {
            label: "x".into(),
            radius: None
        }
        .required_capability(),
        Some(EmeCapability::Edit)
    );
    assert_eq!(
        R::Revert { snapshot: 1 }.required_capability(),
        Some(EmeCapability::RevertOthers)
    );
    assert_eq!(
        R::UndoPlayer {
            player: "x".into(),
            minutes: None,
            radius: None
        }
        .required_capability(),
        Some(EmeCapability::RevertOthers)
    );
    assert!(!R::ObjNearby { radius: 1.0 }.is_mutating());
    assert!(R::ObjDelete {
        guid: 1,
        tx: EmeTxId(1)
    }
    .is_mutating());
}

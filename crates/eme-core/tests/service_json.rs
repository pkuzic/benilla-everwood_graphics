//! `.mveme` JSON against the CONTRACT v3 example frames in `tests/fixtures/` (copied verbatim from
//! `voxel-v2/CONTRACT.md` §12–§13 where it gives one): parse, re-encode, compare.

use eme_core::service::*;
use eme_core::{EmeCapability, EmeRank};
use serde_json::Value;

fn fixture(name: &str) -> Vec<Value> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn num_eq(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(1.0)
}

/// `got` (our re-encoding) says nothing `want` (the contract example) does not: every key we emit
/// is in the example with an equal value (numbers to f32 precision). Keys we omit must be
/// defaults (`false`, `0`, `[]`), which the receiving side assumes when absent.
fn assert_matches(got: &Value, want: &Value, path: &str) {
    match (got, want) {
        (Value::Number(a), Value::Number(b)) => {
            assert!(
                num_eq(a.as_f64().unwrap(), b.as_f64().unwrap()),
                "{path}: {a} vs {b}"
            )
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: array length");
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert_matches(x, y, &format!("{path}[{i}]"));
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in a {
                let w = b
                    .get(k)
                    .unwrap_or_else(|| panic!("{path}.{k}: emitted but not in the example"));
                assert_matches(v, w, &format!("{path}.{k}"));
            }
            for (k, w) in b {
                if !a.contains_key(k) {
                    let default = matches!(w, Value::Bool(false) | Value::Null)
                        || w.as_f64() == Some(0.0)
                        || w.as_array().is_some_and(|x| x.is_empty());
                    assert!(default, "{path}.{k}: dropped a non-default value {w}");
                }
            }
        }
        _ => assert_eq!(got, want, "{path}"),
    }
}

#[test]
fn every_request_fixture_round_trips() {
    let frames = fixture("requests.json");
    assert_eq!(frames.len(), 23);
    for frame in frames {
        let text = frame.to_string();
        let env = EmeServiceEnvelope::from_json(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(env.v, EME_SERVICE_VERSION);
        assert_eq!(frame["op"], env.request.op(), "op name of {text}");
        let back: Value = serde_json::from_str(&env.to_json()).unwrap();
        assert_matches(&back, &frame, &text);
        assert_eq!(EmeServiceEnvelope::from_json(&env.to_json()).unwrap(), env);
    }
}

#[test]
fn every_contract_op_has_a_fixture() {
    let ops: std::collections::HashSet<String> = fixture("requests.json")
        .iter()
        .map(|f| f["op"].as_str().unwrap().to_owned())
        .collect();
    for op in [
        "caps",
        "view",
        "strokes",
        "undo",
        "redo",
        "ov_undo",
        "log",
        "snapshot_create",
        "snapshot_list",
        "revert",
        "undo_player",
        "ov_get",
        "ov_edit",
        "obj_nearby",
        "obj_get",
        "obj_spawn",
        "obj_move",
        "obj_delete",
        "obj_undo",
    ] {
        assert!(ops.contains(op), "no fixture for {op}");
    }
}

#[test]
fn request_shapes_are_exact() {
    let undo = EmeServiceRequest::Undo(EmeUndoArgs {
        stroke: Some(12),
        layer: Some(EmeLayer::Voxel),
        ..Default::default()
    });
    assert_eq!(
        EmeServiceEnvelope::new(EmeRequestId(7), undo).to_json(),
        r#"{"v":1,"id":7,"op":"undo","stroke":12,"layer":"voxel"}"#
    );
    assert_eq!(
        EmeServiceEnvelope::new(EmeRequestId(1), EmeServiceRequest::Caps).to_json(),
        r#"{"v":1,"id":1,"op":"caps"}"#
    );
    let edit = EmeServiceRequest::OvEdit(EmeOverlayEdit::Liquid {
        map: 0,
        stroke: 3,
        cells: vec![EmeLiquidCell {
            cx: 100,
            cy: -20,
            kind: EmeLiquidKind::Water,
            level: 12.5,
            flags: 0,
            fade: 40,
        }],
    });
    assert_eq!(
        EmeServiceEnvelope::new(EmeRequestId(11), edit).to_json(),
        r#"{"v":1,"id":11,"op":"ov_edit","layer":"liquid","map":0,"stroke":3,"cells":[[100,-20,1,12.5,0,40]]}"#
    );
    let area = EmeServiceRequest::OvEdit(EmeOverlayEdit::Area {
        map: 0,
        stroke: 4,
        cells: vec![EmeAreaCell {
            ax: 10,
            ay: -10,
            area_id: 74,
        }],
    });
    assert_eq!(
        EmeServiceEnvelope::new(EmeRequestId(12), area).to_json(),
        r#"{"v":1,"id":12,"op":"ov_edit","layer":"area","map":0,"stroke":4,"cells":[[10,-10,74]]}"#
    );
}

#[test]
fn russian_labels_survive_as_utf8() {
    let env = EmeServiceEnvelope::from_json(&fixture("requests.json")[9].to_string()).unwrap();
    match &env.request {
        EmeServiceRequest::SnapshotCreate { label, radius } => {
            assert_eq!(label, "Перед карьером");
            assert_eq!(*radius, None);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        env.to_json().contains("Перед карьером"),
        "raw UTF-8, no \\u escapes"
    );
}

#[test]
fn malformed_requests_are_errors() {
    assert!(EmeServiceEnvelope::from_json(r#"{"v":1,"id":1,"op":"format_c"}"#).is_err());
    assert!(
        EmeServiceEnvelope::from_json(r#"{"v":1,"id":40000,"op":"caps"}"#).is_ok(),
        "u16"
    );
    assert!(EmeServiceEnvelope::from_json(r#"{"v":1,"id":70000,"op":"caps"}"#).is_err());
    let bad_kind = r#"{"v":1,"id":1,"op":"ov_edit","layer":"liquid","map":0,"stroke":1,"cells":[[1,1,9,1.0,0,0]]}"#;
    assert!(
        EmeServiceEnvelope::from_json(bad_kind).is_err(),
        "liquid kind 9"
    );
    let short =
        r#"{"v":1,"id":1,"op":"ov_edit","layer":"area","map":0,"stroke":1,"cells":[[1,1]]}"#;
    assert!(
        EmeServiceEnvelope::from_json(short).is_err(),
        "area cell needs 3 values"
    );
    let missing =
        r#"{"v":1,"id":1,"op":"obj_move","kind":"go","guid":1,"x":0,"y":0,"z":0,"o":0,"tx":1}"#;
    assert!(
        EmeServiceEnvelope::from_json(missing).is_err(),
        "obj_move: all fields mandatory"
    );
}

#[test]
fn every_reply_fixture_round_trips_exactly() {
    for frame in fixture("replies.json") {
        let text = frame.to_string();
        let EmeDownlink::Reply(r) =
            EmeDownlink::from_json(&text).unwrap_or_else(|e| panic!("{text}: {e}"))
        else {
            panic!("reply expected: {text}")
        };
        let back: Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(back, frame, "re-encode of {text}");
    }
}

#[test]
fn every_push_fixture_round_trips() {
    for frame in fixture("pushes.json") {
        let text = frame.to_string();
        let parsed = EmeDownlink::from_json(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert!(
            matches!(parsed, EmeDownlink::Push(_)),
            "push expected: {text}"
        );
        let back: Value = serde_json::from_str(&parsed.to_json()).unwrap();
        assert_matches(&back, &frame, &text);
        assert_eq!(back["id"], 0);
    }
}

fn reply(i: usize) -> EmeServiceReply {
    match EmeDownlink::from_json(&fixture("replies.json")[i].to_string()).unwrap() {
        EmeDownlink::Reply(r) => r,
        p => panic!("{p:?}"),
    }
}

#[test]
fn caps_reply_decodes_and_re_encodes() {
    let r = reply(0);
    let caps = EmeCaps::from_reply(&r).unwrap();
    assert_eq!((caps.rank, caps.wire), (EmeRank(3), 3));
    assert!(caps.offers_tool("water") && !caps.offers_tool("paint"));
    assert!(caps.has_perm("undo_own") && !caps.has_perm("revert"));
    assert!(!caps.enforced && !caps.is_enforced(EmeOverlayLayer::Liquid));
    let l = &caps.limits;
    assert_eq!(l.modes, vec![0, 1, 2, 3]);
    assert!(l.allows_mode(3) && !l.allows_mode(4));
    assert_eq!(l.radius, Some([0.5, 16.0]));
    assert_eq!(l.ov_cells_max, Some(32));
    assert_eq!(l.obj_scale, Some([0.05, 20.0]));
    assert_eq!(l.liquid_kinds, vec![0, 1]);
    assert!(
        l.other.is_empty(),
        "every contract limit is typed: {:?}",
        l.other
    );
    // Our own encoding of the caps matches the contract example.
    let mut want = serde_json::to_value(&r).unwrap();
    for k in ["v", "id", "ok"] {
        want.as_object_mut().unwrap().remove(k);
    }
    assert_matches(&serde_json::to_value(&caps).unwrap(), &want, "caps");
}

#[test]
fn result_replies_decode() {
    assert_eq!(reply(1).extra_as::<EmeViewResult>().unwrap().ttl, 30);

    let strokes: Vec<EmeStrokeRow> = reply(2).rows_as().unwrap();
    let s = &strokes[0];
    assert_eq!(
        (s.sid, s.stroke, s.layer, s.edits),
        (412, 3, EmeLayer::Voxel, 20)
    );
    assert_eq!(s.bbox, [-141, -2, -141, -2]);
    assert!(!s.undone && s.redo_ok);
    assert!(!reply(2).more());

    let undo: EmeUndoResult = reply(3).extra_as().unwrap();
    assert_eq!(
        (undo.sid, undo.reverted, undo.revert_id),
        (Some(412), Some(20), Some(57))
    );

    let warn = reply(4);
    assert_eq!(warn.error_code(), Some(EmeServiceError::OthersEdited));
    let others: EmeOthersEdited = warn.extra_as().unwrap();
    assert_eq!((others.others, others.count), (vec!["Geoff".to_owned()], 3));

    assert_eq!(
        reply(5).extra_as::<EmeUndoResult>().unwrap().restored,
        Some(20)
    );

    let log = reply(6);
    let page: EmeLogPage = log.extra_as().unwrap();
    assert_eq!((page.count, page.more), (128, true));
    let rows: Vec<EmeLogRow> = log.rows_as().unwrap();
    assert_eq!(
        (rows[0].layer, rows[0].mode, rows[0].stroke),
        (EmeLayer::Voxel, Some(3), Some(3))
    );

    let snap: EmeSnapshotCreated = reply(7).extra_as().unwrap();
    assert_eq!(snap.snapshot, 9);
    assert_eq!(
        snap.at_seq,
        EmeLayerSeqs {
            voxel: 1019,
            liquid: 14,
            area: 3
        }
    );
    let list: Vec<EmeSnapshotRow> = reply(8).rows_as().unwrap();
    assert_eq!((list[0].snapshot, list[0].radius), (9, 100.0));

    let rev: EmeRevertResult = reply(9).extra_as().unwrap();
    assert_eq!(
        rev,
        EmeRevertResult {
            voxel: 20,
            liquid: 4,
            area: 0,
            revert_id: 58
        }
    );

    let liq: Vec<EmeLiquidRow> = reply(10).rows_as().unwrap();
    assert_eq!(
        (liq[0].kind, liq[0].level, liq[0].fade),
        (EmeLiquidKind::Water, 12.5, 40)
    );
    let ov: EmeOvEditResult = reply(11).extra_as().unwrap();
    assert_eq!(
        (ov.accepted, ov.first_seq, ov.last_seq, ov.enforced),
        (2, 14, 15, false)
    );

    let bad = reply(12);
    assert_eq!(bad.error_code(), Some(EmeServiceError::Invalid));
    assert_eq!(bad.msg.as_deref(), Some("MONKEY_EME_ERR_INVALID"));
    assert_eq!(bad.detail.as_ref().unwrap()["index"], 1);
    assert_eq!(
        EmeServiceError::Invalid.locale_key(),
        "MONKEY_EME_ERR_INVALID"
    );

    let spawn: EmeObjectResult = reply(13).extra_as().unwrap();
    let row = spawn.row.unwrap();
    assert_eq!((row.guid, row.kind, row.o), (123, EmeObjectKind::Go, 1.57));
    assert_eq!(row.owner.unwrap().name, "Geoff");
    assert_eq!(spawn.tx, Some(EmeTxId(8812)));

    let undel: EmeObjectResult = reply(14).extra_as().unwrap();
    assert_eq!((undel.row.unwrap().guid, undel.replaces), (124, Some(123)));
    let unspawn: EmeObjectResult = reply(15).extra_as().unwrap();
    assert!(unspawn.row.is_none());

    assert_eq!(
        reply(16).error_code(),
        Some(EmeServiceError::Other("some_new_code".into()))
    );
    assert_eq!(
        reply(16).error_code().unwrap().locale_key(),
        "MONKEY_EME_ERR_OTHER"
    );

    let area: Vec<EmeAreaRow> = reply(17).rows_as().unwrap();
    assert_eq!((area[0].ax, area[0].ay, area[0].area_id), (10, -10, 74));
}

#[test]
fn every_contract_error_code_parses_and_prints_back() {
    for code in [
        "rank",
        "range",
        "rate",
        "invalid",
        "map",
        "stroke",
        "not_found",
        "conflict",
        "others_edited",
        "unsupported",
        "bad_area",
        "not_ready",
        "too_large",
        "unknown_op",
        "fief",
        "entry_unknown",
        "limit",
        "internal",
    ] {
        let e = EmeServiceError::parse(code);
        assert!(!matches!(e, EmeServiceError::Other(_)), "{code} untyped");
        assert_eq!(e.code(), code);
    }
}

#[test]
fn lua_encoder_quirks_are_accepted() {
    let frames = fixture("replies_lua.json");
    let parse = |i: usize| match EmeDownlink::from_json(&frames[i].to_string()).unwrap() {
        EmeDownlink::Reply(r) => r,
        p => panic!("{p:?}"),
    };
    // ok:1, {} for empty lists, empty limits.
    let r = parse(0);
    assert!(r.ok);
    let caps = EmeCaps::from_reply(&r).unwrap();
    assert_eq!(caps.rank, EmeRank::VIEW);
    assert!(caps.tools.is_empty() && caps.perms.is_empty() && caps.enforced_layers.is_empty());
    assert_eq!(caps.limits, EmeLimits::default());
    // rows:{} = no rows; it re-encodes as [].
    let r = parse(1);
    assert_eq!(r.rows, Some(vec![]));
    assert!(r.to_json().contains(r#""rows":[]"#));
    // ok:0.
    assert_eq!(parse(2).error_code(), Some(EmeServiceError::Rank));
    // enforced:true for one layer; unknown limit kept.
    let caps = EmeCaps::from_reply(&parse(3)).unwrap();
    assert!(caps.is_enforced(EmeOverlayLayer::Liquid) && !caps.is_enforced(EmeOverlayLayer::Area));
    assert_eq!(caps.limits.other.get("future_limit"), Some(&Value::from(7)));
    assert!(
        serde_json::to_string(&caps)
            .unwrap()
            .contains(r#""enforced":1"#),
        "re-encodes as int"
    );
    // undone:1, redo_ok:0, modes:{}.
    let rows: Vec<EmeStrokeRow> = parse(4).rows_as().unwrap();
    assert!(rows[0].undone && !rows[0].redo_ok && rows[0].modes.is_empty());
}

#[test]
fn a_non_empty_object_is_not_a_list() {
    let text = r#"{"v":1,"id":1,"ok":true,"rows":{"a":1}}"#;
    assert!(EmeDownlink::from_json(text).is_err());
}

#[test]
fn request_capabilities_match_the_rank_table() {
    use EmeServiceRequest as R;
    let view = Some(EmeCapability::View);
    let edit = Some(EmeCapability::Edit);
    assert_eq!(R::Caps.required_capability(), None);
    for r in [
        R::Log(EmeLogQuery::default()),
        R::SnapshotList {
            radius: None,
            limit: None,
        },
        R::ObjGet {
            kind: EmeObjectKind::Go,
            guid: 1,
        },
        R::Strokes {
            scope: None,
            radius: None,
            minutes: None,
            limit: None,
            before_sid: None,
            project: None,
        },
    ] {
        assert_eq!(r.required_capability(), view, "{}", r.op());
        assert!(!r.is_mutating());
    }
    for r in [
        R::Undo(EmeUndoArgs::default()),
        R::Redo(EmeUndoArgs::default()),
        R::SnapshotCreate {
            label: "x".into(),
            radius: None,
        },
        R::View {
            map: 0,
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        R::ObjUndo {
            tx: EmeTxId(1),
            actor: None,
        },
        R::UndoPlayer {
            name: "me".into(),
            minutes: None,
            radius: None,
        },
    ] {
        assert_eq!(r.required_capability(), edit, "{}", r.op());
    }
    assert!(!R::View {
        map: 0,
        x: 0.0,
        y: 0.0,
        z: 0.0
    }
    .is_mutating());
    assert!(R::ObjDelete {
        kind: EmeObjectKind::Go,
        guid: 1,
        tx: EmeTxId(1)
    }
    .is_mutating());
    assert_eq!(
        R::Revert { snapshot: 1 }.required_capability(),
        Some(EmeCapability::RevertOthers)
    );
}

#[test]
fn layer_wire_ids() {
    assert_eq!(
        [EmeLayer::Voxel, EmeLayer::Liquid, EmeLayer::Area].map(EmeLayer::wire_id),
        [0, 1, 2]
    );
    assert_eq!(EmeLayer::from(EmeOverlayLayer::Area), EmeLayer::Area);
}

//! The EME frame loop (DESIGN-EME §2.2 "command bus"):
//!
//! 1. voxel events → [`EmeWire::inbound`] (our edits confirmed / rejected by the server);
//! 2. `pump` the dispatcher (confirms, replies, pushes, link changes, timeouts); ask `caps` once
//!    the link is up — the rank in it is what lets F9 open EME at all;
//! 3. F9 / [`EmeToggleRequested`] → [`EmeActive`] (rank ≥ 2 only, else inert);
//! 4. active: build the [`EmeInputCtx`] (cursor hit, LMB, modifiers, wheel), feed the active tool,
//!    `dispatch` its commands; Ctrl+Z / Ctrl+Y and [`EmeRequest`]s;
//! 5. submit every sent edit through [`MonkeyVoxelApi::submit`] (local prediction + uplink);
//! 6. resolve the brush preview; re-emit core events as [`EmeEventMsg`];
//! 7. keep the server's free-camera range anchor (`view`) fresh while editing far from the body.

use bevy::prelude::*;

use crate::camera::EmeFreeCam;
use crate::viewport::{active_mode, resolve_preview, EmeAim, EmeBrushPreviewState, EmeCursor};
use crate::{
    now_ms, EmeActive, EmeEventMsg, EmeHost, EmeHostInput, EmePointerOverUi, EmeRequest, EmeSet,
    EmeToggleRequested, EmeUiWantsKeyboard, EmeWire,
};
use benilla_assets::coords::bevy_to_wow;
use benilla_world::voxel::{MonkeyVoxelApi, MonkeyVoxelEvent, MonkeyVoxelSubmit};
use eme_core::tool::{EmeButton, EmeHit, EmeModifiers};
use eme_core::{
    EmeCapability, EmeCommandSink, EmeInbound, EmeInputCtx, EmeRejectReason, EmeServiceRequest,
};

/// How often the `view` anchor is refreshed while editing (its server TTL is 30 s), ms.
const VIEW_REFRESH_MS: u64 = 10_000;
/// …or sooner once the camera moved this far from the last anchor, yards.
const VIEW_MOVE_YD: f32 = 40.0;
/// Never more often than this (server limit 2/s), ms.
const VIEW_MIN_GAP_MS: u64 = 1_000;

pub(crate) fn register(app: &mut App) {
    app.init_resource::<EmeViewAnchor>().add_systems(
        Update,
        (
            collect_voxel_events,
            pump,
            toggle,
            frame,
            crate::offline::serve,
        )
            .chain()
            .in_set(EmeSet::Frame),
    );
}

/// The last `view` anchor sent.
#[derive(Resource, Default)]
struct EmeViewAnchor {
    at: Option<[f32; 3]>,
    sent_ms: u64,
}

/// Our edits' fate, from the voxel layer, into the transport's inbound queue.
fn collect_voxel_events(
    mut events: MessageReader<MonkeyVoxelEvent>,
    mut wire: ResMut<EmeWire>,
    mut others: ResMut<eme_ui::EmeOthersHistory>,
    time: Res<Time>,
) {
    for ev in events.read() {
        match *ev {
            MonkeyVoxelEvent::Remote {
                actor,
                stroke,
                mode,
                pos,
                ..
            } => crate::ui::note_remote(&mut others, actor, stroke, mode, pos, now_ms(&time)),
            MonkeyVoxelEvent::Confirmed {
                nonce, seq, stroke, ..
            } => {
                if wire.awaiting.remove(&nonce) {
                    wire.inbound.push(EmeInbound::EditConfirmed { nonce, seq, stroke });
                }
            }
            MonkeyVoxelEvent::Dropped { nonce, reason, .. } => {
                if wire.awaiting.remove(&nonce) {
                    let reason = if reason == 0 {
                        EmeRejectReason::Other(0)
                    } else {
                        EmeRejectReason::from_code(reason)
                    };
                    wire.inbound.push(EmeInbound::EditRejected { nonce, reason });
                }
            }
            MonkeyVoxelEvent::RegionReset { .. } => {}
        }
    }
}

/// Pump the dispatcher; ask `caps` when the link comes up; close EME if the rank dropped.
fn pump(
    time: Res<Time>,
    mut host: ResMut<EmeHost>,
    mut wire: ResMut<EmeWire>,
    mut active: ResMut<EmeActive>,
    mut events: MessageWriter<EmeEventMsg>,
) {
    let now = now_ms(&time);
    let host = &mut *host;
    let out = host.dispatcher.pump(now, &mut *wire);
    if let Some(link) = out.link {
        if !link.is_usable() {
            host.caps_asked = false;
        }
        info!("EME: link {link:?}");
    }
    for r in &out.replies {
        // MONKEY (EME): seed the stroke ids past this character's newest server-side stroke, so a
        // relogged editor never reuses a closed id (the server refuses it: `R 6` / `err:"stroke"`).
        if r.op == "strokes" && r.tool.is_none() && host.strokes_seeded == 1 {
            host.strokes_seeded = 2;
            let newest = r
                .reply
                .extra
                .get("rows")
                .and_then(|v| v.as_array())
                .and_then(|rows| {
                    rows.iter()
                        .filter_map(|row| row.get("stroke")?.as_u64())
                        .filter(|&n| (1..=u64::from(u16::MAX)).contains(&n))
                        .map(|n| n as u16)
                        .max()
                });
            if let Some(last) = newest {
                host.stroke_ids.ensure_after(last);
            }
            info!(
                "EME: stroke ids seeded past {newest:?} (next {})",
                host.stroke_ids.peek()
            );
        } else if r.op != "caps" && !r.reply.ok {
            info!("EME: {} refused: {:?}", r.op, r.reply.err);
        }
        if r.op == "caps" {
            match host.dispatcher.caps() {
                Some(c) if r.reply.ok => info!(
                    "EME: caps rank {} wire {} tools {:?}",
                    c.rank.0, c.wire, c.tools
                ),
                _ => info!("EME: caps refused ({:?})", r.reply.err),
            }
        }
    }
    // MONKEY (EME projects): R 7 / no_project → prompt; a project closed / reverted elsewhere → say so.
    if out.no_project {
        host.prompt_project = true;
    }
    if let Some(p) = &out.project {
        info!("EME: active project {:?}", p.as_ref().map(|p| (p.id, p.name.as_str())));
    }
    for push in &out.pushes {
        if let eme_core::EmeServicePush::ProjectChanged { project, status, name } = push {
            info!("EME: project {project} is now {status}");
            host.projects_dirty = true;
            if status != "open" {
                let key = match status.as_str() {
                    "closed" => "MONKEY_EME_PROJECT_STATUS_CLOSED",
                    _ => "MONKEY_EME_PROJECT_STATUS_REVERTED",
                };
                host.notices.push((
                    "MONKEY_EME_NOTICE_PROJECT_CHANGED".into(),
                    vec![name.clone().unwrap_or_else(|| format!("#{project}")), key.into()],
                    true,
                ));
            }
        }
    }
    host.replies = out.replies;
    // MONKEY (EME, CONTRACT v3.5): ask nothing of `.mveme` before the server's `K` said rank ≥ 2
    // (a player's client would otherwise print "There is no such command" in chat).
    if wire.link.is_usable() && !host.caps_asked && wire.may_use_service() {
        host.caps_asked = host.dispatcher.request_caps(now, &mut *wire).is_ok();
        host.strokes_seeded = 0;
    }
    // MONKEY (EME): once an editing rank is known, ask for the newest own stroke (seeds the ids).
    if host.strokes_seeded == 0
        && !wire.offline
        && wire.may_use_service()
        && host.dispatcher.permissions().has(EmeCapability::Edit)
    {
        let req = EmeServiceRequest::Strokes {
            scope: Some(eme_core::service::EmeStrokeScope::Mine),
            radius: Some(500.0),
            minutes: Some(10_080),
            limit: Some(25),
            before_sid: None,
            project: None,
        };
        if host.dispatcher.request(now, &mut *wire, None, req).is_ok() {
            host.strokes_seeded = 1;
        }
    }
    if active.0 && !host.dispatcher.permissions().can_open() {
        active.0 = false;
        info!("EME: closed — the server's rank no longer allows it");
    }
    for e in host.dispatcher.drain_events() {
        if matches!(
            e,
            eme_core::EmeEvent::EntryDiscarded { .. } | eme_core::EmeEvent::RequestFailed { .. }
        ) {
            info!("EME: {e:?}");
        }
        events.write(EmeEventMsg(e));
    }
}

/// F9 and toggle requests → [`EmeActive`], for a caller whose rank may open EME; inert otherwise.
fn toggle(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    host_input: Res<EmeHostInput>,
    ui_keyboard: Res<EmeUiWantsKeyboard>,
    mut requests: MessageReader<EmeToggleRequested>,
    mut host: ResMut<EmeHost>,
    mut active: ResMut<EmeActive>,
    mut wire: ResMut<EmeWire>,
    time: Res<Time>,
) {
    let mut wanted: Option<bool> = None;
    for r in requests.read() {
        wanted = Some(r.to.unwrap_or(!wanted.unwrap_or(active.0)));
    }
    if keys.is_some_and(|k| k.just_pressed(KeyCode::F9)) && !host_input.typing && !ui_keyboard.0 {
        wanted = Some(!wanted.unwrap_or(active.0));
    }
    let Some(on) = wanted else { return };
    if on == active.0 {
        return;
    }
    let host = &mut *host;
    if on {
        // Inert for anyone the server has not granted rank ≥ 2 (or before caps arrived).
        if !host.dispatcher.permissions().can_open() {
            return;
        }
        active.0 = true;
        let perms = host.dispatcher.permissions();
        if host.registry.active_id().is_none() {
            let mut sink = EmeCommandSink::new();
            let first = host.registry.visible(&perms, host.dispatcher.caps()).first().copied();
            if let Some(id) = first {
                let _ = host
                    .registry
                    .activate(id.as_str(), &perms, host.dispatcher.caps(), &mut sink);
            }
        }
        info!(
            "EME: opened (rank {}, tool {:?})",
            perms.rank().0,
            host.registry.active_id()
        );
    } else {
        active.0 = false;
        // End any held stroke.
        let mut sink = EmeCommandSink::new();
        if let Some(t) = host.registry.active_mut() {
            t.on_deactivate(&mut sink);
        }
        host.errors = host.dispatcher.dispatch(now_ms(&time), &mut *wire, &mut sink);
        info!("EME: closed");
    }
}

/// One active frame: tool input → commands → dispatcher → voxel seam; undo / redo; preview.
#[allow(clippy::too_many_arguments)]
fn frame(
    time: Res<Time>,
    active: Res<EmeActive>,
    keys: Option<Res<ButtonInput<KeyCode>>>,
    buttons: Option<Res<ButtonInput<MouseButton>>>,
    scroll: Option<Res<bevy::input::mouse::AccumulatedMouseScroll>>,
    ui: (
        Res<EmeHostInput>,
        Res<EmePointerOverUi>,
        Res<EmeUiWantsKeyboard>,
        Res<EmeCursor>,
        Res<EmeAim>,
        Res<EmeFreeCam>,
    ),
    mut requests: MessageReader<EmeRequest>,
    mut host: ResMut<EmeHost>,
    mut wire: ResMut<EmeWire>,
    mut api: MonkeyVoxelApi,
    mut preview: ResMut<EmeBrushPreviewState>,
    mut anchor: ResMut<EmeViewAnchor>,
    mut prev_primary: Local<bool>,
) {
    let (host_input, over_ui, ui_keyboard, cursor, aim, free_cam) = ui;
    let now = now_ms(&time);
    let host = &mut *host;
    api.claim_input(active.0);
    if !active.0 {
        requests.clear();
        preview.visible = false;
        *prev_primary = false;
        submit_edits(&mut wire, &mut api);
        return;
    }
    let perms = host.dispatcher.permissions();
    let typing = host_input.typing || ui_keyboard.0;
    // A virtual (script) cursor is never over a panel: the shell follows the OS pointer.
    let pointer_over_ui = (over_ui.0 || host_input.pointer_over_ui) && cursor.virtual_norm.is_none();

    // Panel / script requests.
    for r in requests.read() {
        match r {
            EmeRequest::Undo => log_undo(host.dispatcher.undo(now, &mut *wire)),
            EmeRequest::Redo => {
                if let Err(e) = host.dispatcher.redo(now, &mut *wire) {
                    info!("EME: redo: {e:?}");
                }
            }
            EmeRequest::ConfirmUndo(id) => {
                log_undo(host.dispatcher.confirm_undo(now, &mut *wire, *id));
            }
            EmeRequest::CancelConfirm(id) => {
                if let Err(e) = host.dispatcher.undo_stack_mut().cancel_confirm(*id) {
                    info!("EME: cancel confirm: {e:?}");
                }
            }
            EmeRequest::Service(req) => {
                if let Err(e) = host.dispatcher.request(now, &mut *wire, None, req.clone()) {
                    info!("EME: {} not sent: {e:?}", req.op());
                    host.notices
                        .push(("MONKEY_EME_NOTICE_NOT_SENT".into(), Vec::new(), true));
                }
            }
            EmeRequest::SelectTool(id) => {
                let mut sink = EmeCommandSink::new();
                if let Err(e) =
                    host.registry
                        .activate(id, &perms, host.dispatcher.caps(), &mut sink)
                {
                    info!("EME: tool {id}: {e}");
                }
                host.errors = host.dispatcher.dispatch(now, &mut *wire, &mut sink);
            }
            EmeRequest::SetOption { key, value } => {
                if let Some(t) = host.registry.active_mut() {
                    t.set_option(key, value.clone());
                }
            }
        }
    }

    // Hotkeys.
    if let (Some(k), false) = (&keys, typing) {
        let ctrl = k.pressed(KeyCode::ControlLeft) || k.pressed(KeyCode::ControlRight);
        let shift = k.pressed(KeyCode::ShiftLeft) || k.pressed(KeyCode::ShiftRight);
        if ctrl && k.just_pressed(KeyCode::KeyZ) {
            if shift {
                let _ = host.dispatcher.redo(now, &mut *wire);
            } else {
                log_undo(host.dispatcher.undo(now, &mut *wire));
            }
        } else if ctrl && k.just_pressed(KeyCode::KeyY) {
            let _ = host.dispatcher.redo(now, &mut *wire);
        }
    }

    // The input snapshot.
    let held = buttons.as_ref().is_some_and(|b| b.pressed(MouseButton::Left)) || cursor.virtual_primary;
    let primary = EmeButton {
        pressed: held && !*prev_primary,
        held,
        released: !held && *prev_primary,
    };
    *prev_primary = held;
    let modifiers = keys.as_ref().map_or(EmeModifiers::default(), |k| EmeModifiers {
        shift: k.pressed(KeyCode::ShiftLeft) || k.pressed(KeyCode::ShiftRight),
        ctrl: k.pressed(KeyCode::ControlLeft) || k.pressed(KeyCode::ControlRight),
        alt: k.pressed(KeyCode::AltLeft) || k.pressed(KeyCode::AltRight),
    });
    let wheel = scroll.map_or(0.0, |s| match s.unit {
        bevy::input::mouse::MouseScrollUnit::Line => s.delta.y,
        bevy::input::mouse::MouseScrollUnit::Pixel => s.delta.y / 100.0,
    });
    let map = api.map().unwrap_or(0);
    let ctx = EmeInputCtx {
        now_ms: now,
        map,
        hit: aim.hit.map(|h| EmeHit {
            pos: h.pos,
            normal: h.normal,
            object: None,
        }),
        primary,
        secondary: EmeButton {
            pressed: buttons.as_ref().is_some_and(|b| b.just_pressed(MouseButton::Right)),
            held: buttons.as_ref().is_some_and(|b| b.pressed(MouseButton::Right)),
            released: buttons.as_ref().is_some_and(|b| b.just_released(MouseButton::Right)),
        },
        modifiers,
        wheel,
        pointer_over_ui,
        perms,
    };
    let mut sink = EmeCommandSink::new();
    host.registry.feed(&ctx, host.dispatcher.caps(), &mut sink);
    if !sink.is_empty() {
        host.errors = host.dispatcher.dispatch(now, &mut *wire, &mut sink);
        for e in &host.errors {
            info!("EME: command refused: {e:?}");
        }
    }
    submit_edits(&mut wire, &mut api);

    // Preview.
    let tool_preview = host
        .registry
        .active_id()
        .and_then(|id| host.registry.get(id.as_str()))
        .and_then(|t| t.preview(&ctx));
    resolve_preview(
        tool_preview,
        active_mode(host),
        aim.hit.as_ref(),
        &api,
        &mut preview,
    );

    // The free camera's range anchor (CONTRACT §9.5): only with a server and edit rights.
    if perms.has(EmeCapability::Edit) && wire.link.is_usable() && !wire.offline {
        let eye = bevy_to_wow(free_cam.pos);
        let moved = anchor.at.is_none_or(|a| {
            ((a[0] - eye[0]).powi(2) + (a[1] - eye[1]).powi(2) + (a[2] - eye[2]).powi(2)).sqrt()
                > VIEW_MOVE_YD
        });
        let age = now.saturating_sub(anchor.sent_ms);
        if (moved || age >= VIEW_REFRESH_MS) && age >= VIEW_MIN_GAP_MS && free_cam.seeded {
            let req = EmeServiceRequest::View {
                map,
                x: eye[0],
                y: eye[1],
                z: eye[2],
            };
            if host.dispatcher.request(now, &mut *wire, None, req).is_ok() {
                anchor.at = Some(eye);
                anchor.sent_ms = now;
            }
        }
    }
}

/// Submit the edits the dispatcher sent this frame as local predictions; one the layer cannot
/// take (no map yet, invalid) is rejected back to the undo stack at once.
fn submit_edits(wire: &mut EmeWire, api: &mut MonkeyVoxelApi) {
    for e in std::mem::take(&mut wire.edits_out) {
        let s = MonkeyVoxelSubmit {
            pos: e.pos,
            radius: e.radius,
            mode: e.mode,
            strength: e.strength,
            stroke: e.stroke,
            param: e.param,
            nonce: Some(e.nonce),
        };
        match api.submit(s) {
            Some(n) => {
                wire.awaiting.insert(n);
                if wire.offline {
                    crate::offline::note_submitted(wire, e);
                }
            }
            None => wire.inbound.push(EmeInbound::EditRejected {
                nonce: e.nonce,
                reason: EmeRejectReason::Invalid,
            }),
        }
    }
}

fn log_undo<T: std::fmt::Debug, E: std::fmt::Debug>(r: Result<T, E>) {
    match r {
        Ok(v) => info!("EME: undo: {v:?}"),
        Err(e) => info!("EME: undo: {e:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::{EmeLink, EmeServiceReply};

    /// An app with just the toggle system, a link that is up and a `caps` reply of `rank`
    /// (`None` = the reply never arrives).
    fn app_with_rank(rank: Option<u8>) -> App {
        let mut app = App::new();
        app.init_resource::<Time>()
            .init_resource::<EmeHost>()
            .init_resource::<EmeActive>()
            .init_resource::<EmeWire>()
            .init_resource::<EmeHostInput>()
            .init_resource::<EmeUiWantsKeyboard>()
            .init_resource::<ButtonInput<KeyCode>>()
            .add_message::<EmeToggleRequested>()
            .add_systems(Update, toggle);
        if let Some(rank) = rank {
            let mut wire = EmeWire::default();
            wire.set_link(EmeLink::Up { server_version: 3 });
            let mut host = EmeHost::default();
            host.dispatcher.request_caps(0, &mut wire).unwrap();
            let id = wire.take_services()[0].id;
            wire.inbound.push(EmeInbound::Reply(
                EmeServiceReply::ok(id)
                    .with("rank", rank)
                    .with("wire", 3)
                    .with("tools", ["sculpt", "flatten"]),
            ));
            host.dispatcher.pump(0, &mut wire);
            app.insert_resource(wire).insert_resource(host);
        }
        app
    }

    fn press_f9(app: &mut App) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::F9);
        app.update();
    }

    /// F9 does nothing for anyone below rank 2 (and before the server has answered `caps`).
    #[test]
    fn f9_is_inert_below_rank_two_and_opens_from_rank_two() {
        for (rank, opens) in [(None, false), (Some(0), false), (Some(1), false), (Some(2), true), (Some(3), true), (Some(4), true)] {
            let mut app = app_with_rank(rank);
            press_f9(&mut app);
            assert_eq!(app.world().resource::<EmeActive>().0, opens, "rank {rank:?}");
            // A toggle request (the host's own path, a script's `open`) obeys the same gate.
            let mut app = app_with_rank(rank);
            app.world_mut().write_message(EmeToggleRequested { to: Some(true) });
            app.update();
            assert_eq!(app.world().resource::<EmeActive>().0, opens, "request, rank {rank:?}");
        }
    }
}

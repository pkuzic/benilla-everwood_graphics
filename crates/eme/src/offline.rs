//! The offline emulated server: with no server (a capture run, `WOW_EME_SCRIPT`, or
//! `WOW_EME_OFFLINE=1`) EME still runs its whole client path — tool → stroke builder → dispatcher
//! → undo stack → voxel seam — against this stand-in, which plays the server's part LOCALLY:
//!
//! - the link is `Up { 3 }` and `caps` grants rank 3 (sculpt + read-only tools);
//! - every submitted edit is "confirmed" with a local seq (it stays a local prediction in the
//!   voxel journal; nothing is uplinked — the relay drops the outbox while out of the world);
//! - `undo` drops the stroke's predictions from the voxel layer
//!   ([`MonkeyVoxelApi::drop_stroke_local`], the chunks rebuild without them) and keeps them for
//!   `redo`, which resubmits them. With a server, undo is a server revert + `X` resync instead.
//!
//! The reply shapes are CONTRACT v3 §12.4's, so the undo stack runs its real state machine.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::EmeWire;
use benilla_world::voxel::{MonkeyVoxelApi, MonkeyVoxelSubmit, VoxelEditOp};
use eme_core::service::EmeUndoArgs;
use eme_core::{EmeInbound, EmeLink, EmeServiceReply, EmeServiceRequest, EmeVoxelEdit};

/// The emulator's state.
#[derive(Resource, Debug, Default)]
pub struct EmeOfflineServer {
    seq: u32,
    revert_id: u32,
    /// Undone strokes' edits, for redo.
    undone: HashMap<u16, Vec<VoxelEditOp>>,
    /// Edits of each live stroke (count, for the reply).
    live: HashMap<u16, u32>,
    /// Edits submitted this frame, to confirm.
    sent: Vec<EmeVoxelEdit>,
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<EmeOfflineServer>();
    if std::env::var("WOW_EME_OFFLINE").is_ok_and(|v| v.trim() == "1") {
        app.world_mut().resource_mut::<EmeWire>().offline = true;
    }
}

/// An edit the dispatch system just submitted (offline only).
pub(crate) fn note_submitted(wire: &mut EmeWire, e: EmeVoxelEdit) {
    wire.awaiting.remove(&e.nonce);
    wire.offline_sent.push(e);
}

/// Serve one frame: link, confirmations, service requests.
pub(crate) fn serve(
    mut wire: ResMut<EmeWire>,
    mut server: ResMut<EmeOfflineServer>,
    mut api: MonkeyVoxelApi,
) {
    if !wire.offline {
        return;
    }
    let server = &mut *server;
    wire.set_link(EmeLink::Up { server_version: 3 });
    server.sent.append(&mut wire.offline_sent);
    for e in std::mem::take(&mut server.sent) {
        server.seq += 1;
        *server.live.entry(e.stroke).or_default() += 1;
        wire.inbound.push(EmeInbound::EditConfirmed {
            nonce: e.nonce,
            seq: server.seq,
            stroke: e.stroke,
        });
    }
    let map = api.map().unwrap_or(0);
    for s in wire.take_services() {
        let reply = answer(server, &mut api, map, s.id, &s.request);
        wire.inbound.push(EmeInbound::Reply(reply));
    }
}

fn stroke_of(a: &EmeUndoArgs) -> Option<u16> {
    a.stroke.or(a.sid.map(|s| s as u16))
}

fn answer(
    server: &mut EmeOfflineServer,
    api: &mut MonkeyVoxelApi,
    map: u16,
    id: eme_core::EmeRequestId,
    req: &EmeServiceRequest,
) -> EmeServiceReply {
    use EmeServiceRequest as R;
    match req {
        R::Caps => EmeServiceReply::ok(id)
            .with("rank", 3)
            .with("wire", 3)
            .with(
                "tools",
                ["sculpt", "flatten", "smooth", "history", "restore", "log"],
            )
            .with("perms", ["edit", "undo_own", "snapshot_create"])
            .with("enforced", 0)
            .with(
                "limits",
                serde_json::json!({"edit_range":120,"fanout":300,"region":64,"modes":[0,1,2,3],
                    "radius":[0.5,16],"frame_max":2800,"log_rows_max":15}),
            ),
        R::View { .. } => EmeServiceReply::ok(id).with("ttl", 30),
        R::Undo(a) => {
            let Some(stroke) = stroke_of(a) else {
                return EmeServiceReply::error(id, "invalid");
            };
            let dropped = api.drop_stroke_local(map, stroke);
            if dropped.is_empty() {
                return EmeServiceReply::error(id, "not_found");
            }
            let n = dropped.len() as u32;
            server.live.remove(&stroke);
            server.undone.insert(stroke, dropped);
            server.revert_id += 1;
            info!("EME offline: undo stroke {stroke} — {n} edit(s) dropped locally");
            EmeServiceReply::ok(id)
                .with("sid", u32::from(stroke))
                .with("layer", "voxel")
                .with("reverted", n)
                .with("revert_id", server.revert_id)
        }
        R::Redo(a) => {
            let Some(stroke) = stroke_of(a) else {
                return EmeServiceReply::error(id, "invalid");
            };
            let Some(edits) = server.undone.remove(&stroke) else {
                return EmeServiceReply::error(id, "conflict");
            };
            let n = edits.len() as u32;
            for e in edits {
                // Fresh nonces: these are not EME's tracked edits any more.
                api.submit(MonkeyVoxelSubmit {
                    pos: e.pos,
                    radius: e.radius,
                    mode: e.mode,
                    strength: e.strength,
                    stroke: e.stroke,
                    param: e.param,
                    nonce: None,
                });
            }
            server.live.insert(stroke, n);
            info!("EME offline: redo stroke {stroke} — {n} edit(s) resubmitted");
            EmeServiceReply::ok(id)
                .with("sid", u32::from(stroke))
                .with("restored", n)
        }
        R::Strokes { .. } | R::Log(_) | R::SnapshotList { .. } => {
            let mut r = EmeServiceReply::ok(id).with("more", false);
            r.rows = Some(Vec::new());
            r
        }
        _ => EmeServiceReply::error(id, "unsupported"),
    }
}

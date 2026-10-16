//! MONKEY (Voxel): P1b multiplayer terrain-edit relay over **guild addon messages**.
//!
//! Every edit a player makes with the local brush is pushed to
//! [`benilla_world::voxel::MonkeyVoxelNet`]'s `outbox` by the world side; a world system drains the
//! `inbox` and re-applies the edits. This module is the transport in between, and needs **no server
//! change**: we send each edit as a **guild** addon message (`MV:<hex>`), and the server relays
//! guild addon traffic to every guild member verbatim (the same mechanism the client's other addons
//! use — a raw custom WoW channel was NOT relayed on this server). So a guild's terraforming syncs.
//!
//! The whole module is gated behind the crate's `voxel` cargo feature (OFF by default), so the
//! shipped player build is byte-unchanged.

use bevy::prelude::*;

use benilla_protocol::{SessionEvent, SessionEventKind};
use benilla_ui::script::AddonDistribution;
use benilla_world::voxel::{MonkeyVoxelEdit, MonkeyVoxelNet};

use crate::net::{ClientCommand, NetCommands, NetHandlerApp, SelfGuid};

/// The tag marking a voxel-edit addon line (`MV:<34 hex>`); any other addon line is ignored. Also
/// used by the chat UI to keep these lines out of the visible chat log.
pub(crate) const MONKEY_VOXEL_PREFIX: &str = "MV:";

/// Wire the relay: the inbound-chat interceptor plus the outbox-drain sender.
pub(crate) fn register(app: &mut App) {
    app.net_handler(SessionEventKind::Chat, monkey_voxel_recv)
        .add_systems(Update, monkey_voxel_drain_outbox);
}

/// Drain the brush's outbox: each local edit goes out as one `MV:<hex>` **guild** addon message.
/// The server relays guild addon traffic to all guild members, so they see the deformation.
fn monkey_voxel_drain_outbox(
    mut net: ResMut<MonkeyVoxelNet>,
    self_guid: Res<SelfGuid>,
    commands: Res<NetCommands>,
) {
    if net.outbox.is_empty() {
        return;
    }
    // Only in-world (a writer exists); drop anything staged while out of the world so a stale edit
    // can't leak into the next session.
    if self_guid.0.is_none() {
        net.outbox.clear();
        return;
    }
    for edit in net.outbox.drain(..).collect::<Vec<_>>() {
        let _ = commands.0.send(ClientCommand::AddonMessage {
            distribution: AddonDistribution::Guild,
            text: format!("{MONKEY_VOXEL_PREFIX}{}", edit.to_hex()),
        });
    }
}

/// Receive side: an addon line tagged `MV:<hex>` from **another** player decodes into the inbox,
/// which the world's apply system drains and re-meshes. Our own echo (same sender guid) is skipped
/// so the edit the local brush already made isn't applied twice.
fn monkey_voxel_recv(
    In(ev): In<SessionEvent>,
    mut net: ResMut<MonkeyVoxelNet>,
    self_guid: Res<SelfGuid>,
) {
    let SessionEvent::Chat(m) = ev else {
        return;
    };
    let Some(hex) = m.text.strip_prefix(MONKEY_VOXEL_PREFIX) else {
        return;
    };
    // Skip our own echo (the server relays our guild message back to us too).
    if self_guid.0 == Some(m.sender_guid) {
        return;
    }
    match MonkeyVoxelEdit::from_hex(hex) {
        Some(edit) => {
            info!("MONKEY (voxel): received a remote terrain edit");
            net.inbox.push(edit);
        }
        None => warn!(
            "MONKEY (voxel): dropping a malformed relay payload {:?}",
            m.text
        ),
    }
}

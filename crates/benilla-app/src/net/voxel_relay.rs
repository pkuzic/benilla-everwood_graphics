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
use benilla_world::voxel::{MonkeyVoxelEdit, MonkeyVoxelNet, MonkeyVoxelSpawn};

use crate::net::{ClientCommand, NetCommands, NetHandlerApp, SelfGuid};

/// The tag marking a voxel-EDIT addon line (`MV:<34 hex>`). Also used by the chat UI to keep these
/// lines out of the visible chat log.
pub(crate) const MONKEY_VOXEL_PREFIX: &str = "MV:";
/// The tag marking a shared-build-SITE placement line (`VS:<38 hex>`).
pub(crate) const MONKEY_VOXEL_SPAWN_PREFIX: &str = "VS:";

/// Wire the relay: the inbound-chat interceptor plus the outbox-drain sender.
pub(crate) fn register(app: &mut App) {
    app.net_handler(SessionEventKind::Chat, monkey_voxel_recv)
        .add_systems(Update, monkey_voxel_drain_outbox);
}

/// Drain the brush's outboxes: each local edit goes out as a `MV:<hex>` and each shared-site
/// placement as a `VS:<hex>` **guild** addon message. The server relays guild addon traffic to all
/// guild members, so they see the deformation / adopt the build site.
fn monkey_voxel_drain_outbox(
    mut net: ResMut<MonkeyVoxelNet>,
    self_guid: Res<SelfGuid>,
    commands: Res<NetCommands>,
) {
    if net.outbox.is_empty() && net.spawn_outbox.is_empty() {
        return;
    }
    // Only in-world (a writer exists); drop anything staged while out of the world so a stale edit
    // can't leak into the next session.
    if self_guid.0.is_none() {
        net.outbox.clear();
        net.spawn_outbox.clear();
        return;
    }
    for spawn in net.spawn_outbox.drain(..).collect::<Vec<_>>() {
        let _ = commands.0.send(ClientCommand::AddonMessage {
            distribution: AddonDistribution::Guild,
            text: format!("{MONKEY_VOXEL_SPAWN_PREFIX}{}", spawn.to_hex()),
        });
    }
    for edit in net.outbox.drain(..).collect::<Vec<_>>() {
        let _ = commands.0.send(ClientCommand::AddonMessage {
            distribution: AddonDistribution::Guild,
            text: format!("{MONKEY_VOXEL_PREFIX}{}", edit.to_hex()),
        });
    }
}

/// Receive side: an addon line tagged `MV:<hex>` (an edit) or `VS:<hex>` (a shared build-site
/// placement) from **another** player decodes into the matching inbox, which the world's apply
/// systems drain and re-mesh. Our own echo (same sender guid) is skipped so what we did locally
/// isn't applied twice.
fn monkey_voxel_recv(
    In(ev): In<SessionEvent>,
    mut net: ResMut<MonkeyVoxelNet>,
    self_guid: Res<SelfGuid>,
) {
    let SessionEvent::Chat(m) = ev else {
        return;
    };
    // Classify by tag first, so a non-voxel addon line is ignored cheaply.
    let edit_hex = m.text.strip_prefix(MONKEY_VOXEL_PREFIX);
    let spawn_hex = m.text.strip_prefix(MONKEY_VOXEL_SPAWN_PREFIX);
    if edit_hex.is_none() && spawn_hex.is_none() {
        return;
    }
    // Skip our own echo (the server relays our guild message back to us too).
    if self_guid.0 == Some(m.sender_guid) {
        return;
    }
    if let Some(hex) = spawn_hex {
        match MonkeyVoxelSpawn::from_hex(hex) {
            Some(spawn) => {
                info!("MONKEY (voxel): received a shared build-site placement");
                net.spawn_inbox.push(spawn);
            }
            None => warn!("MONKEY (voxel): dropping a malformed site payload {:?}", m.text),
        }
        return;
    }
    if let Some(hex) = edit_hex {
        match MonkeyVoxelEdit::from_hex(hex) {
            Some(edit) => {
                info!("MONKEY (voxel): received a remote terrain edit");
                net.inbox.push(edit);
            }
            None => warn!("MONKEY (voxel): dropping a malformed relay payload {:?}", m.text),
        }
    }
}

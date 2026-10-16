//! MONKEY (Voxel): P1b multiplayer terrain-edit relay over a hidden WoW chat channel.
//!
//! Every edit a player makes with the local brush is pushed to
//! [`benilla_world::voxel::MonkeyVoxelNet`]'s `outbox` by the world side; a world system drains
//! the `inbox` and re-applies the edits. This module is the transport in between, and needs **no
//! server change**: all clients join one hidden channel (`MonkeyVoxel`), and the server relays
//! every channel line to its members without parsing the payload. So we send each outbound edit
//! as a `MV:<hex>` channel line, and decode inbound `MV:` lines back into the inbox.
//!
//! The whole module is gated behind the crate's `voxel` cargo feature (OFF by default), so the
//! shipped player build is byte-unchanged. It reuses the existing outbound path
//! ([`crate::net::NetCommands`] → [`crate::net::ClientCommand`] → the write thread); a
//! `ChatKind::Channel` line with language 0 and the channel as its target is wire-identical to
//! `benilla_protocol::messages::client::messagechat_channel(0, "MonkeyVoxel", payload)`.

use bevy::prelude::*;

use benilla_protocol::{SessionEvent, SessionEventKind};
use benilla_world::voxel::{MonkeyVoxelEdit, MonkeyVoxelNet};

use crate::net::{ChatKind, ClientCommand, NetCommands, NetHandlerApp, SelfGuid};

/// The hidden channel every client joins to exchange voxel edits. The server relays its lines to
/// all members verbatim, so terrain edits sync with no server-side understanding of the payload.
pub(crate) const MONKEY_VOXEL_CHANNEL: &str = "MonkeyVoxel";

/// The tag marking a voxel-edit line (`MV:<34 hex>`); any other line on the channel is ignored.
const MONKEY_VOXEL_PREFIX: &str = "MV:";

/// Wire the relay: the inbound-chat interceptor, plus the join-once and outbox-drain systems.
pub(crate) fn register(app: &mut App) {
    app.net_handler(SessionEventKind::Chat, monkey_voxel_recv)
        .add_systems(
            Update,
            (monkey_voxel_join_channel, monkey_voxel_drain_outbox),
        );
}

/// Join `MonkeyVoxel` once per world entry. The `Local` latch clears when we leave the world
/// ([`SelfGuid`] back to `None`), so a re-login or character switch re-joins.
fn monkey_voxel_join_channel(
    mut joined: Local<bool>,
    self_guid: Res<SelfGuid>,
    commands: Res<NetCommands>,
) {
    if self_guid.0.is_none() {
        *joined = false; // out of the world — re-join on the next entry
        return;
    }
    if *joined {
        return;
    }
    *joined = true;
    let _ = commands.0.send(ClientCommand::JoinChannel {
        name: MONKEY_VOXEL_CHANNEL.to_string(),
        password: String::new(),
    });
    info!("MONKEY (voxel): joined the {MONKEY_VOXEL_CHANNEL:?} relay channel");
}

/// Drain the brush's outbox: each local edit goes out as one `MV:<hex>` line on the channel.
fn monkey_voxel_drain_outbox(
    mut net: ResMut<MonkeyVoxelNet>,
    self_guid: Res<SelfGuid>,
    commands: Res<NetCommands>,
) {
    if net.outbox.is_empty() {
        return;
    }
    // Only in-world, where a writer exists and we are on the channel; drop anything staged while
    // out of the world so a stale edit can't leak into the next session.
    if self_guid.0.is_none() {
        net.outbox.clear();
        return;
    }
    for edit in net.outbox.drain(..).collect::<Vec<_>>() {
        let _ = commands.0.send(ClientCommand::Chat {
            kind: ChatKind::Channel,
            target: Some(MONKEY_VOXEL_CHANNEL.to_string()),
            text: format!("{MONKEY_VOXEL_PREFIX}{}", edit.to_hex()),
            // Universal language id 0, so the server relays the bytes verbatim: wire-identical to
            // `messagechat_channel(0, "MonkeyVoxel", payload)`.
            language: Some(0),
        });
    }
}

/// Receive side: intercept inbound chat. A `MonkeyVoxel` line that is our **own** echo is skipped
/// (the server echoes a channel message back to its sender, so applying it would double-apply the
/// edit the local brush already made); any other member's `MV:<hex>` decodes to the inbox, which
/// the world's apply system drains and re-meshes.
fn monkey_voxel_recv(
    In(ev): In<SessionEvent>,
    mut net: ResMut<MonkeyVoxelNet>,
    self_guid: Res<SelfGuid>,
) {
    let SessionEvent::Chat(m) = ev else {
        return;
    };
    if m.channel.as_deref() != Some(MONKEY_VOXEL_CHANNEL) {
        return;
    }
    // Skip our own echo by sender guid (the reliable form of "sender == our character"): a channel
    // line carries the sender's player guid, and ours is live in `SelfGuid` the whole session.
    if self_guid.0 == Some(m.sender_guid) {
        return;
    }
    let Some(hex) = m.text.strip_prefix(MONKEY_VOXEL_PREFIX) else {
        return;
    };
    match MonkeyVoxelEdit::from_hex(hex) {
        Some(edit) => net.inbox.push(edit),
        None => warn!(
            "MONKEY (voxel): dropping a malformed relay payload {:?}",
            m.text
        ),
    }
}

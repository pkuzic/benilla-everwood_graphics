//! MONKEY (Voxel): multiplayer terrain-edit relay over **guild addon messages** — the v2 fallback
//! transport (CONTRACT §4: "no HELLO-ACK → fall back to the guild relay, prefix `MV2:` + the EDIT
//! body, no seq, best effort").
//!
//! Every edit the local brush makes is pushed to [`benilla_world::voxel::MonkeyVoxelNet`]'s
//! `outbox` by the world side; this module sends each as a **guild** addon message
//! `MV2:<46 hex>` (the 23-byte v2 EDIT body: `op 0x01, nonce u16, map u16, x f32, y f32, z f32,
//! radius f32, mode u8, strength u8`, little-endian) and decodes the guild members' lines into the
//! `inbox`, which the world applies through the canonical path (no seq → appended in arrival
//! order). The server relays guild addon traffic verbatim, so this needs no server change.
//!
//! The server-authority lane (`.mvox` + `MONKEYVOXEL` whispers) will sit beside this and drive the
//! same mailbox (`inbox` with seq, `confirms`, `resets`, `region_done`).
//!
//! The whole module is gated behind the crate's `voxel` cargo feature (OFF by default).

use bevy::prelude::*;

use benilla_protocol::{SessionEvent, SessionEventKind};
use benilla_ui::script::AddonDistribution;
use benilla_world::voxel::{MonkeyLocalGm, MonkeyVoxelEdit, MonkeyVoxelNet};

use crate::net::{ClientCommand, NetCommands, NetHandlerApp, ObjectStore, SelfGuid, SelfPlayer};

/// `PLAYER_FLAGS & 0x8` — GM mode on (`.gm on`); same bit chat's `is_gm` reads.
const PLAYER_FLAGS_GM: u32 = 0x8;

/// The tag marking a v2 voxel-EDIT addon line (`MV2:<46 hex>`). Addon traffic, so the chat UI
/// keeps it out of the visible log.
pub(crate) const MONKEY_VOXEL_PREFIX: &str = "MV2:";

/// Wire the relay: the inbound-chat interceptor, the outbox-drain sender, and the GM-status bridge.
pub(crate) fn register(app: &mut App) {
    app.net_handler(SessionEventKind::Chat, monkey_voxel_recv)
        .add_systems(Update, (monkey_voxel_drain_outbox, monkey_voxel_push_gm));
}

/// Bridge the self player's GM flag into benilla-world's `MonkeyLocalGm` each frame (benilla-world
/// can't read the network descriptor), the `FfxDeathFade` pattern. Gates the GM-only brush.
fn monkey_voxel_push_gm(
    self_q: Query<&ObjectStore, With<SelfPlayer>>,
    mut gm: ResMut<MonkeyLocalGm>,
) {
    gm.0 = self_q
        .single()
        .map(|s| s.0.player_flags() & PLAYER_FLAGS_GM != 0)
        .unwrap_or(false);
}

/// Drain the brush's outbox: each local edit goes out as a `MV2:<hex>` **guild** addon message.
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

/// Receive side: an addon line tagged `MV2:<hex>` from **another** player decodes into the inbox.
/// Our own echo (same sender guid) is skipped so what we did locally isn't applied twice. A
/// malformed / invalid body (bad length, NaN, radius out of range, unknown mode) is dropped.
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
    if self_guid.0 == Some(m.sender_guid) {
        return;
    }
    match MonkeyVoxelEdit::from_hex(hex) {
        Some(edit) => net.inbox.push(edit),
        None => warn!("MONKEY (voxel): dropping a malformed MV2 payload {:?}", m.text),
    }
}

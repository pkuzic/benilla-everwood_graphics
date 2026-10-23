//! MONKEY (Voxel): the multiplayer terrain-edit network link (CONTRACT §3–4,
//! `disposable/voxel-v2/CONTRACT.md`).
//!
//! **SERVER mode** (the authority script `MonkeyVoxelServer.lua` answers): on entering the world
//! and on every map change the client sends HELLO `.mvox 1003` (offering v3, CONTRACT §7); an `H`
//! within 10 s selects it, its version byte is the NEGOTIATED version (3, or 2 from a v2 server).
//! - uplink: the brush's [`MonkeyVoxelNet`] `outbox` → `.mvox 02…` (v3: 27-byte EDIT3 with stroke +
//!   param) or `.mvox 01…` (v2: 23-byte EDIT; modes ≥ 3 cannot go there and are dropped locally);
//!   the server consumes `.mvox` silently and nothing local echoes a sent chat command, so these
//!   never show in the chat frame.
//! - downlink: `MONKEYVOXEL<TAB><hex>` addon whispers (sender == receiver; several fixed-size
//!   records per message), decoded by [`benilla_world::voxel::wire`]. Addon traffic never renders
//!   in the chat frame (`ui_chat::net`'s `is_addon` branch). `E` / v3 `e` → a confirm of OUR
//!   prediction (nonce AND body equal; for `e` also stroke, param and actor = our low GUID) or a
//!   remote edit (deduped by seq in the journal); `D` → the region's
//!   sync point (`more` → ask again); `X` → reset the region + re-SYNC from 0; `R` → drop the
//!   prediction (logged with the reason). All go into `MonkeyVoxelNet::events` in ARRIVAL order.
//! - SYNC: the regions (64 yd) whose centre is within 200 yd of the player and not synced this
//!   session, ≤ 12 entries per command, ≤ 1 command per 250 ms, after_seq = the persisted sync
//!   point (journal), so a relog only fetches newer edits. A region whose centre drifts beyond
//!   230 yd is forgotten (re-SYNCed on return: outside the server's 300-yd fan-out it can miss
//!   live edits). A live `E` in a synced region moves its sync point (contiguous while in range).
//!
//! **RELAY mode** (no `H` in 10 s — a server without the script): the v1-style guild relay, each
//! local edit as a guild addon line `MV2:<46 hex>`, decoded from the guild members into the
//! `inbox` (no seq, best effort).
//!
//! The whole module is gated behind the crate's `voxel` cargo feature (OFF by default).

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::prelude::*;

use benilla_assets::coords::bevy_to_wow;
use benilla_protocol::{SessionEvent, SessionEventKind};
use benilla_ui::script::AddonDistribution;
use benilla_world::voxel::wire::{self, ServerRecord, SyncEntry};
use benilla_world::voxel::overlay_cells::{cell_region, LAYER_AREA, LAYER_LIQUID};
use benilla_world::voxel::overlay::{area_event, liquid_event};
use benilla_world::voxel::{MonkeyOverlay, MonkeyOverlayNet, MonkeyOverlayNetEvent};
use benilla_world::voxel::{
    region_of, MonkeyLocalGm, MonkeyVoxelEdit, MonkeyVoxelNet, MonkeyVoxelNetEvent,
    MonkeyVoxelWorld, RegionSync, REGION_YARDS,
};

use crate::net::{
    ChatKind, ClientCommand, NetCommands, NetHandlerApp, ObjectStore, SelfGuid, SelfPlayer,
};

/// `PLAYER_FLAGS & 0x8` — GM mode on (`.gm on`); same bit chat's `is_gm` reads.
const PLAYER_FLAGS_GM: u32 = 0x8;

/// The tag marking a v2 voxel-EDIT guild-relay line (`MV2:<46 hex>`). Addon traffic, so the chat
/// UI keeps it out of the visible log.
pub(crate) const MONKEY_VOXEL_PREFIX: &str = "MV2:";

/// No `H` within this many seconds of HELLO → guild relay (CONTRACT §4).
const HELLO_TIMEOUT_S: f32 = 10.0;
/// HELLO is re-sent once at this age (the server ignores a HELLO < 1 s after the last).
const HELLO_RETRY_S: f32 = 4.0;
/// SYNC regions whose centre is within this of the player (CONTRACT §3).
const SYNC_RADIUS: f32 = 200.0;
/// A synced region whose centre is beyond this is forgotten (re-SYNCed on return).
const UNSYNC_RADIUS: f32 = 230.0;
/// At most one SYNC command per this many seconds (server gap 200 ms).
const SYNC_GAP_S: f32 = 0.25;
/// A SYNC entry with no `D` after this long is asked again (the server drops a SYNC sent inside
/// its 200-ms gap silently).
const SYNC_RETRY_S: f32 = 5.0;
/// Local edits held while the mode is undecided (≤ 10 s at ≤ 8/s).
const HOLD_CAP: usize = 256;

/// `CHAT_MSG_WHISPER` / `_INFORM`: the server's self-whisper lane.
const CHAT_WHISPER: u8 = 0x06;
const CHAT_WHISPER_INFORM: u8 = 0x07;

/// The transport the voxel layer currently uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MonkeyVoxelMode {
    /// Out of the world.
    #[default]
    Idle,
    /// HELLO sent, waiting for `H` (local edits are held).
    Probing,
    /// The authority script answered.
    Server,
    /// No `H`: the guild `MV2:` relay.
    Relay,
}

/// The link state of this session (reset on every world entry / map change).
#[derive(Resource, Default)]
pub(crate) struct MonkeyVoxelLink {
    pub(crate) mode: MonkeyVoxelMode,
    /// MONKEY (voxel): the negotiated protocol version (the `H` byte; 0 = no ACK yet).
    pub(crate) version: u8,
    /// `(self guid, map)` the session state belongs to.
    key: Option<(u64, u32)>,
    hello_at: f32,
    hello_tries: u8,
    /// Decoded server records waiting for the tick (arrival order).
    rx: Vec<ServerRecord>,
    /// The session's view of the sync points (the journal's, updated by D / X / live E).
    known: HashMap<(i16, i16), RegionSync>,
    /// Regions whose SYNC completed (`D more=0`) this session.
    synced: HashSet<(i16, i16)>,
    /// Regions asked, waiting for their `D` → time sent.
    inflight: HashMap<(i16, i16), f32>,
    /// Regions to ask first (`D more=1`, `X`, timeouts).
    again: VecDeque<(i16, i16)>,
    last_sync_cmd: f32,
    /// Predictions uplinked this session (older pending ones were never received → dropped once
    /// their region's SYNC completes without confirming them).
    sent_nonces: HashSet<u16>,
    /// Totals for the log.
    sync_cmds: u32,
    /// `E` records that answered a SYNC (not live) since the last "SYNC complete" line.
    sync_edits: u32,
    /// A SYNC round is open (logged once when it drains).
    sync_open: bool,
    /// MONKEY (EME overlay): the OSYNC state of the liquid (`[0]`) and area (`[1]`) layers.
    ov: [MonkeyOverlayLink; 2],
    /// MONKEY (EME overlay): the next free gap slot tries the overlays first (the server's 200-ms
    /// SYNC gap is SHARED with OSYNC, so the streams take turns).
    ov_turn: bool,
    /// Which overlay layer the next overlay slot tries first.
    ov_layer_turn: usize,
}

/// MONKEY (EME overlay): one overlay layer's OSYNC bookkeeping (the SYNC model, CONTRACT §9.1).
#[derive(Default)]
pub(crate) struct MonkeyOverlayLink {
    known: HashMap<(i16, i16), RegionSync>,
    synced: HashSet<(i16, i16)>,
    inflight: HashMap<(i16, i16), f32>,
    again: VecDeque<(i16, i16)>,
}

/// The wire layer id of `ov[i]`.
const OV_LAYERS: [u8; 2] = [LAYER_LIQUID, LAYER_AREA];

fn ov_index(layer: u8) -> Option<usize> {
    OV_LAYERS.iter().position(|&l| l == layer)
}

/// MONKEY (EME overlay): route the overlay records (`L` `A` `O` `Z`) into overlay events and the
/// OSYNC bookkeeping. Pure, unit-tested.
fn monkey_overlay_route(
    link: &mut MonkeyVoxelLink,
    map: u16,
    records: Vec<ServerRecord>,
    events: &mut Vec<MonkeyOverlayNetEvent>,
) {
    for r in records {
        let (layer, cell_map, region, seq, ev) = match r {
            ServerRecord::Liquid {
                seq,
                stroke,
                map: m,
                cx,
                cy,
                cell,
                actor,
            } => (
                LAYER_LIQUID,
                m,
                cell_region(LAYER_LIQUID, cx, cy),
                seq,
                liquid_event(seq, stroke, m, cx, cy, cell, actor),
            ),
            ServerRecord::Area {
                seq,
                stroke,
                map: m,
                ax,
                ay,
                area_id,
                actor,
            } => (
                LAYER_AREA,
                m,
                cell_region(LAYER_AREA, ax, ay),
                seq,
                area_event(seq, stroke, m, ax, ay, area_id, actor),
            ),
            ServerRecord::OverlayDone {
                layer,
                map: m,
                rx,
                ry,
                epoch,
                last_seq,
                more,
            } => {
                let Some(i) = ov_index(layer) else { continue };
                if m != map {
                    continue;
                }
                let ol = &mut link.ov[i];
                let reg = (rx, ry);
                ol.inflight.remove(&reg);
                let sync = RegionSync { epoch, last_seq };
                ol.known.insert(reg, sync);
                events.push(MonkeyOverlayNetEvent::SyncPoint {
                    layer,
                    map,
                    rx,
                    ry,
                    sync,
                });
                if more {
                    if !ol.again.contains(&reg) {
                        ol.again.push_back(reg);
                    }
                } else {
                    ol.synced.insert(reg);
                }
                continue;
            }
            ServerRecord::OverlayReset {
                layer,
                map: m,
                rx,
                ry,
                epoch,
            } => {
                events.push(MonkeyOverlayNetEvent::Reset {
                    layer,
                    map: m,
                    rx,
                    ry,
                    epoch,
                });
                let Some(i) = ov_index(layer) else { continue };
                if m != map {
                    continue;
                }
                let ol = &mut link.ov[i];
                let reg = (rx, ry);
                info!(
                    "MONKEY (EME overlay): Z OVERLAY-RESET layer {layer} ({rx}, {ry}) epoch {epoch} \
                     - re-OSYNC from 0"
                );
                ol.known.insert(reg, RegionSync { epoch, last_seq: 0 });
                ol.synced.remove(&reg);
                if !ol.inflight.contains_key(&reg) && !ol.again.contains(&reg) {
                    ol.again.push_back(reg);
                }
                continue;
            }
            _ => continue,
        };
        events.push(ev);
        // A live cell in a region already synced this session keeps its sync point contiguous.
        let Some(i) = ov_index(layer) else { continue };
        let ol = &mut link.ov[i];
        if cell_map == map && ol.synced.contains(&region) {
            if let Some(k) = ol.known.get_mut(&region) {
                if seq > k.last_seq {
                    k.last_seq = seq;
                    events.push(MonkeyOverlayNetEvent::SyncPoint {
                        layer,
                        map,
                        rx: region.0,
                        ry: region.1,
                        sync: *k,
                    });
                }
            }
        }
    }
}

/// Is this one of the overlay records?
fn is_overlay_record(r: &ServerRecord) -> bool {
    matches!(
        r,
        ServerRecord::Liquid { .. }
            | ServerRecord::Area { .. }
            | ServerRecord::OverlayDone { .. }
            | ServerRecord::OverlayReset { .. }
    )
}

/// MONKEY (EME overlay): the OSYNC entries of one layer to send now (re-asks first, then unsynced
/// regions near the player, nearest first, ≤ 11 so the line fits 255 chars) — [`monkey_voxel_sync_batch`] for an
/// overlay.
fn monkey_overlay_sync_batch(
    ol: &mut MonkeyOverlayLink,
    near: &[((i16, i16), f32)],
    sync_point: impl Fn((i16, i16)) -> RegionSync,
) -> Vec<SyncEntry> {
    let mut regs: Vec<(i16, i16)> = Vec::new();
    while regs.len() < wire::OSYNC_MAX_ENTRIES {
        let Some(r) = ol.again.pop_front() else {
            break;
        };
        if !regs.contains(&r) {
            regs.push(r);
        }
    }
    for (r, _) in near {
        if regs.len() >= wire::OSYNC_MAX_ENTRIES {
            break;
        }
        if !ol.synced.contains(r) && !ol.inflight.contains_key(r) && !regs.contains(r) {
            regs.push(*r);
        }
    }
    regs.into_iter()
        .map(|r| {
            let s = ol.known.get(&r).copied().unwrap_or_else(|| sync_point(r));
            ol.known.insert(r, s);
            SyncEntry {
                rx: r.0,
                ry: r.1,
                epoch: s.epoch,
                after_seq: s.last_seq,
            }
        })
        .collect()
}

/// MONKEY (EME overlay): use this gap slot for one OSYNC command (liquid / area, alternating);
/// `true` = one was sent.
fn monkey_overlay_sync_slot(
    link: &mut MonkeyVoxelLink,
    map: u16,
    near: &[((i16, i16), f32)],
    overlay: &MonkeyOverlay,
    now: f32,
    commands: &NetCommands,
) -> bool {
    for k in 0..2 {
        let i = (link.ov_layer_turn + k) % 2;
        let layer = OV_LAYERS[i];
        let entries = monkey_overlay_sync_batch(&mut link.ov[i], near, |r| {
            overlay.region_sync(layer, map, r.0, r.1)
        });
        if entries.is_empty() {
            continue;
        }
        link.ov_layer_turn = (i + 1) % 2;
        link.last_sync_cmd = now;
        for e in &entries {
            link.ov[i].inflight.insert((e.rx, e.ry), now);
        }
        let after: Vec<String> = entries
            .iter()
            .map(|e| format!("({},{})e{}>{}", e.rx, e.ry, e.epoch, e.after_seq))
            .collect();
        info!(
            "MONKEY (EME overlay): OSYNC layer {layer} map {map}: {} region(s) {}",
            entries.len(),
            after.join(" ")
        );
        for c in wire::osync_commands(layer, map, &entries) {
            debug_assert!(c.len() <= 255, "a chat line the core would refuse");
            say(commands, c);
        }
        return true;
    }
    false
}

/// Wire the link: the inbound-chat interceptor, the GM-status bridge, the mode / SYNC tick and
/// the outbox drain.
pub(crate) fn register(app: &mut App) {
    app.init_resource::<MonkeyVoxelLink>()
        .net_handler(SessionEventKind::Chat, monkey_voxel_recv)
        .add_systems(
            Update,
            (
                monkey_voxel_push_gm,
                monkey_voxel_link_tick,
                monkey_voxel_drain_outbox,
            )
                .chain(),
        );
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

fn say(commands: &NetCommands, text: String) {
    let _ = commands.0.send(ClientCommand::Chat {
        kind: ChatKind::Say,
        target: None,
        text,
        language: None,
    });
}

/// The regions whose centre lies within `radius` of `(x, y)`, nearest first.
fn regions_near(x: f32, y: f32, radius: f32) -> Vec<((i16, i16), f32)> {
    let (r0x, r0y) = region_of(x - radius, y - radius);
    let (r1x, r1y) = region_of(x + radius, y + radius);
    let mut out = Vec::new();
    for rx in r0x..=r1x {
        for ry in r0y..=r1y {
            let cx = (rx as f32 + 0.5) * REGION_YARDS;
            let cy = (ry as f32 + 0.5) * REGION_YARDS;
            let d = ((cx - x).powi(2) + (cy - y).powi(2)).sqrt();
            if d <= radius {
                out.push(((rx, ry), d));
            }
        }
    }
    out.sort_by(|a, b| a.1.total_cmp(&b.1));
    out
}

/// Turn one batch of decoded records into ordered world events + link bookkeeping. Pure (no
/// Bevy systems), so the confirm rule and the SYNC bookkeeping are unit-tested.
fn monkey_voxel_route(
    link: &mut MonkeyVoxelLink,
    map: u16,
    me: u32,
    pending: &[MonkeyVoxelEdit],
    records: Vec<ServerRecord>,
    events: &mut Vec<MonkeyVoxelNetEvent>,
) {
    let mut confirmed_now: HashSet<u16> = HashSet::new();
    for r in records {
        match r {
            ServerRecord::Hello { .. } => {}
            // MONKEY (EME overlay): routed by `monkey_overlay_route` (split off before this).
            ServerRecord::Liquid { .. }
            | ServerRecord::Area { .. }
            | ServerRecord::OverlayDone { .. }
            | ServerRecord::OverlayReset { .. } => {}
            ServerRecord::Edit(_) | ServerRecord::Edit3 { .. } => {
                // MONKEY (voxel): `E` (v2) confirms on the body; `e` (v3) also on stroke, param
                // and the actor being this character.
                let (e, own, actor) = match r {
                    ServerRecord::Edit3 { edit, actor } => (
                        edit,
                        pending.iter().find(|p| {
                            !confirmed_now.contains(&p.nonce)
                                && wire::is_echo_of3(&edit, actor, me, p)
                        }),
                        Some(actor),
                    ),
                    ServerRecord::Edit(edit) => (
                        edit,
                        pending.iter().find(|p| {
                            !confirmed_now.contains(&p.nonce) && wire::is_echo_of(&edit, p)
                        }),
                        None,
                    ),
                    _ => unreachable!(),
                };
                let seq = e.seq.unwrap_or(0);
                match own {
                    Some(p) => {
                        confirmed_now.insert(p.nonce);
                        events.push(MonkeyVoxelNetEvent::Confirm {
                            map: e.map,
                            nonce: p.nonce,
                            seq,
                        });
                    }
                    None => events.push(MonkeyVoxelNetEvent::Edit(e)),
                }
                // A live edit in a region already synced this session keeps it contiguous.
                let reg = e.region();
                if e.map == map && link.synced.contains(&reg) {
                    if let Some(k) = link.known.get_mut(&reg) {
                        if seq > k.last_seq {
                            k.last_seq = seq;
                            events.push(MonkeyVoxelNetEvent::SyncPoint {
                                map,
                                rx: reg.0,
                                ry: reg.1,
                                epoch: k.epoch,
                                last_seq: seq,
                            });
                        }
                    }
                } else if link.inflight.contains_key(&reg) {
                    link.sync_edits += 1;
                }
                // MONKEY (EME): a LIVE stroke edit by another character (not a SYNC replay) is
                // announced, so an editor can list other GMs' strokes read-only.
                if let (None, Some(actor)) = (own, actor) {
                    if actor != me && e.stroke != 0 && !link.inflight.contains_key(&reg) {
                        events.push(MonkeyVoxelNetEvent::Remote { edit: e, actor });
                    }
                }
            }
            ServerRecord::Done {
                map: m,
                rx,
                ry,
                epoch,
                last_seq,
                more,
            } => {
                if m != map {
                    continue;
                }
                let reg = (rx, ry);
                link.inflight.remove(&reg);
                link.known.insert(reg, RegionSync { epoch, last_seq });
                events.push(MonkeyVoxelNetEvent::SyncPoint {
                    map,
                    rx,
                    ry,
                    epoch,
                    last_seq,
                });
                if more {
                    if !link.again.contains(&reg) {
                        link.again.push_back(reg);
                    }
                } else {
                    link.synced.insert(reg);
                    // A prediction of an EARLIER session the completed SYNC did not confirm never
                    // reached the server: drop it (re-sending could double-apply).
                    for p in pending.iter().filter(|p| {
                        p.map == map
                            && p.region() == reg
                            && !link.sent_nonces.contains(&p.nonce)
                            && !confirmed_now.contains(&p.nonce)
                    }) {
                        info!(
                            "MONKEY (voxel): stale local prediction nonce {} in region ({rx}, \
                             {ry}) was never confirmed by the server — dropping it",
                            p.nonce
                        );
                        events.push(MonkeyVoxelNetEvent::DropLocal {
                            map,
                            nonce: p.nonce,
                            reason: 0,
                        });
                    }
                }
            }
            ServerRecord::Reset {
                map: m,
                rx,
                ry,
                epoch,
            } => {
                events.push(MonkeyVoxelNetEvent::Reset {
                    map: m,
                    rx,
                    ry,
                    epoch,
                });
                if m != map {
                    continue;
                }
                let reg = (rx, ry);
                info!(
                    "MONKEY (voxel): X REGION-RESET ({rx}, {ry}) epoch {epoch} — re-SYNC from 0"
                );
                link.known.insert(reg, RegionSync { epoch, last_seq: 0 });
                link.synced.remove(&reg);
                // A reply to an in-flight SYNC carries its E + D right behind this X: keep waiting.
                if !link.inflight.contains_key(&reg) && !link.again.contains(&reg) {
                    link.again.push_back(reg);
                }
            }
            ServerRecord::Reject { nonce, reason } => {
                warn!(
                    "MONKEY (voxel): server REJECTED our edit nonce {nonce} (R reason {reason}: \
                     {}) — dropping the local prediction",
                    wire::reject_reason(reason)
                );
                events.push(MonkeyVoxelNetEvent::DropLocal { map, nonce, reason });
            }
        }
    }
}

/// The SYNC entries to send now: re-asks first, then unsynced regions near the player, nearest
/// first, ≤ 12.
fn monkey_voxel_sync_batch(
    link: &mut MonkeyVoxelLink,
    near: &[((i16, i16), f32)],
    sync_point: impl Fn((i16, i16)) -> RegionSync,
) -> Vec<SyncEntry> {
    let mut regs: Vec<(i16, i16)> = Vec::new();
    while regs.len() < wire::SYNC_MAX_ENTRIES {
        let Some(r) = link.again.pop_front() else {
            break;
        };
        if !regs.contains(&r) {
            regs.push(r);
        }
    }
    for (r, _) in near {
        if regs.len() >= wire::SYNC_MAX_ENTRIES {
            break;
        }
        if !link.synced.contains(r) && !link.inflight.contains_key(r) && !regs.contains(r) {
            regs.push(*r);
        }
    }
    regs.into_iter()
        .map(|r| {
            let s = link.known.get(&r).copied().unwrap_or_else(|| sync_point(r));
            link.known.insert(r, s);
            SyncEntry {
                rx: r.0,
                ry: r.1,
                epoch: s.epoch,
                after_seq: s.last_seq,
            }
        })
        .collect()
}

/// The mode machine + SYNC scheduler + record router (runs every frame).
#[allow(clippy::too_many_arguments)]
fn monkey_voxel_link_tick(
    time: Res<Time>,
    self_guid: Res<SelfGuid>,
    world: Res<MonkeyVoxelWorld>,
    body: Query<&Transform, With<SelfPlayer>>,
    commands: Res<NetCommands>,
    mut link: ResMut<MonkeyVoxelLink>,
    mut net: ResMut<MonkeyVoxelNet>,
    overlay: (Res<MonkeyOverlay>, ResMut<MonkeyOverlayNet>),
) {
    let (overlay, mut ov_net) = overlay;
    let now = time.elapsed_secs();
    let (Some(guid), Some(map)) = (self_guid.0, world.map()) else {
        if link.mode != MonkeyVoxelMode::Idle {
            *link = MonkeyVoxelLink::default();
        }
        return;
    };
    let link = &mut *link;
    // World entry / relog / map change: a fresh session state + HELLO.
    if link.key != Some((guid, map)) {
        let rx = std::mem::take(&mut link.rx);
        *link = MonkeyVoxelLink {
            key: Some((guid, map)),
            mode: MonkeyVoxelMode::Probing,
            hello_at: now,
            hello_tries: 1,
            // An H that raced the map change still counts.
            rx: rx
                .into_iter()
                .filter(|r| matches!(r, ServerRecord::Hello { .. }))
                .collect(),
            ..Default::default()
        };
        say(&commands, wire::hello_command_v(wire::CLIENT_VERSION_V3));
        info!("MONKEY (voxel): HELLO .mvox 1003 sent (map {map}) — waiting for the server's H");
    }
    let map16 = map as u16;

    // Records → events (H switches the mode).
    let records = std::mem::take(&mut link.rx);
    if !records.is_empty() {
        if let Some(ServerRecord::Hello { version }) = records
            .iter()
            .find(|r| matches!(r, ServerRecord::Hello { .. }))
        {
            link.version = *version;
            if link.mode != MonkeyVoxelMode::Server {
                info!(
                    "MONKEY (voxel): SERVER mode — HELLO-ACK v{version} after {:.1} s (map {map}); \
                     edits go through .mvox, SYNC within {SYNC_RADIUS} yd",
                    now - link.hello_at
                );
                link.mode = MonkeyVoxelMode::Server;
            }
        }
        // MONKEY (EME overlay): the overlay records take their own route.
        let (ov_records, records): (Vec<ServerRecord>, Vec<ServerRecord>) =
            records.into_iter().partition(is_overlay_record);
        monkey_overlay_route(link, map16, ov_records, &mut ov_net.events);
        let pending = world.pending_local(map16);
        // The low GUID (CONTRACT §9.2 `actor_guid` = the character's low 32 bits).
        monkey_voxel_route(link, map16, guid as u32, &pending, records, &mut net.events);
    }

    match link.mode {
        MonkeyVoxelMode::Probing => {
            let age = now - link.hello_at;
            if age >= HELLO_TIMEOUT_S {
                link.mode = MonkeyVoxelMode::Relay;
                info!(
                    "MONKEY (voxel): no HELLO-ACK in {HELLO_TIMEOUT_S} s — RELAY mode (guild \
                     MV2: lines, no server authority)"
                );
            } else if age >= HELLO_RETRY_S && link.hello_tries < 2 {
                link.hello_tries += 1;
                say(&commands, wire::hello_command_v(wire::CLIENT_VERSION_V3));
            }
        }
        MonkeyVoxelMode::Server => {
            let Ok(tf) = body.single() else {
                return;
            };
            let p = bevy_to_wow(tf.translation);
            // Forget regions that drifted out of the fan-out; retry lost SYNCs.
            let far: Vec<(i16, i16)> = link
                .synced
                .iter()
                .copied()
                .filter(|r| {
                    let cx = (r.0 as f32 + 0.5) * REGION_YARDS;
                    let cy = (r.1 as f32 + 0.5) * REGION_YARDS;
                    ((cx - p[0]).powi(2) + (cy - p[1]).powi(2)).sqrt() > UNSYNC_RADIUS
                })
                .collect();
            for r in far {
                link.synced.remove(&r);
            }
            // MONKEY (EME overlay): the same forgetting / retry for both overlay layers.
            for ol in &mut link.ov {
                ol.synced.retain(|r| {
                    let cx = (r.0 as f32 + 0.5) * REGION_YARDS;
                    let cy = (r.1 as f32 + 0.5) * REGION_YARDS;
                    ((cx - p[0]).powi(2) + (cy - p[1]).powi(2)).sqrt() <= UNSYNC_RADIUS
                });
                let lost: Vec<(i16, i16)> = ol
                    .inflight
                    .iter()
                    .filter(|(_, t)| now - **t > SYNC_RETRY_S)
                    .map(|(r, _)| *r)
                    .collect();
                for r in lost {
                    ol.inflight.remove(&r);
                    ol.again.push_back(r);
                }
            }
            let lost: Vec<(i16, i16)> = link
                .inflight
                .iter()
                .filter(|(_, t)| now - **t > SYNC_RETRY_S)
                .map(|(r, _)| *r)
                .collect();
            for r in lost {
                link.inflight.remove(&r);
                link.again.push_back(r);
            }
            if now - link.last_sync_cmd < SYNC_GAP_S {
                return;
            }
            let near = regions_near(p[0], p[1], SYNC_RADIUS);
            // MONKEY (EME overlay): OSYNC only on a v3 negotiation (CONTRACT §7); slots alternate.
            let v3 = link.version >= wire::CLIENT_VERSION_V3;
            if v3 && link.ov_turn {
                link.ov_turn = false;
                if monkey_overlay_sync_slot(link, map16, &near, &overlay, now, &commands) {
                    return;
                }
            }
            let entries =
                monkey_voxel_sync_batch(link, &near, |r| world.region_sync(map16, r.0, r.1));
            if entries.is_empty() {
                if v3 && monkey_overlay_sync_slot(link, map16, &near, &overlay, now, &commands) {
                    return;
                }
                // Nothing left to ask and every answer in: one line per SYNC round.
                if link.sync_open && link.inflight.is_empty() {
                    link.sync_open = false;
                    info!(
                        "MONKEY (voxel): SYNC complete — {} region(s) synced, {} edit(s) fetched \
                         (0 = the journal was already current)",
                        link.synced.len(),
                        std::mem::take(&mut link.sync_edits)
                    );
                }
                return;
            }
            link.last_sync_cmd = now;
            link.ov_turn = true;
            link.sync_cmds += 1;
            link.sync_open = true;
            for e in &entries {
                link.inflight.insert((e.rx, e.ry), now);
            }
            let after: Vec<String> = entries
                .iter()
                .map(|e| format!("({},{})e{}>{}", e.rx, e.ry, e.epoch, e.after_seq))
                .collect();
            info!(
                "MONKEY (voxel): SYNC #{} map {map}: {} region(s) {} — {} synced so far",
                link.sync_cmds,
                entries.len(),
                after.join(" "),
                link.synced.len()
            );
            for c in wire::sync_commands(map16, &entries) {
                say(&commands, c);
            }
        }
        MonkeyVoxelMode::Idle | MonkeyVoxelMode::Relay => {}
    }
}

/// Drain the brush's outbox through the current transport: SERVER → `.mvox` EDIT, RELAY → a
/// `MV2:<hex>` guild addon line, PROBING → held (≤ 10 s) until the mode is known.
fn monkey_voxel_drain_outbox(
    mut net: ResMut<MonkeyVoxelNet>,
    mut link: ResMut<MonkeyVoxelLink>,
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
    match link.mode {
        MonkeyVoxelMode::Idle | MonkeyVoxelMode::Probing => {
            let n = net.outbox.len();
            if n > HOLD_CAP {
                net.outbox.drain(..n - HOLD_CAP);
            }
        }
        MonkeyVoxelMode::Server => {
            for edit in net.outbox.drain(..).collect::<Vec<_>>() {
                match uplink_command(link.version, &edit) {
                    Some(cmd) => {
                        link.sent_nonces.insert(edit.nonce);
                        debug!("MONKEY (voxel): -> EDIT nonce {} (v{})", edit.nonce, link.version);
                        say(&commands, cmd);
                    }
                    None => {
                        warn!(
                            "MONKEY (voxel): edit mode {} needs a v3 server (negotiated v{}) — \
                             dropping the local prediction nonce {}",
                            edit.mode, link.version, edit.nonce
                        );
                        net.events.push(MonkeyVoxelNetEvent::DropLocal {
                            map: edit.map,
                            nonce: edit.nonce,
                            reason: 0,
                        });
                    }
                }
            }
        }
        MonkeyVoxelMode::Relay => {
            for edit in net.outbox.drain(..).collect::<Vec<_>>() {
                if edit.mode > 2 {
                    // The MV2 relay carries the v2 body only (no stroke / param).
                    net.events.push(MonkeyVoxelNetEvent::DropLocal {
                        map: edit.map,
                        nonce: edit.nonce,
                        reason: 0,
                    });
                    continue;
                }
                let _ = commands.0.send(ClientCommand::AddonMessage {
                    distribution: AddonDistribution::Guild,
                    text: format!("{MONKEY_VOXEL_PREFIX}{}", edit.to_hex()),
                });
            }
        }
    }
}

/// MONKEY (voxel): the uplink command for one local edit under the negotiated version: v3 → the
/// EDIT3 body (any mode; stroke + param), v2 → the EDIT body for modes 0-2 only, else `None`
/// (a v2 server cannot store it — CONTRACT §7.4).
fn uplink_command(version: u8, edit: &MonkeyVoxelEdit) -> Option<String> {
    if version >= wire::CLIENT_VERSION_V3 {
        Some(wire::edit3_command(edit))
    } else if edit.mode <= 2 {
        Some(wire::edit_command(edit))
    } else {
        None
    }
}

/// Receive side.
/// - `MONKEYVOXEL<TAB><hex>` addon whisper from OURSELVES (the server's self-whisper; a player can't
///   forge that sender) → decoded records queued for the tick.
/// - `MV2:<hex>` guild line from **another** player → the inbox, unless in SERVER mode (then the
///   server is the only source of truth). A malformed body is dropped.
fn monkey_voxel_recv(
    In(ev): In<SessionEvent>,
    mut net: ResMut<MonkeyVoxelNet>,
    mut link: ResMut<MonkeyVoxelLink>,
    self_guid: Res<SelfGuid>,
) {
    let SessionEvent::Chat(m) = ev else {
        return;
    };
    if !m.is_addon() {
        return;
    }
    if let Some(hex) = wire::server_payload(&m.text) {
        if self_guid.0 != Some(m.sender_guid)
            || !matches!(m.chat_type, CHAT_WHISPER | CHAT_WHISPER_INFORM)
        {
            warn!(
                "MONKEY (voxel): ignoring a MONKEYVOXEL line not whispered by the server to us \
                 (type {:#04x}, sender {:#x})",
                m.chat_type, m.sender_guid
            );
            return;
        }
        let (records, err) = wire::decode_records(hex);
        if let Some(e) = err {
            warn!("MONKEY (voxel): bad MONKEYVOXEL payload ({e:?}): {hex:?}");
        }
        debug!("MONKEY (voxel): <- {} record(s)", records.len());
        link.rx.extend(records);
        return;
    }
    let Some(hex) = m.text.strip_prefix(MONKEY_VOXEL_PREFIX) else {
        return;
    };
    if self_guid.0 == Some(m.sender_guid) || link.mode == MonkeyVoxelMode::Server {
        return;
    }
    match MonkeyVoxelEdit::from_hex(hex) {
        Some(edit) => net.inbox.push(edit),
        None => warn!("MONKEY (voxel): dropping a malformed MV2 payload {:?}", m.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(nonce: u16, x: f32, seq: Option<u32>, local: bool) -> MonkeyVoxelEdit {
        MonkeyVoxelEdit {
            map: 0,
            pos: [x, 10.0, 5.0],
            radius: 2.0,
            mode: 0,
            strength: 255,
            nonce,
            seq,
            local,
            ..Default::default()
        }
    }

    #[test]
    fn route_confirms_own_echo_and_applies_others() {
        let mut link = MonkeyVoxelLink::default();
        let pending = vec![op(7, 10.0, None, true)];
        let mut ev = Vec::new();
        let recs = vec![
            ServerRecord::Edit(op(7, 10.0, Some(41), false)), // our echo
            ServerRecord::Edit(op(7, 30.0, Some(42), false)), // another GM, same nonce
            ServerRecord::Reject { nonce: 9, reason: 1 },
        ];
        monkey_voxel_route(&mut link, 0, 77, &pending, recs, &mut ev);
        assert_eq!(
            ev[0],
            MonkeyVoxelNetEvent::Confirm {
                map: 0,
                nonce: 7,
                seq: 41
            }
        );
        assert!(matches!(ev[1], MonkeyVoxelNetEvent::Edit(e) if e.seq == Some(42)));
        assert_eq!(ev[2], MonkeyVoxelNetEvent::DropLocal { map: 0, nonce: 9, reason: 1 });
    }

    /// MONKEY (voxel): v3 `e` records confirm only with our actor + stroke + param; the uplink
    /// picks EDIT3 under v3 and refuses modes >= 3 under v2.
    #[test]
    fn route_v3_confirms_with_actor_and_uplink_follows_the_version() {
        let mut link = MonkeyVoxelLink::default();
        let mut mine = op(7, 10.0, None, true);
        mine.stroke = 4;
        mine.mode = 3;
        mine.param = 150;
        let echo = |seq: u32, actor: u32| {
            let mut e = mine;
            e.local = false;
            e.seq = Some(seq);
            ServerRecord::Edit3 { edit: e, actor }
        };
        let mut ev = Vec::new();
        monkey_voxel_route(
            &mut link,
            0,
            77,
            &[mine],
            vec![echo(50, 78), echo(51, 77)],
            &mut ev,
        );
        assert!(
            matches!(ev[0], MonkeyVoxelNetEvent::Edit(e) if e.seq == Some(50) && e.stroke == 4),
            "the same body from another character = a remote edit"
        );
        // The other character's live stroke edit is announced right after its `Edit`.
        assert!(matches!(
            ev[1],
            MonkeyVoxelNetEvent::Remote { actor: 78, edit } if edit.stroke == 4
        ));
        assert_eq!(
            ev[2],
            MonkeyVoxelNetEvent::Confirm {
                map: 0,
                nonce: 7,
                seq: 51
            }
        );
        assert!(uplink_command(3, &mine).unwrap().starts_with(".mvox 02"));
        assert_eq!(uplink_command(3, &mine).unwrap().len(), ".mvox ".len() + 54);
        assert_eq!(uplink_command(2, &mine), None, "flatten cannot go to a v2 server");
        let dig = op(8, 10.0, None, true);
        assert!(uplink_command(2, &dig).unwrap().starts_with(".mvox 01"));
    }

    #[test]
    fn route_reset_then_edits_then_done_keeps_order_and_sync_state() {
        let mut link = MonkeyVoxelLink::default();
        link.inflight.insert((0, 0), 0.0);
        let mut ev = Vec::new();
        let recs = vec![
            ServerRecord::Reset {
                map: 0,
                rx: 0,
                ry: 0,
                epoch: 3,
            },
            ServerRecord::Edit(op(1, 10.0, Some(5), false)),
            ServerRecord::Done {
                map: 0,
                rx: 0,
                ry: 0,
                epoch: 3,
                last_seq: 5,
                more: false,
            },
        ];
        // A stale prediction from an earlier session (never sent this session) in that region.
        let pending = vec![op(99, 12.0, None, true)];
        monkey_voxel_route(&mut link, 0, 77, &pending, recs, &mut ev);
        assert!(matches!(ev[0], MonkeyVoxelNetEvent::Reset { epoch: 3, .. }));
        assert!(matches!(ev[1], MonkeyVoxelNetEvent::Edit(_)));
        assert!(matches!(
            ev[2],
            MonkeyVoxelNetEvent::SyncPoint {
                epoch: 3,
                last_seq: 5,
                ..
            }
        ));
        assert_eq!(ev[3], MonkeyVoxelNetEvent::DropLocal { map: 0, nonce: 99, reason: 0 });
        assert!(link.synced.contains(&(0, 0)));
        assert!(link.again.is_empty(), "the X arrived inside the SYNC reply: no extra re-ask");
        // A live E in the now-synced region moves its sync point.
        ev.clear();
        monkey_voxel_route(
            &mut link,
            0,
            77,
            &[],
            vec![ServerRecord::Edit(op(2, 11.0, Some(8), false))],
            &mut ev,
        );
        assert!(matches!(
            ev[1],
            MonkeyVoxelNetEvent::SyncPoint {
                last_seq: 8,
                epoch: 3,
                ..
            }
        ));
        // A live X (revert) outside a SYNC re-asks the region from 0.
        ev.clear();
        monkey_voxel_route(
            &mut link,
            0,
            77,
            &[],
            vec![ServerRecord::Reset {
                map: 0,
                rx: 0,
                ry: 0,
                epoch: 4,
            }],
            &mut ev,
        );
        assert_eq!(link.again.front(), Some(&(0, 0)));
        assert!(!link.synced.contains(&(0, 0)));
        let b = monkey_voxel_sync_batch(&mut link, &[], |_| RegionSync::default());
        assert_eq!(
            b,
            vec![SyncEntry {
                rx: 0,
                ry: 0,
                epoch: 4,
                after_seq: 0
            }]
        );
    }

    #[test]
    fn sync_batches_at_most_twelve_nearest_first_and_more_reasks() {
        let mut link = MonkeyVoxelLink::default();
        let near = regions_near(32.0, 32.0, SYNC_RADIUS);
        assert!(near.len() > 24, "{} regions within 200 yd", near.len());
        assert_eq!(near[0].0, (0, 0), "nearest first");
        let b1 = monkey_voxel_sync_batch(&mut link, &near, |r| RegionSync {
            epoch: 1,
            last_seq: (r.0 as i32 + 100) as u32,
        });
        assert_eq!(b1.len(), 12);
        assert_eq!(b1[0].after_seq, 100);
        for e in &b1 {
            link.inflight.insert((e.rx, e.ry), 0.0);
        }
        // A D more=1 for (0,0) puts it at the head of the next batch.
        let mut ev = Vec::new();
        monkey_voxel_route(
            &mut link,
            0,
            77,
            &[],
            vec![ServerRecord::Done {
                map: 0,
                rx: 0,
                ry: 0,
                epoch: 1,
                last_seq: 300,
                more: true,
            }],
            &mut ev,
        );
        let b2 = monkey_voxel_sync_batch(&mut link, &near, |_| RegionSync::default());
        assert_eq!(b2.len(), 12);
        assert_eq!((b2[0].rx, b2[0].ry, b2[0].after_seq), (0, 0, 300));
        assert!(b2[1..].iter().all(|e| !b1.iter().any(|x| x.rx == e.rx && x.ry == e.ry)));
        let cmds = wire::sync_commands(0, &b2);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].len(), ".mvox ".len() + 2 * (4 + 10 * 12));
    }

    /// MONKEY (EME overlay): L/A → cell events, O → sync point (+ re-ask on more), Z → reset +
    /// re-OSYNC from 0, a live cell in a synced region advances its sync point.
    #[test]
    fn overlay_records_route_to_overlay_events_and_osync_state() {
        use benilla_world::voxel::overlay_cells::LiquidCell;
        let mut link = MonkeyVoxelLink::default();
        let mut ev = Vec::new();
        let reg = cell_region(LAYER_AREA, -557, -8);
        assert_eq!(reg, (-140, -2));
        monkey_overlay_route(
            &mut link,
            0,
            vec![
                ServerRecord::OverlayDone {
                    layer: LAYER_AREA,
                    map: 0,
                    rx: -140,
                    ry: -2,
                    epoch: 0,
                    last_seq: 1,
                    more: false,
                },
                ServerRecord::Area {
                    seq: 2,
                    stroke: 4,
                    map: 0,
                    ax: -557,
                    ay: -8,
                    area_id: 87,
                    actor: 42,
                },
            ],
            &mut ev,
        );
        assert!(link.ov[1].synced.contains(&reg));
        assert_eq!(ev.len(), 3, "{ev:?}");
        assert!(matches!(ev[1], MonkeyOverlayNetEvent::Cell { layer: 2, cell: (-557, -8), .. }));
        assert!(matches!(
            ev[2],
            MonkeyOverlayNetEvent::SyncPoint { layer: 2, sync: RegionSync { epoch: 0, last_seq: 2 }, .. }
        ));
        // Z: reset event, region unsynced and re-asked from 0.
        ev.clear();
        monkey_overlay_route(
            &mut link,
            0,
            vec![ServerRecord::OverlayReset {
                layer: LAYER_AREA,
                map: 0,
                rx: -140,
                ry: -2,
                epoch: 1,
            }],
            &mut ev,
        );
        assert!(matches!(ev[0], MonkeyOverlayNetEvent::Reset { epoch: 1, .. }));
        assert!(!link.ov[1].synced.contains(&reg));
        let b = monkey_overlay_sync_batch(&mut link.ov[1], &[], |_| RegionSync::default());
        assert_eq!(b, vec![SyncEntry { rx: -140, ry: -2, epoch: 1, after_seq: 0 }]);
        // A liquid cell goes to the liquid layer; an unsynced region does not move its sync point.
        ev.clear();
        let cell = LiquidCell { kind: 1, level: 81.0, flags: 0, fade: 0 };
        monkey_overlay_route(
            &mut link,
            0,
            vec![ServerRecord::Liquid { seq: 5, stroke: 3, map: 0, cx: 100, cy: -20, cell, actor: 42 }],
            &mut ev,
        );
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], MonkeyOverlayNetEvent::Cell { layer: 1, .. }));
        assert!(is_overlay_record(&ServerRecord::OverlayReset { layer: 1, map: 0, rx: 0, ry: 0, epoch: 0 }));
        assert!(!is_overlay_record(&ServerRecord::Hello { version: 3 }));
    }
}

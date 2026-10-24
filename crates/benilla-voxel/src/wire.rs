//! MONKEY (voxel): the v2 server-authority wire (CONTRACT §3–4, `disposable/voxel-v2/CONTRACT.md`),
//! engine-free so it is unit-tested here.
//!
//! - Uplink (client → server): `.mvox <hex>` chat commands — HELLO `10 02`, SYNC `11 map count
//!   count×(rx ry epoch after_seq)` (≤ 12 entries, 4 + 10·n bytes), EDIT = the 23-byte
//!   [`VoxelEditOp::to_wire`] body. Little-endian, hex lowercase.
//! - Downlink (server → client): addon whisper `MONKEYVOXEL<TAB><hex>`, one or more fixed-size
//!   records concatenated: `H` hello-ack (2), `E` edit (27), `D` region-done (14), `X`
//!   region-reset (9), `R` reject (4).
//! - MONKEY (voxel) v3 (CONTRACT §7, §9): HELLO offers version 3 (`10 03`); an ACK of 3 selects the
//!   27-byte EDIT3 uplink (`02…`, + stroke + param) and the 35-byte `e` record (+ stroke, param,
//!   actor guid); an ACK of 2 keeps the v2 wire. Reject reason 6 = stroke.
//! - MONKEY (EME) v3 overlays (CONTRACT §9.1, §11.3): uplink `0x12 OSYNC` (5 + 10·n bytes, per layer);
//!   downlink `L` liquid cell (24), `A` area cell (19), `O` overlay region done (15), `Z` overlay
//!   region reset (10).

use crate::world::{VoxelEditOp, EDIT3_WIRE_LEN, EDIT_WIRE_LEN};

/// The addon prefix of the server's whispers.
pub const SERVER_PREFIX: &str = "MONKEYVOXEL";
/// The chat command of the uplink.
pub const MVOX_COMMAND: &str = ".mvox";
/// The v2 protocol version (HELLO byte 2 of a v2 client; an `H 2` = v2 server or v2 negotiation).
pub const CLIENT_VERSION: u8 = 2;
/// MONKEY (voxel): the version this client OFFERS in HELLO (CONTRACT §7); the server ACKs
/// `min(offer, its max)`.
pub const CLIENT_VERSION_V3: u8 = 3;
/// MONKEY (voxel): bytes of the `e` record (CONTRACT §9.2).
pub const EDIT3_RECORD_LEN: usize = 35;
/// Max SYNC entries per command (CONTRACT §3).
pub const SYNC_MAX_ENTRIES: usize = 12;
/// MONKEY (EME): bytes of the `L` record (CONTRACT §11.3).
pub const LIQUID_RECORD_LEN: usize = 24;
/// MONKEY (EME): bytes of the `A` record.
pub const AREA_RECORD_LEN: usize = 19;
/// MONKEY (EME): bytes of the `O` record.
pub const OVERLAY_DONE_LEN: usize = 15;
/// MONKEY (EME): bytes of the `Z` record.
pub const OVERLAY_RESET_LEN: usize = 10;

/// One SYNC entry: a region and the client's persisted sync point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncEntry {
    pub rx: i16,
    pub ry: i16,
    pub epoch: u16,
    pub after_seq: u32,
}

/// One decoded server record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ServerRecord {
    /// `H`: the server runs the authority script (`version` = 2).
    Hello { version: u8 },
    /// MONKEY (EME, CONTRACT v3.5): `K`: the caller's GM rank, sent after an ACK of 3 in its own
    /// message. The client sends `.mveme` only when `rank >= 2` (below it the command is hidden).
    EmeRank { rank: u8 },
    /// `E`: an accepted edit with its global `seq` (`edit.seq = Some`, `edit.local = false`).
    Edit(VoxelEditOp),
    /// MONKEY (voxel): `e` (v3): an accepted edit with `seq`, `stroke`, `param`, and the low GUID
    /// of the character that made it.
    Edit3 { edit: VoxelEditOp, actor: u32 },
    /// `D`: the region's sync point; `more` = ask again with `after_seq = last_seq`.
    Done {
        map: u16,
        rx: i16,
        ry: i16,
        epoch: u16,
        last_seq: u32,
        more: bool,
    },
    /// `X`: the region was reverted — drop its journal, adopt `epoch`, re-SYNC from 0.
    Reset { map: u16, rx: i16, ry: i16, epoch: u16 },
    /// `R`: our EDIT `nonce` was refused (1 rank, 2 range, 3 rate, 4 invalid, 5 map, 6 stroke).
    Reject { nonce: u16, reason: u8 },
    /// MONKEY (EME): `L` — one liquid overlay cell (liquid-layer `seq`).
    Liquid {
        seq: u32,
        stroke: u16,
        map: u16,
        cx: i16,
        cy: i16,
        cell: crate::overlay::LiquidCell,
        actor: u32,
    },
    /// MONKEY (EME): `A` — one area overlay cell (area-layer `seq`; `area_id` 0 = override removed).
    Area {
        seq: u32,
        stroke: u16,
        map: u16,
        ax: i16,
        ay: i16,
        area_id: u16,
        actor: u32,
    },
    /// MONKEY (EME): `O` — an overlay region's sync point (`layer` 1 liquid, 2 area).
    OverlayDone {
        layer: u8,
        map: u16,
        rx: i16,
        ry: i16,
        epoch: u16,
        last_seq: u32,
        more: bool,
    },
    /// MONKEY (EME): `Z` — an overlay region was reverted: drop that layer's cells there, adopt
    /// `epoch`, re-OSYNC from 0.
    OverlayReset {
        layer: u8,
        map: u16,
        rx: i16,
        ry: i16,
        epoch: u16,
    },
}

/// Why a whisper payload could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Odd length / non-hex characters.
    BadHex,
    /// An unknown record type byte at this offset.
    UnknownType(u8),
    /// A record cut short.
    Truncated,
    /// An `E` whose body is not a sane edit (NaN, radius out of range, …).
    BadEdit,
    /// MONKEY (EME): an `L` with a non-finite level, or an `O` / `Z` with an unknown layer.
    BadOverlay,
}

/// The human reason of an `R` code.
pub fn reject_reason(code: u8) -> &'static str {
    match code {
        1 => "rank (not a GM)",
        2 => "range (too far from the brush)",
        3 => "rate (too many edits)",
        4 => "invalid edit",
        5 => "map (not on that map)",
        6 => "stroke (undone / reverted / closed stroke id)",
        _ => "unknown",
    }
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Result<Vec<u8>, WireError> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.is_ascii() {
        return Err(WireError::BadHex);
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| WireError::BadHex))
        .collect()
}

/// `.mvox 1002` — the v2 HELLO.
pub fn hello_command() -> String {
    hello_command_v(CLIENT_VERSION)
}

/// MONKEY (voxel): `.mvox 10<v>` — HELLO offering version `v` (a v3 client sends `.mvox 1003`).
pub fn hello_command_v(v: u8) -> String {
    format!("{MVOX_COMMAND} 10{v:02x}")
}

/// MONKEY (voxel): `.mvox 02…` — one v3 EDIT3 (the 27-byte body). Only after an ACK of 3.
pub fn edit3_command(e: &VoxelEditOp) -> String {
    format!("{MVOX_COMMAND} {}", e.to_hex3())
}

/// `.mvox 01…` — one EDIT (the 23-byte body).
pub fn edit_command(e: &VoxelEditOp) -> String {
    format!("{MVOX_COMMAND} {}", e.to_hex())
}

/// The SYNC body for ≤ 12 entries (4 + 10·n bytes).
pub fn sync_body(map: u16, entries: &[SyncEntry]) -> Vec<u8> {
    debug_assert!(!entries.is_empty() && entries.len() <= SYNC_MAX_ENTRIES);
    let mut b = Vec::with_capacity(4 + 10 * entries.len());
    b.push(0x11);
    b.extend_from_slice(&map.to_le_bytes());
    b.push(entries.len() as u8);
    for e in entries {
        b.extend_from_slice(&e.rx.to_le_bytes());
        b.extend_from_slice(&e.ry.to_le_bytes());
        b.extend_from_slice(&e.epoch.to_le_bytes());
        b.extend_from_slice(&e.after_seq.to_le_bytes());
    }
    b
}

/// The `.mvox 11…` commands for any number of entries, batched ≤ 12 per command.
pub fn sync_commands(map: u16, entries: &[SyncEntry]) -> Vec<String> {
    entries
        .chunks(SYNC_MAX_ENTRIES)
        .map(|c| format!("{MVOX_COMMAND} {}", to_hex(&sync_body(map, c))))
        .collect()
}

/// MONKEY (EME): the OSYNC body (`0x12`, CONTRACT §9.1) for ≤ 12 entries of one layer: `op, layer,
/// map u16, count u8, count × (rx, ry, epoch, after_seq)` = 5 + 10·n bytes.
pub fn osync_body(layer: u8, map: u16, entries: &[SyncEntry]) -> Vec<u8> {
    debug_assert!(!entries.is_empty() && entries.len() <= SYNC_MAX_ENTRIES);
    let mut b = Vec::with_capacity(5 + 10 * entries.len());
    b.push(0x12);
    b.push(layer);
    b.extend_from_slice(&map.to_le_bytes());
    b.push(entries.len() as u8);
    for e in entries {
        b.extend_from_slice(&e.rx.to_le_bytes());
        b.extend_from_slice(&e.ry.to_le_bytes());
        b.extend_from_slice(&e.epoch.to_le_bytes());
        b.extend_from_slice(&e.after_seq.to_le_bytes());
    }
    b
}

/// MONKEY (EME): OSYNC entries per command. CONTRACT §9.1 allows 12, but 12 entries are
/// 5 + 120 = 125 bytes = `.mvox ` + 250 hex = 256 chars, ONE over the 255-char chat line the core
/// accepts (`sent a chatmessage with an invalid link`, dropped): 11 keeps it at 236.
pub const OSYNC_MAX_ENTRIES: usize = 11;

/// MONKEY (EME): the `.mvox 12…` commands for any number of entries of one layer, ≤
/// [`OSYNC_MAX_ENTRIES`] each.
pub fn osync_commands(layer: u8, map: u16, entries: &[SyncEntry]) -> Vec<String> {
    entries
        .chunks(OSYNC_MAX_ENTRIES)
        .map(|c| format!("{MVOX_COMMAND} {}", to_hex(&osync_body(layer, map, c))))
        .collect()
}

/// The hex payload of a `MONKEYVOXEL` addon whisper (`prefix TAB hex`), else `None`.
pub fn server_payload(text: &str) -> Option<&str> {
    text.strip_prefix(SERVER_PREFIX)?.strip_prefix('\t')
}

/// Decode every record of one whisper payload (records are never split across messages). On an
/// error the records decoded before it are returned along with it.
pub fn decode_records(hex: &str) -> (Vec<ServerRecord>, Option<WireError>) {
    let b = match from_hex(hex) {
        Ok(b) => b,
        Err(e) => return (Vec::new(), Some(e)),
    };
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let i16_at = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let mut out = Vec::new();
    let mut o = 0;
    while o < b.len() {
        let t = b[o];
        let len = match t {
            b'H' => 2,
            b'K' => 2,
            b'E' => 27,
            b'D' => 14,
            b'X' => 9,
            b'R' => 4,
            b'e' => EDIT3_RECORD_LEN,
            b'L' => LIQUID_RECORD_LEN,
            b'A' => AREA_RECORD_LEN,
            b'O' => OVERLAY_DONE_LEN,
            b'Z' => OVERLAY_RESET_LEN,
            other => return (out, Some(WireError::UnknownType(other))),
        };
        if o + len > b.len() {
            return (out, Some(WireError::Truncated));
        }
        let rec = match t {
            b'H' => ServerRecord::Hello { version: b[o + 1] },
            b'K' => ServerRecord::EmeRank { rank: b[o + 1] },
            b'E' => {
                // `t, seq u32, nonce u16, map u16, x y z radius f32, mode, strength` = the 23-byte
                // EDIT body with the op byte replaced by `t + seq`.
                let mut body = [0u8; EDIT_WIRE_LEN];
                body[0] = crate::world::EDIT_OP;
                body[1..].copy_from_slice(&b[o + 5..o + 27]);
                match VoxelEditOp::from_wire(&body) {
                    Some(mut e) => {
                        e.seq = Some(u32_at(o + 1));
                        e.local = false;
                        ServerRecord::Edit(e)
                    }
                    None => return (out, Some(WireError::BadEdit)),
                }
            }
            b'e' => {
                // `t, seq u32` + the EDIT3 body minus its op byte + `actor u32`.
                let mut body = [0u8; EDIT3_WIRE_LEN];
                body[0] = crate::world::EDIT3_OP;
                body[1..].copy_from_slice(&b[o + 5..o + 31]);
                match VoxelEditOp::from_wire3(&body) {
                    Some(mut e) => {
                        e.seq = Some(u32_at(o + 1));
                        e.local = false;
                        ServerRecord::Edit3 {
                            edit: e,
                            actor: u32_at(o + 31),
                        }
                    }
                    None => return (out, Some(WireError::BadEdit)),
                }
            }
            b'D' => ServerRecord::Done {
                map: u16_at(o + 1),
                rx: i16_at(o + 3),
                ry: i16_at(o + 5),
                epoch: u16_at(o + 7),
                last_seq: u32_at(o + 9),
                more: b[o + 13] != 0,
            },
            b'X' => ServerRecord::Reset {
                map: u16_at(o + 1),
                rx: i16_at(o + 3),
                ry: i16_at(o + 5),
                epoch: u16_at(o + 7),
            },
            b'L' => {
                let level = f32::from_bits(u32_at(o + 14));
                if !level.is_finite() {
                    return (out, Some(WireError::BadOverlay));
                }
                ServerRecord::Liquid {
                    seq: u32_at(o + 1),
                    stroke: u16_at(o + 5),
                    map: u16_at(o + 7),
                    cx: i16_at(o + 9),
                    cy: i16_at(o + 11),
                    cell: crate::overlay::LiquidCell {
                        kind: b[o + 13],
                        level,
                        flags: b[o + 18],
                        fade: b[o + 19],
                    },
                    actor: u32_at(o + 20),
                }
            }
            b'A' => ServerRecord::Area {
                seq: u32_at(o + 1),
                stroke: u16_at(o + 5),
                map: u16_at(o + 7),
                ax: i16_at(o + 9),
                ay: i16_at(o + 11),
                area_id: u16_at(o + 13),
                actor: u32_at(o + 15),
            },
            b'O' | b'Z' => {
                let layer = b[o + 1];
                if layer != crate::overlay::LAYER_LIQUID && layer != crate::overlay::LAYER_AREA {
                    return (out, Some(WireError::BadOverlay));
                }
                if t == b'O' {
                    ServerRecord::OverlayDone {
                        layer,
                        map: u16_at(o + 2),
                        rx: i16_at(o + 4),
                        ry: i16_at(o + 6),
                        epoch: u16_at(o + 8),
                        last_seq: u32_at(o + 10),
                        more: b[o + 14] != 0,
                    }
                } else {
                    ServerRecord::OverlayReset {
                        layer,
                        map: u16_at(o + 2),
                        rx: i16_at(o + 4),
                        ry: i16_at(o + 6),
                        epoch: u16_at(o + 8),
                    }
                }
            }
            _ => ServerRecord::Reject {
                nonce: u16_at(o + 1),
                reason: b[o + 3],
            },
        };
        out.push(rec);
        o += len;
    }
    (out, None)
}

/// Encode one record (the server's layout; used by the tests and dev tooling).
pub fn encode_record(r: &ServerRecord) -> Vec<u8> {
    let mut b = Vec::new();
    match *r {
        ServerRecord::Hello { version } => b.extend_from_slice(&[b'H', version]),
        ServerRecord::EmeRank { rank } => b.extend_from_slice(&[b'K', rank]),
        ServerRecord::Edit(e) => {
            b.push(b'E');
            b.extend_from_slice(&e.seq.unwrap_or(0).to_le_bytes());
            b.extend_from_slice(&e.to_wire()[1..]);
        }
        ServerRecord::Edit3 { edit, actor } => {
            b.push(b'e');
            b.extend_from_slice(&edit.seq.unwrap_or(0).to_le_bytes());
            b.extend_from_slice(&edit.to_wire3()[1..]);
            b.extend_from_slice(&actor.to_le_bytes());
        }
        ServerRecord::Done {
            map,
            rx,
            ry,
            epoch,
            last_seq,
            more,
        } => {
            b.push(b'D');
            b.extend_from_slice(&map.to_le_bytes());
            b.extend_from_slice(&rx.to_le_bytes());
            b.extend_from_slice(&ry.to_le_bytes());
            b.extend_from_slice(&epoch.to_le_bytes());
            b.extend_from_slice(&last_seq.to_le_bytes());
            b.push(more as u8);
        }
        ServerRecord::Reset { map, rx, ry, epoch } => {
            b.push(b'X');
            b.extend_from_slice(&map.to_le_bytes());
            b.extend_from_slice(&rx.to_le_bytes());
            b.extend_from_slice(&ry.to_le_bytes());
            b.extend_from_slice(&epoch.to_le_bytes());
        }
        ServerRecord::Reject { nonce, reason } => {
            b.push(b'R');
            b.extend_from_slice(&nonce.to_le_bytes());
            b.push(reason);
        }
        ServerRecord::Liquid {
            seq,
            stroke,
            map,
            cx,
            cy,
            cell,
            actor,
        } => {
            b.push(b'L');
            b.extend_from_slice(&seq.to_le_bytes());
            b.extend_from_slice(&stroke.to_le_bytes());
            b.extend_from_slice(&map.to_le_bytes());
            b.extend_from_slice(&cx.to_le_bytes());
            b.extend_from_slice(&cy.to_le_bytes());
            b.push(cell.kind);
            b.extend_from_slice(&cell.level.to_le_bytes());
            b.push(cell.flags);
            b.push(cell.fade);
            b.extend_from_slice(&actor.to_le_bytes());
        }
        ServerRecord::Area {
            seq,
            stroke,
            map,
            ax,
            ay,
            area_id,
            actor,
        } => {
            b.push(b'A');
            b.extend_from_slice(&seq.to_le_bytes());
            b.extend_from_slice(&stroke.to_le_bytes());
            b.extend_from_slice(&map.to_le_bytes());
            b.extend_from_slice(&ax.to_le_bytes());
            b.extend_from_slice(&ay.to_le_bytes());
            b.extend_from_slice(&area_id.to_le_bytes());
            b.extend_from_slice(&actor.to_le_bytes());
        }
        ServerRecord::OverlayDone {
            layer,
            map,
            rx,
            ry,
            epoch,
            last_seq,
            more,
        } => {
            b.push(b'O');
            b.push(layer);
            b.extend_from_slice(&map.to_le_bytes());
            b.extend_from_slice(&rx.to_le_bytes());
            b.extend_from_slice(&ry.to_le_bytes());
            b.extend_from_slice(&epoch.to_le_bytes());
            b.extend_from_slice(&last_seq.to_le_bytes());
            b.push(more as u8);
        }
        ServerRecord::OverlayReset {
            layer,
            map,
            rx,
            ry,
            epoch,
        } => {
            b.push(b'Z');
            b.push(layer);
            b.extend_from_slice(&map.to_le_bytes());
            b.extend_from_slice(&rx.to_le_bytes());
            b.extend_from_slice(&ry.to_le_bytes());
            b.extend_from_slice(&epoch.to_le_bytes());
        }
    }
    b
}

/// Hex of several records concatenated (one whisper payload).
pub fn encode_records_hex(rs: &[ServerRecord]) -> String {
    to_hex(&rs.iter().flat_map(encode_record).collect::<Vec<u8>>())
}

/// CONTRACT §4 confirm rule: a server `E` is the echo of OUR prediction `p` iff the nonce matches
/// AND the body (map, x, y, z, radius, mode, strength) is equal — nonces are per sender only, so a
/// nonce alone can collide with another GM's.
pub fn is_echo_of(server: &VoxelEditOp, p: &VoxelEditOp) -> bool {
    // The server stores the floats in SQL and re-encodes them as f32; allow a hair of slack.
    let close = |a: f32, b: f32| (a - b).abs() <= 1e-3;
    p.local
        && p.seq.is_none()
        && server.nonce == p.nonce
        && server.map == p.map
        && (0..3).all(|i| close(server.pos[i], p.pos[i]))
        && close(server.radius, p.radius)
        && server.mode == p.mode
        && server.strength == p.strength
}

/// MONKEY (voxel): CONTRACT §9.2 confirm rule for a v3 `e`: the v2 body rule ([`is_echo_of`]) AND
/// `stroke`, `param` equal AND `actor` is our own character's low GUID.
pub fn is_echo_of3(server: &VoxelEditOp, actor: u32, my_low_guid: u32, p: &VoxelEditOp) -> bool {
    actor == my_low_guid && server.stroke == p.stroke && server.param == p.param && is_echo_of(server, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(seq: Option<u32>, nonce: u16) -> VoxelEditOp {
        VoxelEditOp {
            map: 0,
            pos: [-8962.08, -110.517, 84.1667],
            radius: 3.0,
            mode: 0,
            strength: 255,
            nonce,
            seq,
            local: false,
            ..Default::default()
        }
    }

    /// MONKEY (voxel): the CONTRACT §9.1 / §9.2 example vectors, byte for byte.
    #[test]
    fn edit3_and_e_record_match_the_contract_vectors() {
        let e = VoxelEditOp {
            map: 0,
            pos: [100.0, -50.0, 20.0],
            radius: 4.0,
            mode: crate::world::MODE_FLATTEN,
            strength: 255,
            nonce: 7,
            stroke: 3,
            param: 150,
            seq: None,
            local: false,
        };
        // Copied verbatim from CONTRACT §9.1 (spaces are for reading only).
        let up = "02 0700 0300 0000 0000c842 000048c2 0000a041 00008040 03 ff 9600".replace(' ', "");
        assert_eq!(e.to_hex3(), up);
        assert_eq!(up.len(), 54);
        assert_eq!(VoxelEditOp::from_hex3(&up), Some(e));
        assert_eq!(edit3_command(&e), format!(".mvox {up}"));
        assert_eq!(hello_command_v(CLIENT_VERSION_V3), ".mvox 1003");
        // 'e': seq 1000, actor 42.
        // CONTRACT §9.2.
        let down = "65 e8030000 0700 0300 0000 0000c842 000048c2 0000a041 00008040 03 ff 9600 2a000000"
            .replace(' ', "");
        assert_eq!(down.len(), 70);
        let mut se = e;
        se.seq = Some(1000);
        let (r, err) = decode_records(&down);
        assert_eq!(err, None);
        assert_eq!(r, vec![ServerRecord::Edit3 { edit: se, actor: 42 }]);
        assert_eq!(encode_records_hex(&r), down);
        // Mixed with v2 records in one payload; a flatten param is an i16 (negative offsets).
        let mut neg = se;
        neg.param = (-250i16) as u16;
        assert_eq!(neg.flatten_plane(), 17.5);
        let recs = [
            ServerRecord::Hello { version: 3 },
            ServerRecord::Edit3 { edit: neg, actor: 7 },
            ServerRecord::Reject { nonce: 7, reason: 6 },
        ];
        let (back, err) = decode_records(&encode_records_hex(&recs));
        assert_eq!(err, None);
        assert_eq!(back, recs.to_vec());
        assert!(reject_reason(6).starts_with("stroke"));
        // Bad bodies: wrong op, wrong length, unknown mode.
        assert!(VoxelEditOp::from_hex3(&e.to_hex()).is_none());
        let mut bad = e;
        bad.mode = 6;
        assert!(VoxelEditOp::from_hex3(&bad.to_hex3()).is_none());
        let mut paint = e;
        paint.mode = crate::world::MODE_ROUGHEN;
        assert!(VoxelEditOp::from_hex3(&paint.to_hex3()).is_some(), "4/5 parse");
    }

    #[test]
    fn confirm3_needs_stroke_param_and_our_actor() {
        let mut p = edit(None, 42);
        p.local = true;
        p.stroke = 9;
        p.mode = crate::world::MODE_FLATTEN;
        p.param = 120;
        let mut s = p;
        s.local = false;
        s.seq = Some(5);
        assert!(is_echo_of3(&s, 77, 77, &p));
        assert!(!is_echo_of3(&s, 78, 77, &p), "another character's edit");
        let mut st = s;
        st.stroke = 10;
        assert!(!is_echo_of3(&st, 77, 77, &p));
        let mut pa = s;
        pa.param = 121;
        assert!(!is_echo_of3(&pa, 77, 77, &p));
    }

    #[test]
    fn records_round_trip_singly_and_concatenated() {
        let recs = [
            ServerRecord::Hello { version: 2 },
            ServerRecord::Edit(edit(Some(77), 9)),
            ServerRecord::Done {
                map: 0,
                rx: -141,
                ry: -2,
                epoch: 3,
                last_seq: 77,
                more: true,
            },
            ServerRecord::Reset {
                map: 1,
                rx: -140,
                ry: 5,
                epoch: 2,
            },
            ServerRecord::Reject {
                nonce: 0xbeef,
                reason: 1,
            },
        ];
        let lens = [2, 27, 14, 9, 4];
        for (r, len) in recs.iter().zip(lens) {
            let hex = encode_records_hex(std::slice::from_ref(r));
            assert_eq!(hex.len(), len * 2);
            let (back, err) = decode_records(&hex);
            assert_eq!(err, None);
            assert_eq!(back, vec![*r]);
        }
        let all = encode_records_hex(&recs);
        let (back, err) = decode_records(&all);
        assert_eq!(err, None);
        assert_eq!(back, recs.to_vec());
        // A cut record keeps the ones before it.
        let (part, err) = decode_records(&all[..all.len() - 2]);
        assert_eq!(err, Some(WireError::Truncated));
        assert_eq!(part.len(), 4);
        assert_eq!(decode_records("zz").1, Some(WireError::BadHex));
        assert_eq!(decode_records("7100").1, Some(WireError::UnknownType(b'q')));
        assert_eq!(decode_records("4100").1, Some(WireError::Truncated), "A = v3 area record");
    }

    /// MONKEY (EME, CONTRACT v3.5): `K` = 2 bytes, the caller's GM rank.
    #[test]
    fn eme_rank_record_is_two_bytes() {
        let (back, err) = decode_records("4b03");
        assert_eq!(err, None);
        assert_eq!(back, vec![ServerRecord::EmeRank { rank: 3 }]);
        assert_eq!(encode_records_hex(&[ServerRecord::EmeRank { rank: 0 }]), "4b00");
        assert_eq!(decode_records("4b").1, Some(WireError::Truncated));
    }

    #[test]
    fn server_lua_records_decode() {
        // Byte-exact against MonkeyVoxelServer.lua's RecDone / RecReject / RecHello shapes.
        let (r, _) = decode_records("4802");
        assert_eq!(r, vec![ServerRecord::Hello { version: 2 }]);
        // D map 0, rx -141 (0xff73), ry -2 (0xfffe), epoch 1, last 3, more 0.
        let (r, _) = decode_records("44000073fffeff01000300000000");
        assert_eq!(
            r,
            vec![ServerRecord::Done {
                map: 0,
                rx: -141,
                ry: -2,
                epoch: 1,
                last_seq: 3,
                more: false
            }]
        );
        let (r, _) = decode_records("52070001");
        assert_eq!(r, vec![ServerRecord::Reject { nonce: 7, reason: 1 }]);
        assert_eq!(server_payload("MONKEYVOXEL\t4802"), Some("4802"));
        assert_eq!(server_payload("MV2:01"), None);
    }

    #[test]
    fn uplink_commands() {
        assert_eq!(hello_command(), ".mvox 1002");
        let e = edit(None, 1);
        let c = edit_command(&e);
        assert_eq!(c.len(), ".mvox ".len() + 46);
        assert!(c.starts_with(".mvox 01"));
        assert_eq!(c, c.to_lowercase());
        let entries: Vec<SyncEntry> = (0..30)
            .map(|i| SyncEntry {
                rx: -141 + i,
                ry: -2,
                epoch: i as u16,
                after_seq: 1000 + i as u32,
            })
            .collect();
        let cmds = sync_commands(0, &entries);
        assert_eq!(cmds.len(), 3, "30 entries → 12 + 12 + 6");
        let bodies: Vec<Vec<u8>> = cmds
            .iter()
            .map(|c| from_hex(c.strip_prefix(".mvox ").unwrap()).unwrap())
            .collect();
        assert_eq!(bodies[0].len(), 4 + 10 * 12);
        assert_eq!(bodies[2].len(), 4 + 10 * 6);
        assert_eq!(bodies[2][3], 6, "count byte");
        assert_eq!(&bodies[0][..3], &[0x11, 0, 0]);
        // Entry 0: rx -141, ry -2, epoch 0, after 1000.
        assert_eq!(&bodies[0][4..14], &[0x73, 0xff, 0xfe, 0xff, 0, 0, 0xe8, 0x03, 0, 0]);
        // The last entry of the last command.
        let last = &bodies[2][4 + 50..];
        assert_eq!(i16::from_le_bytes([last[0], last[1]]), -141 + 29);
        assert_eq!(u32::from_le_bytes([last[6], last[7], last[8], last[9]]), 1029);
    }

    #[test]
    fn confirm_matches_nonce_and_body() {
        let mut p = edit(None, 42);
        p.local = true;
        let s = edit(Some(5), 42);
        assert!(is_echo_of(&s, &p));
        let mut other_gm = s;
        other_gm.pos[0] += 4.0; // same nonce, another GM's edit
        assert!(!is_echo_of(&other_gm, &p));
        let mut other_nonce = s;
        other_nonce.nonce = 43;
        assert!(!is_echo_of(&other_nonce, &p));
        let mut other_mode = s;
        other_mode.mode = 1;
        assert!(!is_echo_of(&other_mode, &p));
        let mut relayed = p;
        relayed.local = false; // not our prediction
        assert!(!is_echo_of(&s, &relayed));
    }

    /// MONKEY (EME): the CONTRACT §11.3 overlay vectors, byte for byte.
    #[test]
    fn overlay_records_match_the_contract_vectors() {
        use crate::overlay::{LiquidCell, LAYER_AREA, LAYER_LIQUID};
        // Copied verbatim from CONTRACT §11.3.
        let l = "4c 05000000 0300 0000 6400 ecff 01 00004841 00 28 2a000000".replace(' ', "");
        assert_eq!(l.len(), LIQUID_RECORD_LEN * 2);
        let want_l = ServerRecord::Liquid {
            seq: 5,
            stroke: 3,
            map: 0,
            cx: 100,
            cy: -20,
            cell: LiquidCell {
                kind: 1,
                level: 12.5,
                flags: 0,
                fade: 40,
            },
            actor: 42,
        };
        let a = "41 06000000 0300 0000 0a00 f6ff 4a00 2a000000".replace(' ', "");
        assert_eq!(a.len(), AREA_RECORD_LEN * 2);
        let want_a = ServerRecord::Area {
            seq: 6,
            stroke: 3,
            map: 0,
            ax: 10,
            ay: -10,
            area_id: 74,
            actor: 42,
        };
        let (r, err) = decode_records(&format!("{l}{a}"));
        assert_eq!(err, None);
        assert_eq!(r, vec![want_l, want_a]);
        assert_eq!(encode_records_hex(&r), format!("{l}{a}"));
        // O / Z mixed with voxel records round-trip.
        let recs = [
            ServerRecord::OverlayReset {
                layer: LAYER_LIQUID,
                map: 0,
                rx: -141,
                ry: -2,
                epoch: 1,
            },
            want_l,
            ServerRecord::OverlayDone {
                layer: LAYER_AREA,
                map: 1,
                rx: -140,
                ry: -2,
                epoch: 0,
                last_seq: 1,
                more: true,
            },
            ServerRecord::Hello { version: 3 },
        ];
        let hex = encode_records_hex(&recs);
        assert_eq!(hex.len(), 2 * (10 + 24 + 15 + 2));
        let (back, err) = decode_records(&hex);
        assert_eq!(err, None);
        assert_eq!(back, recs.to_vec());
        // The Lua RecOverlayDone shape: O layer 2, map 0, (-140, -2), epoch 0, last 1, more 0.
        let (r, _) = decode_records("4f02000074fffeff00000100000000");
        assert_eq!(
            r,
            vec![ServerRecord::OverlayDone {
                layer: 2,
                map: 0,
                rx: -140,
                ry: -2,
                epoch: 0,
                last_seq: 1,
                more: false
            }]
        );
        // A NaN level / an unknown layer are bad records (the ones before them survive).
        let nan = "4c 05000000 0300 0000 6400 ecff 01 0000c07f 00 28 2a000000".replace(' ', "");
        assert_eq!(decode_records(&nan).1, Some(WireError::BadOverlay));
        let (r, err) = decode_records("48035a070000000000000000");
        assert_eq!(r, vec![ServerRecord::Hello { version: 3 }]);
        assert_eq!(err, Some(WireError::BadOverlay));
    }

    /// MONKEY (EME): the CONTRACT §9.1 OSYNC example and batching.
    #[test]
    fn osync_matches_the_contract_vector() {
        let e = SyncEntry {
            rx: -141,
            ry: -2,
            epoch: 0,
            after_seq: 0,
        };
        let want = "12 01 0000 01 73ff feff 0000 00000000".replace(' ', "");
        assert_eq!(to_hex(&osync_body(1, 0, &[e])), want);
        assert_eq!(osync_commands(1, 0, &[e]), vec![format!(".mvox {want}")]);
        let many: Vec<SyncEntry> = (0..13).map(|i| SyncEntry { rx: i, ..e }).collect();
        let cmds = osync_commands(2, 1, &many);
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].starts_with(".mvox 120201000b"), "{}", cmds[0]);
        assert!(cmds[1].starts_with(".mvox 1202010002"), "{}", cmds[1]);
        // Every line fits the core's 255-char chat limit (12 entries would be 256).
        assert!(cmds.iter().all(|c| c.len() <= 255));
        assert_eq!(cmds[0].len(), 6 + 2 * (5 + 10 * 11));
    }
}

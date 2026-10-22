//! MONKEY (voxel): the v2 server-authority wire (CONTRACT §3–4, `disposable/voxel-v2/CONTRACT.md`),
//! engine-free so it is unit-tested here.
//!
//! - Uplink (client → server): `.mvox <hex>` chat commands — HELLO `10 02`, SYNC `11 map count
//!   count×(rx ry epoch after_seq)` (≤ 12 entries, 4 + 10·n bytes), EDIT = the 23-byte
//!   [`VoxelEditOp::to_wire`] body. Little-endian, hex lowercase.
//! - Downlink (server → client): addon whisper `MONKEYVOXEL<TAB><hex>`, one or more fixed-size
//!   records concatenated: `H` hello-ack (2), `E` edit (27), `D` region-done (14), `X`
//!   region-reset (9), `R` reject (4).

use crate::world::{VoxelEditOp, EDIT_WIRE_LEN};

/// The addon prefix of the server's whispers.
pub const SERVER_PREFIX: &str = "MONKEYVOXEL";
/// The chat command of the uplink.
pub const MVOX_COMMAND: &str = ".mvox";
/// Our protocol version (HELLO byte 2) and the one we expect back in `H`.
pub const CLIENT_VERSION: u8 = 2;
/// Max SYNC entries per command (CONTRACT §3).
pub const SYNC_MAX_ENTRIES: usize = 12;

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
    /// `E`: an accepted edit with its global `seq` (`edit.seq = Some`, `edit.local = false`).
    Edit(VoxelEditOp),
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
    /// `R`: our EDIT `nonce` was refused (1 rank, 2 range, 3 rate, 4 invalid, 5 map).
    Reject { nonce: u16, reason: u8 },
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
}

/// The human reason of an `R` code.
pub fn reject_reason(code: u8) -> &'static str {
    match code {
        1 => "rank (not a GM)",
        2 => "range (too far from the brush)",
        3 => "rate (too many edits)",
        4 => "invalid edit",
        5 => "map (not on that map)",
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

/// `.mvox 1002` — HELLO.
pub fn hello_command() -> String {
    format!("{MVOX_COMMAND} 10{CLIENT_VERSION:02x}")
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
            b'E' => 27,
            b'D' => 14,
            b'X' => 9,
            b'R' => 4,
            other => return (out, Some(WireError::UnknownType(other))),
        };
        if o + len > b.len() {
            return (out, Some(WireError::Truncated));
        }
        let rec = match t {
            b'H' => ServerRecord::Hello { version: b[o + 1] },
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
        ServerRecord::Edit(e) => {
            b.push(b'E');
            b.extend_from_slice(&e.seq.unwrap_or(0).to_le_bytes());
            b.extend_from_slice(&e.to_wire()[1..]);
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
        }
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
        assert_eq!(decode_records("4100").1, Some(WireError::UnknownType(b'A')));
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
}

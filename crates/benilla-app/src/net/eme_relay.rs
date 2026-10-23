//! MONKEY (EME): `EmeChatTransport` — EME's transport over the existing voxel relay link
//! (DESIGN-EME §2.4, CONTRACT v3 §12), behind the `eme` feature.
//!
//! - **Hot path** (voxel edits): nothing here. EME submits its strokes through the voxel seam
//!   (`MonkeyVoxelApi::submit`), the voxel relay uplinks them as `.mvox 02…` EDIT3 (v3) and routes
//!   the server's `e` / `R` back into the voxel layer, whose `MonkeyVoxelEvent`s EME turns into
//!   confirmations / rejections for its undo stack. One wire, one journal.
//! - **Link**: the relay's mode + negotiated version → [`EmeWire::set_link`]: SERVER v3 = `Up`,
//!   SERVER v2 / guild RELAY = `Unsupported` (EME shows nothing, even to a GM), probing =
//!   `Handshaking`, out of the world = `Down`.
//! - **Cold path** (service JSON): each queued request's envelope → base64url (no padding) →
//!   chunks of ≤ 200 chars → `.mveme <fid>/<part>/<total> <chunk>` SAY lines, paced ≤ 10 lines/s;
//!   replies and pushes arrive as `MONKEYEME<TAB><fid>/<part>/<total>/<chunk>` addon whispers from
//!   ourselves (the server's self-whisper; a player cannot forge that sender), reassembled (≤ 4
//!   incomplete frames, 5 s expiry, duplicate parts ignored), decoded and parsed into
//!   [`EmeInbound::Reply`] / [`EmeInbound::Push`]. A request over 2800 JSON bytes is never sent:
//!   it fails at once with `too_large`.

use std::collections::{HashMap, VecDeque};

use bevy::prelude::*;

use benilla_protocol::{SessionEvent, SessionEventKind};
use eme::EmeWire;
use eme_core::service::EME_MAX_FRAME_BYTES;
use eme_core::{EmeDownlink, EmeInbound, EmeLink, EmeServiceReply};

use super::voxel_relay::{MonkeyVoxelLink, MonkeyVoxelMode};
use crate::net::{ChatKind, ClientCommand, NetCommands, NetHandlerApp, SelfGuid};

/// The addon prefix of the server's EME whispers.
pub(crate) const EME_PREFIX: &str = "MONKEYEME";
/// The uplink chat command.
pub(crate) const MVEME_COMMAND: &str = ".mveme";
/// base64url chars per frame part.
const CHUNK: usize = 200;
/// Parts per frame at most.
const MAX_PARTS: usize = 19;
/// Uplink pacing: one `.mveme` line per this many seconds (≤ 10/s).
const PACE_S: f32 = 0.1;
/// An incomplete downlink frame is dropped after this long.
const FRAME_TTL_S: f32 = 5.0;
/// Incomplete downlink frames kept at most.
const MAX_INCOMPLETE: usize = 4;

/// `CHAT_MSG_WHISPER` / `_INFORM`: the server's self-whisper lane.
const CHAT_WHISPER: u8 = 0x06;
const CHAT_WHISPER_INFORM: u8 = 0x07;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// RFC 4648 §5 base64url, no padding.
pub(crate) fn b64url_encode(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        let chars = c.len() + 1;
        for i in 0..chars {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Decodes base64url (padding tolerated); `None` on a bad character or length.
pub(crate) fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    if s.len() % 4 == 1 {
        return None;
    }
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, &ch) in c.iter().enumerate() {
            n |= val(ch)? << (18 - 6 * i);
        }
        let bytes = c.len() - 1;
        for i in 0..bytes {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

/// The `.mveme` lines of one request frame; `None` = too large (> 2800 bytes or > 19 parts).
pub(crate) fn frame_lines(fid: u16, json: &str) -> Option<Vec<String>> {
    if json.len() > EME_MAX_FRAME_BYTES {
        return None;
    }
    let enc = b64url_encode(json.as_bytes());
    let parts: Vec<&str> = enc
        .as_bytes()
        .chunks(CHUNK)
        .map(|c| std::str::from_utf8(c).expect("base64 is ascii"))
        .collect();
    let total = parts.len().max(1);
    if total > MAX_PARTS {
        return None;
    }
    if parts.is_empty() {
        return Some(vec![format!("{MVEME_COMMAND} {fid}/1/1 ")]);
    }
    Some(
        parts
            .iter()
            .enumerate()
            .map(|(i, p)| format!("{MVEME_COMMAND} {fid}/{}/{total} {p}", i + 1))
            .collect(),
    )
}

/// One downlink frame being reassembled.
#[derive(Debug)]
struct Partial {
    parts: Vec<Option<String>>,
    started: f32,
}

/// Reassembles `<fid>/<part>/<total>/<chunk>` downlink parts into JSON frames.
#[derive(Debug, Default)]
pub(crate) struct EmeReassembler {
    frames: HashMap<u16, Partial>,
}

impl EmeReassembler {
    /// Feed one whisper payload at time `now` (s); returns the frame's JSON when it completes.
    pub(crate) fn push(&mut self, text: &str, now: f32) -> Option<String> {
        self.frames.retain(|_, p| now - p.started <= FRAME_TTL_S);
        let mut it = text.splitn(4, '/');
        let fid: u16 = it.next()?.trim().parse().ok()?;
        let part: usize = it.next()?.parse().ok()?;
        let total: usize = it.next()?.parse().ok()?;
        let chunk = it.next().unwrap_or("").trim();
        if total == 0 || total > MAX_PARTS || part == 0 || part > total || chunk.len() > CHUNK {
            return None;
        }
        if total == 1 {
            self.frames.remove(&fid);
            return Self::decode(chunk);
        }
        if !self.frames.contains_key(&fid) && self.frames.len() >= MAX_INCOMPLETE {
            // Drop the oldest incomplete frame.
            if let Some(old) = self
                .frames
                .iter()
                .min_by(|a, b| a.1.started.total_cmp(&b.1.started))
                .map(|(k, _)| *k)
            {
                self.frames.remove(&old);
            }
        }
        let p = self.frames.entry(fid).or_insert_with(|| Partial {
            parts: vec![None; total],
            started: now,
        });
        if p.parts.len() != total {
            // A new frame reusing the id (the old one never completed).
            *p = Partial {
                parts: vec![None; total],
                started: now,
            };
        }
        if p.parts[part - 1].is_none() {
            p.parts[part - 1] = Some(chunk.to_owned());
        }
        if p.parts.iter().all(Option::is_some) {
            let p = self.frames.remove(&fid)?;
            let joined: String = p.parts.into_iter().map(Option::unwrap_or_default).collect();
            return Self::decode(&joined);
        }
        None
    }

    fn decode(b64: &str) -> Option<String> {
        String::from_utf8(b64url_decode(b64)?).ok()
    }
}

/// The chat transport's state.
#[derive(Resource, Default)]
pub(crate) struct EmeChatTransport {
    /// `.mveme` lines waiting for the pacer.
    lines: VecDeque<String>,
    last_line: f32,
    /// Whisper payloads received since the last tick.
    rx: Vec<String>,
    reasm: EmeReassembler,
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<EmeChatTransport>()
        .net_handler(SessionEventKind::Chat, eme_chat_recv)
        .add_systems(
            Update,
            (eme_chat_link, eme_chat_rx, eme_chat_send)
                .chain()
                .before(eme::EmeSet::Input),
        );
}

/// The relay's link → EME's.
fn eme_chat_link(link: Res<MonkeyVoxelLink>, mut wire: ResMut<EmeWire>) {
    if wire.offline {
        return;
    }
    let l = match link.mode {
        MonkeyVoxelMode::Idle => EmeLink::Down,
        MonkeyVoxelMode::Probing => EmeLink::Handshaking,
        MonkeyVoxelMode::Server if link.version >= 3 => EmeLink::Up {
            server_version: link.version,
        },
        MonkeyVoxelMode::Server | MonkeyVoxelMode::Relay => EmeLink::Unsupported,
    };
    wire.set_link(l);
}

/// Received payloads → frames → replies / pushes.
fn eme_chat_rx(time: Res<Time<Real>>, mut chat: ResMut<EmeChatTransport>, mut wire: ResMut<EmeWire>) {
    if chat.rx.is_empty() {
        return;
    }
    let now = time.elapsed_secs();
    let chat = &mut *chat;
    for text in std::mem::take(&mut chat.rx) {
        let Some(json) = chat.reasm.push(&text, now) else {
            continue;
        };
        match EmeDownlink::from_json(&json) {
            Ok(EmeDownlink::Reply(r)) => wire.inbound.push(EmeInbound::Reply(r)),
            Ok(EmeDownlink::Push(p)) => wire.inbound.push(EmeInbound::Push(p)),
            Err(e) => warn!("EME: bad MONKEYEME frame ({e}): {json:?}"),
        }
    }
}

/// Queued service requests → paced `.mveme` lines.
fn eme_chat_send(
    time: Res<Time<Real>>,
    commands: Res<NetCommands>,
    mut chat: ResMut<EmeChatTransport>,
    mut wire: ResMut<EmeWire>,
) {
    if wire.offline {
        return;
    }
    for s in wire.take_services() {
        match frame_lines(s.id.0, &s.json) {
            Some(lines) => chat.lines.extend(lines),
            None => {
                warn!("EME: {} request {} too large ({} B) — not sent", s.request.op(), s.id.0, s.json.len());
                wire.inbound
                    .push(EmeInbound::Reply(EmeServiceReply::error(s.id, "too_large")));
            }
        }
    }
    if !wire.link.is_usable() {
        chat.lines.clear();
        return;
    }
    let now = time.elapsed_secs();
    if now - chat.last_line < PACE_S {
        return;
    }
    if let Some(line) = chat.lines.pop_front() {
        chat.last_line = now;
        let _ = commands.0.send(ClientCommand::Chat {
            kind: ChatKind::Say,
            target: None,
            text: line,
            language: None,
        });
    }
}

/// `MONKEYEME<TAB>…` addon whispers from ourselves → the reassembler.
fn eme_chat_recv(
    In(ev): In<SessionEvent>,
    mut chat: ResMut<EmeChatTransport>,
    self_guid: Res<SelfGuid>,
) {
    let SessionEvent::Chat(m) = ev else {
        return;
    };
    if !m.is_addon() {
        return;
    }
    let Some(payload) = m
        .text
        .strip_prefix(EME_PREFIX)
        .and_then(|t| t.strip_prefix('\t'))
    else {
        return;
    };
    if self_guid.0 != Some(m.sender_guid)
        || !matches!(m.chat_type, CHAT_WHISPER | CHAT_WHISPER_INFORM)
    {
        warn!("EME: ignoring a MONKEYEME line not whispered by the server to us");
        return;
    }
    chat.rx.push(payload.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_every_length() {
        for n in 0..40usize {
            let b: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37).wrapping_add(250)).collect();
            let e = b64url_encode(&b);
            assert!(!e.contains('=') && !e.contains('+') && !e.contains('/'));
            assert_eq!(b64url_decode(&e).unwrap(), b, "n={n}");
        }
        assert_eq!(b64url_encode(b"{\"v\":1}"), "eyJ2IjoxfQ");
        assert_eq!(b64url_decode("eyJ2IjoxfQ").unwrap(), b"{\"v\":1}");
        assert!(b64url_decode("a").is_none());
        assert!(b64url_decode("ab$d").is_none());
    }

    #[test]
    fn request_frames_split_at_200_chars_and_reassemble_in_any_order() {
        let json = format!(r#"{{"v":1,"id":7,"op":"snapshot_create","label":"{}"}}"#, "é".repeat(200));
        let lines = frame_lines(7, &json).unwrap();
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(l.len() <= 255, "line {} chars", l.len());
            assert!(l.starts_with(".mveme 7/"));
        }
        // The server's downlink format is `<fid>/<part>/<total>/<chunk>`: rebuild it from ours.
        let mut parts: Vec<String> = lines
            .iter()
            .map(|l| {
                let rest = l.strip_prefix(".mveme ").unwrap();
                let (hdr, chunk) = rest.split_once(' ').unwrap();
                format!("{hdr}/{chunk}")
            })
            .collect();
        parts.reverse();
        let mut r = EmeReassembler::default();
        let mut got = None;
        for (i, p) in parts.iter().enumerate() {
            let out = r.push(p, 0.0);
            // A duplicate part is ignored.
            assert!(r.push(p, 0.0).is_none() || i + 1 == parts.len());
            if out.is_some() {
                got = out;
            }
        }
        assert_eq!(got.as_deref(), Some(json.as_str()));
        assert!(frame_lines(1, &"x".repeat(EME_MAX_FRAME_BYTES + 1)).is_none());
    }

    #[test]
    fn incomplete_frames_expire_and_are_capped() {
        let mut r = EmeReassembler::default();
        for fid in 1..=6u16 {
            assert!(r.push(&format!("{fid}/1/2/eyJ2"), fid as f32 * 0.1).is_none());
        }
        assert!(r.frames.len() <= MAX_INCOMPLETE);
        assert!(r.push("6/2/2/IjoxfQ", 1.0).is_some());
        assert!(r.push("5/2/2/IjoxfQ", 10.0).is_none(), "expired after 5 s");
    }

    #[test]
    fn a_server_reply_parses_into_an_inbound_reply() {
        let json = r#"{"v":1,"id":3,"ok":true,"rank":3,"wire":3,"tools":["sculpt"]}"#;
        let line = format!("3/1/1/{}", b64url_encode(json.as_bytes()));
        let mut r = EmeReassembler::default();
        let got = r.push(&line, 0.0).unwrap();
        match EmeDownlink::from_json(&got).unwrap() {
            EmeDownlink::Reply(rep) => assert!(rep.ok && rep.id.0 == 3),
            other => panic!("{other:?}"),
        }
    }
}

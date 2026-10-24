//! The transport seam (DESIGN-EME §2.4): how EME talks to the server, without saying how.
//!
//! Implementations live outside this crate (the chat transport in `benilla-app/src/net/eme_relay.rs`;
//! others may add their own). A transport moves two kinds of traffic:
//! - the **hot path**: voxel EDIT3 frames (`.mvox`), confirmed by the server's `e`/`R` records;
//! - the **cold path**: service JSON frames ([`crate::service`]), answered by replies and pushes.
//!
//! The binary EDIT3 codec belongs to `benilla-voxel`; the transport converts its records into the
//! engine-free [`EmeInbound`] values below.

use crate::service::{
    EmeRequestId, EmeServiceEnvelope, EmeServicePush, EmeServiceReply, EmeServiceRequest,
    EME_MAX_REQUEST_ID,
};

/// One voxel brush application as EME emits it (the EDIT3 fields minus the op byte).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeVoxelEdit {
    /// Per-sender nonce, echoed by the server's confirmation.
    pub nonce: u16,
    /// Stroke id 1..=65535 (wraps 65535 → 1); 0 = "no stroke" (not undoable per stroke).
    pub stroke: u16,
    /// Map id.
    pub map: u16,
    /// Brush centre, WoW yards (x north, y west, z up).
    pub pos: [f32; 3],
    /// Brush radius, yards.
    pub radius: f32,
    /// 0 dig, 1 build, 2 smooth, 3 flatten, 4 paint, 5 roughen.
    pub mode: u8,
    /// Strength /255.
    pub strength: u8,
    /// Mode parameter: flatten = plane offset in cm as an **i16** two's complement
    /// (`z_plane = z + (param as i16) / 100`); paint = material id; roughen = PRNG seed; else 0.
    pub param: u16,
}

/// Voxel brush modes (EDIT3 `mode`).
pub mod mode {
    /// Subtract a sphere.
    pub const DIG: u8 = 0;
    /// Add a sphere.
    pub const BUILD: u8 = 1;
    /// Smooth (not idempotent).
    pub const SMOOTH: u8 = 2;
    /// Pull toward the plane `z = pos.z + (param as i16) / 100`.
    pub const FLATTEN: u8 = 3;
    /// Paint material `param`.
    pub const PAINT: u8 = 4;
    /// Roughen with seed `param`.
    pub const ROUGHEN: u8 = 5;

    /// Whether re-applying the same edit at the same place changes nothing (CSG union/subtract).
    /// Mode only: a SOFT dig / build ([`is_soft`]) is not idempotent — use [`is_idempotent_edit`].
    pub const fn is_idempotent(mode: u8) -> bool {
        matches!(mode, DIG | BUILD | PAINT)
    }

    /// CONTRACT v3.3: `param` bit 15 on modes 0 / 1 / 2 = a SOFT brush (lower / raise / smooth
    /// with a radial falloff; `benilla_voxel::world::SoftBrush`).
    pub const SOFT: u16 = 0x8000;
    /// Soft falloff (`param` bits 0-1): smoothstep from the inner (hardness) disc to the rim.
    pub const FALLOFF_SMOOTH: u8 = 0;
    /// Soft falloff: linear to the rim.
    pub const FALLOFF_LINEAR: u8 = 1;
    /// Soft falloff: none (full amount up to the rim).
    pub const FALLOFF_FLAT: u8 = 2;

    /// Whether `(mode, param)` is a soft brush.
    pub const fn is_soft(mode: u8, param: u16) -> bool {
        matches!(mode, DIG | BUILD | SMOOTH) && param & SOFT != 0
    }

    /// The soft-brush `param`: bit 15, falloff in bits 0-1, hardness percent (0..=100) in 2-8.
    pub const fn soft_param(falloff: u8, hardness: u8) -> u16 {
        let f = if falloff > 2 { 0 } else { falloff };
        let h = if hardness > 100 { 100 } else { hardness };
        SOFT | f as u16 | (h as u16) << 2
    }

    /// CONTRACT v3.6: brush shape in `param` bits 9-10 (modes 0 / 1, hard and soft): a sphere
    /// (every earlier edit) …
    pub const SHAPE_SPHERE: u8 = 0;
    /// … an axis-aligned cube (half-size = radius) …
    pub const SHAPE_CUBE: u8 = 1;
    /// … or a vertical cylinder (radius across, half-height = radius). 3 = reserved (a sphere).
    pub const SHAPE_CYLINDER: u8 = 2;

    /// `param` with its shape bits (9-10) set to `shape` (> 2 = sphere).
    pub const fn with_shape(param: u16, shape: u8) -> u16 {
        let s = if shape > 2 { 0 } else { shape as u16 };
        (param & !(0x3 << 9)) | s << 9
    }

    /// The shape of `(mode, param)` (a sphere for every mode but dig / build, and for shape 3).
    pub const fn shape_of(mode: u8, param: u16) -> u8 {
        if !matches!(mode, DIG | BUILD) {
            return SHAPE_SPHERE;
        }
        let s = ((param >> 9) & 0x3) as u8;
        if s > 2 {
            SHAPE_SPHERE
        } else {
            s
        }
    }

    /// [`is_idempotent`] for a whole edit: a soft dig / build accumulates (holding it still keeps
    /// raising / lowering), so it keeps the stroke builder's time trigger.
    pub const fn is_idempotent_edit(mode: u8, param: u16) -> bool {
        is_idempotent(mode) && !is_soft(mode, param)
    }
}

/// Why the server rejected an edit (`R` record reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmeRejectReason {
    /// 1: rank too low.
    Rank,
    /// 2: out of range.
    Range,
    /// 3: rate limited.
    Rate,
    /// 4: invalid (e.g. unknown mode).
    Invalid,
    /// 5: map not allowed.
    Map,
    /// 6: the stroke id was undone / reverted, or reused after being closed: start a new stroke.
    Stroke,
    /// 7: MONKEY (EME projects, CONTRACT v3.4): no active project.
    NoProject,
    /// Any other code.
    Other(u8),
}

impl EmeRejectReason {
    /// From the `R` record byte.
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Rank,
            2 => Self::Range,
            3 => Self::Rate,
            4 => Self::Invalid,
            5 => Self::Map,
            6 => Self::Stroke,
            7 => Self::NoProject,
            c => Self::Other(c),
        }
    }

    /// The `R` record byte.
    pub fn code(self) -> u8 {
        match self {
            Self::Rank => 1,
            Self::Range => 2,
            Self::Rate => 3,
            Self::Invalid => 4,
            Self::Map => 5,
            Self::Stroke => 6,
            Self::NoProject => 7,
            Self::Other(c) => c,
        }
    }
}

/// Link state of a transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmeLink {
    /// Not connected / not in world.
    #[default]
    Down,
    /// HELLO sent, waiting for the ACK.
    Handshaking,
    /// The server speaks voxel v3 (strokes) and the service wire: EME is usable.
    Up {
        /// The server's HELLO-ACK version.
        server_version: u8,
    },
    /// The server answered with v2 (no strokes) or never answered: EME shows nothing, even to a GM.
    Unsupported,
}

impl EmeLink {
    /// Whether EME may operate over this link.
    pub fn is_usable(self) -> bool {
        matches!(self, EmeLink::Up { server_version } if server_version >= 3)
    }
}

/// Everything a transport delivers to EME.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeInbound {
    /// The server accepted one of our edits (`e` record with our nonce).
    EditConfirmed {
        /// Our nonce.
        nonce: u16,
        /// Server seq.
        seq: u32,
        /// The stroke the server filed it under.
        stroke: u16,
    },
    /// The server rejected one of our edits (`R` record).
    EditRejected {
        /// Our nonce.
        nonce: u16,
        /// Why.
        reason: EmeRejectReason,
    },
    /// A reply to a service request.
    Reply(EmeServiceReply),
    /// An unsolicited service push.
    Push(EmeServicePush),
    /// The link state changed.
    Link(EmeLink),
}

/// A bidirectional EME channel to the server. Implementations must never block.
pub trait EmeTransport {
    /// Queue one voxel edit for the hot path.
    fn send_edit(&mut self, edit: &EmeVoxelEdit);
    /// Queue one service request; returns the id written into its envelope.
    fn send_service(&mut self, request: &EmeServiceRequest) -> EmeRequestId;
    /// MONKEY (EME objects, CONTRACT v3.7): queue `request` AGAIN under the SAME frame id `id`
    /// (an unanswered idempotent mutation: the server dedups on its `tx` and the reply still
    /// matches the pending request).
    fn resend_service(&mut self, id: EmeRequestId, request: &EmeServiceRequest);
    /// Everything received since the last poll, in arrival order.
    fn poll(&mut self) -> Vec<EmeInbound>;
    /// Current link state.
    fn state(&self) -> EmeLink;
}

/// Allocates service request ids for a transport: 1..=32767, wrapping (0 is the push id).
#[derive(Debug, Clone, Default)]
pub struct EmeRequestIdAlloc {
    last: u16,
}

impl EmeRequestIdAlloc {
    /// The next id.
    pub fn next_id(&mut self) -> EmeRequestId {
        self.last = if self.last >= EME_MAX_REQUEST_ID {
            1
        } else {
            self.last + 1
        };
        EmeRequestId(self.last)
    }

    /// Allocates an id and builds the envelope JSON for `request` (a transport's usual first step).
    pub fn envelope(&mut self, request: &EmeServiceRequest) -> (EmeRequestId, String) {
        let id = self.next_id();
        (id, EmeServiceEnvelope::new(id, request.clone()).to_json())
    }
}

/// An in-memory transport for tests and offline tooling: records what was sent, returns what was
/// queued with [`EmeLoopbackTransport::deliver`].
#[derive(Debug, Default)]
pub struct EmeLoopbackTransport {
    /// Edits sent, in order.
    pub sent_edits: Vec<EmeVoxelEdit>,
    /// Service frames sent, in order, with their ids.
    pub sent_services: Vec<(EmeRequestId, EmeServiceRequest)>,
    inbox: Vec<EmeInbound>,
    ids: EmeRequestIdAlloc,
    link: EmeLink,
}

impl EmeLoopbackTransport {
    /// A loopback in state `link`.
    pub fn new(link: EmeLink) -> Self {
        Self {
            link,
            ..Default::default()
        }
    }

    /// Queue an inbound item for the next poll.
    pub fn deliver(&mut self, inbound: EmeInbound) {
        if let EmeInbound::Link(l) = inbound {
            self.link = l;
        }
        self.inbox.push(inbound);
    }
}

impl EmeTransport for EmeLoopbackTransport {
    fn send_edit(&mut self, edit: &EmeVoxelEdit) {
        self.sent_edits.push(*edit);
    }

    fn send_service(&mut self, request: &EmeServiceRequest) -> EmeRequestId {
        let id = self.ids.next_id();
        self.sent_services.push((id, request.clone()));
        id
    }

    fn resend_service(&mut self, id: EmeRequestId, request: &EmeServiceRequest) {
        self.sent_services.push((id, request.clone()));
    }

    fn poll(&mut self) -> Vec<EmeInbound> {
        std::mem::take(&mut self.inbox)
    }

    fn state(&self) -> EmeLink {
        self.link
    }
}

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
};

/// One voxel brush application as EME emits it (the EDIT3 fields minus the op byte).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeVoxelEdit {
    /// Per-sender nonce, echoed by the server's confirmation.
    pub nonce: u16,
    /// Stroke id; 0 = no stroke (a v2 server).
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
    /// Mode parameter (flatten: plane offset in cm; paint: material; roughen: seed).
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
    /// Pull toward a plane `z = pos.z + param / 100`.
    pub const FLATTEN: u8 = 3;
    /// Paint material `param`.
    pub const PAINT: u8 = 4;
    /// Roughen with seed `param`.
    pub const ROUGHEN: u8 = 5;

    /// Whether re-applying the same edit at the same place changes nothing (CSG union/subtract).
    pub const fn is_idempotent(mode: u8) -> bool {
        matches!(mode, DIG | BUILD | PAINT)
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
    /// Everything received since the last poll, in arrival order.
    fn poll(&mut self) -> Vec<EmeInbound>;
    /// Current link state.
    fn state(&self) -> EmeLink;
}

/// Allocates service request ids for a transport (wrapping, never 0).
#[derive(Debug, Clone, Default)]
pub struct EmeRequestIdAlloc {
    last: u32,
}

impl EmeRequestIdAlloc {
    /// The next id.
    pub fn next_id(&mut self) -> EmeRequestId {
        self.last = self.last.wrapping_add(1);
        if self.last == 0 {
            self.last = 1;
        }
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

    fn poll(&mut self) -> Vec<EmeInbound> {
        std::mem::take(&mut self.inbox)
    }

    fn state(&self) -> EmeLink {
        self.link
    }
}

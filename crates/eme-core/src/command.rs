//! Commands: the only thing a tool produces (DESIGN-EME §2.2). [`crate::bus::EmeDispatcher`]
//! routes them.

use crate::perm::EmeCapability;
use crate::service::{EmeObjMove, EmeObjSpawn, EmeObjectKind, EmeOverlayEdit, EmeServiceRequest};
use crate::tool::EmeToolId;
use crate::transport::EmeVoxelEdit;

/// Where a stroke command sits in its stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmeStrokeMark {
    /// More edits of this stroke may follow.
    Continue,
    /// The brush was released. `trailing` = one more (rate-limited) edit of this stroke follows
    /// ([`crate::stroke::EmeStrokeEnd::trailing`]).
    End {
        /// A trailing edit follows.
        trailing: bool,
    },
}

/// An object operation. The dispatcher assigns the transaction id (any `tx` set by the tool is
/// overwritten).
#[derive(Debug, Clone, PartialEq)]
pub enum EmeObjectRequest {
    /// Place a GO / NPC.
    Spawn(EmeObjSpawn),
    /// Move / rotate / scale.
    Move(EmeObjMove),
    /// Delete.
    Delete {
        /// Kind (guid namespaces overlap between kinds).
        kind: EmeObjectKind,
        /// Object guid (low).
        guid: u32,
    },
}

/// One command from a tool.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeCommand {
    /// Hot path: coalesced voxel edits of one stroke (from [`crate::stroke::EmeStrokeBuilder`]).
    VoxelStroke {
        /// The stroke id.
        stroke: u16,
        /// Edits to uplink now (usually 0 or 1 per frame).
        ops: Vec<EmeVoxelEdit>,
        /// Stroke progress.
        mark: EmeStrokeMark,
    },
    /// A batch of liquid / area cells (one stroke may span several batches).
    Overlay(EmeOverlayEdit),
    /// A cold-path service request (log query, snapshots, …); the reply goes back to the tool.
    Service(EmeServiceRequest),
    /// An object operation.
    Object(EmeObjectRequest),
}

impl EmeCommand {
    /// The capability needed to issue this command (`None` = anyone).
    pub fn required_capability(&self) -> Option<EmeCapability> {
        match self {
            EmeCommand::VoxelStroke { .. } | EmeCommand::Overlay(_) | EmeCommand::Object(_) => {
                Some(EmeCapability::Edit)
            }
            EmeCommand::Service(r) => r.required_capability(),
        }
    }
}

/// A command and the tool that issued it.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeIssued {
    /// The issuing tool (`None` for host-issued commands).
    pub origin: Option<EmeToolId>,
    /// The command.
    pub command: EmeCommand,
}

/// Where tools write their commands for this frame.
#[derive(Debug, Clone, Default)]
pub struct EmeCommandSink {
    items: Vec<EmeIssued>,
}

impl EmeCommandSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit a command (the registry stamps the origin tool).
    pub fn push(&mut self, command: EmeCommand) {
        self.items.push(EmeIssued {
            origin: None,
            command,
        });
    }

    /// Emit a command on behalf of `origin`.
    pub fn push_from(&mut self, origin: EmeToolId, command: EmeCommand) {
        self.items.push(EmeIssued {
            origin: Some(origin),
            command,
        });
    }

    /// Stamp `origin` on every command from index `from` on that has none.
    pub(crate) fn stamp_origin(&mut self, from: usize, origin: EmeToolId) {
        for it in &mut self.items[from..] {
            it.origin.get_or_insert(origin);
        }
    }

    /// Number of queued commands.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Queued commands.
    pub fn iter(&self) -> impl Iterator<Item = &EmeIssued> {
        self.items.iter()
    }

    /// Take every queued command, in order.
    pub fn drain(&mut self) -> Vec<EmeIssued> {
        std::mem::take(&mut self.items)
    }
}

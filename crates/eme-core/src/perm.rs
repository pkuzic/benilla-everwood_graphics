//! The rank → capability model (DESIGN-EME §3.4).
//!
//! The server enforces every permission; this module only decides what the client SHOWS and what
//! it bothers to send. A hand-edited client that skips these checks still gets `R 1` / `ok:false`.

use serde::{Deserialize, Serialize};

/// A GM rank as the server reports it in `caps.rank` (`player:GetGMRank()`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct EmeRank(pub u8);

impl EmeRank {
    /// Mod: opens EME read-only (log browser, history, restore-point list, overlay viewer).
    pub const VIEW: EmeRank = EmeRank(2);
    /// GM: every mutating tool, undo of own strokes, create restore points.
    pub const EDIT: EmeRank = EmeRank(3);
    /// Admin: revert others' work, delete others' objects, area flag overrides.
    pub const REVERT: EmeRank = EmeRank(4);
}

/// One thing a rank may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmeCapability {
    /// Open the EME window and use read-only panels (rank 2+).
    View,
    /// Use any mutating tool, undo/redo own work, create restore points (rank 3+).
    Edit,
    /// Revert other players' work: `revert` a snapshot, `undo_player` (rank 4+).
    RevertOthers,
    /// Delete objects placed by someone else (rank 4+).
    DeleteOthersObjects,
    /// Override `AreaTable` flags of an area id (rank 4+).
    AreaFlags,
}

impl EmeCapability {
    /// The lowest rank that holds this capability.
    pub const fn min_rank(self) -> EmeRank {
        match self {
            EmeCapability::View => EmeRank::VIEW,
            EmeCapability::Edit => EmeRank::EDIT,
            EmeCapability::RevertOthers
            | EmeCapability::DeleteOthersObjects
            | EmeCapability::AreaFlags => EmeRank::REVERT,
        }
    }
}

/// The capabilities of one caller, derived from a rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmePermissions {
    rank: EmeRank,
}

impl EmePermissions {
    /// Permissions for `rank`. Ranks 0–1 hold nothing: EME stays invisible and inert.
    pub const fn from_rank(rank: EmeRank) -> Self {
        Self { rank }
    }

    /// Nobody: the state before the server's `caps` reply arrives.
    pub const fn none() -> Self {
        Self { rank: EmeRank(0) }
    }

    /// The rank these permissions came from.
    pub const fn rank(&self) -> EmeRank {
        self.rank
    }

    /// Whether the caller holds `cap`.
    pub fn has(&self, cap: EmeCapability) -> bool {
        self.rank >= cap.min_rank()
    }

    /// Whether EME may open at all (F9 does nothing below this).
    pub fn can_open(&self) -> bool {
        self.has(EmeCapability::View)
    }

    /// Whether a tool with `min_rank` is usable (shown and enabled).
    pub fn allows_rank(&self, min_rank: u8) -> bool {
        self.rank.0 >= min_rank && self.can_open()
    }
}

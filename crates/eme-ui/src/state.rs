//! The resources the shell reads and the messages it writes.
//!
//! The shell owns no tool or session state: a host (or another lane's crate) keeps these
//! resources current, and applies the [`EmeUiCommand`]s the shell writes.

use bevy::prelude::{Message, Resource};
use eme_core::service::{EmeLogRow, EmeSnapshotRow};
use eme_core::{
    EmeCaps, EmeEntryId, EmeLink, EmeOptionValue, EmePermissions, EmeServiceRequest, EmeToolId,
    EmeToolRegistry, EmeUndoStack,
};

use crate::layout::EmeRightTab;

/// Where the shell reads the tool registry and the undo stack from: the host's own state (the
/// `eme` plugin implements it for its `EmeHost`, so there is ONE registry and ONE stack), or
/// [`EmeStandaloneSource`] for a host without the `eme` plugin (tests, a tool demo).
pub trait EmeShellSource: Resource {
    /// Registered tools (the toolbar shows `registry.visible(perms, caps)`).
    fn registry(&self) -> &EmeToolRegistry;
    /// The shared undo stack (History tab, undo / redo state, the confirmation modal).
    fn undo_stack(&self) -> &EmeUndoStack;
}

/// A registry + undo stack owned by the UI crate itself, for a host without the `eme` plugin.
#[derive(Resource, Debug, Default)]
pub struct EmeStandaloneSource {
    /// Registered tools.
    pub registry: EmeToolRegistry,
    /// The undo stack.
    pub undo: EmeUndoStack,
}

impl EmeShellSource for EmeStandaloneSource {
    fn registry(&self) -> &EmeToolRegistry {
        &self.registry
    }
    fn undo_stack(&self) -> &EmeUndoStack {
        &self.undo
    }
}

/// Who the caller is and how the link looks. Fed from the `caps` reply and the transport.
#[derive(Resource, Debug, Clone, Default)]
pub struct EmeSession {
    /// Rank-derived permissions.
    pub perms: EmePermissions,
    /// The server's `caps`, once known (tools it offers).
    pub caps: Option<EmeCaps>,
    /// Transport link state.
    pub link: EmeLink,
}

/// The status bar's numbers.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct EmeStatus {
    /// Current map id.
    pub map: u16,
    /// Region text (zone / area name or `rx,ry`).
    pub region: String,
    /// Journal epoch.
    pub epoch: u64,
    /// Edits sent and not yet confirmed.
    pub pending: u32,
    /// Last remesh time, ms.
    pub remesh_ms: f32,
    /// Uplink rate, edits per second.
    pub rate: f32,
}

/// The restore-point list (`snapshot_list` reply rows).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct EmeRestorePoints {
    /// Rows, newest first.
    pub rows: Vec<EmeSnapshotRow>,
}

/// The current page of the audit log (`log` reply).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct EmeLogTable {
    /// Rows of this page.
    pub rows: Vec<EmeLogRow>,
    /// Total matching rows.
    pub count: u32,
    /// An older page exists.
    pub more: bool,
    /// Zero-based page number (0 = newest).
    pub page: u32,
}

/// Everything the shell asks of the host. All are typed with eme-core's command types; the host
/// applies them (calls the undo stack, sends the request, changes the tool).
#[derive(Message, Debug, Clone, PartialEq)]
pub enum EmeUiCommand {
    /// A toolbar button: activate this tool.
    ActivateTool(EmeToolId),
    /// Undo button.
    Undo,
    /// Redo button.
    Redo,
    /// The modal's "undo anyway": `EmeUndoStack::confirm_undo` (forces past `others_edited`).
    ConfirmUndo(EmeEntryId),
    /// The modal's cancel: `EmeUndoStack::cancel_confirm`.
    CancelConfirm(EmeEntryId),
    /// An option control changed: `EmeTool::set_option(key, value)` on `tool`.
    SetOption {
        /// The tool.
        tool: EmeToolId,
        /// The option key.
        key: String,
        /// The new value.
        value: EmeOptionValue,
    },
    /// A cold-path request (create / revert a restore point).
    Service(EmeServiceRequest),
    /// The log's paging buttons.
    LogPage {
        /// True = older page, false = newer.
        older: bool,
    },
    /// The user opened a right-hand tab: a good moment to refresh its data.
    TabOpened(EmeRightTab),
}

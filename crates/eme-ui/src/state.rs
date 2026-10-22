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

/// The shell renders only while this is true. The F9 toggle lives in another crate; it flips this
/// (directly, or by writing [`EmeToggleRequested`] and letting [`crate::apply_toggle_requests`] do
/// it).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmeActive(pub bool);

/// "Please toggle EME". Written by whatever owns the hotkey.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmeToggleRequested;

/// The tool registry, as a resource (the host registers its tools into `.0`).
#[derive(Resource, Debug, Default)]
pub struct EmeTools(pub EmeToolRegistry);

/// The shared undo stack, as a resource.
#[derive(Resource, Debug, Default)]
pub struct EmeUndo(pub EmeUndoStack);

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

/// Whether the pointer / keyboard is over EME UI. The viewport reads this to know whether a click
/// is for the world or for a panel. Reset to false while the shell is hidden.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmeInputArbiter {
    /// The pointer is over an EME panel, window or modal (or egui is dragging).
    pub pointer_over_ui: bool,
    /// An EME text field has keyboard focus (hotkeys must not fire).
    pub keyboard_captured: bool,
}

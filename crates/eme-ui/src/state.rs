//! The resources the shell reads and the messages it writes.
//!
//! The shell owns no tool or session state: a host (or another lane's crate) keeps these
//! resources current, and applies the [`EmeUiCommand`]s the shell writes.

use bevy::prelude::{Message, Resource};
use eme_core::service::{
    EmeLogQuery, EmeLogRow, EmeProjectInfo, EmeProjectRef, EmeProjectRow, EmeSnapshotRow,
};
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
    /// MONKEY (EME projects): the active server-side project, if any.
    pub project: Option<EmeProjectRef>,
    /// MONKEY (EME projects): map edits need an active project (the server knows projects).
    pub projects_required: bool,
}

impl EmeSession {
    /// MONKEY (EME projects): map-editing tools are locked: projects are required and none is open.
    pub fn needs_project(&self) -> bool {
        self.projects_required && self.project.is_none()
    }
}

/// MONKEY (EME projects): which projects the Projects tab lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmeProjectFilter {
    /// The caller's own (any status).
    Mine,
    /// Every open project.
    #[default]
    Open,
    /// Everything.
    All,
}

impl EmeProjectFilter {
    /// The `project_list` request for this filter.
    pub fn request(self, limit: u32) -> EmeServiceRequest {
        let (status, mine) = match self {
            Self::Mine => (Some("all".to_owned()), true),
            Self::Open => (Some("open".to_owned()), false),
            Self::All => (Some("all".to_owned()), false),
        };
        EmeServiceRequest::ProjectList {
            status,
            mine,
            limit: Some(limit),
            before_id: None,
        }
    }
}

/// MONKEY (EME projects): the server answered a project revert with `others_edited`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeProjectConflict {
    /// Project id.
    pub project: u32,
    /// Its name.
    pub name: String,
    /// The other authors.
    pub others: Vec<String>,
    /// Their live edits.
    pub count: u32,
}

/// MONKEY (EME projects): the Projects tab's data (the host fills it from `project_list`,
/// `project_info` and a refused `project_revert`).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct EmeProjects {
    /// Rows of the current filter, newest first.
    pub rows: Vec<EmeProjectRow>,
    /// The filter the rows belong to.
    pub filter: EmeProjectFilter,
    /// The last `project_info` reply.
    pub info: Option<EmeProjectInfo>,
    /// A revert that needs a forced resend (`others_edited`).
    pub conflict: Option<EmeProjectConflict>,
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
    /// The restore point selected in the list (the host outlines its radius in the world).
    pub selected: Option<u32>,
}

/// A one-line message under the top strip: a server error, or the result of a revert. The host
/// sets it (`key` is a locale key, `args` fill its `{0}`, `{1}`…); the shell shows it until the
/// user dismisses it ([`EmeUiCommand::DismissNotice`]) or the host clears it. An empty `key`
/// means no message.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct EmeNotice {
    /// Locale key; empty = nothing to show.
    pub key: String,
    /// Substitutions.
    pub args: Vec<String>,
    /// An error (red) rather than a result (green).
    pub error: bool,
    /// Bumped by the host on every new message (so the same text twice re-appears).
    pub seq: u32,
}

impl EmeNotice {
    /// Shows `key` (an error or a result).
    pub fn set(&mut self, key: impl Into<String>, args: Vec<String>, error: bool) {
        self.key = key.into();
        self.args = args;
        self.error = error;
        self.seq = self.seq.wrapping_add(1);
    }

    /// Hides the message.
    pub fn clear(&mut self) {
        if !self.key.is_empty() {
            self.key.clear();
            self.args.clear();
        }
    }
}

/// One stroke another GM made, learned from the live `e` records (read-only History rows).
#[derive(Debug, Clone, PartialEq)]
pub struct EmeOtherStroke {
    /// The editing character's low GUID.
    pub actor: u32,
    /// Their name, when the log told us (else the shell shows `#<guid>`).
    pub name: String,
    /// The stroke id (per character).
    pub stroke: u16,
    /// Edits seen so far.
    pub edits: u32,
    /// Voxel mode of the first edit.
    pub mode: u8,
    /// Where (WoW yards) of the latest edit.
    pub pos: [f32; 3],
    /// Game-clock ms of the latest edit.
    pub seen_ms: u64,
}

/// Strokes by other GMs, newest first (read-only: they are theirs to undo, or an admin's).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct EmeOthersHistory {
    /// Game-clock ms now (for the "N s ago" text).
    pub now_ms: u64,
    /// Rows, newest first.
    pub rows: Vec<EmeOtherStroke>,
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
    /// Apply the log filters: fetch page 1 of this query.
    LogQuery(EmeLogQuery),
    /// A restore-point row was (de)selected.
    SelectRestore(Option<u32>),
    /// "Show in world": jump the camera to this spot and mark it. `z` is looked up when `None`.
    ShowInWorld {
        /// WoW x.
        x: f32,
        /// WoW y.
        y: f32,
        /// WoW z, if known.
        z: Option<f32>,
    },
    /// The notice's dismiss button.
    DismissNotice,
    /// The log's paging buttons.
    LogPage {
        /// True = older page, false = newer.
        older: bool,
    },
    /// The user opened a right-hand tab: a good moment to refresh its data.
    TabOpened(EmeRightTab),
    /// MONKEY (EME projects): list projects with this filter.
    ProjectFilter(EmeProjectFilter),
    /// MONKEY (EME projects): the `others_edited` conflict was answered (force sent or cancelled).
    DismissProjectConflict,
}

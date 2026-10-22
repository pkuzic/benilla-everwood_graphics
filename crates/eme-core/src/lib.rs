//! # eme-core — the engine-free core of Everwood ME (EME)
//!
//! EME is an in-game, server-authoritative map editor for benilla. This crate holds everything
//! that needs no engine: tools and their registry, the command bus, the one shared undo stack with
//! server-confirmed semantics, brush stroke coalescing, the transport seam, the `.mveme` service
//! JSON, the rank → capability model and the public integration API. No Bevy, no UI toolkit, no
//! networking: every piece is a plain state machine driven by explicit inputs (including time),
//! so it is unit-tested with fixtures.
//!
//! | Module | What |
//! |---|---|
//! | [`tool`] | [`EmeTool`] trait, [`EmeToolRegistry`], input / options / preview models |
//! | [`command`] | [`EmeCommand`], [`EmeCommandSink`] |
//! | [`bus`] | [`EmeDispatcher`]: routes commands and inbound traffic |
//! | [`undo`] | [`EmeUndoStack`]: one Ctrl+Z for strokes, overlays and objects |
//! | [`stroke`] | [`EmeStrokeBuilder`]: held brush → one stroke of rate-limited edits |
//! | [`transport`] | [`EmeTransport`] trait, [`EmeVoxelEdit`], [`EmeInbound`], a loopback |
//! | [`service`] | `.mveme` JSON: requests, replies, pushes, typed rows |
//! | [`perm`] | ranks and capabilities |
//! | [`api`] | public integration surface: [`EmeEvent`], [`EmeHistoryHook`], [`EmeServiceClient`] |
//!
//! See the crate README for how to add a tool.

#![warn(missing_docs)]

pub mod api;
pub mod bus;
pub mod command;
mod lenient;
pub mod perm;
pub mod service;
pub mod stroke;
pub mod tool;
pub mod transport;
pub mod undo;

pub use api::{
    EmeCompleted, EmeDiscardReason, EmeEvent, EmeEvents, EmeHistoryHook, EmeSendError,
    EmeServiceClient,
};
pub use bus::{EmeDispatchError, EmeDispatcher, EmePumpOutput, EmeRoute, EmeToolReply};
pub use command::{EmeCommand, EmeCommandSink, EmeIssued, EmeObjectRequest, EmeStrokeMark};
pub use perm::{EmeCapability, EmePermissions, EmeRank};
pub use service::{
    EmeCaps, EmeDownlink, EmeRequestId, EmeServiceEnvelope, EmeServiceError, EmeServicePush,
    EmeServiceReply, EmeServiceRequest, EmeTxId,
};
pub use stroke::{EmeBrushSample, EmeStrokeBuilder, EmeStrokeConfig, EmeStrokeEnd};
pub use tool::{
    EmeInputCtx, EmeOptionField, EmeOptionKind, EmeOptionValue, EmeOptions, EmePreview, EmeTool,
    EmeToolId, EmeToolRegistry,
};
pub use transport::{
    EmeInbound, EmeLink, EmeLoopbackTransport, EmeRejectReason, EmeTransport, EmeVoxelEdit,
};
pub use undo::{
    EmeEntryId, EmeEntryState, EmeHistoryRequest, EmeHistoryTicket, EmeUndoBackend, EmeUndoEntry,
    EmeUndoEntryMeta, EmeUndoError, EmeUndoStack, EmeUndoStarted,
};

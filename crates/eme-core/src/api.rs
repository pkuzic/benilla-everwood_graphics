//! The public integration surface (DESIGN-EME §2.5): what a host (any client that embeds EME, or a
//! tool built on top of it) may use. Stable and semver-governed.
//!
//! - [`EmeEvent`] / [`EmeEvents`]: what happened (entry pushed / committed / undone / redone /
//!   discarded, selection changed). Engine-free here; the Bevy plugin re-emits them as messages.
//! - [`EmeHistoryHook`]: mirror EME's history into a host's own journal or action log, or arbitrate a
//!   host-wide Ctrl+Z. EME never references a host; the host registers a hook.
//! - [`EmeServiceClient`]: send a service request, get its reply (with timeouts and routing).

use crate::perm::{EmeCapability, EmePermissions};
use crate::service::{
    EmeOthersEdited, EmeRequestId, EmeServiceEnvelope, EmeServiceError, EmeServiceReply,
    EmeServiceRequest, EME_MAX_FRAME_BYTES, EME_MAX_REQUEST_ID,
};
use crate::transport::EmeTransport;
use crate::undo::EmeUndoEntryMeta;
use std::collections::HashMap;
use std::fmt;

/// Why an undo entry left the stack without being undone.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeDiscardReason {
    /// The server refused the work itself (every edit rejected, or the commit reply failed).
    CommitFailed(EmeServiceError),
    /// A new action cleared the redo branch this entry was on.
    RedoBranchCleared,
    /// The stack grew past its capacity; the oldest settled entry fell off.
    Evicted,
    /// The link dropped before the server confirmed the work.
    LinkLost,
    /// The host cleared the stack.
    Cleared,
    /// The server refused a redo for good (`err:"conflict"`: a newer stroke or a revert made it
    /// non-redoable).
    RedoConflict,
    /// MONKEY (EME projects): the entry's whole project was reverted on the server.
    ProjectReverted,
}

/// Something EME did. Hosts read these; EME never needs to.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeEvent {
    /// A new history entry exists (its work may still await server confirmation).
    EntryPushed(EmeUndoEntryMeta),
    /// The server accepted the entry's work (a stroke's edits, an overlay batch, an object tx).
    EntryCommitted(EmeUndoEntryMeta),
    /// The server confirmed an undo.
    EntryUndone(EmeUndoEntryMeta),
    /// The server confirmed a redo.
    EntryRedone(EmeUndoEntryMeta),
    /// The entry left the stack.
    EntryDiscarded {
        /// The entry.
        meta: EmeUndoEntryMeta,
        /// Why.
        reason: EmeDiscardReason,
    },
    /// Undo needs the user's confirmation: others edited the area afterwards (Q4: warn).
    UndoNeedsConfirm {
        /// The entry.
        meta: EmeUndoEntryMeta,
        /// Who edited after it.
        others: EmeOthersEdited,
    },
    /// An undo or redo request failed; the entry is back in its previous state.
    RequestFailed {
        /// The entry.
        meta: EmeUndoEntryMeta,
        /// `"undo"` or `"redo"`.
        op: &'static str,
        /// The error.
        error: EmeServiceError,
    },
    /// The object selection changed (guids, in selection order).
    SelectionChanged(Vec<u64>),
}

/// A plain event queue (the engine-free stand-in for a Bevy message channel).
#[derive(Debug, Clone, Default)]
pub struct EmeEvents {
    queue: Vec<EmeEvent>,
}

impl EmeEvents {
    /// Queue an event.
    pub fn push(&mut self, event: EmeEvent) {
        self.queue.push(event);
    }

    /// Take every queued event, oldest first.
    pub fn drain(&mut self) -> Vec<EmeEvent> {
        std::mem::take(&mut self.queue)
    }

    /// Queued events, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &EmeEvent> {
        self.queue.iter()
    }

    /// Number of queued events.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// A host's view of EME's history. Every method has a no-op default; implement what you need.
/// Called synchronously from the undo stack, in the order things happen, after the state change.
pub trait EmeHistoryHook: Send + Sync {
    /// An entry was pushed (work sent, not yet confirmed).
    fn on_push(&self, _meta: &EmeUndoEntryMeta) {}
    /// The server accepted the entry's work.
    fn on_commit(&self, _meta: &EmeUndoEntryMeta) {}
    /// The server confirmed an undo of the entry.
    fn on_undo(&self, _meta: &EmeUndoEntryMeta) {}
    /// The server confirmed a redo of the entry.
    fn on_redo(&self, _meta: &EmeUndoEntryMeta) {}
    /// The entry left the stack.
    fn on_discard(&self, _meta: &EmeUndoEntryMeta, _reason: &EmeDiscardReason) {}
}

/// Why a service request was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmeSendError {
    /// The caller's rank lacks this capability (the server would refuse it anyway).
    NotPermitted(EmeCapability),
    /// The link is not usable.
    LinkDown,
    /// The frame exceeds [`EME_MAX_FRAME_BYTES`] (split the request, e.g. fewer cells).
    TooLarge(usize),
    /// MONKEY (EME projects, CONTRACT v3.4): a map edit with no active project (the server would
    /// refuse it: `R 7` / `no_project`).
    NoProject,
}

impl fmt::Display for EmeSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPermitted(c) => write!(f, "not permitted: {c:?}"),
            Self::LinkDown => f.write_str("link down"),
            Self::TooLarge(n) => write!(f, "frame too large: {n} > {EME_MAX_FRAME_BYTES} bytes"),
            Self::NoProject => f.write_str("no active project"),
        }
    }
}

impl std::error::Error for EmeSendError {}

/// A finished request: its reply (real, or synthesized for timeout / link loss) and its route.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeCompleted<R> {
    /// The request id.
    pub id: EmeRequestId,
    /// The `op` that was sent.
    pub op: &'static str,
    /// Where the caller wanted the reply delivered.
    pub route: R,
    /// The reply. A synthesized failure has `err` = `timeout` or `link_lost`.
    pub reply: EmeServiceReply,
}

#[derive(Debug, Clone)]
struct Pending<R> {
    route: R,
    op: &'static str,
    sent_ms: u64,
    /// MONKEY (EME objects): an idempotent mutation that is resent with the SAME id and `tx`.
    retry: Option<Retry>,
}

/// How an idempotent request is resent (CONTRACT v3.7: the server dedups on `(character, tx)`).
#[derive(Debug, Clone)]
struct Retry {
    request: EmeServiceRequest,
    /// Wait this long for a reply before each resend and before giving up, ms.
    after_ms: u64,
    /// Resends left.
    left: u8,
}

/// Resend policy of an idempotent request: wait [`EME_RETRY_AFTER_MS`] for the reply, resend up to
/// [`EME_RETRIES`] times with the same frame id and `tx`, then fail with `timeout`.
pub const EME_RETRY_AFTER_MS: u64 = 4_000;
/// See [`EME_RETRY_AFTER_MS`].
pub const EME_RETRIES: u8 = 2;

/// Sends service requests and matches replies to them. `R` is the caller's routing tag (e.g. "this
/// reply is for the undo stack, ticket T" or "for the log panel").
#[derive(Debug, Clone)]
pub struct EmeServiceClient<R> {
    pending: HashMap<EmeRequestId, Pending<R>>,
    perms: EmePermissions,
    timeout_ms: u64,
}

impl<R> EmeServiceClient<R> {
    /// A client whose requests time out after `timeout_ms`. Permissions start at none: only
    /// `caps` may be sent until [`Self::set_permissions`].
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            pending: HashMap::new(),
            perms: EmePermissions::none(),
            timeout_ms,
        }
    }

    /// Set the caller's permissions (from the `caps` reply).
    pub fn set_permissions(&mut self, perms: EmePermissions) {
        self.perms = perms;
    }

    /// The caller's permissions.
    pub fn permissions(&self) -> EmePermissions {
        self.perms
    }

    /// Requests awaiting a reply.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Gate, size-check and send `request`; the reply comes back through [`Self::on_reply`].
    pub fn send(
        &mut self,
        transport: &mut dyn EmeTransport,
        now_ms: u64,
        request: EmeServiceRequest,
        route: R,
    ) -> Result<EmeRequestId, EmeSendError> {
        self.send_inner(transport, now_ms, request, route, false)
    }

    /// MONKEY (EME objects): like [`Self::send`] for a mutation carrying a `tx` the server dedups
    /// (`obj_spawn` / `obj_move` / `obj_delete`): if no reply comes within [`EME_RETRY_AFTER_MS`]
    /// the SAME frame id and `tx` are sent again ([`Self::expire_with`]), up to [`EME_RETRIES`]
    /// times, before the request fails with `timeout`.
    pub fn send_idempotent(
        &mut self,
        transport: &mut dyn EmeTransport,
        now_ms: u64,
        request: EmeServiceRequest,
        route: R,
    ) -> Result<EmeRequestId, EmeSendError> {
        self.send_inner(transport, now_ms, request, route, true)
    }

    fn send_inner(
        &mut self,
        transport: &mut dyn EmeTransport,
        now_ms: u64,
        request: EmeServiceRequest,
        route: R,
        idempotent: bool,
    ) -> Result<EmeRequestId, EmeSendError> {
        if let Some(cap) = request.required_capability() {
            if !self.perms.has(cap) {
                return Err(EmeSendError::NotPermitted(cap));
            }
        }
        if !transport.state().is_usable() {
            return Err(EmeSendError::LinkDown);
        }
        let len = EmeServiceEnvelope::new(EmeRequestId(EME_MAX_REQUEST_ID), request.clone())
            .to_json()
            .len();
        if len > EME_MAX_FRAME_BYTES {
            return Err(EmeSendError::TooLarge(len));
        }
        let op = request.op();
        let id = transport.send_service(&request);
        let retry = idempotent.then(|| Retry {
            request,
            after_ms: EME_RETRY_AFTER_MS,
            left: EME_RETRIES,
        });
        self.pending.insert(
            id,
            Pending {
                route,
                op,
                sent_ms: now_ms,
                retry,
            },
        );
        Ok(id)
    }

    /// MONKEY (EME objects): [`Self::expire`] that first RESENDS idempotent requests which waited
    /// [`EME_RETRY_AFTER_MS`] (same frame id, same `tx`); one out of resends fails with `timeout`
    /// after one more wait. Everything else expires as in [`Self::expire`].
    pub fn expire_with(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
    ) -> Vec<EmeCompleted<R>> {
        let timeout = self.timeout_ms;
        let mut due: Vec<EmeRequestId> = Vec::new();
        let mut dead: Vec<EmeRequestId> = Vec::new();
        for (id, p) in &self.pending {
            let waited = now_ms.saturating_sub(p.sent_ms);
            match &p.retry {
                Some(r) if waited >= r.after_ms => {
                    if r.left > 0 {
                        due.push(*id);
                    } else {
                        dead.push(*id);
                    }
                }
                Some(_) => {}
                None if waited >= timeout => dead.push(*id),
                None => {}
            }
        }
        due.sort();
        for id in due {
            if let Some(p) = self.pending.get_mut(&id) {
                if let Some(r) = p.retry.as_mut() {
                    r.left -= 1;
                    p.sent_ms = now_ms;
                    transport.resend_service(id, &r.request);
                }
            }
        }
        dead.sort();
        self.fail(dead, EmeServiceError::Timeout)
    }

    /// Match a reply; `None` for an unknown or already-completed id (a late reply after a timeout).
    pub fn on_reply(&mut self, reply: EmeServiceReply) -> Option<EmeCompleted<R>> {
        let p = self.pending.remove(&reply.id)?;
        Some(EmeCompleted {
            id: reply.id,
            op: p.op,
            route: p.route,
            reply,
        })
    }

    /// Fail every request older than the timeout with `err = "timeout"`.
    pub fn expire(&mut self, now_ms: u64) -> Vec<EmeCompleted<R>> {
        let timeout = self.timeout_ms;
        let mut ids: Vec<EmeRequestId> = self
            .pending
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.sent_ms) >= timeout)
            .map(|(id, _)| *id)
            .collect();
        ids.sort();
        self.fail(ids, EmeServiceError::Timeout)
    }

    /// Fail every in-flight request with `err = "link_lost"` (the link dropped).
    pub fn fail_all(&mut self) -> Vec<EmeCompleted<R>> {
        let mut ids: Vec<EmeRequestId> = self.pending.keys().copied().collect();
        ids.sort();
        self.fail(ids, EmeServiceError::LinkLost)
    }

    fn fail(&mut self, ids: Vec<EmeRequestId>, err: EmeServiceError) -> Vec<EmeCompleted<R>> {
        ids.into_iter()
            .filter_map(|id| {
                let p = self.pending.remove(&id)?;
                Some(EmeCompleted {
                    id,
                    op: p.op,
                    route: p.route,
                    reply: EmeServiceReply::error(id, err.code()),
                })
            })
            .collect()
    }
}

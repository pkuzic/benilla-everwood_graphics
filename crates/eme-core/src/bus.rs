//! The command bus: one dispatcher routes every tool command to the transport, the undo stack and
//! the service client, and routes everything the transport delivers back (DESIGN-EME §2.2).
//!
//! Per frame the host does:
//! 1. `registry.feed(&ctx, caps, &mut sink)` — the active tool emits commands;
//! 2. `dispatcher.dispatch(now, &mut transport, &mut sink)` — gate, send, record history;
//! 3. `dispatcher.pump(now, &mut transport)` — confirmations, replies, pushes, timeouts.
//!
//! Ctrl+Z / Ctrl+Y call [`EmeDispatcher::undo`] / [`EmeDispatcher::redo`].

use crate::api::{EmeEvent, EmeEvents, EmeSendError, EmeServiceClient};
use crate::command::{EmeCommand, EmeCommandSink, EmeObjectRequest, EmeStrokeMark};
use crate::perm::{EmeCapability, EmePermissions};
use crate::service::{EmeCaps, EmeServicePush, EmeServiceReply, EmeServiceRequest, EmeTxId};
use crate::tool::EmeToolId;
use crate::transport::{EmeInbound, EmeLink, EmeTransport};
use crate::undo::{
    EmeEntryId, EmeHistoryTicket, EmeObjectAction, EmeUndoBackend, EmeUndoError, EmeUndoStack,
    EmeUndoStarted,
};

/// Where a service reply goes.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeRoute {
    /// The `caps` probe.
    Caps,
    /// An undo / redo request of the undo stack.
    History(EmeHistoryTicket),
    /// The work of an undo entry (`ov_edit`, `obj_*`), plus the issuing tool.
    Commit(EmeEntryId, Option<EmeToolId>),
    /// A plain request from a tool or the host.
    Tool(Option<EmeToolId>),
}

/// A reply for a tool / panel (the dispatcher handles caps and history replies itself).
#[derive(Debug, Clone, PartialEq)]
pub struct EmeToolReply {
    /// The tool that asked (`None` = host).
    pub tool: Option<EmeToolId>,
    /// The `op` that was sent.
    pub op: &'static str,
    /// The reply.
    pub reply: EmeServiceReply,
}

/// What one [`EmeDispatcher::pump`] produced for the host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmePumpOutput {
    /// Replies for tools / panels.
    pub replies: Vec<EmeToolReply>,
    /// Server pushes (overlay rows, live strokes, caps changes), in arrival order.
    pub pushes: Vec<EmeServicePush>,
    /// The link changed to this state during the pump.
    pub link: Option<EmeLink>,
}

/// A command the dispatcher refused.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeDispatchError {
    /// The issuing tool.
    pub origin: Option<EmeToolId>,
    /// Why.
    pub error: EmeSendError,
}

/// Routes commands and inbound traffic. Owns the undo stack and the service client.
#[derive(Debug)]
pub struct EmeDispatcher {
    undo: EmeUndoStack,
    client: EmeServiceClient<EmeRoute>,
    caps: Option<EmeCaps>,
    events: EmeEvents,
    next_tx: u32,
}

/// Default service request timeout, ms.
pub const EME_REQUEST_TIMEOUT_MS: u64 = 10_000;

impl Default for EmeDispatcher {
    fn default() -> Self {
        Self::new(EmeUndoStack::new(), EME_REQUEST_TIMEOUT_MS)
    }
}

impl EmeDispatcher {
    /// A dispatcher around `undo` whose requests time out after `timeout_ms`.
    pub fn new(undo: EmeUndoStack, timeout_ms: u64) -> Self {
        Self {
            undo,
            client: EmeServiceClient::new(timeout_ms),
            caps: None,
            events: EmeEvents::default(),
            next_tx: 1,
        }
    }

    /// The undo stack (read: history panel).
    pub fn undo_stack(&self) -> &EmeUndoStack {
        &self.undo
    }

    /// The undo stack (write: hooks, confirm/cancel).
    pub fn undo_stack_mut(&mut self) -> &mut EmeUndoStack {
        &mut self.undo
    }

    /// The caps from the server, once known.
    pub fn caps(&self) -> Option<&EmeCaps> {
        self.caps.as_ref()
    }

    /// The caller's permissions (none until caps arrive).
    pub fn permissions(&self) -> EmePermissions {
        self.client.permissions()
    }

    /// Events since the last drain (the undo stack's and the dispatcher's own).
    pub fn drain_events(&mut self) -> Vec<EmeEvent> {
        self.collect_events();
        self.events.drain()
    }

    fn collect_events(&mut self) {
        for e in self.undo.drain_events() {
            self.events.push(e);
        }
    }

    /// Host-issued event (e.g. the viewport's selection changed).
    pub fn push_event(&mut self, event: EmeEvent) {
        self.collect_events();
        self.events.push(event);
    }

    fn set_caps(&mut self, caps: EmeCaps) {
        self.client
            .set_permissions(EmePermissions::from_rank(caps.rank));
        self.caps = Some(caps);
    }

    /// Ask the server who we are. Call once the link is up.
    pub fn request_caps(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
    ) -> Result<(), EmeSendError> {
        self.client
            .send(transport, now_ms, EmeServiceRequest::Caps, EmeRoute::Caps)
            .map(|_| ())
    }

    /// Send a host / panel request; the reply comes back from [`Self::pump`].
    pub fn request(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
        tool: Option<EmeToolId>,
        request: EmeServiceRequest,
    ) -> Result<(), EmeSendError> {
        self.client
            .send(transport, now_ms, request, EmeRoute::Tool(tool))
            .map(|_| ())
    }

    fn alloc_tx(&mut self) -> EmeTxId {
        let tx = EmeTxId(self.next_tx);
        self.next_tx = self.next_tx.wrapping_add(1).max(1);
        tx
    }

    /// Route every command in `sink`. Commands the caller may not issue, or that cannot be sent,
    /// are dropped and reported.
    pub fn dispatch(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
        sink: &mut EmeCommandSink,
    ) -> Vec<EmeDispatchError> {
        let mut errors = Vec::new();
        let perms = self.client.permissions();
        for issued in sink.drain() {
            let origin = issued.origin;
            let tool = origin.map_or("host", |t| t.as_str());
            if let Some(cap) = issued.command.required_capability() {
                if !perms.has(cap) {
                    errors.push(EmeDispatchError {
                        origin,
                        error: EmeSendError::NotPermitted(cap),
                    });
                    continue;
                }
            }
            let link_ok = transport.state().is_usable();
            match issued.command {
                EmeCommand::VoxelStroke { stroke, ops, mark } => {
                    if !link_ok {
                        if !ops.is_empty() {
                            errors.push(EmeDispatchError {
                                origin,
                                error: EmeSendError::LinkDown,
                            });
                        }
                        continue;
                    }
                    for op in &ops {
                        transport.send_edit(op);
                        self.undo.note_edit(now_ms, tool, op.stroke, op.nonce);
                    }
                    if let EmeStrokeMark::End { trailing } = mark {
                        self.undo.close_stroke(stroke, trailing);
                    }
                }
                EmeCommand::Overlay(edit) => {
                    let backend = EmeUndoBackend::Overlay {
                        layer: edit.layer(),
                        stroke: edit.stroke(),
                    };
                    let request = EmeServiceRequest::OvEdit(edit);
                    self.send_commit(now_ms, transport, origin, backend, request, &mut errors);
                }
                EmeCommand::Object(obj) => {
                    let tx = self.alloc_tx();
                    let (action, request) = match obj {
                        EmeObjectRequest::Spawn(mut s) => {
                            s.tx = tx;
                            (EmeObjectAction::Spawn, EmeServiceRequest::ObjSpawn(s))
                        }
                        EmeObjectRequest::Move(mut m) => {
                            m.tx = tx;
                            (EmeObjectAction::Move, EmeServiceRequest::ObjMove(m))
                        }
                        EmeObjectRequest::Delete { guid } => (
                            EmeObjectAction::Delete,
                            EmeServiceRequest::ObjDelete { guid, tx },
                        ),
                    };
                    let backend = EmeUndoBackend::Object { tx, action };
                    self.send_commit(now_ms, transport, origin, backend, request, &mut errors);
                }
                EmeCommand::Service(request) => {
                    if let Err(error) =
                        self.client
                            .send(transport, now_ms, request, EmeRoute::Tool(origin))
                    {
                        errors.push(EmeDispatchError { origin, error });
                    }
                }
            }
        }
        self.flush_history(now_ms, transport);
        errors
    }

    fn send_commit(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
        origin: Option<EmeToolId>,
        backend: EmeUndoBackend,
        request: EmeServiceRequest,
        errors: &mut Vec<EmeDispatchError>,
    ) {
        // Check first so a refused send never leaves a phantom history entry.
        if !transport.state().is_usable() {
            errors.push(EmeDispatchError {
                origin,
                error: EmeSendError::LinkDown,
            });
            return;
        }
        let tool = origin.map_or("host", |t| t.as_str());
        let entry = self.undo.push_committing(now_ms, tool, backend);
        if let Err(error) =
            self.client
                .send(transport, now_ms, request, EmeRoute::Commit(entry, origin))
        {
            self.undo.on_commit_reply(
                entry,
                &EmeServiceReply::error(crate::service::EmeRequestId(0), error_code(&error)),
            );
            errors.push(EmeDispatchError { origin, error });
        }
    }

    /// Send whatever the undo stack queued; a send failure fails the entry's request at once.
    fn flush_history(&mut self, now_ms: u64, transport: &mut dyn EmeTransport) {
        for r in self.undo.drain_requests() {
            if let Err(error) =
                self.client
                    .send(transport, now_ms, r.request, EmeRoute::History(r.ticket))
            {
                let reply =
                    EmeServiceReply::error(crate::service::EmeRequestId(0), error_code(&error));
                self.undo.on_reply(r.ticket, &reply);
            }
        }
    }

    /// Ctrl+Z.
    pub fn undo(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
    ) -> Result<EmeUndoStarted, EmeUndoError> {
        if !self.client.permissions().has(EmeCapability::Edit) {
            return Err(EmeUndoError::Nothing);
        }
        let r = self.undo.undo();
        self.flush_history(now_ms, transport);
        r
    }

    /// Ctrl+Y.
    pub fn redo(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
    ) -> Result<EmeEntryId, EmeUndoError> {
        if !self.client.permissions().has(EmeCapability::Edit) {
            return Err(EmeUndoError::Nothing);
        }
        let r = self.undo.redo();
        self.flush_history(now_ms, transport);
        r
    }

    /// "Undo anyway" after an [`EmeUndoStarted::NeedsConfirmation`].
    pub fn confirm_undo(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
        entry: EmeEntryId,
    ) -> Result<EmeUndoStarted, EmeUndoError> {
        let r = self.undo.confirm_undo(entry);
        self.flush_history(now_ms, transport);
        r
    }

    /// Drain the transport and route everything; expire timed-out requests.
    pub fn pump(&mut self, now_ms: u64, transport: &mut dyn EmeTransport) -> EmePumpOutput {
        let mut out = EmePumpOutput::default();
        for inbound in transport.poll() {
            match inbound {
                EmeInbound::EditConfirmed { nonce, seq, .. } => {
                    self.undo.on_edit_confirmed(nonce, seq);
                }
                EmeInbound::EditRejected { nonce, reason } => {
                    self.undo.on_edit_rejected(nonce, reason);
                }
                EmeInbound::Reply(reply) => {
                    if let Some(done) = self.client.on_reply(reply) {
                        self.route(done.route, done.op, done.reply, &mut out);
                    }
                }
                EmeInbound::Push(push) => {
                    if let EmeServicePush::Caps(caps) = &push {
                        self.set_caps(caps.clone());
                    }
                    out.pushes.push(push);
                }
                EmeInbound::Link(link) => {
                    out.link = Some(link);
                    if !link.is_usable() {
                        for done in self.client.fail_all() {
                            self.route(done.route, done.op, done.reply, &mut out);
                        }
                        self.undo.on_link_lost();
                    }
                }
            }
        }
        for done in self.client.expire(now_ms) {
            self.route(done.route, done.op, done.reply, &mut out);
        }
        // A commit that just settled may have released a queued undo.
        self.flush_history(now_ms, transport);
        self.collect_events();
        out
    }

    fn route(
        &mut self,
        route: EmeRoute,
        op: &'static str,
        reply: EmeServiceReply,
        out: &mut EmePumpOutput,
    ) {
        match route {
            EmeRoute::Caps => {
                if reply.ok {
                    if let Ok(caps) = EmeCaps::from_reply(&reply) {
                        self.set_caps(caps);
                    }
                }
                out.replies.push(EmeToolReply {
                    tool: None,
                    op,
                    reply,
                });
            }
            EmeRoute::History(ticket) => {
                self.undo.on_reply(ticket, &reply);
            }
            EmeRoute::Commit(entry, tool) => {
                self.undo.on_commit_reply(entry, &reply);
                out.replies.push(EmeToolReply { tool, op, reply });
            }
            EmeRoute::Tool(tool) => out.replies.push(EmeToolReply { tool, op, reply }),
        }
    }
}

fn error_code(e: &EmeSendError) -> &'static str {
    match e {
        EmeSendError::NotPermitted(_) => "rank",
        EmeSendError::LinkDown => "link_lost",
        EmeSendError::TooLarge(_) => "invalid",
    }
}

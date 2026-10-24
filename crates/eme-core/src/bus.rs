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
use crate::service::{
    EmeCaps, EmeProjectRef, EmeRequestId, EmeServiceEnvelope, EmeServicePush, EmeServiceReply,
    EmeServiceRequest, EME_MAX_FRAME_BYTES, EME_MAX_REQUEST_ID,
};
use crate::transport::EmeRejectReason;
use crate::tool::EmeToolId;
use crate::transport::{EmeInbound, EmeLink, EmeTransport};
use crate::undo::{EmeEntryId, EmeHistoryTicket, EmeUndoError, EmeUndoStack, EmeUndoStarted};

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
    /// MONKEY (EME projects): the active project changed during the pump (to the new value).
    pub project: Option<Option<EmeProjectRef>>,
    /// MONKEY (EME projects): the server refused a map edit for want of a project (`R 7` /
    /// `no_project`): the host prompts the user to open or create one.
    pub no_project: bool,
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
    /// MONKEY (EME projects): the server-confirmed active project.
    project: Option<EmeProjectRef>,
    /// MONKEY (EME projects): the project the user last opened / created; re-opened after every
    /// `caps` that says "none" (a HELLO / map change / reconnect starts a fresh server selection,
    /// CONTRACT v3.4.1). Cleared by leave / close / revert or when the server refuses to open it.
    wanted: Option<u32>,
    /// A `project_open` of `wanted` is in flight (sent at most once per caps).
    reopening: bool,
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
            project: None,
            wanted: None,
            reopening: false,
        }
    }

    /// MONKEY (EME projects): the server-confirmed active project.
    pub fn project(&self) -> Option<&EmeProjectRef> {
        self.project.as_ref()
    }

    /// MONKEY (EME projects): the project the client re-opens after a HELLO / map change.
    pub fn wanted_project(&self) -> Option<u32> {
        self.wanted
    }

    /// MONKEY (EME projects): whether map edits need an active project (the server's `caps`
    /// knows projects, CONTRACT v3.4). A server before v3.4 gates nothing.
    pub fn projects_required(&self) -> bool {
        self.caps.as_ref().is_some_and(|c| c.projects_ready.is_some())
    }

    /// MONKEY (EME projects): a map edit may be sent now (a project is active, or none is needed).
    pub fn may_edit_map(&self) -> bool {
        !self.projects_required() || self.project.is_some()
    }

    fn set_project(&mut self, p: Option<EmeProjectRef>, out: &mut EmePumpOutput) {
        if self.project != p {
            self.undo.set_project(p.as_ref().map(|p| p.id));
            out.project = Some(p.clone());
            self.project = p;
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
            // MONKEY (EME projects, CONTRACT v3.4): no map edit without an active project.
            let map_edit = matches!(
                issued.command,
                EmeCommand::VoxelStroke { .. } | EmeCommand::Overlay(_) | EmeCommand::Object(_)
            );
            if map_edit && !self.may_edit_map() {
                let silent = matches!(&issued.command, EmeCommand::VoxelStroke { ops, .. } if ops.is_empty());
                if !silent {
                    errors.push(EmeDispatchError {
                        origin,
                        error: EmeSendError::NoProject,
                    });
                }
                continue;
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
                    if !link_ok {
                        errors.push(EmeDispatchError {
                            origin,
                            error: EmeSendError::LinkDown,
                        });
                        continue;
                    }
                    let (layer, stroke) = (edit.layer(), edit.stroke());
                    let request = EmeServiceRequest::OvEdit(edit);
                    if let Err(error) = Self::check_size(&request) {
                        errors.push(EmeDispatchError { origin, error });
                        continue;
                    }
                    let entry = self.undo.push_overlay(now_ms, tool, layer, stroke);
                    self.send_commit(now_ms, transport, origin, entry, request, &mut errors);
                }
                EmeCommand::Object(obj) => {
                    if !link_ok {
                        errors.push(EmeDispatchError {
                            origin,
                            error: EmeSendError::LinkDown,
                        });
                        continue;
                    }
                    let tx = self.undo.alloc_tx();
                    let request = match obj {
                        EmeObjectRequest::Spawn(mut s) => {
                            s.tx = tx;
                            EmeServiceRequest::ObjSpawn(s)
                        }
                        EmeObjectRequest::Move(mut m) => {
                            m.tx = tx;
                            EmeServiceRequest::ObjMove(m)
                        }
                        EmeObjectRequest::Delete { kind, guid } => {
                            EmeServiceRequest::ObjDelete { kind, guid, tx }
                        }
                    };
                    let entry = self
                        .undo
                        .push_object(now_ms, tool, request.clone())
                        .expect("object mutation");
                    self.send_commit(now_ms, transport, origin, entry, request, &mut errors);
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

    fn check_size(request: &EmeServiceRequest) -> Result<(), EmeSendError> {
        let len = EmeServiceEnvelope::new(EmeRequestId(EME_MAX_REQUEST_ID), request.clone())
            .to_json()
            .len();
        if len > EME_MAX_FRAME_BYTES {
            return Err(EmeSendError::TooLarge(len));
        }
        Ok(())
    }

    fn send_commit(
        &mut self,
        now_ms: u64,
        transport: &mut dyn EmeTransport,
        origin: Option<EmeToolId>,
        entry: EmeEntryId,
        request: EmeServiceRequest,
        errors: &mut Vec<EmeDispatchError>,
    ) {
        let route = EmeRoute::Commit(entry, origin);
        let sent = if is_tx_mutation(&request) {
            self.client.send_idempotent(transport, now_ms, request, route)
        } else {
            self.client.send(transport, now_ms, request, route)
        };
        if let Err(error) = sent {
            let reply = EmeServiceReply::error(EmeRequestId::PUSH, error_code(&error));
            self.undo.on_commit_reply(entry, &reply);
            errors.push(EmeDispatchError { origin, error });
        }
    }

    /// Send whatever the undo stack queued; a send failure fails the entry's request at once.
    fn flush_history(&mut self, now_ms: u64, transport: &mut dyn EmeTransport) {
        for r in self.undo.drain_requests() {
            // A redo of an object op re-issues it with a fresh `tx`: safe to resend as it is.
            let route = EmeRoute::History(r.ticket);
            let sent = if is_tx_mutation(&r.request) {
                self.client.send_idempotent(transport, now_ms, r.request, route)
            } else {
                self.client.send(transport, now_ms, r.request, route)
            };
            if let Err(error) = sent {
                let reply = EmeServiceReply::error(EmeRequestId::PUSH, error_code(&error));
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
        let mut refresh_caps = false;
        for inbound in transport.poll() {
            match inbound {
                EmeInbound::EditConfirmed { nonce, seq, .. } => {
                    self.undo.on_edit_confirmed(nonce, seq);
                }
                EmeInbound::EditRejected { nonce, reason } => {
                    self.undo.on_edit_rejected(nonce, reason);
                    if reason == EmeRejectReason::NoProject {
                        // The server lost (or never had) our selection: re-open it on the next caps.
                        self.set_project(None, &mut out);
                        out.no_project = true;
                        refresh_caps = true;
                    }
                }
                EmeInbound::Reply(reply) => {
                    if let Some(done) = self.client.on_reply(reply) {
                        if done.reply.err.as_deref() == Some("no_project") {
                            self.set_project(None, &mut out);
                            out.no_project = true;
                            refresh_caps = true;
                        }
                        self.route(done.route, done.op, done.reply, &mut out);
                    }
                }
                EmeInbound::Push(push) => {
                    match &push {
                        EmeServicePush::CapsChanged { rank } => {
                            // Rank changed: act on the new rank at once, then refresh the caps.
                            self.client
                                .set_permissions(EmePermissions::from_rank(*rank));
                            if let Some(c) = self.caps.as_mut() {
                                c.rank = *rank;
                            }
                            refresh_caps = true;
                        }
                        EmeServicePush::Enforce { enforced } => {
                            if let Some(c) = self.caps.as_mut() {
                                c.enforced = *enforced;
                            }
                        }
                        EmeServicePush::Obj { .. } => {}
                        EmeServicePush::ProjectChanged { project, status, .. } => {
                            if status != "open" {
                                if self.project.as_ref().is_some_and(|p| p.id == *project) {
                                    self.set_project(None, &mut out);
                                }
                                if self.wanted == Some(*project) {
                                    self.wanted = None;
                                }
                                if status == "reverted" {
                                    self.undo.discard_project(*project);
                                }
                            }
                        }
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
        for done in self.client.expire_with(now_ms, transport) {
            self.route(done.route, done.op, done.reply, &mut out);
        }
        if refresh_caps && transport.state().is_usable() {
            // Best effort: a failure surfaces as the stale caps the host already holds.
            let _ = self.request_caps(now_ms, transport);
        }
        // MONKEY (EME projects): a fresh server selection (caps said none) → re-open ours.
        if self.reopening_due() && transport.state().is_usable() {
            let id = self.wanted.expect("wanted");
            let req = EmeServiceRequest::ProjectOpen { project: id };
            if self
                .client
                .send(transport, now_ms, req, EmeRoute::Tool(None))
                .is_ok()
            {
                self.reopening = true;
            }
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
                        let p = caps.project.clone();
                        self.set_caps(caps);
                        self.set_project(p, out);
                        self.reopening = false;
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
            EmeRoute::Tool(tool) => {
                self.note_project_reply(op, &reply, out);
                out.replies.push(EmeToolReply { tool, op, reply })
            }
        }
    }

    /// A `project_open` of `wanted` should be sent (caps arrived and named no project).
    fn reopening_due(&self) -> bool {
        self.wanted.is_some()
            && self.project.is_none()
            && !self.reopening
            && self.caps.is_some()
            && self.projects_required()
            && self.client.permissions().has(EmeCapability::Edit)
    }

    /// MONKEY (EME projects): track the active project from the project ops' replies.
    fn note_project_reply(&mut self, op: &str, reply: &EmeServiceReply, out: &mut EmePumpOutput) {
        let project_of = |r: &EmeServiceReply| -> Option<EmeProjectRef> {
            serde_json::from_value(r.extra.get("project")?.clone()).ok()
        };
        let id_of = |r: &EmeServiceReply| {
            r.extra
                .get("project_id")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
        };
        match op {
            "project_create" | "project_open" => {
                if op == "project_open" {
                    self.reopening = false;
                }
                if reply.ok {
                    if let Some(p) = project_of(reply) {
                        self.wanted = Some(p.id);
                        self.set_project(Some(p), out);
                    }
                } else if op == "project_open"
                    && matches!(reply.err.as_deref(), Some("not_found" | "conflict"))
                {
                    // Closed / reverted / gone: stop re-opening it.
                    self.wanted = None;
                }
            }
            "project_leave" if reply.ok => {
                self.wanted = None;
                self.set_project(None, out);
            }
            "project_close" | "project_revert" if reply.ok => {
                let id = id_of(reply);
                if id.is_some() && self.project.as_ref().map(|p| p.id) == id {
                    self.set_project(None, out);
                }
                if id.is_some() && self.wanted == id {
                    self.wanted = None;
                }
                if op == "project_revert" {
                    if let Some(id) = id {
                        self.undo.discard_project(id);
                    }
                }
            }
            _ => {}
        }
    }
}

/// An object mutation whose `tx` the server dedups: resending it is safe (CONTRACT v3.7).
fn is_tx_mutation(r: &EmeServiceRequest) -> bool {
    matches!(
        r,
        EmeServiceRequest::ObjSpawn(_)
            | EmeServiceRequest::ObjMove(_)
            | EmeServiceRequest::ObjDelete { .. }
    )
}

fn error_code(e: &EmeSendError) -> &'static str {
    match e {
        EmeSendError::NotPermitted(_) => "rank",
        EmeSendError::LinkDown => "link_lost",
        EmeSendError::TooLarge(_) => "invalid",
        EmeSendError::NoProject => "no_project",
    }
}

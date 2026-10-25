//! `EmeUndoStack`: EME's one shared Ctrl+Z under server authority (DESIGN-EME §3.2–§3.3,
//! CONTRACT v3 §10, §13).
//!
//! The server owns the truth. Undo is not an inverse edit: it is a **request** that reverts an
//! entry's rows server-side (`undo` for voxel and overlay strokes, `obj_undo` for an object
//! transaction); redo un-reverts them (`redo`), or for objects re-issues the original op with a
//! fresh transaction id. So every entry moves through server-confirmed states:
//!
//! ```text
//!   Open ──close──► Committing ──all confirmed / commit ok──► Applied ◄───────────────┐
//!                       │ every edit rejected / commit failed → discarded               │
//!   Applied ──undo()──► UndoPending ──ok──► Undone ──redo()──► RedoPending ──ok───────┘
//!                         │ others_edited → NeedsConfirm ──confirm_undo()──► UndoPending
//!                         │ other error   → Applied (RequestFailed event)
//!                                            Undone ◄── error ── RedoPending
//!                                            discarded ◄── conflict ──┘ (no longer redoable)
//! ```
//!
//! - **Targets.** Ctrl+Z acts on the NEWEST entry that is not already undone or being undone;
//!   Ctrl+Y on the OLDEST undone entry. Requests may be in flight for several entries at once and
//!   their replies may arrive in any order; each entry settles on its own reply.
//! - **Tickets.** Every request carries a ticket `(entry, generation)`; a reply whose ticket is stale
//!   (the entry has since moved on, e.g. after a link loss) is ignored.
//! - **Not yet committed.** Undo of an entry still `Committing` is queued and sent the moment the
//!   server confirms its work. An `Open` stroke (brush held) cannot be undone.
//! - **New work clears redo** (the server sets `redo_ok = 0` too): pushing a new entry discards
//!   every `Undone` entry; one whose undo is still in flight is discarded when it settles.
//! - **Objects.** An object undo that re-spawns a deleted object gets a NEW guid; the stack keeps a
//!   guid alias table so later redo requests name the live object.
//! - The stack is session-local; durable history is server-side (`strokes`, `log`).
//!
//! The stack never touches the network: it queues [`EmeHistoryRequest`]s
//! ([`EmeUndoStack::drain_requests`]) which the host sends (usually through
//! [`crate::api::EmeServiceClient`]) and routes the replies back to [`EmeUndoStack::on_reply`].

use crate::api::{EmeDiscardReason, EmeEvent, EmeEvents, EmeHistoryHook};
use crate::service::{
    EmeLayer, EmeObjectAction, EmeObjectKind, EmeObjectResult, EmeOthersEdited, EmeOverlayLayer,
    EmeServiceError, EmeServiceReply, EmeServiceRequest, EmeTxId, EmeUndoArgs, EmeUndoResult,
};
use crate::transport::EmeRejectReason;
use std::collections::HashMap;

/// A history entry id, unique for the life of the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EmeEntryId(pub u64);

/// Which server backend an entry undoes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmeUndoBackend {
    /// A voxel stroke.
    Stroke {
        /// The stroke id.
        stroke: u16,
    },
    /// An overlay stroke.
    Overlay {
        /// Layer.
        layer: EmeOverlayLayer,
        /// The stroke id.
        stroke: u16,
    },
    /// An object transaction (the tx changes when a redo re-issues the op).
    Object {
        /// The live transaction.
        tx: EmeTxId,
        /// What it did.
        action: EmeObjectAction,
    },
}

/// The public description of an entry (what hooks and events carry).
#[derive(Debug, Clone, PartialEq)]
pub struct EmeUndoEntryMeta {
    /// Entry id.
    pub id: EmeEntryId,
    /// Backend.
    pub backend: EmeUndoBackend,
    /// Tool that produced it (`"sculpt"`, `"water"`, …).
    pub tool: String,
    /// When it was pushed (host clock, ms).
    pub created_ms: u64,
    /// MONKEY (EME projects): the project that was active when it was pushed.
    pub project: Option<u32>,
}

/// Where an entry is in its server-confirmed life.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeEntryState {
    /// A stroke whose brush is still held.
    Open,
    /// Work sent; waiting for the server to confirm it.
    Committing,
    /// The work is live on the server.
    Applied,
    /// An undo request is in flight.
    UndoPending,
    /// The server warned that others edited after this entry; waiting for the user.
    NeedsConfirm(EmeOthersEdited),
    /// Reverted on the server.
    Undone,
    /// A redo request is in flight.
    RedoPending,
}

/// The original op of an object entry, kept so a redo can re-issue it.
#[derive(Debug, Clone, PartialEq)]
struct ObjectOp {
    request: EmeServiceRequest,
    kind: EmeObjectKind,
    /// The object the entry is about (learned from the reply for a spawn).
    guid: Option<u32>,
}

/// One history entry.
#[derive(Debug, Clone)]
pub struct EmeUndoEntry {
    meta: EmeUndoEntryMeta,
    state: EmeEntryState,
    generation: u32,
    pending_nonces: Vec<u16>,
    pending_commits: u32,
    confirmed: u32,
    rejected: u32,
    first_seq: Option<u32>,
    last_seq: Option<u32>,
    sid: Option<u32>,
    object: Option<ObjectOp>,
    awaiting_trailing: bool,
    undo_queued: bool,
    drop_on_settle: bool,
    last_error: Option<EmeServiceError>,
    last_reject: Option<EmeRejectReason>,
}

impl EmeUndoEntry {
    /// Public description.
    pub fn meta(&self) -> &EmeUndoEntryMeta {
        &self.meta
    }
    /// Current state.
    pub fn state(&self) -> &EmeEntryState {
        &self.state
    }
    /// Edits (strokes) or batches (overlay / object) the server accepted.
    pub fn confirmed(&self) -> u32 {
        self.confirmed
    }
    /// Edits or batches the server rejected.
    pub fn rejected(&self) -> u32 {
        self.rejected
    }
    /// Edits or batches still awaiting confirmation.
    pub fn pending(&self) -> usize {
        self.pending_nonces.len() + self.pending_commits as usize
    }
    /// Server seq range of the confirmed voxel edits.
    pub fn seq_range(&self) -> Option<(u32, u32)> {
        Some((self.first_seq?, self.last_seq?))
    }
    /// The server's stroke row id, once an undo reply named it.
    pub fn sid(&self) -> Option<u32> {
        self.sid
    }
    /// The object the entry is about (object entries).
    pub fn object_guid(&self) -> Option<u32> {
        self.object.as_ref().and_then(|o| o.guid)
    }
    /// An undo is queued until the work is confirmed.
    pub fn undo_queued(&self) -> bool {
        self.undo_queued
    }
    /// The last request error, if any.
    pub fn last_error(&self) -> Option<&EmeServiceError> {
        self.last_error.as_ref()
    }
    /// The last edit reject reason, if any.
    pub fn last_reject(&self) -> Option<EmeRejectReason> {
        self.last_reject
    }

    /// On the "applied" side (what Ctrl+Z may target).
    fn on_applied_side(&self) -> bool {
        match self.state {
            EmeEntryState::Open
            | EmeEntryState::Applied
            | EmeEntryState::RedoPending
            | EmeEntryState::NeedsConfirm(_) => true,
            EmeEntryState::Committing => !self.undo_queued,
            EmeEntryState::UndoPending | EmeEntryState::Undone => false,
        }
    }

    /// On the "undone" side (what Ctrl+Y may target).
    fn on_undone_side(&self) -> bool {
        if self.drop_on_settle {
            return false;
        }
        match self.state {
            EmeEntryState::UndoPending | EmeEntryState::Undone => true,
            EmeEntryState::Committing => self.undo_queued,
            _ => false,
        }
    }

    fn layer(&self) -> Option<EmeLayer> {
        match self.meta.backend {
            EmeUndoBackend::Stroke { .. } => Some(EmeLayer::Voxel),
            EmeUndoBackend::Overlay { layer, .. } => Some(layer.into()),
            EmeUndoBackend::Object { .. } => None,
        }
    }

    fn stroke_args(&self, force: bool) -> EmeUndoArgs {
        match (self.sid, self.meta.backend) {
            (Some(sid), _) => EmeUndoArgs {
                sid: Some(sid),
                force,
                ..Default::default()
            },
            (None, EmeUndoBackend::Stroke { stroke } | EmeUndoBackend::Overlay { stroke, .. }) => {
                EmeUndoArgs {
                    stroke: Some(stroke),
                    layer: self.layer(),
                    force,
                    sid: None,
                }
            }
            (None, EmeUndoBackend::Object { .. }) => EmeUndoArgs::default(),
        }
    }
}

/// A request the stack wants sent, and the ticket its reply must be routed back with.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeHistoryRequest {
    /// Route the reply back with this.
    pub ticket: EmeHistoryTicket,
    /// What to send.
    pub request: EmeServiceRequest,
}

/// Identifies one request of one entry; stale tickets are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmeHistoryTicket {
    /// The entry.
    pub entry: EmeEntryId,
    generation: u32,
}

/// What an undo call did.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeUndoStarted {
    /// A request was queued for sending.
    Sent(EmeEntryId),
    /// The entry's work is not confirmed yet; the undo goes out once it is.
    Queued(EmeEntryId),
    /// The user must confirm first (others edited afterwards); call
    /// [`EmeUndoStack::confirm_undo`] or [`EmeUndoStack::cancel_confirm`].
    NeedsConfirmation(EmeEntryId, EmeOthersEdited),
}

/// Why an undo / redo did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmeUndoError {
    /// Nothing to undo / redo.
    Nothing,
    /// The target is busy (a held brush, or the opposite request in flight); try again shortly.
    Busy(EmeEntryId),
    /// Unknown entry, or not in a state that accepts this call.
    InvalidEntry,
}

/// Default number of entries kept.
pub const EME_UNDO_CAPACITY: usize = 200;

/// EME's single undo stack (terrain strokes, overlay strokes, object transactions).
pub struct EmeUndoStack {
    entries: Vec<EmeUndoEntry>,
    next_id: u64,
    next_tx: u32,
    capacity: usize,
    nonce_owner: HashMap<u16, EmeEntryId>,
    guid_alias: HashMap<(EmeObjectKind, u32), u32>,
    outbox: Vec<EmeHistoryRequest>,
    events: EmeEvents,
    hooks: Vec<Box<dyn EmeHistoryHook>>,
    /// MONKEY (EME projects): stamped on every new entry's meta.
    project: Option<u32>,
}

impl Default for EmeUndoStack {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EmeUndoStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmeUndoStack")
            .field("entries", &self.entries)
            .field("outbox", &self.outbox)
            .field("hooks", &self.hooks.len())
            .finish()
    }
}

impl EmeUndoStack {
    /// An empty stack keeping [`EME_UNDO_CAPACITY`] entries.
    pub fn new() -> Self {
        Self::with_capacity(EME_UNDO_CAPACITY)
    }

    /// An empty stack keeping at most `capacity` settled entries.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
            next_tx: 1,
            capacity: capacity.max(1),
            nonce_owner: HashMap::new(),
            guid_alias: HashMap::new(),
            outbox: Vec::new(),
            events: EmeEvents::default(),
            hooks: Vec::new(),
            project: None,
        }
    }

    /// MONKEY (EME projects): the active project, stamped on entries pushed from now on.
    pub fn set_project(&mut self, project: Option<u32>) {
        self.project = project;
    }

    /// MONKEY (EME projects): the project stamped on new entries.
    pub fn project(&self) -> Option<u32> {
        self.project
    }

    /// MONKEY (EME projects): a whole project was reverted on the server: its settled entries can
    /// no longer be undone or redone one by one, so they leave the stack
    /// ([`EmeDiscardReason::ProjectReverted`]). Entries still waiting for the server stay.
    pub fn discard_project(&mut self, project: u32) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < self.entries.len() {
            let e = &self.entries[i];
            let settled = matches!(e.state, EmeEntryState::Applied | EmeEntryState::Undone)
                && e.pending_nonces.is_empty()
                && e.pending_commits == 0;
            if e.meta.project == Some(project) && settled {
                self.remove_at(i, EmeDiscardReason::ProjectReverted);
                n += 1;
            } else {
                i += 1;
            }
        }
        n
    }

    /// Continue object transaction ids after `last` (tx ids are unique per character on the
    /// server; a repeat of a processed `(character, tx)` is replayed, not applied).
    pub fn with_tx_start(mut self, next_tx: u32) -> Self {
        self.next_tx = next_tx.max(1);
        self
    }

    /// The first transaction id of a session that starts at `unix_secs`.
    ///
    /// The server keeps every `(character, tx)` it ever processed and ANSWERS A REPEAT FROM ITS
    /// JOURNAL without doing anything (CONTRACT v3.7 `replayed`), so a client that restarted its
    /// counter at 1 would have its first spawns silently "replayed" onto objects that no longer
    /// exist (found live, 2026-10-25). Seeding from the clock (16 ids per second since
    /// 2026-01-01, good until 2034) keeps sessions apart; a session would need to send more than
    /// 16 object ops per second of the gap to the next one to collide.
    pub fn tx_seed(unix_secs: u64) -> u32 {
        const EPOCH_2026: u64 = 1_767_225_600;
        (unix_secs.saturating_sub(EPOCH_2026).saturating_mul(16)).min(u64::from(u32::MAX - 1_000_000)) as u32 + 1
    }

    /// Register a host hook (§2.5).
    pub fn add_hook(&mut self, hook: Box<dyn EmeHistoryHook>) {
        self.hooks.push(hook);
    }

    /// A fresh object transaction id.
    pub fn alloc_tx(&mut self) -> EmeTxId {
        let tx = EmeTxId(self.next_tx);
        self.next_tx = self.next_tx.checked_add(1).unwrap_or(1);
        tx
    }

    /// Entries, oldest first.
    pub fn entries(&self) -> &[EmeUndoEntry] {
        &self.entries
    }

    /// One entry.
    pub fn get(&self, id: EmeEntryId) -> Option<&EmeUndoEntry> {
        self.entries.iter().find(|e| e.meta.id == id)
    }

    /// Number of entries on the applied side (the classic undo cursor).
    pub fn cursor(&self) -> usize {
        self.entries.iter().filter(|e| e.on_applied_side()).count()
    }

    /// Whether Ctrl+Z has a target.
    pub fn can_undo(&self) -> bool {
        self.undo_target().is_some()
    }

    /// Whether Ctrl+Y has a target.
    pub fn can_redo(&self) -> bool {
        self.redo_target().is_some()
    }

    /// The live guid of an object the stack has seen re-spawned (follows the alias chain).
    pub fn resolve_guid(&self, kind: EmeObjectKind, guid: u32) -> u32 {
        let mut g = guid;
        for _ in 0..64 {
            match self.guid_alias.get(&(kind, g)) {
                Some(&next) if next != g => g = next,
                _ => break,
            }
        }
        g
    }

    /// Requests to send, oldest first.
    pub fn drain_requests(&mut self) -> Vec<EmeHistoryRequest> {
        std::mem::take(&mut self.outbox)
    }

    /// Events since the last drain, oldest first.
    pub fn drain_events(&mut self) -> Vec<EmeEvent> {
        self.events.drain()
    }

    // -----------------------------------------------------------------------------------------
    // Pushing work
    // -----------------------------------------------------------------------------------------

    fn index(&self, id: EmeEntryId) -> Option<usize> {
        self.entries.iter().position(|e| e.meta.id == id)
    }

    fn stroke_entry(&self, stroke: u16) -> Option<usize> {
        self.entries.iter().rposition(
            |e| matches!(e.meta.backend, EmeUndoBackend::Stroke { stroke: s } if s == stroke),
        )
    }

    fn push(
        &mut self,
        now_ms: u64,
        backend: EmeUndoBackend,
        tool: &str,
        state: EmeEntryState,
    ) -> usize {
        self.clear_redo_branch();
        let id = EmeEntryId(self.next_id);
        self.next_id += 1;
        let meta = EmeUndoEntryMeta {
            id,
            backend,
            tool: tool.to_owned(),
            created_ms: now_ms,
            project: self.project,
        };
        self.entries.push(EmeUndoEntry {
            meta: meta.clone(),
            state,
            generation: 0,
            pending_nonces: Vec::new(),
            pending_commits: 0,
            confirmed: 0,
            rejected: 0,
            first_seq: None,
            last_seq: None,
            sid: None,
            object: None,
            awaiting_trailing: false,
            undo_queued: false,
            drop_on_settle: false,
            last_error: None,
            last_reject: None,
        });
        for h in &self.hooks {
            h.on_push(&meta);
        }
        self.events.push(EmeEvent::EntryPushed(meta));
        self.evict();
        self.index(id).expect("just pushed")
    }

    /// New work clears the redo branch: undone entries go now, in-flight undos when they settle,
    /// and a pending "undo anyway?" question is cancelled.
    fn clear_redo_branch(&mut self) {
        let mut i = 0;
        while i < self.entries.len() {
            let e = &mut self.entries[i];
            match e.state {
                EmeEntryState::Undone => {
                    self.remove_at(i, EmeDiscardReason::RedoBranchCleared);
                    continue;
                }
                EmeEntryState::UndoPending => e.drop_on_settle = true,
                EmeEntryState::Committing if e.undo_queued => e.drop_on_settle = true,
                EmeEntryState::NeedsConfirm(_) => e.state = EmeEntryState::Applied,
                _ => {}
            }
            i += 1;
        }
    }

    fn evict(&mut self) {
        while self.entries.len() > self.capacity {
            let Some(i) = self
                .entries
                .iter()
                .position(|e| e.state == EmeEntryState::Applied)
            else {
                break;
            };
            self.remove_at(i, EmeDiscardReason::Evicted);
        }
    }

    fn remove_at(&mut self, i: usize, reason: EmeDiscardReason) {
        let e = self.entries.remove(i);
        for n in &e.pending_nonces {
            self.nonce_owner.remove(n);
        }
        for h in &self.hooks {
            h.on_discard(&e.meta, &reason);
        }
        self.events.push(EmeEvent::EntryDiscarded {
            meta: e.meta,
            reason,
        });
    }

    /// A stroke edit was uplinked: records `nonce` under `stroke`, opening the stroke's entry on
    /// its first edit. Opening a new stroke closes any other still-open stroke (a split stroke).
    pub fn note_edit(&mut self, now_ms: u64, tool: &str, stroke: u16, nonce: u16) -> EmeEntryId {
        let idx = match self.stroke_entry(stroke) {
            Some(i)
                if self.entries[i].state == EmeEntryState::Open
                    || self.entries[i].awaiting_trailing =>
            {
                i
            }
            _ => {
                let open: Vec<u16> = self
                    .entries
                    .iter()
                    .filter(|e| e.state == EmeEntryState::Open)
                    .filter_map(|e| match e.meta.backend {
                        EmeUndoBackend::Stroke { stroke } => Some(stroke),
                        _ => None,
                    })
                    .collect();
                for s in open {
                    self.close_stroke(s, false);
                }
                self.push(
                    now_ms,
                    EmeUndoBackend::Stroke { stroke },
                    tool,
                    EmeEntryState::Open,
                )
            }
        };
        let e = &mut self.entries[idx];
        e.pending_nonces.push(nonce);
        let id = e.meta.id;
        // The trailing release edit arrived: the stroke is now fully sent.
        e.awaiting_trailing = false;
        self.nonce_owner.insert(nonce, id);
        id
    }

    /// The brush was released. `trailing` = one more edit of this stroke is still waiting for the
    /// rate limit ([`crate::stroke::EmeStrokeEnd::trailing`]); the stroke counts as sent only after it.
    pub fn close_stroke(&mut self, stroke: u16, trailing: bool) {
        let Some(i) = self.stroke_entry(stroke) else {
            return;
        };
        if self.entries[i].state != EmeEntryState::Open {
            return;
        }
        self.entries[i].state = EmeEntryState::Committing;
        self.entries[i].awaiting_trailing = trailing;
        self.settle_commit_if_done(i);
    }

    /// An `ov_edit` batch was sent; route its reply to [`Self::on_commit_reply`]. A further batch of
    /// the same overlay stroke, while that stroke is still the newest entry, joins its entry.
    pub fn push_overlay(
        &mut self,
        now_ms: u64,
        tool: &str,
        layer: EmeOverlayLayer,
        stroke: u16,
    ) -> EmeEntryId {
        let backend = EmeUndoBackend::Overlay { layer, stroke };
        if let Some(last) = self.entries.last_mut() {
            if last.meta.backend == backend
                && !last.undo_queued
                && matches!(
                    last.state,
                    EmeEntryState::Committing | EmeEntryState::Applied
                )
            {
                last.state = EmeEntryState::Committing;
                last.pending_commits += 1;
                return last.meta.id;
            }
        }
        let i = self.push(now_ms, backend, tool, EmeEntryState::Committing);
        self.entries[i].pending_commits = 1;
        self.entries[i].meta.id
    }

    /// An object mutation (`obj_spawn` / `obj_move` / `obj_delete`, with its `tx` set) was sent;
    /// route its reply to [`Self::on_commit_reply`]. Other requests are refused (`None`).
    pub fn push_object(
        &mut self,
        now_ms: u64,
        tool: &str,
        request: EmeServiceRequest,
    ) -> Option<EmeEntryId> {
        let (tx, action, kind, guid) = match &request {
            EmeServiceRequest::ObjSpawn(s) => (s.tx, EmeObjectAction::Spawn, s.kind, None),
            EmeServiceRequest::ObjMove(m) => (m.tx, EmeObjectAction::Move, m.kind, Some(m.guid)),
            EmeServiceRequest::ObjDelete { kind, guid, tx } => {
                (*tx, EmeObjectAction::Delete, *kind, Some(*guid))
            }
            _ => return None,
        };
        let i = self.push(
            now_ms,
            EmeUndoBackend::Object { tx, action },
            tool,
            EmeEntryState::Committing,
        );
        let e = &mut self.entries[i];
        e.pending_commits = 1;
        e.object = Some(ObjectOp {
            request,
            kind,
            guid,
        });
        Some(e.meta.id)
    }

    /// The reply to a request that carried an entry's own work (`ov_edit`, `obj_*`). The entry
    /// commits once every batch has answered and at least one succeeded.
    pub fn on_commit_reply(&mut self, entry: EmeEntryId, reply: &EmeServiceReply) -> bool {
        let Some(i) = self.index(entry) else {
            return false;
        };
        let e = &mut self.entries[i];
        if e.state != EmeEntryState::Committing || e.pending_commits == 0 {
            return false;
        }
        e.pending_commits -= 1;
        if reply.ok {
            e.confirmed += 1;
            if let Some(obj) = e.object.as_mut() {
                if let Ok(EmeObjectResult { row: Some(row), .. }) = reply.extra_as() {
                    obj.guid = Some(row.guid);
                }
            }
        } else {
            e.rejected += 1;
            e.last_error = Some(reply.error_code().unwrap_or(EmeServiceError::Invalid));
        }
        if e.pending_commits > 0 {
            return true;
        }
        if e.confirmed > 0 {
            self.commit(i);
        } else {
            let err = e.last_error.clone().unwrap_or(EmeServiceError::Invalid);
            self.remove_at(i, EmeDiscardReason::CommitFailed(err));
        }
        true
    }

    /// The server confirmed one of our edits (`e` with our nonce). Order does not matter.
    pub fn on_edit_confirmed(&mut self, nonce: u16, seq: u32) -> bool {
        let Some(i) = self.take_nonce(nonce) else {
            return false;
        };
        let e = &mut self.entries[i];
        e.confirmed += 1;
        e.first_seq = Some(e.first_seq.map_or(seq, |s| s.min(seq)));
        e.last_seq = Some(e.last_seq.map_or(seq, |s| s.max(seq)));
        self.settle_commit_if_done(i);
        true
    }

    /// The server rejected one of our edits (`R` with our nonce).
    pub fn on_edit_rejected(&mut self, nonce: u16, reason: EmeRejectReason) -> bool {
        let Some(i) = self.take_nonce(nonce) else {
            return false;
        };
        self.entries[i].rejected += 1;
        self.entries[i].last_reject = Some(reason);
        self.settle_commit_if_done(i);
        true
    }

    fn take_nonce(&mut self, nonce: u16) -> Option<usize> {
        let id = self.nonce_owner.remove(&nonce)?;
        let i = self.index(id)?;
        let e = &mut self.entries[i];
        let p = e.pending_nonces.iter().position(|n| *n == nonce)?;
        e.pending_nonces.swap_remove(p);
        Some(i)
    }

    fn settle_commit_if_done(&mut self, i: usize) {
        let e = &self.entries[i];
        if e.state != EmeEntryState::Committing
            || e.awaiting_trailing
            || !e.pending_nonces.is_empty()
        {
            return;
        }
        if e.confirmed == 0 {
            let reason = EmeServiceError::parse(reject_code(e.last_reject));
            self.remove_at(i, EmeDiscardReason::CommitFailed(reason));
        } else {
            self.commit(i);
        }
    }

    fn commit(&mut self, i: usize) {
        let e = &mut self.entries[i];
        e.state = EmeEntryState::Applied;
        let meta = e.meta.clone();
        let queued = std::mem::take(&mut e.undo_queued);
        for h in &self.hooks {
            h.on_commit(&meta);
        }
        self.events.push(EmeEvent::EntryCommitted(meta));
        if queued {
            self.issue_undo(i, false);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Undo / redo
    // -----------------------------------------------------------------------------------------

    fn undo_target(&self) -> Option<usize> {
        self.entries.iter().rposition(|e| e.on_applied_side())
    }

    fn redo_target(&self) -> Option<usize> {
        self.entries.iter().position(|e| e.on_undone_side())
    }

    fn ticket(&mut self, i: usize) -> EmeHistoryTicket {
        let e = &mut self.entries[i];
        e.generation += 1;
        e.last_error = None;
        EmeHistoryTicket {
            entry: e.meta.id,
            generation: e.generation,
        }
    }

    fn issue_undo(&mut self, i: usize, force: bool) {
        let ticket = self.ticket(i);
        let e = &mut self.entries[i];
        e.state = EmeEntryState::UndoPending;
        let request = match e.meta.backend {
            EmeUndoBackend::Object { tx, .. } => EmeServiceRequest::ObjUndo { tx, actor: None },
            _ => EmeServiceRequest::Undo(e.stroke_args(force)),
        };
        self.outbox.push(EmeHistoryRequest { ticket, request });
    }

    fn issue_redo(&mut self, i: usize) {
        let request = match self.entries[i].meta.backend {
            EmeUndoBackend::Object { action, .. } => {
                // Objects redo client-side: the original op again, fresh tx, live guid.
                let tx = self.alloc_tx();
                let obj = self.entries[i].object.clone().expect("object entry");
                let live = obj.guid.map(|g| self.resolve_guid(obj.kind, g));
                let request = match obj.request {
                    EmeServiceRequest::ObjSpawn(mut s) => {
                        s.tx = tx;
                        EmeServiceRequest::ObjSpawn(s)
                    }
                    EmeServiceRequest::ObjMove(mut m) => {
                        m.tx = tx;
                        m.guid = live.unwrap_or(m.guid);
                        EmeServiceRequest::ObjMove(m)
                    }
                    EmeServiceRequest::ObjDelete { kind, guid, .. } => {
                        EmeServiceRequest::ObjDelete {
                            kind,
                            guid: live.unwrap_or(guid),
                            tx,
                        }
                    }
                    other => other,
                };
                self.entries[i].meta.backend = EmeUndoBackend::Object { tx, action };
                request
            }
            _ => EmeServiceRequest::Redo(self.entries[i].stroke_args(false)),
        };
        let ticket = self.ticket(i);
        self.entries[i].state = EmeEntryState::RedoPending;
        self.outbox.push(EmeHistoryRequest { ticket, request });
    }

    /// Ctrl+Z.
    pub fn undo(&mut self) -> Result<EmeUndoStarted, EmeUndoError> {
        let i = self.undo_target().ok_or(EmeUndoError::Nothing)?;
        let id = self.entries[i].meta.id;
        match &self.entries[i].state {
            EmeEntryState::Applied => {
                self.issue_undo(i, false);
                Ok(EmeUndoStarted::Sent(id))
            }
            EmeEntryState::Committing => {
                self.entries[i].undo_queued = true;
                Ok(EmeUndoStarted::Queued(id))
            }
            EmeEntryState::NeedsConfirm(others) => {
                Ok(EmeUndoStarted::NeedsConfirmation(id, others.clone()))
            }
            _ => Err(EmeUndoError::Busy(id)),
        }
    }

    /// Ctrl+Y.
    pub fn redo(&mut self) -> Result<EmeEntryId, EmeUndoError> {
        let i = self.redo_target().ok_or(EmeUndoError::Nothing)?;
        let id = self.entries[i].meta.id;
        if self.entries[i].state == EmeEntryState::Undone {
            self.issue_redo(i);
            Ok(id)
        } else {
            Err(EmeUndoError::Busy(id))
        }
    }

    /// "Undo anyway": resend the undo of an entry in `NeedsConfirm` with `force`.
    pub fn confirm_undo(&mut self, entry: EmeEntryId) -> Result<EmeUndoStarted, EmeUndoError> {
        let i = self.index(entry).ok_or(EmeUndoError::InvalidEntry)?;
        if !matches!(self.entries[i].state, EmeEntryState::NeedsConfirm(_)) {
            return Err(EmeUndoError::InvalidEntry);
        }
        self.issue_undo(i, true);
        Ok(EmeUndoStarted::Sent(entry))
    }

    /// "Keep it": the user declined the forced undo.
    pub fn cancel_confirm(&mut self, entry: EmeEntryId) -> Result<(), EmeUndoError> {
        let i = self.index(entry).ok_or(EmeUndoError::InvalidEntry)?;
        if !matches!(self.entries[i].state, EmeEntryState::NeedsConfirm(_)) {
            return Err(EmeUndoError::InvalidEntry);
        }
        self.entries[i].state = EmeEntryState::Applied;
        Ok(())
    }

    fn alias(&mut self, kind: EmeObjectKind, old: u32, new: u32) {
        if old != new {
            self.guid_alias.insert((kind, old), new);
        }
    }

    /// Route a reply to an undo / redo request. Returns false for a stale or unknown ticket.
    pub fn on_reply(&mut self, ticket: EmeHistoryTicket, reply: &EmeServiceReply) -> bool {
        let Some(i) = self.index(ticket.entry) else {
            return false;
        };
        if self.entries[i].generation != ticket.generation {
            return false;
        }
        let err = reply.error_code();
        match self.entries[i].state {
            EmeEntryState::UndoPending => match err {
                None => {
                    self.learn_from_undo(i, reply);
                    let e = &mut self.entries[i];
                    e.state = EmeEntryState::Undone;
                    let meta = e.meta.clone();
                    let drop = e.drop_on_settle;
                    for h in &self.hooks {
                        h.on_undo(&meta);
                    }
                    self.events.push(EmeEvent::EntryUndone(meta));
                    if drop {
                        self.remove_at(i, EmeDiscardReason::RedoBranchCleared);
                    }
                }
                Some(EmeServiceError::OthersEdited) if !self.entries[i].drop_on_settle => {
                    let others: EmeOthersEdited = reply.extra_as().unwrap_or_default();
                    let e = &mut self.entries[i];
                    e.state = EmeEntryState::NeedsConfirm(others.clone());
                    self.events.push(EmeEvent::UndoNeedsConfirm {
                        meta: e.meta.clone(),
                        others,
                    });
                }
                Some(error) => {
                    let e = &mut self.entries[i];
                    e.state = EmeEntryState::Applied;
                    e.drop_on_settle = false;
                    e.last_error = Some(error.clone());
                    self.events.push(EmeEvent::RequestFailed {
                        meta: e.meta.clone(),
                        op: "undo",
                        error,
                    });
                }
            },
            EmeEntryState::RedoPending => match err {
                None => {
                    self.learn_from_redo(i, reply);
                    let e = &mut self.entries[i];
                    e.state = EmeEntryState::Applied;
                    let meta = e.meta.clone();
                    for h in &self.hooks {
                        h.on_redo(&meta);
                    }
                    self.events.push(EmeEvent::EntryRedone(meta));
                }
                Some(EmeServiceError::Conflict) => {
                    self.entries[i].last_error = Some(EmeServiceError::Conflict);
                    self.remove_at(i, EmeDiscardReason::RedoConflict);
                }
                Some(error) => {
                    let e = &mut self.entries[i];
                    e.state = EmeEntryState::Undone;
                    e.last_error = Some(error.clone());
                    self.events.push(EmeEvent::RequestFailed {
                        meta: e.meta.clone(),
                        op: "redo",
                        error,
                    });
                }
            },
            _ => return false,
        }
        true
    }

    fn learn_from_undo(&mut self, i: usize, reply: &EmeServiceReply) {
        if self.entries[i].object.is_some() {
            // Undo of a delete re-spawns the object under a NEW guid.
            if let Ok(EmeObjectResult {
                row: Some(row),
                replaces: Some(old),
                ..
            }) = reply.extra_as()
            {
                let kind = self.entries[i].object.as_ref().expect("object").kind;
                self.alias(kind, old, row.guid);
            }
        } else if let Ok(EmeUndoResult { sid: Some(sid), .. }) = reply.extra_as() {
            self.entries[i].sid = Some(sid);
        }
    }

    fn learn_from_redo(&mut self, i: usize, reply: &EmeServiceReply) {
        let Some(obj) = self.entries[i].object.clone() else {
            return;
        };
        // A re-issued spawn creates a new object: later entries naming the old guid follow it.
        if let Ok(EmeObjectResult { row: Some(row), .. }) = reply.extra_as() {
            if let Some(old) = obj.guid {
                let live = self.resolve_guid(obj.kind, old);
                self.alias(obj.kind, live, row.guid);
            }
            if let Some(o) = self.entries[i].object.as_mut() {
                o.guid = Some(row.guid);
            }
        }
    }

    /// The link dropped: in-flight undo/redo requests fail back to their previous state (their
    /// late replies are ignored), held strokes close, and work the server never confirmed is
    /// discarded (an entry with some confirmed work stays, as those rows exist).
    pub fn on_link_lost(&mut self) {
        let mut i = 0;
        while i < self.entries.len() {
            let e = &mut self.entries[i];
            e.generation += 1;
            match e.state {
                EmeEntryState::UndoPending => {
                    e.state = EmeEntryState::Applied;
                    e.drop_on_settle = false;
                    e.last_error = Some(EmeServiceError::LinkLost);
                }
                EmeEntryState::RedoPending => {
                    e.state = EmeEntryState::Undone;
                    e.last_error = Some(EmeServiceError::LinkLost);
                }
                EmeEntryState::Open | EmeEntryState::Committing => {
                    let nonces = std::mem::take(&mut e.pending_nonces);
                    e.awaiting_trailing = false;
                    e.undo_queued = false;
                    e.pending_commits = 0;
                    let keep = e.confirmed > 0;
                    if keep {
                        e.state = EmeEntryState::Applied;
                    }
                    for n in nonces {
                        self.nonce_owner.remove(&n);
                    }
                    if !keep {
                        self.remove_at(i, EmeDiscardReason::LinkLost);
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Drop every entry (relog, map change). Requests already drained are not recalled; their
    /// replies are ignored.
    pub fn clear(&mut self) {
        while !self.entries.is_empty() {
            self.remove_at(0, EmeDiscardReason::Cleared);
        }
        self.nonce_owner.clear();
        self.guid_alias.clear();
        self.outbox.clear();
    }
}

fn reject_code(r: Option<EmeRejectReason>) -> &'static str {
    match r {
        Some(EmeRejectReason::Rank) => "rank",
        Some(EmeRejectReason::Range) => "range",
        Some(EmeRejectReason::Rate) => "rate",
        Some(EmeRejectReason::Map) => "map",
        Some(EmeRejectReason::Stroke) => "stroke",
        Some(EmeRejectReason::NoProject) => "no_project",
        Some(EmeRejectReason::Invalid | EmeRejectReason::Other(_)) | None => "invalid",
    }
}

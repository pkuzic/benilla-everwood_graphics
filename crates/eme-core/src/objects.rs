//! The Objects tool's engine-free brain (DESIGN-EME §5, CONTRACT v3 §13 / v3.7): the outliner
//! rows, the selection, the gizmo drag, the ghost preview, the spawn placement and the requests
//! that keep the list fresh.
//!
//! [`EmeObjectsEditor`] is a plain state machine. A host feeds it
//! - the streamed objects it can pick ([`EmeObjectsEditor::set_world`]),
//! - one input frame ([`EmeObjectsEditor::step`]) which emits [`EmeCommand`]s — `obj_move`,
//!   `obj_spawn`, `obj_delete` as [`EmeCommand::Object`] (so they enter the shared undo stack with
//!   a transaction id) and `obj_nearby` / `obj_get` as plain service requests,
//! - the replies and pushes ([`EmeObjectsEditor::on_reply`], [`EmeObjectsEditor::on_push`]),
//! - the commands of the panel and the hotkeys ([`EmeObjectsCmd`]),
//!
//! and reads back snapshots for the panel ([`EmeObjectsView`]) and the viewport
//! ([`EmeObjectsDraw`]). Nothing here touches Bevy, egui or the network.
//!
//! Rotation: the wire carries ONE orientation `o` (yaw about the vertical, CONTRACT §13), so the
//! gizmo rotates about +Z only; pitch and roll stay whatever the server holds.

use std::collections::{BTreeMap, BTreeSet};

use crate::command::{EmeCommand, EmeCommandSink, EmeObjectRequest};
use crate::gizmo::{
    add, apply_delta, hit_test, mul, normalize, pick_object, sub, wrap_tau, EmeGizmoDelta,
    EmeGizmoDrag, EmeGizmoFrame, EmeGizmoHandle, EmeGizmoMode, EmeGizmoSpace, EmeObb, EmeObjXform,
    EmeSnap, EmeWorldObject, V3,
};
use crate::perm::EmeCapability;
use crate::service::{
    EmeObjMove, EmeObjSpawn, EmeObjectAction, EmeObjectFilter, EmeObjectKind, EmeObjectResult,
    EmeObjectRow, EmeServiceError, EmeServicePush, EmeServiceReply, EmeServiceRequest, EmeTxId,
};
use crate::tool::{EmeInputCtx, EmeTerrain, EmeToolNotice};

/// Rows asked per `obj_nearby` page (the server caps it at 16).
pub const PAGE_ROWS: u32 = 16;
/// Pages fetched per refresh (96 rows); "Load more" fetches that many again.
pub const PAGES_PER_LOAD: u32 = 6;
/// Smallest gap between two object requests, ms (the server allows 10 requests/s).
pub const REQUEST_GAP_MS: u64 = 160;
/// A request without an answer is forgotten after this long, ms.
pub const REQUEST_TTL_MS: u64 = 12_000;
/// A ghost that never got its reply is dropped after this long, ms.
pub const GHOST_TTL_MS: u64 = 15_000;
/// The camera moving this far from the last list centre asks for a fresh list, yards.
pub const RECENTRE_YD: f32 = 30.0;
/// Fewest ms between two automatic refreshes.
pub const REFRESH_GAP_MS: u64 = 2_500;
/// How far a click ray reaches, yards.
pub const PICK_REACH: f32 = 600.0;
/// The offset a duplicate gets from its original, yards.
pub const DUPLICATE_OFFSET: f32 = 1.0;
/// The default list radius, yards.
pub const DEFAULT_RADIUS: f32 = 80.0;
/// Most spawn picks remembered.
pub const RECENT_MAX: usize = 12;
/// Position limits the server accepts (CONTRACT §13).
pub const POS_LIMIT: f32 = 20_000.0;

/// An inspector field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmeObjField {
    /// X, yards.
    X,
    /// Y, yards.
    Y,
    /// Z, yards.
    Z,
    /// Orientation, DEGREES (the wire carries radians).
    Orientation,
    /// Uniform scale.
    Scale,
}

/// What the panel and the hotkeys ask of the editor.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeObjectsCmd {
    /// Select exactly this object.
    Select(u32),
    /// Add / remove this object from the selection.
    ToggleSelect(u32),
    /// Clear the selection.
    SelectNone,
    /// Point the camera at the selection (the host moves it).
    Frame,
    /// Delete the selected objects.
    Delete,
    /// Spawn a copy of every selected object next to it.
    Duplicate,
    /// Re-read the list from the server.
    Refresh,
    /// Fetch another batch of pages.
    LoadMore,
    /// Set a field of the primary (last selected) object and send the move.
    SetField {
        /// Which.
        field: EmeObjField,
        /// The new value (degrees for the orientation).
        value: f32,
    },
    /// Place this template by clicking the ground.
    ArmSpawn {
        /// Template entry.
        entry: u32,
        /// Name to show, if known.
        name: Option<String>,
    },
    /// Stop placing.
    DisarmSpawn,
    /// Switch the gizmo.
    SetMode(EmeGizmoMode),
    /// World / local axes.
    SetSpace(EmeGizmoSpace),
}

/// Where the host should put the camera.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeFrameRequest {
    /// Centre of what to show, WoW yards.
    pub center: V3,
    /// Bounding radius of it, yards.
    pub radius: f32,
}

/// The panel's snapshot of the editor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmeObjectsView {
    /// Bumped on every change (the host copies the view only when it moves).
    pub rev: u64,
    /// The Objects tool is the active tool.
    pub active: bool,
    /// The caller may change objects (rank 3+).
    pub can_edit: bool,
    /// The outliner rows, nearest first.
    pub rows: Vec<EmeObjectRow>,
    /// More rows exist than were loaded.
    pub truncated: bool,
    /// A list refresh is in flight.
    pub loading: bool,
    /// The selection, in order (the last is the primary).
    pub selected: Vec<u32>,
    /// The primary object's row (the inspector).
    pub primary: Option<EmeObjectRow>,
    /// The primary object's row is still being fetched.
    pub primary_loading: bool,
    /// The template being placed.
    pub armed: Option<(u32, Option<String>)>,
    /// Templates the client has seen (entry, name): recent spawns first, then what is nearby.
    pub templates: Vec<(u32, String)>,
    /// Moves / spawns / deletes sent and not yet answered.
    pub pending: u32,
    /// The gizmo mode.
    pub mode: EmeGizmoMode,
    /// World / local axes.
    pub space: EmeGizmoSpace,
}

/// What the viewport draws for the Objects tool.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmeObjectsDraw {
    /// Boxes around the selected objects.
    pub selected: Vec<EmeObb>,
    /// The box under the cursor.
    pub hover: Option<EmeObb>,
    /// Where the objects will land (a drag in progress, or a move awaiting its reply).
    pub ghosts: Vec<EmeObb>,
    /// The gizmo, when something is selected and the caller may edit.
    pub gizmo: Option<EmeGizmoDraw>,
    /// Where a click would place the armed template.
    pub placing: Option<V3>,
}

/// The gizmo to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeGizmoDraw {
    /// Mode.
    pub mode: EmeGizmoMode,
    /// Frame (pivot, axes, size).
    pub frame: EmeGizmoFrame,
    /// The handle being dragged.
    pub active: Option<EmeGizmoHandle>,
    /// The handle under the cursor.
    pub hover: Option<EmeGizmoHandle>,
}

#[derive(Debug, Clone)]
struct Ghost {
    from: EmeObjXform,
    to: EmeObjXform,
    /// The move was sent; the box stays until the reply.
    sent: bool,
    expires_ms: u64,
}

#[derive(Debug, Clone)]
struct ActiveDrag {
    drag: EmeGizmoDrag,
    guids: Vec<u32>,
    start: Vec<EmeObjXform>,
    pivot: V3,
    moved: bool,
}

#[derive(Debug, Clone, Default)]
struct Nearby {
    dirty: bool,
    loaded: bool,
    in_flight: bool,
    sent_ms: u64,
    after: u32,
    pages: u32,
    page_budget: u32,
    acc: Vec<EmeObjectRow>,
    centre: Option<V3>,
    done_ms: u64,
    failed_until: u64,
    truncated: bool,
}

/// The Objects tool's state machine (see the module docs).
#[derive(Debug)]
pub struct EmeObjectsEditor {
    mode: EmeGizmoMode,
    space: EmeGizmoSpace,
    snap: EmeSnap,
    ground_snap: bool,
    radius: f32,
    spawn_scale: f32,
    spawn_yaw_deg: f32,
    scale_range: [f32; 2],

    rows: BTreeMap<u32, EmeObjectRow>,
    world: Vec<EmeWorldObject>,
    templates: BTreeMap<u32, String>,
    recent: Vec<(u32, String)>,

    selection: Vec<u32>,
    hover: Option<u32>,
    hover_handle: Option<EmeGizmoHandle>,
    spawn: Option<(u32, Option<String>)>,

    drag: Option<ActiveDrag>,
    ghosts: BTreeMap<u32, Ghost>,
    placing: Option<V3>,

    net: Nearby,
    want_get: BTreeSet<u32>,
    gets_in_flight: BTreeMap<u32, u64>,
    last_request_ms: u64,
    pending_replies: u32,
    /// Spawns whose rows should become the selection (duplicates).
    select_next_spawns: u32,
    select_reset: bool,

    inbox: Vec<EmeObjectsCmd>,
    notices: Vec<EmeToolNotice>,
    frame_req: Option<EmeFrameRequest>,
    eye: Option<V3>,
    now: u64,
    active: bool,
    can_edit: bool,
    stepped: bool,
    rev: u64,
}

impl Default for EmeObjectsEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl EmeObjectsEditor {
    /// A fresh editor (translate, world axes, no snapping, ground snap on).
    pub fn new() -> Self {
        Self {
            mode: EmeGizmoMode::Translate,
            space: EmeGizmoSpace::World,
            snap: EmeSnap::default(),
            ground_snap: true,
            radius: DEFAULT_RADIUS,
            spawn_scale: 1.0,
            spawn_yaw_deg: 0.0,
            scale_range: [0.1, 20.0],
            rows: BTreeMap::new(),
            world: Vec::new(),
            templates: BTreeMap::new(),
            recent: Vec::new(),
            selection: Vec::new(),
            hover: None,
            hover_handle: None,
            spawn: None,
            drag: None,
            ghosts: BTreeMap::new(),
            placing: None,
            net: Nearby::default(),
            want_get: BTreeSet::new(),
            gets_in_flight: BTreeMap::new(),
            last_request_ms: 0,
            pending_replies: 0,
            select_next_spawns: 0,
            select_reset: false,
            inbox: Vec::new(),
            notices: Vec::new(),
            frame_req: None,
            eye: None,
            now: 0,
            active: false,
            can_edit: false,
            stepped: false,
            rev: 1,
        }
    }

    // ---- options ------------------------------------------------------------------------------

    /// The gizmo mode.
    pub fn mode(&self) -> EmeGizmoMode {
        self.mode
    }
    /// World / local axes.
    pub fn space(&self) -> EmeGizmoSpace {
        self.space
    }
    /// Switch the gizmo mode (an option change; the hotkeys go through [`EmeObjectsCmd`]).
    pub fn set_mode(&mut self, m: EmeGizmoMode) {
        self.mode = m;
        self.bump();
    }
    /// Switch world / local axes.
    pub fn set_space(&mut self, s: EmeGizmoSpace) {
        self.space = s;
        self.bump();
    }
    /// Snap settings.
    pub fn snap(&self) -> EmeSnap {
        self.snap
    }
    /// Snap on / off.
    pub fn set_snap_enabled(&mut self, on: bool) {
        self.snap.enabled = on;
        self.bump();
    }
    /// Grid step, yards.
    pub fn set_grid(&mut self, yards: f32) {
        if yards.is_finite() && yards > 0.0 {
            self.snap.grid = yards;
        }
        self.bump();
    }
    /// Rotation step, degrees.
    pub fn set_angle_step(&mut self, deg: f32) {
        if deg.is_finite() && deg > 0.0 {
            self.snap.angle_deg = deg;
        }
        self.bump();
    }
    /// Ground snap on / off.
    pub fn ground_snap(&self) -> bool {
        self.ground_snap
    }
    /// Ground snap on / off.
    pub fn set_ground_snap(&mut self, on: bool) {
        self.ground_snap = on;
        self.bump();
    }
    /// The list radius, yards.
    pub fn radius(&self) -> f32 {
        self.radius
    }
    /// The list radius, yards (1..200); asks for a fresh list.
    pub fn set_radius(&mut self, r: f32) {
        if r.is_finite() {
            self.radius = r.clamp(5.0, 200.0);
            self.net.dirty = true;
        }
    }
    /// Scale of a newly placed object.
    pub fn spawn_scale(&self) -> f32 {
        self.spawn_scale
    }
    /// Scale of a newly placed object.
    pub fn set_spawn_scale(&mut self, s: f32) {
        if s.is_finite() {
            self.spawn_scale = s.clamp(self.scale_range[0], self.scale_range[1]);
        }
        self.bump();
    }
    /// Orientation of a newly placed object, degrees.
    pub fn spawn_yaw_deg(&self) -> f32 {
        self.spawn_yaw_deg
    }
    /// Orientation of a newly placed object, degrees.
    pub fn set_spawn_yaw_deg(&mut self, d: f32) {
        if d.is_finite() {
            self.spawn_yaw_deg = d.rem_euclid(360.0);
        }
        self.bump();
    }
    /// The server's object scale limits (`caps.limits.obj_scale`).
    pub fn set_scale_range(&mut self, r: [f32; 2]) {
        if r[0].is_finite() && r[1].is_finite() && r[0] > 0.0 && r[0] < r[1] {
            self.scale_range = r;
        }
    }
    /// The scale limits in force.
    pub fn scale_range(&self) -> [f32; 2] {
        self.scale_range
    }

    /// The tool became active.
    pub fn activate(&mut self) {
        self.active = true;
        self.net.dirty = true;
        self.bump();
    }

    /// The tool stopped being active: a drag in progress is dropped, queued commands forgotten.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.drag = None;
        self.ghosts.retain(|_, g| g.sent);
        self.inbox.clear();
        self.spawn = None;
        self.placing = None;
        self.hover = None;
        self.hover_handle = None;
        self.bump();
    }

    fn bump(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }

    // ---- inputs from the host -----------------------------------------------------------------

    /// Queue a panel / hotkey command (ignored while the tool is not active).
    pub fn command(&mut self, cmd: EmeObjectsCmd) {
        if self.active {
            self.inbox.push(cmd);
        }
    }

    /// The streamed objects the viewport can pick right now.
    pub fn set_world(&mut self, world: Vec<EmeWorldObject>) {
        for o in &world {
            if let Some(n) = o.name.as_ref().filter(|n| !n.is_empty()) {
                self.templates.entry(o.entry).or_insert_with(|| n.clone());
            }
        }
        if self.world != world {
            self.world = world;
            self.bump();
        }
    }

    /// Where the camera should go, if a [`EmeObjectsCmd::Frame`] asked (taken once).
    pub fn take_frame_request(&mut self) -> Option<EmeFrameRequest> {
        self.frame_req.take()
    }

    /// A notice for the UI (taken once).
    pub fn take_notice(&mut self) -> Option<EmeToolNotice> {
        if self.notices.is_empty() {
            None
        } else {
            Some(self.notices.remove(0))
        }
    }

    /// The commands the host dispatched for this tool were refused (no project, link down…):
    /// nothing is in flight any more.
    pub fn on_dispatch_refused(&mut self) {
        self.ghosts.retain(|_, g| !g.sent);
        self.drag = None;
        self.pending_replies = 0;
        self.bump();
    }

    /// The link dropped: forget what was in flight.
    pub fn on_link_lost(&mut self) {
        self.net.in_flight = false;
        self.gets_in_flight.clear();
        self.ghosts.clear();
        self.drag = None;
        self.pending_replies = 0;
        self.net.dirty = true;
        self.bump();
    }

    /// Ask for a fresh list (an undo / redo settled, a project reverted).
    pub fn mark_dirty(&mut self) {
        self.net.dirty = true;
    }

    // ---- selection ----------------------------------------------------------------------------

    /// The selection (the last is the primary).
    pub fn selection(&self) -> &[u32] {
        &self.selection
    }

    /// The primary (last selected) object.
    pub fn primary(&self) -> Option<u32> {
        self.selection.last().copied()
    }

    /// A row by guid.
    pub fn row(&self, guid: u32) -> Option<&EmeObjectRow> {
        self.rows.get(&guid)
    }

    /// Every loaded row.
    pub fn rows(&self) -> impl Iterator<Item = &EmeObjectRow> {
        self.rows.values()
    }

    /// Select exactly `guid` (fetching its row if unknown).
    pub fn select_only(&mut self, guid: u32) {
        if self.selection != [guid] {
            self.selection = vec![guid];
            self.bump();
        }
        self.need_row(guid);
    }

    fn toggle(&mut self, guid: u32) {
        if let Some(i) = self.selection.iter().position(|g| *g == guid) {
            self.selection.remove(i);
        } else {
            self.selection.push(guid);
            self.need_row(guid);
        }
        self.bump();
    }

    fn need_row(&mut self, guid: u32) {
        if !self.rows.contains_key(&guid) && !self.gets_in_flight.contains_key(&guid) {
            self.want_get.insert(guid);
        }
    }

    fn deselect(&mut self, guid: u32) {
        self.selection.retain(|g| *g != guid);
        self.bump();
    }

    // ---- replies and pushes -------------------------------------------------------------------

    /// A reply to one of this tool's requests (`op` = the request's op name).
    pub fn on_reply(&mut self, op: &str, reply: &EmeServiceReply) {
        let ok = reply.ok;
        match op {
            "obj_nearby" => self.on_nearby(reply),
            "obj_get" => {
                // The reply does not echo the guid on failure: forget the oldest ask.
                if ok {
                    if let Ok(EmeObjectResult { row: Some(row), .. }) = reply.extra_as() {
                        self.gets_in_flight.remove(&row.guid);
                        self.rows.insert(row.guid, row);
                    }
                } else {
                    let gone = reply.error_code() == Some(EmeServiceError::NotFound);
                    let asked: Vec<u32> = self.gets_in_flight.keys().copied().collect();
                    if let Some(g) = asked.first().copied() {
                        self.gets_in_flight.remove(&g);
                        if gone {
                            self.rows.remove(&g);
                            self.deselect(g);
                        }
                    }
                }
                self.bump();
            }
            "obj_spawn" | "obj_move" | "obj_delete" => {
                self.pending_replies = self.pending_replies.saturating_sub(1);
                let row = reply.extra_as::<EmeObjectResult>().ok().and_then(|r| r.row);
                if ok {
                    if let Some(row) = row {
                        self.on_mutation_ok(op, row);
                    }
                } else {
                    self.on_mutation_failed(op, reply);
                }
                self.bump();
            }
            _ => {}
        }
    }

    fn on_mutation_ok(&mut self, op: &str, row: EmeObjectRow) {
        let guid = row.guid;
        match op {
            "obj_delete" => {
                self.rows.remove(&guid);
                self.ghosts.remove(&guid);
                self.deselect(guid);
            }
            "obj_spawn" => {
                self.remember_template(row.entry, row.name.clone());
                if self.select_next_spawns > 0 {
                    self.select_next_spawns -= 1;
                    if std::mem::take(&mut self.select_reset) {
                        self.selection.clear();
                    }
                    if !self.selection.contains(&guid) {
                        self.selection.push(guid);
                    }
                } else if self.can_edit && self.spawn.is_none() && self.selection.is_empty() {
                    self.selection.push(guid);
                }
                self.rows.insert(guid, row);
            }
            _ => {
                self.ghosts.remove(&guid);
                self.rows.insert(guid, row);
            }
        }
    }

    fn on_mutation_failed(&mut self, op: &str, reply: &EmeServiceReply) {
        // The server did not do it: whatever we showed is wrong, resync.
        self.ghosts.clear();
        self.net.dirty = true;
        for g in self.selection.clone() {
            self.rows.remove(&g);
            self.want_get.insert(g);
        }
        let code = reply.err.clone().unwrap_or_else(|| "internal".to_owned());
        let _ = op;
        let err = EmeServiceError::parse(&code);
        // `no_project` has its own prompt (the host opens the Projects tab).
        if err == EmeServiceError::NoProject {
            return;
        }
        let (key, args) = match err {
            EmeServiceError::Other(_) => ("MONKEY_EME_ERR_OTHER".to_owned(), vec![code]),
            e => (e.locale_key(), Vec::new()),
        };
        self.notices.push(EmeToolNotice {
            key,
            args,
            error: true,
        });
    }

    fn remember_template(&mut self, entry: u32, name: Option<String>) {
        let name = name.filter(|n| !n.is_empty()).unwrap_or_else(|| format!("#{entry}"));
        self.templates.insert(entry, name.clone());
        self.recent.retain(|(e, _)| *e != entry);
        self.recent.insert(0, (entry, name));
        self.recent.truncate(RECENT_MAX);
    }

    fn on_nearby(&mut self, reply: &EmeServiceReply) {
        self.net.in_flight = false;
        if !reply.ok {
            // `unsupported` / `not_ready` / `rank`: do not hammer the server.
            self.net.failed_until = self.now + 10_000;
            self.net.page_budget = 0;
            self.net.acc.clear();
            self.net.loaded = true;
            if reply.error_code().is_some_and(|e| {
                !matches!(e, EmeServiceError::Timeout | EmeServiceError::LinkLost)
            }) {
                let code = reply.err.clone().unwrap_or_default();
                self.notices.push(EmeToolNotice {
                    key: EmeServiceError::parse(&code).locale_key(),
                    args: Vec::new(),
                    error: true,
                });
            }
            self.bump();
            return;
        }
        let rows: Vec<EmeObjectRow> = reply.rows_as().unwrap_or_default();
        let more = reply.more();
        let last = rows.iter().map(|r| r.guid).max();
        self.net.acc.extend(rows);
        self.net.pages += 1;
        match (more, last) {
            (true, Some(last)) if self.net.pages < self.net.page_budget => {
                self.net.after = last;
            }
            _ => {
                self.net.truncated = more;
                // The list replaces the old one; a selected object outside the radius keeps its row.
                let keep: Vec<EmeObjectRow> = self
                    .selection
                    .iter()
                    .filter_map(|g| self.rows.get(g).cloned())
                    .collect();
                self.rows.clear();
                for r in std::mem::take(&mut self.net.acc) {
                    self.rows.insert(r.guid, r);
                }
                for r in keep {
                    self.rows.entry(r.guid).or_insert(r);
                }
                for r in self.rows.values().cloned().collect::<Vec<_>>() {
                    if let Some(n) = r.name.as_ref().filter(|n| !n.is_empty()) {
                        self.templates.entry(r.entry).or_insert_with(|| n.clone());
                    }
                }
                self.net.after = 0;
                self.net.pages = 0;
                self.net.loaded = true;
                self.net.done_ms = self.now;
                for g in self.selection.clone() {
                    self.need_row(g);
                }
            }
        }
        self.bump();
    }

    /// A push from the server (someone else changed an object, or a project revert moved many).
    pub fn on_push(&mut self, push: &EmeServicePush) {
        let EmeServicePush::Obj { action, row } = push else {
            return;
        };
        match (action, row) {
            (EmeObjectAction::Refresh, _) => {
                self.net.dirty = true;
                for g in self.selection.clone() {
                    self.rows.remove(&g);
                    self.want_get.insert(g);
                }
            }
            (EmeObjectAction::Delete, Some(row)) => {
                self.rows.remove(&row.guid);
                self.ghosts.remove(&row.guid);
                self.deselect(row.guid);
            }
            (_, Some(row)) => {
                self.rows.insert(row.guid, row.clone());
                self.ghosts.remove(&row.guid);
            }
            _ => {}
        }
        self.bump();
    }

    // ---- the frame ----------------------------------------------------------------------------

    /// Bumped on every change a snapshot could show (the host copies [`Self::view`] only then).
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// Whether [`Self::step`] ran since the last [`Self::end_frame`].
    pub fn stepped(&self) -> bool {
        self.stepped
    }

    /// Mark the frame over (the tool calls it after draining the sink).
    pub fn end_frame(&mut self) {
        self.stepped = false;
    }

    /// The transform the server holds for `guid` — or the one a pending move will give it.
    fn xform_of(&self, guid: u32) -> Option<EmeObjXform> {
        if let Some(g) = self.ghosts.get(&guid) {
            return Some(g.to);
        }
        self.rows.get(&guid).map(|r| EmeObjXform {
            pos: [r.x, r.y, r.z],
            o: r.o,
            scale: r.scale,
        })
    }

    /// The gizmo's frame for the current selection (needs every selected row).
    fn gizmo_frame(&self) -> Option<EmeGizmoFrame> {
        if self.selection.is_empty() || !self.can_edit {
            return None;
        }
        let xs: Vec<EmeObjXform> = self
            .selection
            .iter()
            .map(|g| self.xform_of(*g))
            .collect::<Option<_>>()?;
        let n = xs.len() as f32;
        let mut c = [0.0; 3];
        for x in &xs {
            c = add(c, x.pos);
        }
        let pivot = mul(c, 1.0 / n);
        let yaw = xs.last().map_or(0.0, |x| x.o);
        let eye = self.eye.unwrap_or([pivot[0] - 20.0, pivot[1], pivot[2] + 10.0]);
        Some(EmeGizmoFrame::new(pivot, yaw, self.space, eye))
    }

    /// One input frame.
    pub fn step(
        &mut self,
        ctx: &EmeInputCtx,
        terrain: Option<&dyn EmeTerrain>,
        out: &mut EmeCommandSink,
    ) {
        self.stepped = true;
        self.now = ctx.now_ms;
        self.can_edit = ctx.perms.has(EmeCapability::Edit);
        if let Some(o) = ctx.ray_origin {
            self.eye = Some(o);
        }
        // Ghosts whose reply never came.
        let now = self.now;
        let before = self.ghosts.len();
        self.ghosts.retain(|_, g| g.expires_ms > now);
        if self.ghosts.len() != before {
            self.bump();
        }
        self.drain_inbox(ctx, out);
        self.pointer(ctx, terrain, out);
        self.pump_net(ctx, out);
    }

    fn ray(ctx: &EmeInputCtx) -> Option<(V3, V3)> {
        match (ctx.ray_origin, ctx.view_dir) {
            (Some(o), Some(d)) => Some((o, d)),
            _ => None,
        }
    }

    fn drain_inbox(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        for cmd in std::mem::take(&mut self.inbox) {
            match cmd {
                EmeObjectsCmd::Select(g) => self.select_only(g),
                EmeObjectsCmd::ToggleSelect(g) => self.toggle(g),
                EmeObjectsCmd::SelectNone => {
                    self.selection.clear();
                    self.bump();
                }
                EmeObjectsCmd::Frame => self.request_frame(),
                EmeObjectsCmd::Refresh => {
                    self.net.dirty = true;
                    self.net.failed_until = 0;
                    for g in self.selection.clone() {
                        self.rows.remove(&g);
                        self.want_get.insert(g);
                    }
                }
                EmeObjectsCmd::LoadMore => {
                    if self.net.truncated {
                        self.net.dirty = true;
                        self.net.page_budget = self.net.page_budget.max(PAGES_PER_LOAD) + PAGES_PER_LOAD;
                        self.net.truncated = false;
                    }
                }
                EmeObjectsCmd::Delete => self.delete_selection(out),
                EmeObjectsCmd::Duplicate => self.duplicate_selection(ctx, out),
                EmeObjectsCmd::SetField { field, value } => self.set_field(field, value, out),
                EmeObjectsCmd::ArmSpawn { entry, name } => {
                    if entry > 0 && self.can_edit {
                        self.spawn = Some((entry, name));
                        self.bump();
                    }
                }
                EmeObjectsCmd::DisarmSpawn => {
                    self.spawn = None;
                    self.placing = None;
                    self.bump();
                }
                EmeObjectsCmd::SetMode(m) => {
                    self.mode = m;
                    self.bump();
                }
                EmeObjectsCmd::SetSpace(s) => {
                    self.space = s;
                    self.bump();
                }
            }
        }
    }

    fn request_frame(&mut self) {
        let mut pts: Vec<(V3, f32)> = Vec::new();
        for g in &self.selection {
            if let Some(w) = self.world.iter().find(|w| w.guid == *g) {
                let r = (w.obb.half[0].powi(2) + w.obb.half[1].powi(2) + w.obb.half[2].powi(2)).sqrt();
                pts.push((w.obb.center, r.max(0.5)));
            } else if let Some(r) = self.rows.get(g) {
                pts.push(([r.x, r.y, r.z], 1.0 * r.scale.max(0.5)));
            }
        }
        if pts.is_empty() {
            return;
        }
        let n = pts.len() as f32;
        let mut c = [0.0; 3];
        for (p, _) in &pts {
            c = add(c, *p);
        }
        let center = mul(c, 1.0 / n);
        let radius = pts
            .iter()
            .map(|(p, r)| crate::gizmo::len(sub(*p, center)) + r)
            .fold(0.5f32, f32::max);
        self.frame_req = Some(EmeFrameRequest { center, radius });
    }

    fn send_move(&mut self, guid: u32, x: &EmeObjXform, out: &mut EmeCommandSink) {
        out.push(EmeCommand::Object(EmeObjectRequest::Move(EmeObjMove {
            kind: EmeObjectKind::Go,
            guid,
            x: x.pos[0],
            y: x.pos[1],
            z: x.pos[2],
            o: wrap_tau(x.o),
            scale: x.scale,
            tx: EmeTxId(0),
        })));
        self.pending_replies += 1;
    }

    fn delete_selection(&mut self, out: &mut EmeCommandSink) {
        if !self.can_edit {
            return;
        }
        for g in self.selection.clone() {
            out.push(EmeCommand::Object(EmeObjectRequest::Delete {
                kind: EmeObjectKind::Go,
                guid: g,
            }));
            self.pending_replies += 1;
        }
    }

    fn duplicate_selection(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        if !self.can_edit {
            return;
        }
        let mut n = 0;
        for g in self.selection.clone() {
            let Some(r) = self.rows.get(&g) else { continue };
            out.push(EmeCommand::Object(EmeObjectRequest::Spawn(EmeObjSpawn {
                kind: EmeObjectKind::Go,
                entry: r.entry,
                map: ctx.map,
                x: r.x + DUPLICATE_OFFSET,
                y: r.y + DUPLICATE_OFFSET,
                z: r.z,
                o: r.o,
                scale: r.scale,
                tx: EmeTxId(0),
            })));
            self.pending_replies += 1;
            n += 1;
        }
        if n > 0 {
            self.select_next_spawns = n;
            self.select_reset = true;
        }
    }

    fn set_field(&mut self, field: EmeObjField, value: f32, out: &mut EmeCommandSink) {
        if !self.can_edit || !value.is_finite() {
            return;
        }
        let Some(g) = self.primary() else { return };
        let Some(mut x) = self.xform_of(g) else {
            self.notices.push(EmeToolNotice {
                key: "MONKEY_EME_OBJECTS_LOADING".into(),
                args: Vec::new(),
                error: false,
            });
            return;
        };
        let lim = |v: f32| v.clamp(-POS_LIMIT, POS_LIMIT);
        match field {
            EmeObjField::X => x.pos[0] = lim(value),
            EmeObjField::Y => x.pos[1] = lim(value),
            EmeObjField::Z => x.pos[2] = value.clamp(-10_000.0, 10_000.0),
            EmeObjField::Orientation => x.o = wrap_tau(value.to_radians()),
            EmeObjField::Scale => x.scale = value.clamp(self.scale_range[0], self.scale_range[1]),
        }
        let from = self.xform_of(g).unwrap_or(x);
        self.send_move(g, &x, out);
        self.ghosts.insert(
            g,
            Ghost {
                from,
                to: x,
                sent: true,
                expires_ms: self.now + GHOST_TTL_MS,
            },
        );
    }

    fn pointer(&mut self, ctx: &EmeInputCtx, terrain: Option<&dyn EmeTerrain>, out: &mut EmeCommandSink) {
        // Esc: cancel the drag, else the armed spawn, else the selection.
        if ctx.cancel {
            if self.drag.take().is_some() {
                self.ghosts.retain(|_, g| g.sent);
            } else if self.spawn.take().is_some() {
                self.placing = None;
            } else if !self.selection.is_empty() {
                self.selection.clear();
            }
            self.bump();
            return;
        }
        let ray = Self::ray(ctx);

        // A drag in progress keeps going wherever the pointer is.
        if self.drag.is_some() {
            if ctx.primary.held {
                if let Some((o, d)) = ray {
                    self.update_drag(o, d, terrain);
                }
            }
            if ctx.primary.released || !ctx.primary.held {
                self.finish_drag(out);
            }
            return;
        }

        if ctx.pointer_over_ui {
            if self.hover.is_some() || self.hover_handle.is_some() || self.placing.is_some() {
                self.hover = None;
                self.hover_handle = None;
                self.placing = None;
                self.bump();
            }
            return;
        }
        let Some((o, d)) = ray else { return };

        // Placement preview.
        let placing = self.spawn.as_ref().and(ctx.hit.map(|h| h.pos));
        if placing != self.placing {
            self.placing = placing;
            self.bump();
        }

        // Hover.
        let frame = self.gizmo_frame();
        let hover_handle = frame.and_then(|f| hit_test(self.mode, &f, o, d));
        let hover = if hover_handle.is_some() || self.spawn.is_some() {
            None
        } else {
            self.pick(o, d)
        };
        if hover != self.hover || hover_handle != self.hover_handle {
            self.hover = hover;
            self.hover_handle = hover_handle;
            self.bump();
        }

        if !ctx.primary.pressed {
            return;
        }
        // 1. Placing a template.
        if let Some((entry, name)) = self.spawn.clone() {
            if let (true, Some(hit)) = (self.can_edit, ctx.hit) {
                out.push(EmeCommand::Object(EmeObjectRequest::Spawn(EmeObjSpawn {
                    kind: EmeObjectKind::Go,
                    entry,
                    map: ctx.map,
                    x: hit.pos[0],
                    y: hit.pos[1],
                    z: hit.pos[2],
                    o: wrap_tau(self.spawn_yaw_deg.to_radians()),
                    scale: self.spawn_scale.clamp(self.scale_range[0], self.scale_range[1]),
                    tx: EmeTxId(0),
                })));
                self.pending_replies += 1;
                self.remember_template(entry, name);
                if !ctx.modifiers.shift {
                    self.spawn = None;
                    self.placing = None;
                }
                self.bump();
            }
            return;
        }
        // 2. A gizmo handle.
        if let (Some(frame), Some(handle)) = (frame, hover_handle) {
            self.begin_drag(frame, handle, o, d);
            return;
        }
        // 3. Select what is under the cursor.
        match self.pick(o, d) {
            Some(g) => {
                if ctx.modifiers.ctrl {
                    self.toggle(g);
                } else if !self.selection.contains(&g) {
                    self.select_only(g);
                } else if self.selection.len() > 1 && self.primary() != Some(g) {
                    // Clicking a member of a group makes it the primary without dropping the group.
                    self.selection.retain(|s| *s != g);
                    self.selection.push(g);
                    self.bump();
                }
            }
            None => {
                if !ctx.modifiers.ctrl && !self.selection.is_empty() {
                    self.selection.clear();
                    self.bump();
                }
            }
        }
    }

    fn begin_drag(&mut self, frame: EmeGizmoFrame, handle: EmeGizmoHandle, o: V3, d: V3) {
        let guids = self.selection.clone();
        let Some(start) = guids
            .iter()
            .map(|g| self.xform_of(*g))
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        let Some(drag) = EmeGizmoDrag::begin(self.mode, handle, frame, o, d) else {
            return;
        };
        self.drag = Some(ActiveDrag {
            drag,
            guids,
            start,
            pivot: frame.origin,
            moved: false,
        });
        self.bump();
    }

    fn update_drag(&mut self, o: V3, d: V3, terrain: Option<&dyn EmeTerrain>) {
        let Some(ad) = self.drag.as_ref() else { return };
        let Some(delta) = ad.drag.solve(o, d, &self.snap) else {
            return;
        };
        let mut new = apply_delta(delta, ad.pivot, &ad.start, self.scale_range, &self.snap);
        // Ground snap: a drag along the ground (not up or down) keeps the objects on it.
        let horizontal = matches!(delta, EmeGizmoDelta::Move(_))
            && matches!(
                ad.drag.handle,
                EmeGizmoHandle::AxisX | EmeGizmoHandle::AxisY | EmeGizmoHandle::PlaneXY
            );
        // Local axes keep Z up (yaw only), so the ground rule holds in both spaces.
        if self.ground_snap && horizontal && !delta.is_noop() {
            if let Some(t) = terrain {
                for x in &mut new {
                    if let Some(z) = t.ground_z(x.pos[0], x.pos[1], x.pos[2], 12.0) {
                        x.pos[2] = z;
                    }
                }
            }
        }
        let moved = !delta.is_noop();
        let guids = ad.guids.clone();
        let start = ad.start.clone();
        let expires = self.now + GHOST_TTL_MS;
        for ((g, from), to) in guids.iter().zip(start).zip(new) {
            self.ghosts.insert(
                *g,
                Ghost {
                    from,
                    to,
                    sent: false,
                    expires_ms: expires,
                },
            );
        }
        if let Some(ad) = self.drag.as_mut() {
            ad.moved = moved;
        }
        self.bump();
    }

    fn finish_drag(&mut self, out: &mut EmeCommandSink) {
        let Some(ad) = self.drag.take() else { return };
        if !ad.moved || !self.can_edit {
            self.ghosts.retain(|_, g| g.sent);
            self.bump();
            return;
        }
        for g in ad.guids {
            let Some(ghost) = self.ghosts.get(&g).cloned() else {
                continue;
            };
            if ghost.sent {
                continue;
            }
            self.send_move(g, &ghost.to, out);
            if let Some(gh) = self.ghosts.get_mut(&g) {
                gh.sent = true;
                gh.expires_ms = self.now + GHOST_TTL_MS;
            }
        }
        self.bump();
    }

    /// Keep the list and the selected rows fresh: at most one request per [`REQUEST_GAP_MS`].
    fn pump_net(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        if !self.active || !ctx.perms.can_open() {
            return;
        }
        let now = self.now;
        // Forget requests nobody answered.
        if self.net.in_flight && now.saturating_sub(self.net.sent_ms) > REQUEST_TTL_MS {
            self.net.in_flight = false;
        }
        self.gets_in_flight.retain(|_, t| now.saturating_sub(*t) <= REQUEST_TTL_MS);
        // The camera moved away from the list's centre.
        if let (Some(eye), Some(c)) = (self.eye, self.net.centre) {
            let moved = crate::gizmo::len([eye[0] - c[0], eye[1] - c[1], 0.0]);
            if moved > RECENTRE_YD && now.saturating_sub(self.net.done_ms) > REFRESH_GAP_MS {
                self.net.dirty = true;
            }
        }
        if now.saturating_sub(self.last_request_ms) < REQUEST_GAP_MS || now < self.net.failed_until {
            return;
        }
        // 1. A row we are missing.
        if self.gets_in_flight.len() < 2 {
            if let Some(g) = self.want_get.iter().next().copied() {
                self.want_get.remove(&g);
                self.gets_in_flight.insert(g, now);
                self.last_request_ms = now;
                out.push(EmeCommand::Service(EmeServiceRequest::ObjGet {
                    kind: EmeObjectKind::Go,
                    guid: g,
                }));
                return;
            }
        }
        // 2. The list.
        if self.net.in_flight {
            return;
        }
        let continuing = self.net.after > 0;
        if !continuing && !(self.net.dirty || !self.net.loaded) {
            return;
        }
        if !continuing {
            if self.net.loaded && now.saturating_sub(self.net.done_ms) < REFRESH_GAP_MS && !self.net.dirty {
                return;
            }
            self.net.dirty = false;
            self.net.acc.clear();
            self.net.pages = 0;
            self.net.centre = self.eye;
            if self.net.page_budget == 0 {
                self.net.page_budget = PAGES_PER_LOAD;
            }
        }
        self.net.in_flight = true;
        self.net.sent_ms = now;
        self.last_request_ms = now;
        out.push(EmeCommand::Service(EmeServiceRequest::ObjNearby {
            radius: self.radius,
            kind: Some(EmeObjectFilter::Go),
            limit: Some(PAGE_ROWS),
            after_guid: continuing.then_some(self.net.after),
        }));
    }

    // ---- snapshots ----------------------------------------------------------------------------

    /// The panel's snapshot.
    pub fn view(&self) -> EmeObjectsView {
        let eye = self.eye.unwrap_or([0.0; 3]);
        let mut rows: Vec<EmeObjectRow> = self.rows.values().cloned().collect();
        rows.sort_by(|a, b| {
            let da = (a.x - eye[0]).powi(2) + (a.y - eye[1]).powi(2);
            let db = (b.x - eye[0]).powi(2) + (b.y - eye[1]).powi(2);
            da.total_cmp(&db).then(a.guid.cmp(&b.guid))
        });
        let mut templates: Vec<(u32, String)> = self.recent.clone();
        for (e, n) in &self.templates {
            if !templates.iter().any(|(te, _)| te == e) {
                templates.push((*e, n.clone()));
            }
        }
        let primary_guid = self.primary();
        EmeObjectsView {
            rev: self.rev,
            active: self.active,
            can_edit: self.can_edit,
            truncated: self.net.truncated,
            loading: self.net.in_flight || self.net.dirty || !self.net.loaded,
            primary: primary_guid.and_then(|g| self.rows.get(&g).cloned()),
            primary_loading: primary_guid.is_some_and(|g| !self.rows.contains_key(&g)),
            rows,
            selected: self.selection.clone(),
            armed: self.spawn.clone(),
            templates,
            pending: self.pending_replies,
            mode: self.mode,
            space: self.space,
        }
    }

    /// The object under the ray: the streamed world first, and (the server streams objects to the
    /// client around the CHARACTER only, so far ones have no model) every listed row without a
    /// streamed twin, as a small box at its position. The nearer hit wins.
    fn pick(&self, o: V3, d: V3) -> Option<u32> {
        let streamed = pick_object(&self.world, o, d, PICK_REACH);
        let unstreamed: Vec<EmeWorldObject> = self
            .rows
            .values()
            .filter(|r| !self.world.iter().any(|w| w.guid == r.guid))
            .map(|r| EmeWorldObject {
                guid: r.guid,
                entry: r.entry,
                name: None,
                pos: [r.x, r.y, r.z],
                obb: Self::row_box(r),
            })
            .collect();
        let far = pick_object(&unstreamed, o, d, PICK_REACH);
        match (streamed, far) {
            (Some(a), Some(b)) => Some(if b.1 + 0.05 < a.1 { b.0 } else { a.0 }),
            (Some(a), None) => Some(a.0),
            (None, Some(b)) => Some(b.0),
            (None, None) => None,
        }
    }

    /// The box standing in for a row whose model the client does not have.
    fn row_box(r: &EmeObjectRow) -> EmeObb {
        EmeObb::point([r.x, r.y, r.z + 0.5 * r.scale], 0.5 * r.scale.max(0.3))
    }

    fn obb_of(&self, guid: u32) -> Option<EmeObb> {
        if let Some(w) = self.world.iter().find(|w| w.guid == guid) {
            return Some(w.obb);
        }
        self.rows
            .get(&guid)
            .map(Self::row_box)
    }

    /// What the viewport draws.
    pub fn draw(&self) -> EmeObjectsDraw {
        let mut d = EmeObjectsDraw::default();
        if !self.active {
            return d;
        }
        for g in &self.selection {
            if let Some(b) = self.obb_of(*g) {
                d.selected.push(b);
            }
        }
        d.hover = self.hover.filter(|h| !self.selection.contains(h)).and_then(|h| self.obb_of(h));
        for (g, gh) in &self.ghosts {
            if let Some(b) = self.obb_of(*g) {
                // The world box follows the VISUAL object, which sits at the server's position,
                // not at the ghost's: move it from there.
                let from = self.rows.get(g).map_or(gh.from, |r| EmeObjXform {
                    pos: [r.x, r.y, r.z],
                    o: r.o,
                    scale: r.scale,
                });
                d.ghosts.push(b.moved(&from, &gh.to));
            }
        }
        d.placing = self.placing;
        if let Some(mut frame) = self.gizmo_frame() {
            // While dragging, the pivot follows the ghosts.
            if let Some(ad) = &self.drag {
                if ad.moved {
                    let n = ad.guids.len() as f32;
                    let mut c = [0.0; 3];
                    for g in &ad.guids {
                        if let Some(gh) = self.ghosts.get(g) {
                            c = add(c, gh.to.pos);
                        }
                    }
                    frame.origin = mul(c, 1.0 / n);
                }
            }
            d.gizmo = Some(EmeGizmoDraw {
                mode: self.mode,
                frame,
                active: self.drag.as_ref().map(|a| a.drag.handle),
                hover: self.hover_handle,
            });
        }
        d
    }

    /// Whether the user is dragging a gizmo handle now.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The ghost transform of `guid` (a drag in progress or a move awaiting its reply).
    pub fn ghost_of(&self, guid: u32) -> Option<EmeObjXform> {
        self.ghosts.get(&guid).map(|g| g.to)
    }

    /// Normalised direction helper for hosts that build the ray themselves.
    pub fn unit(v: V3) -> V3 {
        normalize(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perm::{EmePermissions, EmeRank};
    use crate::service::EmeOwner;
    use crate::tool::{EmeButton, EmeHit};

    fn row(guid: u32, entry: u32, x: f32, y: f32, z: f32) -> EmeObjectRow {
        EmeObjectRow {
            guid,
            kind: EmeObjectKind::Go,
            entry,
            name: Some(format!("Thing{entry}")),
            map: 0,
            x,
            y,
            z,
            o: 0.0,
            scale: 1.0,
            owner: Some(EmeOwner {
                guid: 1,
                name: "Geoff".into(),
            }),
            tx: None,
        }
    }

    fn world_obj(guid: u32, x: f32, y: f32, z: f32) -> EmeWorldObject {
        EmeWorldObject {
            guid,
            entry: 5,
            name: None,
            pos: [x, y, z],
            obb: EmeObb::axis_aligned([x, y, z + 1.0], [1.0, 1.0, 1.0]),
        }
    }

    const EYE: V3 = [-30.0, 0.0, 20.0];

    fn ctx(now: u64, rank: u8, target: Option<V3>, primary: EmeButton) -> EmeInputCtx {
        let mut c = EmeInputCtx::idle(now, 0, EmePermissions::from_rank(EmeRank(rank)));
        c.primary = primary;
        if let Some(t) = target {
            c.ray_origin = Some(EYE);
            c.view_dir = Some(normalize(sub(t, EYE)));
            c.hit = Some(EmeHit {
                pos: [t[0], t[1], 0.0],
                normal: [0.0, 0.0, 1.0],
                object: None,
            });
        }
        c
    }

    fn press() -> EmeButton {
        EmeButton {
            pressed: true,
            held: true,
            released: false,
        }
    }
    fn hold() -> EmeButton {
        EmeButton {
            pressed: false,
            held: true,
            released: false,
        }
    }
    fn release() -> EmeButton {
        EmeButton {
            pressed: false,
            held: false,
            released: true,
        }
    }
    fn idle_btn() -> EmeButton {
        EmeButton::default()
    }

    fn editor() -> EmeObjectsEditor {
        let mut e = EmeObjectsEditor::new();
        e.activate();
        e.net.loaded = true; // no list fetch in the click tests
        e.net.dirty = false;
        e.can_edit = true;
        e.net.done_ms = 0;
        e
    }

    fn step(e: &mut EmeObjectsEditor, c: &EmeInputCtx) -> Vec<EmeCommand> {
        let mut sink = EmeCommandSink::new();
        e.step(c, None, &mut sink);
        e.end_frame();
        sink.drain().into_iter().map(|i| i.command).collect()
    }

    #[test]
    fn click_selects_the_object_under_the_cursor_and_ctrl_toggles() {
        let mut e = editor();
        e.set_world(vec![world_obj(1, 0.0, 0.0, 0.0), world_obj(2, 0.0, 10.0, 0.0)]);
        // Hover first, then press: the object under the cursor is selected, a row requested.
        step(&mut e, &ctx(1000, 3, Some([0.0, 0.0, 1.0]), idle_btn()));
        assert_eq!(e.hover, Some(1));
        let cmds = step(&mut e, &ctx(1100, 3, Some([0.0, 0.0, 1.0]), press()));
        assert_eq!(e.selection(), &[1]);
        assert!(cmds.iter().any(|c| matches!(
            c,
            EmeCommand::Service(EmeServiceRequest::ObjGet { guid: 1, .. })
        )));
        // Ctrl+click adds, Ctrl+click again removes.
        let mut c = ctx(1300, 3, Some([0.0, 10.0, 1.0]), press());
        c.modifiers.ctrl = true;
        step(&mut e, &c);
        assert_eq!(e.selection(), &[1, 2]);
        step(&mut e, &ctx(1400, 3, Some([0.0, 10.0, 1.0]), idle_btn()));
        step(&mut e, &c);
        assert_eq!(e.selection(), &[1]);
        // A click on empty ground clears; Ctrl+click on ground keeps.
        let mut gc = ctx(1600, 3, Some([0.0, 40.0, 1.0]), press());
        gc.modifiers.ctrl = true;
        step(&mut e, &gc);
        assert_eq!(e.selection(), &[1]);
        step(&mut e, &ctx(1700, 3, Some([0.0, 40.0, 1.0]), press()));
        assert!(e.selection().is_empty());
    }

    #[test]
    fn an_object_the_client_has_no_model_for_is_still_clickable_through_its_row() {
        let mut e = editor();
        // Streamed: object 1. Listed but not streamed (far from the character): row 2.
        e.set_world(vec![world_obj(1, 0.0, 0.0, 0.0)]);
        e.rows.insert(2, row(2, 9, 0.0, 10.0, 0.0));
        step(&mut e, &ctx(1000, 3, Some([0.0, 10.0, 0.5]), idle_btn()));
        assert_eq!(e.hover, Some(2));
        step(&mut e, &ctx(1100, 3, Some([0.0, 10.0, 0.5]), press()));
        assert_eq!(e.selection(), &[2]);
        // A streamed object nearer to the eye still wins over a row behind it.
        e.selection.clear();
        e.rows.insert(3, row(3, 9, 0.0, 0.0, 0.0));
        e.rows.remove(&3);
        step(&mut e, &ctx(1200, 3, Some([0.0, 0.0, 1.0]), press()));
        assert_eq!(e.selection(), &[1]);
    }

    #[test]
    fn a_rank_two_viewer_selects_but_gets_no_gizmo_and_no_mutation() {
        let mut e = editor();
        e.set_world(vec![world_obj(1, 0.0, 0.0, 0.0)]);
        e.rows.insert(1, row(1, 9, 0.0, 0.0, 0.0));
        let cmds = step(&mut e, &ctx(1000, 2, Some([0.0, 0.0, 1.0]), press()));
        assert_eq!(e.selection(), &[1]);
        assert!(cmds.iter().all(|c| !matches!(c, EmeCommand::Object(_))));
        assert!(e.draw().gizmo.is_none());
        e.command(EmeObjectsCmd::Delete);
        let cmds = step(&mut e, &ctx(1200, 2, None, idle_btn()));
        assert!(cmds.iter().all(|c| !matches!(c, EmeCommand::Object(_))));
    }

    #[test]
    fn esc_cancels_in_stages() {
        let mut e = editor();
        e.set_world(vec![world_obj(1, 0.0, 0.0, 0.0)]);
        e.select_only(1);
        e.rows.insert(1, row(1, 9, 0.0, 0.0, 0.0));
        e.command(EmeObjectsCmd::ArmSpawn {
            entry: 77,
            name: None,
        });
        step(&mut e, &ctx(1000, 3, None, idle_btn()));
        assert!(e.spawn.is_some());
        let mut c = ctx(1100, 3, None, idle_btn());
        c.cancel = true;
        step(&mut e, &c);
        assert!(e.spawn.is_none() && !e.selection().is_empty());
        step(&mut e, &c);
        assert!(e.selection().is_empty());
    }

    /// Selects object 1 (row at the origin) and returns the editor ready to drag.
    fn selected_editor() -> EmeObjectsEditor {
        let mut e = editor();
        e.set_world(vec![world_obj(1, 0.0, 0.0, 0.0)]);
        e.rows.insert(1, row(1, 9, 0.0, 0.0, 0.0));
        e.selection = vec![1];
        e.eye = Some(EYE);
        e
    }

    #[test]
    fn dragging_the_arrow_shows_a_ghost_and_release_sends_one_move() {
        let mut e = selected_editor();
        let f = e.gizmo_frame().expect("gizmo");
        // Grab the Y arrow (west).
        let grab = [0.0, f.size * 0.6, 0.0];
        let c = ctx(2000, 3, Some(grab), idle_btn());
        step(&mut e, &c);
        assert_eq!(e.hover_handle, Some(EmeGizmoHandle::AxisY));
        let cmds = step(&mut e, &ctx(2100, 3, Some(grab), press()));
        assert!(cmds.is_empty());
        assert!(e.dragging());
        // Move the cursor 4 yd further along Y: ground snap needs terrain, none here.
        let to = [0.0, f.size * 0.6 + 4.0, 0.0];
        let cmds = step(&mut e, &ctx(2200, 3, Some(to), hold()));
        assert!(cmds.is_empty(), "no uplink while dragging");
        let g = e.ghost_of(1).expect("ghost");
        assert!((g.pos[1] - 4.0).abs() < 0.02 && g.pos[0].abs() < 0.02, "{:?}", g.pos);
        assert_eq!(e.draw().ghosts.len(), 1);
        // Release: exactly one obj_move with the full transform.
        let cmds = step(&mut e, &ctx(2300, 3, Some(to), release()));
        let moves: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                EmeCommand::Object(EmeObjectRequest::Move(m)) => Some(m.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].guid, 1);
        assert!((moves[0].y - 4.0).abs() < 0.02 && moves[0].scale == 1.0 && moves[0].o == 0.0);
        assert!(!e.dragging());
        // The ghost stays until the authoritative reply lands.
        assert!(e.ghost_of(1).is_some());
        let mut r = row(1, 9, 0.0, 4.0, 0.0);
        r.tx = Some(EmeTxId(5));
        let reply = EmeServiceReply::ok(crate::service::EmeRequestId(3)).with("row", serde_json::to_value(&r).unwrap());
        e.on_reply("obj_move", &reply);
        assert!(e.ghost_of(1).is_none());
        assert_eq!(e.row(1).unwrap().y, 4.0);
    }

    #[test]
    fn esc_during_a_drag_sends_nothing() {
        let mut e = selected_editor();
        let f = e.gizmo_frame().unwrap();
        let grab = [0.0, f.size * 0.6, 0.0];
        step(&mut e, &ctx(2000, 3, Some(grab), idle_btn()));
        step(&mut e, &ctx(2100, 3, Some(grab), press()));
        step(&mut e, &ctx(2200, 3, Some([0.0, f.size * 0.6 + 3.0, 0.0]), hold()));
        assert!(e.ghost_of(1).is_some());
        let mut c = ctx(2300, 3, Some(grab), hold());
        c.cancel = true;
        let cmds = step(&mut e, &c);
        assert!(cmds.is_empty() && e.ghost_of(1).is_none() && !e.dragging());
        // The button still being held afterwards does not restart a drag.
        let cmds = step(&mut e, &ctx(2400, 3, Some(grab), release()));
        assert!(cmds.is_empty());
    }

    #[test]
    fn rotate_drag_sends_the_new_orientation_and_snaps() {
        let mut e = selected_editor();
        e.command(EmeObjectsCmd::SetMode(EmeGizmoMode::Rotate));
        e.set_snap_enabled(true);
        e.set_angle_step(15.0);
        step(&mut e, &ctx(1000, 3, None, idle_btn())); // applies the queued mode
        let eye = [0.0, -0.001, 40.0];
        e.eye = Some(eye);
        let f = e.gizmo_frame().unwrap();
        let ring = |a: f32| [f.size * a.cos(), f.size * a.sin(), 0.0];
        let mk = |t: V3, b: EmeButton, now| {
            let mut c = ctx(now, 3, None, b);
            c.ray_origin = Some(eye);
            c.view_dir = Some(normalize(sub(t, eye)));
            c.hit = None;
            c
        };
        step(&mut e, &mk(ring(0.0), idle_btn(), 1100));
        assert_eq!(e.hover_handle, Some(EmeGizmoHandle::Ring));
        step(&mut e, &mk(ring(0.0), press(), 1200));
        step(&mut e, &mk(ring(50.0f32.to_radians()), hold(), 1300));
        let cmds = step(&mut e, &mk(ring(50.0f32.to_radians()), release(), 1400));
        let m = cmds
            .iter()
            .find_map(|c| match c {
                EmeCommand::Object(EmeObjectRequest::Move(m)) => Some(m.clone()),
                _ => None,
            })
            .expect("move");
        assert!((m.o - 45.0f32.to_radians()).abs() < 0.01, "o={}", m.o);
        assert_eq!((m.x, m.y, m.z), (0.0, 0.0, 0.0));
    }

    #[test]
    fn scale_drag_clamps_to_the_server_limits() {
        let mut e = selected_editor();
        e.command(EmeObjectsCmd::SetMode(EmeGizmoMode::Scale));
        e.set_scale_range([0.1, 3.0]);
        step(&mut e, &ctx(1000, 3, None, idle_btn()));
        let eye = [0.0, 0.0, 40.0];
        e.eye = Some(eye);
        let f = e.gizmo_frame().unwrap();
        let mk = |t: V3, b: EmeButton, now| {
            let mut c = ctx(now, 3, None, b);
            c.ray_origin = Some(eye);
            c.view_dir = Some(normalize(sub(t, eye)));
            c
        };
        let grab = [f.size, 0.0, 0.0];
        step(&mut e, &mk(grab, idle_btn(), 1100));
        step(&mut e, &mk(grab, press(), 1200));
        step(&mut e, &mk([f.size * 8.0, 0.0, 0.0], hold(), 1300));
        let cmds = step(&mut e, &mk([f.size * 8.0, 0.0, 0.0], release(), 1400));
        let m = cmds
            .iter()
            .find_map(|c| match c {
                EmeCommand::Object(EmeObjectRequest::Move(m)) => Some(m.clone()),
                _ => None,
            })
            .expect("move");
        assert_eq!(m.scale, 3.0);
    }

    struct Flat(f32);
    impl EmeTerrain for Flat {
        fn ground_z(&self, _x: f32, _y: f32, _near: f32, _reach: f32) -> Option<f32> {
            Some(self.0)
        }
    }

    #[test]
    fn ground_snap_keeps_a_horizontal_drag_on_the_terrain_but_not_a_vertical_one() {
        let mut e = selected_editor();
        e.rows.insert(1, row(1, 9, 0.0, 0.0, 3.0));
        let f = e.gizmo_frame().unwrap();
        let ground = Flat(1.5);
        let at = |e: &mut EmeObjectsEditor, t: V3, b: EmeButton, now| {
            let c = ctx(now, 3, Some(t), b);
            let mut sink = EmeCommandSink::new();
            e.step(&c, Some(&ground), &mut sink);
            e.end_frame();
        };
        let origin = e.gizmo_frame().unwrap().origin;
        let grab = add(origin, [0.0, f.size * 0.6, 0.0]);
        at(&mut e, grab, idle_btn(), 1000);
        at(&mut e, grab, press(), 1100);
        at(&mut e, add(grab, [0.0, 2.0, 0.0]), hold(), 1200);
        let g = e.ghost_of(1).unwrap();
        assert!((g.pos[2] - 1.5).abs() < 1e-4, "snapped to ground, z={}", g.pos[2]);
        e.drag = None;
        e.ghosts.clear();
        // The vertical arrow is left alone.
        let grab = add(origin, [0.0, 0.0, f.size * 0.6]);
        at(&mut e, grab, idle_btn(), 1300);
        assert_eq!(e.hover_handle, Some(EmeGizmoHandle::AxisZ));
        at(&mut e, grab, press(), 1400);
        at(&mut e, add(grab, [0.0, 0.0, 2.0]), hold(), 1500);
        let g = e.ghost_of(1).unwrap();
        assert!((g.pos[2] - 5.0).abs() < 0.05, "z={}", g.pos[2]);
    }

    #[test]
    fn inspector_fields_send_a_full_move() {
        let mut e = selected_editor();
        e.command(EmeObjectsCmd::SetField {
            field: EmeObjField::Orientation,
            value: -90.0,
        });
        let cmds = step(&mut e, &ctx(1000, 3, None, idle_btn()));
        let m = cmds
            .iter()
            .find_map(|c| match c {
                EmeCommand::Object(EmeObjectRequest::Move(m)) => Some(m.clone()),
                _ => None,
            })
            .expect("move");
        assert!((m.o - 270.0f32.to_radians()).abs() < 1e-4);
        assert_eq!((m.x, m.y, m.z, m.scale), (0.0, 0.0, 0.0, 1.0));
        // A scale beyond the limits is clamped; NaN is ignored.
        e.rows.insert(1, row(1, 9, 0.0, 0.0, 0.0));
        e.ghosts.clear();
        e.command(EmeObjectsCmd::SetField {
            field: EmeObjField::Scale,
            value: 500.0,
        });
        e.command(EmeObjectsCmd::SetField {
            field: EmeObjField::X,
            value: f32::NAN,
        });
        let cmds = step(&mut e, &ctx(1200, 3, None, idle_btn()));
        let moves: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                EmeCommand::Object(EmeObjectRequest::Move(m)) => Some(m.scale),
                _ => None,
            })
            .collect();
        assert_eq!(moves, vec![20.0]);
    }

    #[test]
    fn placing_a_template_spawns_at_the_aim_and_shift_keeps_placing() {
        let mut e = editor();
        e.command(EmeObjectsCmd::ArmSpawn {
            entry: 2910,
            name: Some("Chair".into()),
        });
        e.set_spawn_scale(2.0);
        e.set_spawn_yaw_deg(90.0);
        step(&mut e, &ctx(1000, 3, Some([5.0, 6.0, 0.0]), idle_btn()));
        assert_eq!(e.draw().placing, Some([5.0, 6.0, 0.0]));
        let cmds = step(&mut e, &ctx(1100, 3, Some([5.0, 6.0, 0.0]), press()));
        let s = cmds
            .iter()
            .find_map(|c| match c {
                EmeCommand::Object(EmeObjectRequest::Spawn(s)) => Some(s.clone()),
                _ => None,
            })
            .expect("spawn");
        assert_eq!((s.entry, s.x, s.y, s.scale), (2910, 5.0, 6.0, 2.0));
        assert!((s.o - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
        assert!(e.spawn.is_none(), "one placement disarms");
        // Shift keeps it armed.
        e.command(EmeObjectsCmd::ArmSpawn {
            entry: 2910,
            name: None,
        });
        step(&mut e, &ctx(1200, 3, Some([5.0, 6.0, 0.0]), idle_btn()));
        let mut c = ctx(1300, 3, Some([7.0, 6.0, 0.0]), press());
        c.modifiers.shift = true;
        step(&mut e, &c);
        assert!(e.spawn.is_some());
        // A rank-2 viewer cannot arm it at all.
        let mut v = editor();
        v.command(EmeObjectsCmd::ArmSpawn {
            entry: 1,
            name: None,
        });
        step(&mut v, &ctx(1000, 2, None, idle_btn()));
        assert!(v.spawn.is_none());
    }

    #[test]
    fn a_spawn_reply_is_remembered_and_selected_when_nothing_was() {
        let mut e = editor();
        e.can_edit = true;
        let r = row(50, 2910, 1.0, 2.0, 3.0);
        let reply = EmeServiceReply::ok(crate::service::EmeRequestId(1)).with("row", serde_json::to_value(&r).unwrap());
        e.on_reply("obj_spawn", &reply);
        assert_eq!(e.selection(), &[50]);
        assert_eq!(e.view().templates[0].0, 2910);
        assert!(e.view().rows.iter().any(|r| r.guid == 50));
    }

    #[test]
    fn duplicate_copies_next_to_the_original_and_selects_the_copies() {
        let mut e = selected_editor();
        e.command(EmeObjectsCmd::Duplicate);
        let cmds = step(&mut e, &ctx(1000, 3, None, idle_btn()));
        let s = cmds
            .iter()
            .find_map(|c| match c {
                EmeCommand::Object(EmeObjectRequest::Spawn(s)) => Some(s.clone()),
                _ => None,
            })
            .expect("spawn");
        assert_eq!((s.entry, s.x, s.y), (9, DUPLICATE_OFFSET, DUPLICATE_OFFSET));
        let copy = row(60, 9, 1.0, 1.0, 0.0);
        let reply = EmeServiceReply::ok(crate::service::EmeRequestId(1)).with("row", serde_json::to_value(&copy).unwrap());
        e.on_reply("obj_spawn", &reply);
        assert_eq!(e.selection(), &[60], "the copy replaces the selection");
    }

    #[test]
    fn delete_removes_rows_on_the_reply_and_failures_resync() {
        let mut e = selected_editor();
        e.command(EmeObjectsCmd::Delete);
        let cmds = step(&mut e, &ctx(1000, 3, None, idle_btn()));
        assert!(cmds.iter().any(|c| matches!(
            c,
            EmeCommand::Object(EmeObjectRequest::Delete { guid: 1, .. })
        )));
        assert_eq!(e.view().pending, 1);
        let pre = row(1, 9, 0.0, 0.0, 0.0);
        let ok = EmeServiceReply::ok(crate::service::EmeRequestId(1)).with("row", serde_json::to_value(&pre).unwrap());
        e.on_reply("obj_delete", &ok);
        assert!(e.row(1).is_none() && e.selection().is_empty() && e.view().pending == 0);
        // A refusal raises a notice and schedules a refresh.
        e.rows.insert(2, row(2, 9, 0.0, 0.0, 0.0));
        e.selection = vec![2];
        let bad = EmeServiceReply::error(crate::service::EmeRequestId(2), "rank");
        e.on_reply("obj_delete", &bad);
        assert!(e.net.dirty && e.row(2).is_none());
        let n = e.take_notice().expect("notice");
        assert_eq!(n.key, "MONKEY_EME_ERR_RANK");
        assert!(n.error);
    }

    fn page(rows: &[EmeObjectRow], more: bool) -> EmeServiceReply {
        let mut r = EmeServiceReply::ok(crate::service::EmeRequestId(1)).with("more", more);
        r.rows = Some(rows.iter().map(|r| serde_json::to_value(r).unwrap()).collect());
        r
    }

    #[test]
    fn the_list_pages_by_after_guid_and_replaces_rows_when_done() {
        let mut e = EmeObjectsEditor::new();
        e.activate();
        e.eye = Some([0.0; 3]);
        e.rows.insert(99, row(99, 1, 0.0, 0.0, 0.0)); // gone from the server
        let first = step(&mut e, &ctx(1000, 3, None, idle_btn()));
        match first.as_slice() {
            [EmeCommand::Service(EmeServiceRequest::ObjNearby {
                radius,
                kind,
                limit,
                after_guid,
            })] => {
                assert_eq!(*radius, DEFAULT_RADIUS);
                assert_eq!(*kind, Some(EmeObjectFilter::Go));
                assert_eq!(*limit, Some(16));
                assert_eq!(*after_guid, None);
            }
            other => panic!("{other:?}"),
        }
        // No second request while one is in flight.
        assert!(step(&mut e, &ctx(1500, 3, None, idle_btn())).is_empty());
        let p1: Vec<_> = (1..=16).map(|g| row(g, 1, g as f32, 0.0, 0.0)).collect();
        e.on_reply("obj_nearby", &page(&p1, true));
        let next = step(&mut e, &ctx(1800, 3, None, idle_btn()));
        assert!(matches!(
            next.as_slice(),
            [EmeCommand::Service(EmeServiceRequest::ObjNearby {
                after_guid: Some(16),
                ..
            })]
        ));
        let p2 = vec![row(17, 1, 17.0, 0.0, 0.0)];
        e.on_reply("obj_nearby", &page(&p2, false));
        assert_eq!(e.rows().count(), 17, "the stale row 99 is gone");
        assert!(e.row(99).is_none() && !e.view().truncated);
        // Nothing more to ask until something changes.
        assert!(step(&mut e, &ctx(5000, 3, None, idle_btn())).is_empty());
        // A push asks again (after the gap).
        e.on_push(&EmeServicePush::Obj {
            action: EmeObjectAction::Refresh,
            row: None,
        });
        let again = step(&mut e, &ctx(9000, 3, None, idle_btn()));
        assert!(matches!(
            again.as_slice(),
            [EmeCommand::Service(EmeServiceRequest::ObjNearby { after_guid: None, .. })]
        ));
    }

    #[test]
    fn a_long_list_stops_at_the_page_budget_and_load_more_continues() {
        let mut e = EmeObjectsEditor::new();
        e.activate();
        let mut now = 1000;
        for p in 0..PAGES_PER_LOAD {
            let cmds = step(&mut e, &ctx(now, 3, None, idle_btn()));
            assert_eq!(cmds.len(), 1, "page {p}");
            let rows: Vec<_> = (0..16).map(|i| row(p * 16 + i + 1, 1, 0.0, 0.0, 0.0)).collect();
            e.on_reply("obj_nearby", &page(&rows, true));
            now += 300;
        }
        assert!(e.view().truncated);
        assert_eq!(e.rows().count() as u32, PAGES_PER_LOAD * 16);
        assert!(step(&mut e, &ctx(now + 3000, 3, None, idle_btn())).is_empty());
        e.command(EmeObjectsCmd::LoadMore);
        let cmds = step(&mut e, &ctx(now + 6000, 3, None, idle_btn()));
        assert!(matches!(
            cmds.as_slice(),
            [EmeCommand::Service(EmeServiceRequest::ObjNearby { .. })]
        ));
    }

    #[test]
    fn a_refused_list_backs_off_instead_of_hammering() {
        let mut e = EmeObjectsEditor::new();
        e.activate();
        assert_eq!(step(&mut e, &ctx(1000, 3, None, idle_btn())).len(), 1);
        e.on_reply(
            "obj_nearby",
            &EmeServiceReply::error(crate::service::EmeRequestId(1), "unsupported"),
        );
        assert_eq!(e.take_notice().unwrap().key, "MONKEY_EME_ERR_UNSUPPORTED");
        assert!(step(&mut e, &ctx(2000, 3, None, idle_btn())).is_empty());
        assert!(step(&mut e, &ctx(9000, 3, None, idle_btn())).is_empty());
        e.command(EmeObjectsCmd::Refresh);
        assert_eq!(step(&mut e, &ctx(9100, 3, None, idle_btn())).len(), 1);
    }

    #[test]
    fn pushes_from_other_gms_update_rows_and_selection() {
        let mut e = editor();
        e.rows.insert(1, row(1, 9, 0.0, 0.0, 0.0));
        e.selection = vec![1];
        e.on_push(&EmeServicePush::Obj {
            action: EmeObjectAction::Move,
            row: Some(row(1, 9, 7.0, 0.0, 0.0)),
        });
        assert_eq!(e.row(1).unwrap().x, 7.0);
        e.on_push(&EmeServicePush::Obj {
            action: EmeObjectAction::Spawn,
            row: Some(row(2, 9, 1.0, 0.0, 0.0)),
        });
        assert!(e.row(2).is_some() && e.selection() == [1]);
        e.on_push(&EmeServicePush::Obj {
            action: EmeObjectAction::Delete,
            row: Some(row(1, 9, 7.0, 0.0, 0.0)),
        });
        assert!(e.row(1).is_none() && e.selection().is_empty());
    }

    #[test]
    fn frame_asks_the_host_to_point_the_camera_at_the_selection() {
        let mut e = selected_editor();
        e.command(EmeObjectsCmd::Frame);
        step(&mut e, &ctx(1000, 3, None, idle_btn()));
        let f = e.take_frame_request().expect("frame");
        assert!((f.center[2] - 1.0).abs() < 1e-4 && f.radius > 1.0);
        assert!(e.take_frame_request().is_none());
    }

    #[test]
    fn deactivating_drops_a_drag_and_forgets_commands() {
        let mut e = selected_editor();
        let f = e.gizmo_frame().unwrap();
        let grab = [0.0, f.size * 0.6, 0.0];
        step(&mut e, &ctx(1000, 3, Some(grab), idle_btn()));
        step(&mut e, &ctx(1100, 3, Some(grab), press()));
        assert!(e.dragging());
        e.deactivate();
        assert!(!e.dragging() && e.draw() == EmeObjectsDraw::default());
        e.command(EmeObjectsCmd::Delete);
        e.activate();
        assert!(step(&mut e, &ctx(1200, 3, None, idle_btn()))
            .iter()
            .all(|c| !matches!(c, EmeCommand::Object(_))));
    }

    #[test]
    fn a_dispatch_refusal_clears_pending_ghosts() {
        let mut e = selected_editor();
        e.command(EmeObjectsCmd::SetField {
            field: EmeObjField::X,
            value: 3.0,
        });
        step(&mut e, &ctx(1000, 3, None, idle_btn()));
        assert!(e.ghost_of(1).is_some() && e.view().pending == 1);
        e.on_dispatch_refused();
        assert!(e.ghost_of(1).is_none() && e.view().pending == 0);
    }

    #[test]
    fn the_viewport_draws_boxes_the_gizmo_and_the_hover() {
        let mut e = selected_editor();
        e.set_world(vec![world_obj(1, 0.0, 0.0, 0.0), world_obj(2, 0.0, 10.0, 0.0)]);
        step(&mut e, &ctx(1000, 3, Some([0.0, 10.0, 1.0]), idle_btn()));
        let d = e.draw();
        assert_eq!(d.selected.len(), 1);
        assert!(d.hover.is_some());
        let g = d.gizmo.expect("gizmo");
        assert_eq!(g.mode, EmeGizmoMode::Translate);
        assert!(g.active.is_none());
    }
}

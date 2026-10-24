//! The `.mveme` service wire: JSON requests, replies and server pushes, exactly as CONTRACT v3
//! §12–§13 (`disposable/voxel-v2/CONTRACT.md`) specifies them.
//!
//! Every frame is one minified JSON object. Uplink: `{"v":1,"id":7,"op":"undo","stroke":12}`
//! (`id` 1..=32767). Downlink is either a **reply** `{"v":1,"id":7,"ok":true,…}` carrying the
//! request's `id`, or a **push** `{"v":1,"id":0,"ev":"caps_changed",…}`. Base64url framing and
//! `fid/part/total` chunking belong to the transport.
//!
//! Op-specific result fields sit at the top level of the reply (`rank`, `row`, `more`, `count`, …)
//! and are read with [`EmeServiceReply::extra_as`]; list results are `rows`
//! ([`EmeServiceReply::rows_as`]). Unknown fields are ignored in both directions, as the contract
//! requires.

use crate::lenient;
use crate::perm::{EmeCapability, EmeRank};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// The service schema version, the `v` of every frame.
pub const EME_SERVICE_VERSION: u8 = 1;
/// The largest reassembled JSON frame either side may send, bytes (`caps.limits.frame_max`).
pub const EME_MAX_FRAME_BYTES: usize = 2800;
/// The largest request id; ids run 1..=32767 and wrap.
pub const EME_MAX_REQUEST_ID: u16 = 32767;
/// Most cells one `ov_edit` may carry (`caps.limits.ov_cells_max`).
pub const EME_OV_CELLS_MAX: usize = 32;

/// A request id: 1..=32767 for requests (echoed by the reply); 0 on pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmeRequestId(pub u16);

impl EmeRequestId {
    /// The id every push carries.
    pub const PUSH: EmeRequestId = EmeRequestId(0);
}

/// A client transaction id (u32, unique per character); makes `obj_*` mutations idempotent and
/// names them for `obj_undo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmeTxId(pub u32);

// ---------------------------------------------------------------------------------------------
// Small enums
// ---------------------------------------------------------------------------------------------

/// An edit layer. Binary layer ids: voxel 0, liquid 1, area 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeLayer {
    /// Voxel terrain.
    Voxel,
    /// Liquid overlay.
    Liquid,
    /// Area overlay.
    Area,
}

impl EmeLayer {
    /// The binary layer id (`voxel_stroke.layer`, `OSYNC.layer`).
    pub const fn wire_id(self) -> u8 {
        match self {
            EmeLayer::Voxel => 0,
            EmeLayer::Liquid => 1,
            EmeLayer::Area => 2,
        }
    }
}

/// An overlay layer (the layers `ov_*` ops take).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeOverlayLayer {
    /// Liquid cells (25/6-yd ADT liquid grid).
    Liquid,
    /// Area-id cells (16-yd cells in the MVP).
    Area,
}

impl From<EmeOverlayLayer> for EmeLayer {
    fn from(l: EmeOverlayLayer) -> Self {
        match l {
            EmeOverlayLayer::Liquid => EmeLayer::Liquid,
            EmeOverlayLayer::Area => EmeLayer::Area,
        }
    }
}

/// The `log` layer filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeLayerFilter {
    /// Every layer.
    All,
    /// Voxel only.
    Voxel,
    /// Liquid only.
    Liquid,
    /// Area only.
    Area,
}

/// The `strokes` scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeStrokeScope {
    /// The caller's strokes.
    Mine,
    /// Everyone's strokes near the caller.
    Nearby,
}

/// Game object or creature. Guid namespaces overlap, so every op naming a guid names the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeObjectKind {
    /// Game object.
    Go,
    /// Creature.
    Npc,
}

/// The `obj_nearby` kind filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeObjectFilter {
    /// Both kinds.
    All,
    /// Game objects.
    Go,
    /// Creatures.
    Npc,
}

/// An object mutation kind (`obj` push `action`, history entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeObjectAction {
    /// `obj_spawn`.
    Spawn,
    /// `obj_move`.
    Move,
    /// `obj_delete`.
    Delete,
    /// MONKEY (EME objects, CONTRACT v3.7): `obj` push only: a project revert / un-revert moved
    /// many objects; the receiver re-queries (the push carries no row).
    Refresh,
}

/// A liquid kind (wire `u8`). MVP servers accept `None` and `Water` (`caps.limits.liquid_kinds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum EmeLiquidKind {
    /// 0: erase the baseline liquid in the cell.
    None,
    /// 1: water.
    Water,
    /// 2: ocean.
    Ocean,
    /// 3: magma.
    Magma,
    /// 4: slime.
    Slime,
}

impl From<EmeLiquidKind> for u8 {
    fn from(k: EmeLiquidKind) -> u8 {
        k as u8
    }
}

impl TryFrom<u8> for EmeLiquidKind {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, String> {
        Ok(match v {
            0 => Self::None,
            1 => Self::Water,
            2 => Self::Ocean,
            3 => Self::Magma,
            4 => Self::Slime,
            other => return Err(format!("unknown liquid kind {other}")),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// One uplink frame: `{"v":1,"id":…,"op":…, …args}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeServiceEnvelope {
    /// Schema version, [`EME_SERVICE_VERSION`].
    pub v: u8,
    /// Request id (1..=32767), echoed by the reply.
    pub id: EmeRequestId,
    /// The operation and its arguments.
    #[serde(flatten)]
    pub request: EmeServiceRequest,
}

impl EmeServiceEnvelope {
    /// Wraps `request` as version [`EME_SERVICE_VERSION`].
    pub fn new(id: EmeRequestId, request: EmeServiceRequest) -> Self {
        Self {
            v: EME_SERVICE_VERSION,
            id,
            request,
        }
    }

    /// The minified JSON text of this frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("service envelope serializes")
    }

    /// Parses an uplink frame (the server side, tests and tools use this).
    pub fn from_json(text: &str) -> Result<Self, EmeJsonError> {
        serde_json::from_str(text).map_err(EmeJsonError::from)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Arguments of `undo` / `redo` / `ov_undo`: a stroke id (newest non-undone row of the caller's
/// with that id), or an exact stroke row `sid` (another GM's needs rank 4), optionally filtered by
/// layer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmeUndoArgs {
    /// The caller's stroke id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<u16>,
    /// An exact `voxel_stroke.id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<u32>,
    /// Layer filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<EmeLayer>,
    /// Go ahead although others edited the area afterwards (Q4: warn, don't block).
    #[serde(default, skip_serializing_if = "is_false")]
    pub force: bool,
}

/// `log` filters. Absent fields take the server defaults (radius 60, minutes 60, limit 15, all layers).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmeLogQuery {
    /// Yards around the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
    /// Look-back window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u32>,
    /// Only this character's rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player: Option<String>,
    /// Only this voxel mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u8>,
    /// Layer filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<EmeLayerFilter>,
    /// Row cap, 1..25.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Paging: rows with a smaller seq.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<u32>,
    /// MONKEY (EME projects, CONTRACT v3.4): only rows of this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<u32>,
}

/// `ov_get`: live overlay cells inside a rectangle (span ≤ 256 yd per axis).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeOvGetQuery {
    /// Layer.
    pub layer: EmeOverlayLayer,
    /// Map id.
    pub map: u16,
    /// Rectangle corner.
    pub x0: f32,
    /// Rectangle corner.
    pub y0: f32,
    /// Rectangle corner.
    pub x1: f32,
    /// Rectangle corner.
    pub y1: f32,
    /// Row cap, 1..25.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Paging: rows with a smaller seq.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<u32>,
}

/// A service operation (the `op` field and its arguments).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EmeServiceRequest {
    /// Who am I to EME: rank, wire version, tools, perms, enforcement, limits.
    Caps,
    /// Set the edit-range anchor for a free camera (valid 30 s; ≤ 2/s).
    View {
        /// Map id (the character's).
        map: u16,
        /// Anchor.
        x: f32,
        /// Anchor.
        y: f32,
        /// Anchor.
        z: f32,
    },
    /// Stroke history.
    Strokes {
        /// `mine` (default) or `nearby`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<EmeStrokeScope>,
        /// Yards, 1..500.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
        /// Look-back, 1..10080.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minutes: Option<u32>,
        /// Row cap, 1..25.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        /// Paging: rows with a smaller `sid`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_sid: Option<u32>,
        /// MONKEY (EME projects): only strokes of this project.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<u32>,
    },
    /// Revert a stroke.
    Undo(EmeUndoArgs),
    /// Un-revert a stroke (`err:"conflict"` once it is no longer redoable).
    Redo(EmeUndoArgs),
    /// Alias of `undo` restricted to overlay layers.
    OvUndo(EmeUndoArgs),
    /// The audit log browser.
    Log(EmeLogQuery),
    /// Create a restore point around the caller.
    SnapshotCreate {
        /// UTF-8 label (≤ 64 chars), EN or RU.
        label: String,
        /// Yards, 10..500.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
    },
    /// List restore points near the caller.
    SnapshotList {
        /// Yards.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
        /// Row cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Revert all three layers after a restore point (rank 4).
    Revert {
        /// The snapshot id.
        snapshot: u32,
    },
    /// Revert one player's recent work (rank 4; own name at 3).
    UndoPlayer {
        /// Character name.
        name: String,
        /// Look-back, 1..10080.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minutes: Option<u32>,
        /// Yards around the caller's character.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
    },
    /// Overlay cell inspector.
    OvGet(EmeOvGetQuery),
    /// Stamp overlay cells (1..32, atomic).
    OvEdit(EmeOverlayEdit),
    /// Objects near the caller.
    ObjNearby {
        /// Yards, 1..200.
        radius: f32,
        /// Kind filter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<EmeObjectFilter>,
        /// Row cap, 1..16.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        /// Paging cursor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_guid: Option<u32>,
    },
    /// One object's authoritative row.
    ObjGet {
        /// Kind.
        kind: EmeObjectKind,
        /// Guid (low).
        guid: u32,
    },
    /// Place a game object or NPC.
    ObjSpawn(EmeObjSpawn),
    /// Move / rotate / scale an object (every field mandatory).
    ObjMove(EmeObjMove),
    /// Delete an object.
    ObjDelete {
        /// Kind.
        kind: EmeObjectKind,
        /// Guid (low).
        guid: u32,
        /// Transaction id.
        tx: EmeTxId,
    },
    /// Invert transaction `tx` (spawn → delete, move → move back, delete → re-spawn with a NEW guid).
    ObjUndo {
        /// The transaction to invert.
        tx: EmeTxId,
        /// Rank 4: the actor whose tx it is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<u32>,
    },
    /// MONKEY (EME projects, CONTRACT v3.4): create a project and make it the caller's active one.
    ProjectCreate {
        /// Name, 1..64 chars (EN or RU).
        name: String,
        /// Description, ≤ 255 chars.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// List projects (rank 2).
    ProjectList {
        /// `open` (server default), `closed`, `reverted` or `all`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// Only the caller's own.
        #[serde(default, skip_serializing_if = "is_false")]
        mine: bool,
        /// Row cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        /// Paging: ids below this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_id: Option<u32>,
    },
    /// Join an OPEN project (it becomes the caller's active one).
    ProjectOpen {
        /// Project id (`project`, not `id`: top-level `id` is the frame id).
        project: u32,
    },
    /// No active project any more.
    ProjectLeave,
    /// Close a project (owner or rank 4): nobody can edit into it.
    ProjectClose {
        /// Project id.
        project: u32,
    },
    /// Revert every live edit of a project (owner without other authors, else rank 4 + `force`).
    ProjectRevert {
        /// Project id.
        project: u32,
        /// Go ahead although other GMs edited in it (`others_edited`; needs rank 4).
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    /// Re-apply a reverted project (rank 4).
    ProjectUnrevert {
        /// Project id.
        project: u32,
    },
    /// Counts per layer, authors, bounding box, time span (rank 2).
    ProjectInfo {
        /// Project id.
        project: u32,
    },
}

impl EmeServiceRequest {
    /// The wire `op` name.
    pub fn op(&self) -> &'static str {
        use EmeServiceRequest as R;
        match self {
            R::Caps => "caps",
            R::View { .. } => "view",
            R::Strokes { .. } => "strokes",
            R::Undo(_) => "undo",
            R::Redo(_) => "redo",
            R::OvUndo(_) => "ov_undo",
            R::Log(_) => "log",
            R::SnapshotCreate { .. } => "snapshot_create",
            R::SnapshotList { .. } => "snapshot_list",
            R::Revert { .. } => "revert",
            R::UndoPlayer { .. } => "undo_player",
            R::OvGet(_) => "ov_get",
            R::OvEdit(_) => "ov_edit",
            R::ObjNearby { .. } => "obj_nearby",
            R::ObjGet { .. } => "obj_get",
            R::ObjSpawn(_) => "obj_spawn",
            R::ObjMove(_) => "obj_move",
            R::ObjDelete { .. } => "obj_delete",
            R::ObjUndo { .. } => "obj_undo",
            R::ProjectCreate { .. } => "project_create",
            R::ProjectList { .. } => "project_list",
            R::ProjectOpen { .. } => "project_open",
            R::ProjectLeave => "project_leave",
            R::ProjectClose { .. } => "project_close",
            R::ProjectRevert { .. } => "project_revert",
            R::ProjectUnrevert { .. } => "project_unrevert",
            R::ProjectInfo { .. } => "project_info",
        }
    }

    /// The capability the client requires before sending this request (CONTRACT §8). `None` =
    /// sent unconditionally (`caps` is the probe; below rank 2 the server hides `.mveme`, so it
    /// simply gets no reply).
    ///
    /// Ops whose rank depends on ownership (`undo`/`redo` of another GM's `sid`, `obj_delete` /
    /// `obj_undo` of others' objects, `undo_player` of another name) are gated at
    /// [`EmeCapability::Edit`] here; the server applies the rank-4 rule, which only it can decide.
    pub fn required_capability(&self) -> Option<EmeCapability> {
        use EmeServiceRequest as R;
        match self {
            R::Caps => None,
            R::Strokes { .. }
            | R::Log(_)
            | R::SnapshotList { .. }
            | R::OvGet(_)
            | R::ObjNearby { .. }
            | R::ObjGet { .. }
            | R::ProjectList { .. }
            | R::ProjectInfo { .. } => Some(EmeCapability::View),
            R::View { .. }
            | R::Undo(_)
            | R::Redo(_)
            | R::OvUndo(_)
            | R::SnapshotCreate { .. }
            | R::UndoPlayer { .. }
            | R::OvEdit(_)
            | R::ObjSpawn(_)
            | R::ObjMove(_)
            | R::ObjDelete { .. }
            | R::ObjUndo { .. }
            | R::ProjectCreate { .. }
            | R::ProjectOpen { .. }
            | R::ProjectLeave
            | R::ProjectClose { .. }
            | R::ProjectRevert { .. } => Some(EmeCapability::Edit),
            R::Revert { .. } | R::ProjectUnrevert { .. } => Some(EmeCapability::RevertOthers),
        }
    }

    /// Whether the request changes server state (`view` only moves the range anchor).
    pub fn is_mutating(&self) -> bool {
        !matches!(self.required_capability(), None | Some(EmeCapability::View))
            && !matches!(self, EmeServiceRequest::View { .. })
    }
}

// ---------------------------------------------------------------------------------------------
// Overlay cells (CONTRACT §11.1); on the wire each cell is a JSON array
// ---------------------------------------------------------------------------------------------

/// One liquid cell edit, wire `[cx, cy, kind, level, flags, fade]`: a flat surface at `level` over
/// liquid cell `c = floor(world * 6 / 25)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(
    from = "(i16, i16, EmeLiquidKind, f32, u8, u8)",
    into = "(i16, i16, EmeLiquidKind, f32, u8, u8)"
)]
pub struct EmeLiquidCell {
    /// Liquid-cell x.
    pub cx: i16,
    /// Liquid-cell y.
    pub cy: i16,
    /// Liquid kind.
    pub kind: EmeLiquidKind,
    /// Surface height z, yards.
    pub level: f32,
    /// Bit 0 dark, bit 1 hidden, bit 2 fatigue.
    pub flags: u8,
    /// Depth fade 0..255.
    pub fade: u8,
}

impl From<(i16, i16, EmeLiquidKind, f32, u8, u8)> for EmeLiquidCell {
    fn from((cx, cy, kind, level, flags, fade): (i16, i16, EmeLiquidKind, f32, u8, u8)) -> Self {
        Self {
            cx,
            cy,
            kind,
            level,
            flags,
            fade,
        }
    }
}

impl From<EmeLiquidCell> for (i16, i16, EmeLiquidKind, f32, u8, u8) {
    fn from(c: EmeLiquidCell) -> Self {
        (c.cx, c.cy, c.kind, c.level, c.flags, c.fade)
    }
}

/// One area cell edit, wire `[ax, ay, area_id]`: area cell `a = floor(world / 16)`; `area_id` is
/// an existing `AreaTable` id, `0` removes the override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "(i16, i16, u16)", into = "(i16, i16, u16)")]
pub struct EmeAreaCell {
    /// Area-cell x.
    pub ax: i16,
    /// Area-cell y.
    pub ay: i16,
    /// `AreaTable` id, 0 = remove the override.
    pub area_id: u16,
}

impl From<(i16, i16, u16)> for EmeAreaCell {
    fn from((ax, ay, area_id): (i16, i16, u16)) -> Self {
        Self { ax, ay, area_id }
    }
}

impl From<EmeAreaCell> for (i16, i16, u16) {
    fn from(c: EmeAreaCell) -> Self {
        (c.ax, c.ay, c.area_id)
    }
}

/// The `ov_edit` payload: up to 32 cells of one layer, belonging to one stroke.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "layer", rename_all = "snake_case")]
pub enum EmeOverlayEdit {
    /// Liquid cells.
    Liquid {
        /// Map id.
        map: u16,
        /// The caller's stroke id (shared numbering with voxel strokes).
        stroke: u16,
        /// The cells.
        #[serde(deserialize_with = "lenient::vec_or_empty_map")]
        cells: Vec<EmeLiquidCell>,
    },
    /// Area cells.
    Area {
        /// Map id.
        map: u16,
        /// The caller's stroke id.
        stroke: u16,
        /// The cells.
        #[serde(deserialize_with = "lenient::vec_or_empty_map")]
        cells: Vec<EmeAreaCell>,
    },
}

impl EmeOverlayEdit {
    /// The edited layer.
    pub fn layer(&self) -> EmeOverlayLayer {
        match self {
            EmeOverlayEdit::Liquid { .. } => EmeOverlayLayer::Liquid,
            EmeOverlayEdit::Area { .. } => EmeOverlayLayer::Area,
        }
    }

    /// The stroke this batch belongs to.
    pub fn stroke(&self) -> u16 {
        match self {
            EmeOverlayEdit::Liquid { stroke, .. } | EmeOverlayEdit::Area { stroke, .. } => *stroke,
        }
    }

    /// Number of cells.
    pub fn len(&self) -> usize {
        match self {
            EmeOverlayEdit::Liquid { cells, .. } => cells.len(),
            EmeOverlayEdit::Area { cells, .. } => cells.len(),
        }
    }

    /// Whether the batch has no cells.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------------------------
// Objects (CONTRACT §13)
// ---------------------------------------------------------------------------------------------

/// `obj_spawn` arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeObjSpawn {
    /// GO or NPC.
    pub kind: EmeObjectKind,
    /// Template entry.
    pub entry: u32,
    /// Map id.
    pub map: u16,
    /// Position, WoW yards.
    pub x: f32,
    /// Position.
    pub y: f32,
    /// Position.
    pub z: f32,
    /// Orientation, radians.
    pub o: f32,
    /// Uniform scale (`caps.limits.obj_scale`).
    pub scale: f32,
    /// Transaction id.
    pub tx: EmeTxId,
}

/// `obj_move` arguments: the full new transform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeObjMove {
    /// Kind.
    pub kind: EmeObjectKind,
    /// Guid (low).
    pub guid: u32,
    /// Position, WoW yards.
    pub x: f32,
    /// Position.
    pub y: f32,
    /// Position.
    pub z: f32,
    /// Orientation, radians.
    pub o: f32,
    /// Uniform scale.
    pub scale: f32,
    /// Transaction id.
    pub tx: EmeTxId,
}

/// Who placed an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeOwner {
    /// Character guid (low).
    pub guid: u32,
    /// Character name.
    pub name: String,
}

/// The authoritative object row (`row` of every object reply and push).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeObjectRow {
    /// Guid (low).
    pub guid: u32,
    /// Kind.
    pub kind: EmeObjectKind,
    /// Template entry.
    pub entry: u32,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Map id.
    pub map: u16,
    /// Position.
    pub x: f32,
    /// Position.
    pub y: f32,
    /// Position.
    pub z: f32,
    /// Orientation, radians, in [0, 2π).
    pub o: f32,
    /// Uniform scale.
    pub scale: f32,
    /// Recorded placer; `null` = baseline spawn (counts as "others'").
    #[serde(default)]
    pub owner: Option<EmeOwner>,
    /// The transaction that last wrote the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx: Option<EmeTxId>,
}

/// The result fields of `obj_get` / `obj_spawn` / `obj_move` / `obj_delete` / `obj_undo`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmeObjectResult {
    /// The resulting row (pre-delete row for `obj_delete`; `null` when an undo deleted it).
    #[serde(default)]
    pub row: Option<EmeObjectRow>,
    /// The transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx: Option<EmeTxId>,
    /// `obj_undo` of a delete: the old guid the re-spawned row replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<u32>,
    /// A repeat of an already-processed `(character, tx)`: nothing changed.
    #[serde(
        default,
        deserialize_with = "lenient::bool_or_int",
        skip_serializing_if = "is_false"
    )]
    pub replayed: bool,
}

// ---------------------------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------------------------

/// One reply frame: `{"v":1,"id":…,"ok":…,"err"?,"msg"?,"detail"?,"rows"?, …result fields}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeServiceReply {
    /// Schema version.
    pub v: u8,
    /// The request's id.
    pub id: EmeRequestId,
    /// Whether the request was carried out.
    #[serde(deserialize_with = "lenient::bool_or_int")]
    pub ok: bool,
    /// Error code when `ok` is false (see [`EmeServiceError`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    /// Locale key for the error text (`MONKEY_EME_ERR_<CODE>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    /// Error detail, e.g. `{"field":"cells","index":1}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    /// List results, typed per op (read with [`EmeServiceReply::rows_as`]).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_vec_or_empty_map"
    )]
    pub rows: Option<Vec<Value>>,
    /// Every other top-level field (`rank`, `row`, `more`, `count`, `others`, …).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl EmeServiceReply {
    /// A successful reply with no result fields.
    pub fn ok(id: EmeRequestId) -> Self {
        Self {
            v: EME_SERVICE_VERSION,
            id,
            ok: true,
            err: None,
            msg: None,
            detail: None,
            rows: None,
            extra: Map::new(),
        }
    }

    /// A failed reply with error code `err`.
    pub fn error(id: EmeRequestId, err: impl Into<String>) -> Self {
        Self {
            ok: false,
            err: Some(err.into()),
            ..Self::ok(id)
        }
    }

    /// Adds a top-level result field (builder style; tests and reference servers use it).
    pub fn with(mut self, key: &str, value: impl Serialize) -> Self {
        self.extra.insert(
            key.to_owned(),
            serde_json::to_value(value).expect("serializable"),
        );
        self
    }

    /// The parsed error, `None` when `ok`.
    pub fn error_code(&self) -> Option<EmeServiceError> {
        if self.ok {
            None
        } else {
            Some(EmeServiceError::parse(self.err.as_deref().unwrap_or("")))
        }
    }

    /// `rows` decoded as `T` (an absent `rows` is an empty list).
    pub fn rows_as<T: DeserializeOwned>(&self) -> Result<Vec<T>, EmeJsonError> {
        match &self.rows {
            None => Ok(Vec::new()),
            Some(rows) => rows
                .iter()
                .map(|r| serde_json::from_value(r.clone()).map_err(EmeJsonError::from))
                .collect(),
        }
    }

    /// The top-level result fields decoded as `T`.
    pub fn extra_as<T: DeserializeOwned>(&self) -> Result<T, EmeJsonError> {
        serde_json::from_value(Value::Object(self.extra.clone())).map_err(EmeJsonError::from)
    }

    /// Paging flag: `more` true/1 means "ask again with the paging cursor".
    pub fn more(&self) -> bool {
        match self.extra.get("more") {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            _ => false,
        }
    }

    /// The minified JSON text of this frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("service reply serializes")
    }
}

/// A reply error code (CONTRACT §12.3), plus two client-side codes that never travel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EmeServiceError {
    /// Above the caller's rank.
    Rank,
    /// Out of range.
    Range,
    /// Rate limited.
    Rate,
    /// Malformed / out-of-range field (`detail.field`).
    Invalid,
    /// Wrong map.
    Map,
    /// Stroke closed / undone / unknown.
    Stroke,
    /// No such row.
    NotFound,
    /// Redo no longer allowed (`redo_ok = 0`).
    Conflict,
    /// Q4 warn: others edited the area after this stroke; resend with `force`.
    OthersEdited,
    /// Mode / liquid kind not enabled.
    Unsupported,
    /// Unknown `AreaTable` id.
    BadArea,
    /// The server's v3 SQL is missing.
    NotReady,
    /// Frame over 2800 bytes.
    TooLarge,
    /// Unknown `op`.
    UnknownOp,
    /// Object adapter policy refused (e.g. a fief guard).
    Fief,
    /// No such object template.
    EntryUnknown,
    /// Per-GM object cap reached.
    Limit,
    /// Server error.
    Internal,
    /// MONKEY (EME projects): no active project (open or create one first).
    NoProject,
    /// Client-side: no reply in time.
    Timeout,
    /// Client-side: the link dropped before the reply.
    LinkLost,
    /// Any other code, kept verbatim.
    Other(String),
}

impl EmeServiceError {
    /// Parses a wire code.
    pub fn parse(code: &str) -> Self {
        match code {
            "rank" => Self::Rank,
            "range" => Self::Range,
            "rate" => Self::Rate,
            "invalid" => Self::Invalid,
            "map" => Self::Map,
            "stroke" => Self::Stroke,
            "not_found" => Self::NotFound,
            "conflict" => Self::Conflict,
            "others_edited" => Self::OthersEdited,
            "unsupported" => Self::Unsupported,
            "bad_area" => Self::BadArea,
            "not_ready" => Self::NotReady,
            "too_large" => Self::TooLarge,
            "unknown_op" => Self::UnknownOp,
            "fief" => Self::Fief,
            "entry_unknown" => Self::EntryUnknown,
            "limit" => Self::Limit,
            "internal" => Self::Internal,
            "no_project" => Self::NoProject,
            "timeout" => Self::Timeout,
            "link_lost" => Self::LinkLost,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The wire code.
    pub fn code(&self) -> &str {
        match self {
            Self::Rank => "rank",
            Self::Range => "range",
            Self::Rate => "rate",
            Self::Invalid => "invalid",
            Self::Map => "map",
            Self::Stroke => "stroke",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::OthersEdited => "others_edited",
            Self::Unsupported => "unsupported",
            Self::BadArea => "bad_area",
            Self::NotReady => "not_ready",
            Self::TooLarge => "too_large",
            Self::UnknownOp => "unknown_op",
            Self::Fief => "fief",
            Self::EntryUnknown => "entry_unknown",
            Self::Limit => "limit",
            Self::Internal => "internal",
            Self::NoProject => "no_project",
            Self::Timeout => "timeout",
            Self::LinkLost => "link_lost",
            Self::Other(s) => s,
        }
    }

    /// The client locale key for this error (`MONKEY_EME_ERR_<CODE>`; unknown codes share
    /// `MONKEY_EME_ERR_OTHER`).
    pub fn locale_key(&self) -> String {
        let code = match self {
            Self::Other(_) => "other",
            c => c.code(),
        };
        format!("MONKEY_EME_ERR_{}", code.to_ascii_uppercase())
    }
}

impl fmt::Display for EmeServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// A JSON decode/encode failure.
#[derive(Debug)]
pub struct EmeJsonError(pub serde_json::Error);

impl From<serde_json::Error> for EmeJsonError {
    fn from(e: serde_json::Error) -> Self {
        Self(e)
    }
}

impl fmt::Display for EmeJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EME JSON: {}", self.0)
    }
}

impl std::error::Error for EmeJsonError {}

// ---------------------------------------------------------------------------------------------
// Typed results
// ---------------------------------------------------------------------------------------------

/// The `caps` reply: what this caller may do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeCaps {
    /// The caller's GM rank.
    pub rank: EmeRank,
    /// The negotiated binary wire version (3 for EME).
    #[serde(default)]
    pub wire: u8,
    /// Tool ids the server offers this caller; the toolbar shows only these.
    #[serde(default, deserialize_with = "lenient::vec_or_empty_map")]
    pub tools: Vec<String>,
    /// Permission names (`edit`, `undo_own`, `revert`, `obj_delete_others`, …).
    #[serde(default, deserialize_with = "lenient::vec_or_empty_map")]
    pub perms: Vec<String>,
    /// 1 iff the server enforces overlay edits through its core hook; 0 = "visual only".
    #[serde(
        default,
        deserialize_with = "lenient::bool_or_int",
        serialize_with = "bool_as_int"
    )]
    pub enforced: bool,
    /// The enforced overlay layers (empty when `enforced` is 0).
    #[serde(default, deserialize_with = "lenient::vec_or_empty_map")]
    pub enforced_layers: Vec<EmeOverlayLayer>,
    /// Server limits.
    #[serde(default)]
    pub limits: EmeLimits,
    /// MONKEY (EME projects, CONTRACT v3.4): the caller's active project, `None` = none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<EmeProjectRef>,
    /// Whether the server knows projects at all: `None` = a server before v3.4 (nothing is gated
    /// on a project), `Some(false)` = it does but its tables are missing (every edit is refused).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_bool_or_int"
    )]
    pub projects_ready: Option<bool>,
}

fn bool_as_int<S: serde::Serializer>(b: &bool, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u8(u8::from(*b))
}

impl EmeCaps {
    /// Decodes the caps from a `caps` reply.
    pub fn from_reply(reply: &EmeServiceReply) -> Result<Self, EmeJsonError> {
        reply.extra_as()
    }

    /// Whether the server lists `tool`.
    pub fn offers_tool(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == tool)
    }

    /// Whether the server lists permission `perm`.
    pub fn has_perm(&self, perm: &str) -> bool {
        self.perms.iter().any(|p| p == perm)
    }

    /// Whether edits of `layer` change server game logic (else the UI says "visual only").
    pub fn is_enforced(&self, layer: EmeOverlayLayer) -> bool {
        self.enforced && self.enforced_layers.contains(&layer)
    }
}

/// `caps.limits`. Absent fields mean "contract default".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmeLimits {
    /// Edit range around the character or view anchor, yards (120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_range: Option<f32>,
    /// Fan-out radius, yards (300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fanout: Option<f32>,
    /// Region size, yards (64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<f32>,
    /// Voxel modes the server accepts (MVP `[0,1,2,3]`).
    #[serde(
        default,
        deserialize_with = "lenient::vec_or_empty_map",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub modes: Vec<u8>,
    /// Brush radius `[min, max]`, yards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<[f32; 2]>,
    /// Liquid cell size, yards (25/6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquid_cell: Option<f32>,
    /// Area cell size, yards (16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area_cell: Option<f32>,
    /// Liquid kinds the server accepts (MVP `[0,1]`).
    #[serde(
        default,
        deserialize_with = "lenient::vec_or_empty_map",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub liquid_kinds: Vec<u8>,
    /// Most cells per `ov_edit` (32).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ov_cells_max: Option<u32>,
    /// Largest JSON frame, bytes (2800).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_max: Option<u32>,
    /// Object reach, yards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obj_range: Option<f32>,
    /// Object scale `[min, max]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obj_scale: Option<[f32; 2]>,
    /// Largest `log` page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_rows_max: Option<u32>,
    /// Any limit this client version does not know.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

impl EmeLimits {
    /// Whether voxel mode `mode` is enabled.
    pub fn allows_mode(&self, mode: u8) -> bool {
        self.modes.contains(&mode)
    }
}

/// A `strokes` row: one `voxel_stroke` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeStrokeRow {
    /// `voxel_stroke.id`.
    pub sid: u32,
    /// The author's stroke id.
    pub stroke: u16,
    /// Layer.
    pub layer: EmeLayer,
    /// Author guid (low).
    pub guid: u32,
    /// Author name.
    pub name: String,
    /// Map id.
    pub map: u16,
    /// Touched regions `[rx_min, ry_min, rx_max, ry_max]`.
    pub bbox: [i16; 4],
    /// First row seq.
    pub first_seq: u32,
    /// Last row seq.
    pub last_seq: u32,
    /// Row count.
    pub edits: u32,
    /// Voxel modes used.
    #[serde(default, deserialize_with = "lenient::vec_or_empty_map")]
    pub modes: Vec<u8>,
    /// Unix seconds.
    pub t: u64,
    /// Currently undone.
    #[serde(default, deserialize_with = "lenient::bool_or_int")]
    pub undone: bool,
    /// Redo still allowed.
    #[serde(default, deserialize_with = "lenient::bool_or_int")]
    pub redo_ok: bool,
    /// Live rows by other guids after this stroke in its bbox.
    #[serde(default)]
    pub others_after: u32,
}

/// The result of `undo` / `redo` / `ov_undo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeUndoResult {
    /// The stroke row acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<u32>,
    /// Its layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<EmeLayer>,
    /// Rows reverted (undo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverted: Option<u32>,
    /// Rows restored (redo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored: Option<u32>,
    /// The revert id (undo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert_id: Option<u32>,
}

/// MONKEY (EME projects): a project as `caps.project` and the create / open replies name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeProjectRef {
    /// Project id.
    pub id: u32,
    /// Its name.
    #[serde(default)]
    pub name: String,
    /// `open`, `closed`, `reverted` (absent in `caps`, where it is always open).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// A `project_list` row (also the `project` of a `project_info` reply).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeProjectRow {
    /// Project id.
    pub id: u32,
    /// Name.
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// Owner's character name.
    #[serde(default)]
    pub owner: String,
    /// Owner's guid (low).
    #[serde(default)]
    pub owner_guid: u32,
    /// `open`, `closed` or `reverted`.
    pub status: String,
    /// Map id it was created on.
    #[serde(default)]
    pub map: u32,
    /// Unix seconds.
    #[serde(default)]
    pub created: u64,
    /// Edit rows in it (all layers).
    #[serde(default)]
    pub edits: u32,
    /// Of `edits`, not reverted.
    #[serde(default)]
    pub live: u32,
    /// Unix seconds of the newest edit.
    #[serde(default)]
    pub last_edit: Option<u64>,
    /// The caller owns it.
    #[serde(default, deserialize_with = "lenient::bool_or_int")]
    pub mine: bool,
    /// It is the caller's active project.
    #[serde(default, deserialize_with = "lenient::bool_or_int")]
    pub active: bool,
}

/// Rows of one layer in a project (`project_info.counts.<layer>`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeProjectCount {
    /// Rows.
    #[serde(default)]
    pub total: u32,
    /// Of them, not reverted.
    #[serde(default)]
    pub live: u32,
}

/// An author of a project (`project_info.authors`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeProjectAuthor {
    /// Guid (low).
    #[serde(default)]
    pub guid: u32,
    /// Character name.
    #[serde(default)]
    pub name: String,
    /// Rows.
    #[serde(default)]
    pub edits: u32,
    /// Of them, not reverted.
    #[serde(default)]
    pub live: u32,
}

/// The `project_info` reply (top-level fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeProjectInfo {
    /// The project row.
    pub project: EmeProjectRow,
    /// Per layer (`voxel`, `liquid`, `area`, `object`).
    #[serde(default)]
    pub counts: std::collections::BTreeMap<String, EmeProjectCount>,
    /// Authors, most edits first.
    #[serde(default, deserialize_with = "lenient::vec_or_empty_map")]
    pub authors: Vec<EmeProjectAuthor>,
    /// `[x0, y0, x1, y1]` of the edits, WoW yards.
    #[serde(default)]
    pub bbox: Option<[f32; 4]>,
    /// Unix seconds.
    #[serde(default)]
    pub first_edit: Option<u64>,
    /// Unix seconds.
    #[serde(default)]
    pub last_edit: Option<u64>,
}

/// The `project_revert` / `project_unrevert` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeProjectRevertResult {
    /// Project id.
    #[serde(default)]
    pub project_id: u32,
    /// The new status.
    #[serde(default)]
    pub status: String,
    /// Voxel rows.
    #[serde(default)]
    pub voxel: u32,
    /// Liquid rows.
    #[serde(default)]
    pub liquid: u32,
    /// Area rows.
    #[serde(default)]
    pub area: u32,
    /// Objects.
    #[serde(default)]
    pub object: u32,
    /// The revert id.
    #[serde(default)]
    pub revert_id: u32,
}

/// The fields of an `others_edited` error: who edited after the stroke.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeOthersEdited {
    /// Character names.
    #[serde(default, deserialize_with = "lenient::vec_or_empty_map")]
    pub others: Vec<String>,
    /// Live rows by them.
    #[serde(default)]
    pub count: u32,
}

/// A `log` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeLogRow {
    /// Layer.
    pub layer: EmeLayer,
    /// Row seq (paging cursor `before_seq`).
    pub seq: u32,
    /// Unix seconds.
    pub t: u64,
    /// Who: guid (low).
    pub guid: u32,
    /// Who: name.
    pub name: String,
    /// Who: account.
    #[serde(default)]
    pub account: u32,
    /// What: voxel mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u8>,
    /// What: brush radius.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
    /// Where.
    pub x: f32,
    /// Where.
    pub y: f32,
    /// Where.
    pub z: f32,
    /// The stroke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<u16>,
    /// Currently reverted.
    #[serde(default, deserialize_with = "lenient::bool_or_int")]
    pub reverted: bool,
}

/// The `log` page fields besides `rows`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeLogPage {
    /// Total matching rows.
    #[serde(default)]
    pub count: u32,
    /// More rows exist.
    #[serde(default, deserialize_with = "lenient::bool_or_int")]
    pub more: bool,
}

/// Per-layer seqs (`at_seq` of a restore point).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeLayerSeqs {
    /// Voxel seq.
    #[serde(default)]
    pub voxel: u32,
    /// Liquid seq.
    #[serde(default)]
    pub liquid: u32,
    /// Area seq.
    #[serde(default)]
    pub area: u32,
}

/// The result of `snapshot_create`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeSnapshotCreated {
    /// The new snapshot id.
    pub snapshot: u32,
    /// Seqs at creation.
    #[serde(default)]
    pub at_seq: EmeLayerSeqs,
}

/// A `snapshot_list` row: one restore point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeSnapshotRow {
    /// Snapshot id.
    pub snapshot: u32,
    /// UTF-8 label.
    pub label: String,
    /// Creator name.
    pub name: String,
    /// Unix seconds.
    pub t: u64,
    /// Map id.
    pub map: u16,
    /// Centre.
    pub x: f32,
    /// Centre.
    pub y: f32,
    /// Radius, yards.
    pub radius: f32,
    /// Seqs at creation.
    #[serde(default)]
    pub at_seq: EmeLayerSeqs,
}

/// The result of `revert` / `undo_player`: rows reverted per layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeRevertResult {
    /// Voxel rows.
    #[serde(default)]
    pub voxel: u32,
    /// Liquid rows.
    #[serde(default)]
    pub liquid: u32,
    /// Area rows.
    #[serde(default)]
    pub area: u32,
    /// The revert id.
    #[serde(default)]
    pub revert_id: u32,
}

/// The result of `view`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeViewResult {
    /// Seconds the anchor stays valid.
    pub ttl: u32,
}

/// The result of `ov_edit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeOvEditResult {
    /// Cells accepted.
    pub accepted: u32,
    /// First seq assigned.
    pub first_seq: u32,
    /// Last seq assigned.
    pub last_seq: u32,
    /// Enforced by the server (else "visual only").
    #[serde(
        default,
        deserialize_with = "lenient::bool_or_int",
        serialize_with = "bool_as_int"
    )]
    pub enforced: bool,
}

/// An `ov_get` row of the liquid layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeLiquidRow {
    /// Layer seq.
    pub seq: u32,
    /// Cell x.
    pub cx: i16,
    /// Cell y.
    pub cy: i16,
    /// Kind.
    pub kind: EmeLiquidKind,
    /// Level.
    pub level: f32,
    /// Flags.
    #[serde(default)]
    pub flags: u8,
    /// Depth fade.
    #[serde(default)]
    pub fade: u8,
    /// Author guid (low).
    pub guid: u32,
    /// Author name.
    pub name: String,
    /// Author's stroke id.
    #[serde(default)]
    pub stroke: u16,
    /// Unix seconds.
    pub t: u64,
}

/// An `ov_get` row of the area layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeAreaRow {
    /// Layer seq.
    pub seq: u32,
    /// Cell x.
    pub ax: i16,
    /// Cell y.
    pub ay: i16,
    /// `AreaTable` id.
    pub area_id: u16,
    /// Author guid (low).
    pub guid: u32,
    /// Author name.
    pub name: String,
    /// Author's stroke id.
    #[serde(default)]
    pub stroke: u16,
    /// Unix seconds.
    pub t: u64,
}

// ---------------------------------------------------------------------------------------------
// Pushes (CONTRACT §12.5)
// ---------------------------------------------------------------------------------------------

/// An unsolicited downlink frame `{"v":1,"id":0,"ev":…, …}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum EmeServicePush {
    /// The caller's rank changed; re-request `caps`.
    CapsChanged {
        /// The new rank.
        rank: EmeRank,
    },
    /// Overlay enforcement switched.
    Enforce {
        /// New `caps.enforced`.
        #[serde(
            deserialize_with = "lenient::bool_or_int",
            serialize_with = "bool_as_int"
        )]
        enforced: bool,
    },
    /// Someone else changed an object nearby.
    Obj {
        /// What happened.
        action: EmeObjectAction,
        /// The row (pre-delete row for a delete); none for `refresh`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        row: Option<EmeObjectRow>,
    },
    /// MONKEY (EME projects): a project this GM is in was closed / reverted elsewhere (or its row
    /// is gone): it is no longer the active one.
    ProjectChanged {
        /// Project id.
        project: u32,
        /// `closed`, `reverted`, `open` or `gone`.
        #[serde(default)]
        status: String,
        /// Its name, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// Any downlink frame.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeDownlink {
    /// A reply to one of our requests.
    Reply(EmeServiceReply),
    /// An unsolicited push.
    Push(EmeServicePush),
}

impl EmeDownlink {
    /// Parses a downlink frame: an object with `ev` is a push, otherwise a reply.
    pub fn from_json(text: &str) -> Result<Self, EmeJsonError> {
        let value: Value = serde_json::from_str(text)?;
        if value.get("ev").is_some() {
            Ok(Self::Push(serde_json::from_value(value)?))
        } else {
            Ok(Self::Reply(serde_json::from_value(value)?))
        }
    }

    /// The minified JSON text of this frame.
    pub fn to_json(&self) -> String {
        match self {
            Self::Reply(r) => r.to_json(),
            Self::Push(p) => {
                let mut m = Map::new();
                m.insert("v".into(), Value::from(EME_SERVICE_VERSION));
                m.insert("id".into(), Value::from(EmeRequestId::PUSH.0));
                if let Value::Object(fields) = serde_json::to_value(p).expect("push serializes") {
                    m.extend(fields);
                }
                Value::Object(m).to_string()
            }
        }
    }
}

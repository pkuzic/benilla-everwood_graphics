//! The `.mveme` service wire: JSON requests, replies and server pushes (DESIGN-EME §3.5, §4.3, §5.3).
//!
//! Every frame is one JSON object. Uplink: `{"v":1,"id":7,"op":"undo","stroke":12}`. Downlink is
//! either a **reply** `{"v":1,"id":7,"ok":true,"rows":[…]}` (it carries the request's `id`) or a
//! **push** `{"v":1,"ev":"ov",…}` (no `id`, carries `ev`). The transport (chat whispers or another
//! channel) only moves these strings; base64url framing and `seq/part/total` chunking are its job.
//!
//! The schema is identical on every transport so the server has one handler. Op-specific reply
//! fields that are not `rows` (e.g. the `caps` object) sit at the top level of the reply and are
//! read with [`EmeServiceReply::extra_as`].

use crate::lenient;
use crate::perm::{EmeCapability, EmeRank};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// The service schema version, the `v` of every frame.
pub const EME_SERVICE_VERSION: u8 = 1;
/// The largest JSON frame (before transport encoding) either side may send, in bytes.
pub const EME_MAX_FRAME_BYTES: usize = 2800;

/// A request id, chosen by the sender of the request and echoed in the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmeRequestId(pub u32);

/// A client-chosen object transaction id; makes `obj_*` mutations idempotent and names them for
/// `obj_undo` / `obj_redo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmeTxId(pub u32);

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// One uplink frame: `{"v":1,"id":…,"op":…, …op fields}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeServiceEnvelope {
    /// Schema version, [`EME_SERVICE_VERSION`].
    pub v: u8,
    /// Request id, echoed by the reply.
    pub id: EmeRequestId,
    /// The operation and its fields.
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

    /// The compact JSON text of this frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("service envelope serializes")
    }

    /// Parses an uplink frame (the server side, tests and tools use this).
    pub fn from_json(text: &str) -> Result<Self, EmeJsonError> {
        serde_json::from_str(text).map_err(EmeJsonError::from)
    }
}

/// A service operation (the `op` field and its arguments).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EmeServiceRequest {
    /// Who am I to EME: rank, usable tools, limits, enforcement. Sent once on link-up.
    Caps,
    /// Recent strokes near the caller (history panel).
    Strokes {
        /// Yards around the caller; server default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
        /// Only the caller's own strokes.
        #[serde(default, skip_serializing_if = "is_false")]
        mine: bool,
        /// Row cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Revert one of the caller's own voxel strokes.
    Undo {
        /// The caller's stroke id (strokes are keyed by `(guid, stroke)` on the server).
        stroke: u16,
        /// Undo even though another GM edited the area afterwards (Q4: warn, don't block).
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    /// Un-revert a stroke the caller undid.
    Redo {
        /// The caller's stroke id.
        stroke: u16,
    },
    /// The audit log browser.
    Log(EmeLogQuery),
    /// Create a restore point around the caller.
    SnapshotCreate {
        /// UTF-8 label (EN or RU).
        label: String,
        /// Yards; server default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
    },
    /// List restore points near the caller.
    SnapshotList {
        /// Yards; server default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
    },
    /// Revert everything after a restore point inside its radius (rank 4).
    Revert {
        /// The `voxel_snapshot.id`.
        snapshot: u32,
    },
    /// Revert one player's recent work (anti-grief, rank 4).
    UndoPlayer {
        /// Character name.
        player: String,
        /// Look-back window; server default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minutes: Option<u32>,
        /// Yards around the caller; server default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f32>,
    },
    /// Sync overlay rows for regions, exactly like the voxel SYNC (epoch + after_seq).
    OvGet {
        /// Which overlay.
        layer: EmeOverlayLayer,
        /// Map id.
        map: u16,
        /// Region cursors (≤ 12 per request, like voxel SYNC).
        regions: Vec<EmeRegionCursor>,
    },
    /// Stamp overlay cells.
    OvEdit(EmeOverlayEdit),
    /// Revert one of the caller's overlay strokes.
    OvUndo {
        /// Which overlay.
        layer: EmeOverlayLayer,
        /// The caller's stroke id.
        stroke: u16,
        /// As [`EmeServiceRequest::Undo::force`].
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    /// Un-revert an overlay stroke.
    OvRedo {
        /// Which overlay.
        layer: EmeOverlayLayer,
        /// The caller's stroke id.
        stroke: u16,
    },
    /// Objects near the caller.
    ObjNearby {
        /// Yards.
        radius: f32,
    },
    /// One object's authoritative row.
    ObjGet {
        /// Object guid (low part; GO or creature per the row's `kind`).
        guid: u64,
    },
    /// Place a game object or NPC.
    ObjSpawn(EmeObjSpawn),
    /// Move / rotate / scale an object.
    ObjMove(EmeObjMove),
    /// Delete an object.
    ObjDelete {
        /// Object guid.
        guid: u64,
        /// Transaction id.
        tx: EmeTxId,
    },
    /// Apply the inverse of transaction `tx` (server re-checks rank and placement policy).
    ObjUndo {
        /// The transaction to invert.
        tx: EmeTxId,
    },
    /// Re-apply transaction `tx` after an `obj_undo`.
    ObjRedo {
        /// The transaction to re-apply.
        tx: EmeTxId,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl EmeServiceRequest {
    /// The wire `op` name.
    pub fn op(&self) -> &'static str {
        match self {
            EmeServiceRequest::Caps => "caps",
            EmeServiceRequest::Strokes { .. } => "strokes",
            EmeServiceRequest::Undo { .. } => "undo",
            EmeServiceRequest::Redo { .. } => "redo",
            EmeServiceRequest::Log(_) => "log",
            EmeServiceRequest::SnapshotCreate { .. } => "snapshot_create",
            EmeServiceRequest::SnapshotList { .. } => "snapshot_list",
            EmeServiceRequest::Revert { .. } => "revert",
            EmeServiceRequest::UndoPlayer { .. } => "undo_player",
            EmeServiceRequest::OvGet { .. } => "ov_get",
            EmeServiceRequest::OvEdit(_) => "ov_edit",
            EmeServiceRequest::OvUndo { .. } => "ov_undo",
            EmeServiceRequest::OvRedo { .. } => "ov_redo",
            EmeServiceRequest::ObjNearby { .. } => "obj_nearby",
            EmeServiceRequest::ObjGet { .. } => "obj_get",
            EmeServiceRequest::ObjSpawn(_) => "obj_spawn",
            EmeServiceRequest::ObjMove(_) => "obj_move",
            EmeServiceRequest::ObjDelete { .. } => "obj_delete",
            EmeServiceRequest::ObjUndo { .. } => "obj_undo",
            EmeServiceRequest::ObjRedo { .. } => "obj_redo",
        }
    }

    /// The capability the client requires before sending this request. `None` = anyone may
    /// send it (`caps` is the probe that discovers the rank; below rank 2 the server hides the
    /// whole command, so the probe simply gets no reply).
    ///
    /// Deleting an object placed by someone else additionally needs
    /// [`EmeCapability::DeleteOthersObjects`]; only the server knows the owner, so the client
    /// gates `obj_delete` at [`EmeCapability::Edit`].
    pub fn required_capability(&self) -> Option<EmeCapability> {
        use EmeServiceRequest as R;
        match self {
            R::Caps => None,
            R::Strokes { .. }
            | R::Log(_)
            | R::SnapshotList { .. }
            | R::OvGet { .. }
            | R::ObjNearby { .. }
            | R::ObjGet { .. } => Some(EmeCapability::View),
            R::Undo { .. }
            | R::Redo { .. }
            | R::SnapshotCreate { .. }
            | R::OvEdit(_)
            | R::OvUndo { .. }
            | R::OvRedo { .. }
            | R::ObjSpawn(_)
            | R::ObjMove(_)
            | R::ObjDelete { .. }
            | R::ObjUndo { .. }
            | R::ObjRedo { .. } => Some(EmeCapability::Edit),
            R::Revert { .. } | R::UndoPlayer { .. } => Some(EmeCapability::RevertOthers),
        }
    }

    /// Whether the request changes server state.
    pub fn is_mutating(&self) -> bool {
        !matches!(self.required_capability(), None | Some(EmeCapability::View))
    }
}

/// Filters for the `log` op. Every field is optional; the server applies its defaults
/// (radius 60 yd, 60 minutes, like `.voxel log`).
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
    /// Only this tool / op name (`"dig"`, `"flatten"`, `"ov_edit"`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Paging: only rows with `seq` greater than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u32>,
    /// Row cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A sync cursor for one 64-yd region (CONTRACT §2): the client's epoch and last seen seq.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeRegionCursor {
    /// `floor(x / 64)`.
    pub rx: i16,
    /// `floor(y / 64)`.
    pub ry: i16,
    /// The epoch the client holds; a mismatch makes the server reset the region.
    pub epoch: u16,
    /// Only rows with a greater seq.
    pub after_seq: u32,
}

// ---------------------------------------------------------------------------------------------
// Overlays (water / area), §4
// ---------------------------------------------------------------------------------------------

/// Which overlay layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeOverlayLayer {
    /// Liquid cells (4.1667-yd ADT liquid grid).
    Liquid,
    /// Area-id cells (16-yd cells in the MVP).
    Area,
}

/// A liquid kind. `None` erases the baseline ADT liquid in the cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeLiquidKind {
    /// Erase baseline liquid.
    None,
    /// Water.
    Water,
    /// Ocean.
    Ocean,
    /// Magma.
    Magma,
    /// Slime.
    Slime,
}

/// One liquid cell edit: a flat surface at `level` over global liquid cell `(cx, cy)`,
/// `c = floor(world / 4.1667)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EmeLiquidCell {
    /// Global liquid-cell x.
    pub cx: i32,
    /// Global liquid-cell y.
    pub cy: i32,
    /// Liquid kind.
    pub kind: EmeLiquidKind,
    /// Surface height, yards.
    pub level: f32,
    /// Bits: dark / hidden / fatigue.
    #[serde(default)]
    pub flags: u8,
    /// Depth fade.
    #[serde(default)]
    pub depth_fade: u8,
}

/// One area cell edit: area id `area_id` (an existing `AreaTable.dbc` id) over area cell
/// `(cx, cy)`, `c = floor(world / 16)` in the MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeAreaCell {
    /// Area-cell x.
    pub cx: i32,
    /// Area-cell y.
    pub cy: i32,
    /// `AreaTable.dbc` id.
    pub area_id: u32,
}

/// The `ov_edit` payload: a batch of cells of one layer, belonging to one stroke.
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
// Objects, §5.3
// ---------------------------------------------------------------------------------------------

/// Game object or creature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmeObjectKind {
    /// Game object.
    Go,
    /// Creature.
    Npc,
}

fn one() -> f32 {
    1.0
}

/// `obj_spawn` arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeObjSpawn {
    /// GO or NPC.
    pub kind: EmeObjectKind,
    /// `gameobject_template` / `creature_template` entry.
    pub entry: u32,
    /// Position, WoW yards (x north, y west, z up).
    pub pos: [f32; 3],
    /// Yaw, radians.
    pub ori: f32,
    /// Pitch, radians (GOs only; NPCs ignore it).
    #[serde(default)]
    pub pitch: f32,
    /// Roll, radians (GOs only; NPCs ignore it).
    #[serde(default)]
    pub roll: f32,
    /// Uniform scale.
    #[serde(default = "one")]
    pub scale: f32,
    /// Transaction id.
    pub tx: EmeTxId,
}

/// `obj_move` arguments: the full new transform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeObjMove {
    /// Object guid.
    pub guid: u64,
    /// Position, WoW yards.
    pub pos: [f32; 3],
    /// Yaw, radians.
    pub ori: f32,
    /// Pitch, radians.
    #[serde(default)]
    pub pitch: f32,
    /// Roll, radians.
    #[serde(default)]
    pub roll: f32,
    /// Uniform scale.
    #[serde(default = "one")]
    pub scale: f32,
    /// Transaction id.
    pub tx: EmeTxId,
}

/// The authoritative object row every `obj_*` reply carries in `rows`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeObjectRow {
    /// Object guid.
    pub guid: u64,
    /// GO or NPC.
    pub kind: EmeObjectKind,
    /// Template entry.
    pub entry: u32,
    /// Map id.
    pub map: u16,
    /// Position, WoW yards.
    pub pos: [f32; 3],
    /// Yaw, radians.
    pub ori: f32,
    /// Pitch, radians.
    #[serde(default)]
    pub pitch: f32,
    /// Roll, radians.
    #[serde(default)]
    pub roll: f32,
    /// Uniform scale.
    #[serde(default = "one")]
    pub scale: f32,
    /// Display name, if the server sends one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Who placed it, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placed_by: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------------------------

/// One reply frame: `{"v":1,"id":…,"ok":…,"err"?:…,"rows"?:[…], …op-specific fields}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeServiceReply {
    /// Schema version.
    pub v: u8,
    /// The request's id.
    pub id: EmeRequestId,
    /// Whether the request was carried out.
    #[serde(deserialize_with = "lenient::bool_or_int")]
    pub ok: bool,
    /// Machine error code when `ok` is false (see [`EmeServiceError`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    /// Result rows, typed per op (read with [`EmeServiceReply::rows_as`]).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_vec_or_empty_map"
    )]
    pub rows: Option<Vec<Value>>,
    /// Op-specific top-level fields (e.g. the `caps` object, `more`, `reverted`).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl EmeServiceReply {
    /// A successful reply with no rows.
    pub fn ok(id: EmeRequestId) -> Self {
        Self {
            v: EME_SERVICE_VERSION,
            id,
            ok: true,
            err: None,
            rows: None,
            extra: Map::new(),
        }
    }

    /// A failed reply with error code `err`.
    pub fn error(id: EmeRequestId, err: impl Into<String>) -> Self {
        Self {
            v: EME_SERVICE_VERSION,
            id,
            ok: false,
            err: Some(err.into()),
            rows: None,
            extra: Map::new(),
        }
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

    /// The op-specific top-level fields decoded as `T`.
    pub fn extra_as<T: DeserializeOwned>(&self) -> Result<T, EmeJsonError> {
        serde_json::from_value(Value::Object(self.extra.clone())).map_err(EmeJsonError::from)
    }

    /// Paging flag: `more` true/1 means "ask again after the last row".
    pub fn more(&self) -> bool {
        match self.extra.get("more") {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            _ => false,
        }
    }

    /// The compact JSON text of this frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("service reply serializes")
    }
}

/// A server error code (`err` of a failed reply). Unknown codes are kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EmeServiceError {
    /// Rank too low for the op.
    Rank,
    /// Target outside the allowed range / radius.
    Range,
    /// Rate limited.
    Rate,
    /// Malformed or out-of-bounds arguments.
    Invalid,
    /// Map not allowed.
    Map,
    /// Unknown stroke / snapshot / object / tx.
    NotFound,
    /// The target belongs to someone else and the caller may not touch it.
    NotOwner,
    /// Another GM edited the area after this stroke; resend with `force` to undo anyway.
    OthersAfter,
    /// Already in the requested state (undo of an already reverted stroke, redo of a live one).
    Already,
    /// The placement policy refused the position (e.g. a fief guard).
    Denied,
    /// Server busy; retry later.
    Busy,
    /// No reply in time (client-side, never on the wire).
    Timeout,
    /// The link went down before the reply (client-side, never on the wire).
    LinkLost,
    /// Anything else.
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
            "not_found" => Self::NotFound,
            "not_owner" => Self::NotOwner,
            "others_after" => Self::OthersAfter,
            "already" => Self::Already,
            "denied" => Self::Denied,
            "busy" => Self::Busy,
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
            Self::NotFound => "not_found",
            Self::NotOwner => "not_owner",
            Self::OthersAfter => "others_after",
            Self::Already => "already",
            Self::Denied => "denied",
            Self::Busy => "busy",
            Self::Timeout => "timeout",
            Self::LinkLost => "link_lost",
            Self::Other(s) => s,
        }
    }

    /// The client locale key for this error (`MONKEY_EME_ERR_<CODE>`).
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
// Typed reply payloads
// ---------------------------------------------------------------------------------------------

/// The `caps` reply (top-level fields): what this caller may do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeCaps {
    /// The caller's GM rank.
    pub rank: EmeRank,
    /// Tool ids the server offers this caller; the toolbar shows only these.
    #[serde(default, deserialize_with = "lenient::vec_or_empty_map")]
    pub tools: Vec<String>,
    /// Server limits.
    #[serde(default)]
    pub limits: EmeLimits,
    /// Which overlay effects the server enforces (vs. "visual only").
    #[serde(default)]
    pub enforced: EmeEnforced,
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
}

/// Server limits from `caps.limits`. Absent fields mean "server default / unlimited".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmeLimits {
    /// Largest brush radius, yards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_radius: Option<f32>,
    /// Voxel EDIT uplink rate, edits per second (CONTRACT §3: 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_rate: Option<f32>,
    /// Most edits one stroke may hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke_edits: Option<u32>,
    /// Most cells one `ov_edit` may carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ov_cells: Option<u32>,
    /// Largest JSON frame, bytes (default [`EME_MAX_FRAME_BYTES`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_bytes: Option<u32>,
    /// Any limit this client version does not know.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// What the server enforces for overlays. `false` = "visual only": the edit is stored, synced
/// and logged, but server game logic (zone, swimming, damage) still reads the ADT.
///
/// On the wire either an object `{"area":1,"liquid":0,"notes":[…]}` or one flag for both.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EmeEnforced {
    /// Area-id overlay honoured by the server's area lookup.
    pub area: bool,
    /// Liquid overlay honoured by the server's liquid lookup.
    pub liquid: bool,
    /// Known mismatches (locale keys), e.g. client-read area flags.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl<'de> Deserialize<'de> for EmeEnforced {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Obj {
            #[serde(default, deserialize_with = "lenient::bool_or_int")]
            area: bool,
            #[serde(default, deserialize_with = "lenient::bool_or_int")]
            liquid: bool,
            #[serde(default, deserialize_with = "lenient::vec_or_empty_map")]
            notes: Vec<String>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Flag(bool),
            Num(f64),
            Obj(Obj),
        }
        Ok(match Wire::deserialize(d)? {
            Wire::Flag(b) => Self {
                area: b,
                liquid: b,
                notes: Vec::new(),
            },
            Wire::Num(n) => Self {
                area: n != 0.0,
                liquid: n != 0.0,
                notes: Vec::new(),
            },
            Wire::Obj(o) => Self {
                area: o.area,
                liquid: o.liquid,
                notes: o.notes,
            },
        })
    }
}

/// A `strokes` row (and the `stroke` push): one stroke's summary from `voxel_stroke`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeStrokeRow {
    /// The author's stroke id.
    pub stroke: u16,
    /// Author character guid (low).
    pub guid: u32,
    /// Author character name.
    pub name: String,
    /// Map id.
    pub map: u16,
    /// Touched region bounds (inclusive).
    pub rx_min: i16,
    /// Touched region bounds (inclusive).
    pub ry_min: i16,
    /// Touched region bounds (inclusive).
    pub rx_max: i16,
    /// Touched region bounds (inclusive).
    pub ry_max: i16,
    /// First edit seq.
    pub first_seq: u32,
    /// Last edit seq.
    pub last_seq: u32,
    /// Number of edits.
    pub edits: u32,
    /// Unix seconds.
    pub created_at: u64,
    /// Whether the stroke is currently reverted.
    #[serde(default, deserialize_with = "lenient::bool_or_int")]
    pub reverted: bool,
    /// Which layer: absent = voxel terrain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<EmeOverlayLayer>,
}

/// A `log` row: one audited attempt (the four W's, like `voxel_log`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeLogRow {
    /// Log row id (paging cursor for `log.after`).
    pub seq: u32,
    /// Unix seconds.
    pub time: u64,
    /// Who: character guid (low).
    pub guid: u32,
    /// Who: character name.
    pub name: String,
    /// Who: account id.
    #[serde(default)]
    pub account: u32,
    /// Who: GM rank at the time.
    #[serde(default)]
    pub rank: EmeRank,
    /// What: op / tool (`"dig"`, `"flatten"`, `"undo"`, `"ov_edit"`, `"obj_move"`, …).
    pub op: String,
    /// What: result (`"ok"` or an error code).
    pub result: String,
    /// What: the stroke, when the op had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<u16>,
    /// What: brush radius, when the op had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
    /// Where: map id.
    pub map: u16,
    /// Where: zone id.
    #[serde(default)]
    pub zone: u32,
    /// Where: area id.
    #[serde(default)]
    pub area: u32,
    /// Where: position.
    pub x: f32,
    /// Where: position.
    pub y: f32,
    /// Where: position.
    pub z: f32,
}

/// A `snapshot_list` / `snapshot_create` row: one restore point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeSnapshotRow {
    /// `voxel_snapshot.id`.
    pub id: u32,
    /// UTF-8 label.
    pub label: String,
    /// Creator guid (low).
    pub guid: u32,
    /// Creator name.
    pub name: String,
    /// Map id.
    pub map: u16,
    /// Centre.
    pub x: f32,
    /// Centre.
    pub y: f32,
    /// Radius, yards.
    pub radius: f32,
    /// Voxel seq at creation.
    pub at_seq: u32,
    /// Liquid overlay seq at creation.
    #[serde(default)]
    pub at_seq_liquid: u32,
    /// Area overlay seq at creation.
    #[serde(default)]
    pub at_seq_area: u32,
    /// Unix seconds.
    pub created_at: u64,
}

/// The top-level result of `revert` / `undo_player` / `undo` / `redo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeRevertResult {
    /// Rows reverted (or restored, for redo).
    #[serde(default)]
    pub reverted: u32,
    /// Regions whose epoch was bumped.
    #[serde(default)]
    pub regions: u32,
}

/// An actor row: who edited after a stroke (the `rows` of an `others_after` error).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmeActorRow {
    /// Character guid (low).
    pub guid: u32,
    /// Character name.
    pub name: String,
}

// ---------------------------------------------------------------------------------------------
// Pushes
// ---------------------------------------------------------------------------------------------

/// One overlay row pushed live (or replayed by `ov_get`): last-writer-wins per cell in seq order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmeOverlayRecord {
    /// Layer.
    pub layer: EmeOverlayLayer,
    /// Server seq (per layer, monotonic).
    pub seq: u32,
    /// Author's stroke id.
    #[serde(default)]
    pub stroke: u16,
    /// Map id.
    pub map: u16,
    /// Cell x.
    pub cx: i32,
    /// Cell y.
    pub cy: i32,
    /// Liquid kind (liquid layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<EmeLiquidKind>,
    /// Liquid level (liquid layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
    /// Liquid flags (liquid layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<u8>,
    /// Area id (area layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area_id: Option<u32>,
    /// Author character name.
    pub actor: String,
}

/// An unsolicited downlink frame `{"v":1,"ev":…, …}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum EmeServicePush {
    /// A live overlay row.
    Ov(EmeOverlayRecord),
    /// An overlay region was reverted: drop its rows and resync (like voxel `X`).
    OvReset {
        /// Layer.
        layer: EmeOverlayLayer,
        /// Map id.
        map: u16,
        /// Region x.
        rx: i16,
        /// Region y.
        ry: i16,
        /// New epoch.
        epoch: u16,
    },
    /// Overlay sync page finished for a region (like voxel `D`).
    OvDone {
        /// Layer.
        layer: EmeOverlayLayer,
        /// Map id.
        map: u16,
        /// Region x.
        rx: i16,
        /// Region y.
        ry: i16,
        /// Region epoch.
        epoch: u16,
        /// Last seq sent.
        last_seq: u32,
        /// Ask again with `after_seq = last_seq`.
        #[serde(default, deserialize_with = "lenient::bool_or_int")]
        more: bool,
    },
    /// Someone's stroke was committed nearby (live history panel).
    Stroke(EmeStrokeRow),
    /// A stroke was reverted or restored by anyone (undo, redo, snapshot revert, undo_player).
    StrokeState {
        /// Author guid (low).
        guid: u32,
        /// Author's stroke id.
        stroke: u16,
        /// Layer; absent = voxel.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layer: Option<EmeOverlayLayer>,
        /// New state.
        #[serde(deserialize_with = "lenient::bool_or_int")]
        reverted: bool,
        /// Who changed it.
        by: String,
    },
    /// The caller's caps changed (rank change, server reload).
    Caps(EmeCaps),
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

    /// The compact JSON text of this frame.
    pub fn to_json(&self) -> String {
        match self {
            Self::Reply(r) => r.to_json(),
            Self::Push(p) => {
                let mut v = serde_json::to_value(p).expect("push serializes");
                if let Value::Object(m) = &mut v {
                    m.insert("v".into(), Value::from(EME_SERVICE_VERSION));
                }
                v.to_string()
            }
        }
    }
}

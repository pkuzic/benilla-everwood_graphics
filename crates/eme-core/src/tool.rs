//! Tools and their registry (DESIGN-EME §2.2).
//!
//! A tool turns input into [`EmeCommand`]s and nothing else: it never touches the network, the
//! journal or the UI toolkit. It describes its options declaratively ([`EmeOptions`]) and its
//! in-world preview ([`EmePreview`]); the UI and viewport crates render those.

use crate::command::{EmeCommand, EmeCommandSink};
use crate::perm::EmePermissions;
use crate::service::{EmeCaps, EmeOverlayLayer};
use std::fmt;

/// A tool's stable id (`"sculpt"`, `"flatten"`, `"water"`, …). Also the suffix of its locale keys
/// (`MONKEY_EME_<ID>_*`) and the name the server lists in `caps.tools`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EmeToolId(pub &'static str);

impl EmeToolId {
    /// The id text.
    pub fn as_str(&self) -> &'static str {
        self.0
    }

    /// The locale key prefix, `MONKEY_EME_<ID>_`.
    pub fn locale_prefix(&self) -> String {
        format!("MONKEY_EME_{}_", self.0.to_ascii_uppercase())
    }
}

impl fmt::Display for EmeToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Where the cursor ray hit the world this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmeHit {
    /// Hit point, WoW yards.
    pub pos: [f32; 3],
    /// Surface normal.
    pub normal: [f32; 3],
    /// The hit object's guid, if the ray hit an object rather than ground.
    pub object: Option<u64>,
}

/// One button's state this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmeButton {
    /// Went down this frame.
    pub pressed: bool,
    /// Is down.
    pub held: bool,
    /// Went up this frame.
    pub released: bool,
}

/// Modifier keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmeModifiers {
    /// Shift.
    pub shift: bool,
    /// Ctrl.
    pub ctrl: bool,
    /// Alt.
    pub alt: bool,
}

/// The engine-free input snapshot a tool sees each frame.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeInputCtx {
    /// Host clock, ms.
    pub now_ms: u64,
    /// Current map id.
    pub map: u16,
    /// Cursor ray hit (`None` when over the sky or a UI panel).
    pub hit: Option<EmeHit>,
    /// Primary (left) button.
    pub primary: EmeButton,
    /// Secondary (right) button.
    pub secondary: EmeButton,
    /// Modifiers.
    pub modifiers: EmeModifiers,
    /// Mouse wheel steps this frame.
    pub wheel: f32,
    /// The pointer is over an EME panel (the viewport must not act).
    pub pointer_over_ui: bool,
    /// The caller's permissions.
    pub perms: EmePermissions,
}

impl EmeInputCtx {
    /// An idle frame at `now_ms` on `map`.
    pub fn idle(now_ms: u64, map: u16, perms: EmePermissions) -> Self {
        Self {
            now_ms,
            map,
            hit: None,
            primary: EmeButton::default(),
            secondary: EmeButton::default(),
            modifiers: EmeModifiers::default(),
            wheel: 0.0,
            pointer_over_ui: false,
            perms,
        }
    }
}

/// A value of a tool option.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeOptionValue {
    /// Numeric.
    Number(f32),
    /// On/off.
    Bool(bool),
    /// Index into a choice list.
    Choice(usize),
    /// Text.
    Text(String),
    /// A button was clicked.
    Click,
}

/// The kind of control an option renders as.
#[derive(Debug, Clone, PartialEq)]
pub enum EmeOptionKind {
    /// Slider over `[min, max]` with `step`.
    Slider {
        /// Minimum.
        min: f32,
        /// Maximum.
        max: f32,
        /// Step.
        step: f32,
    },
    /// Numeric entry.
    Number,
    /// Checkbox.
    Toggle,
    /// One of several labels (locale keys).
    Choice(Vec<String>),
    /// Text entry.
    Text,
    /// A push button.
    Button,
}

/// One declarative option field.
#[derive(Debug, Clone, PartialEq)]
pub struct EmeOptionField {
    /// Field key, unique within the tool (passed back to [`EmeTool::set_option`]).
    pub key: &'static str,
    /// Locale key of the label (`MONKEY_EME_<TOOL>_<KEY>`).
    pub label: String,
    /// Control kind.
    pub kind: EmeOptionKind,
    /// Current value.
    pub value: EmeOptionValue,
    /// Disabled (shown greyed), e.g. a mutating control for a read-only rank.
    pub disabled: bool,
}

/// A tool's option panel model.
pub type EmeOptions = Vec<EmeOptionField>;

/// What a tool wants drawn in the world.
#[derive(Debug, Clone, PartialEq)]
pub enum EmePreview {
    /// Brush ring: inner radius = full strength, outer = falloff edge.
    Ring {
        /// Centre.
        center: [f32; 3],
        /// Radius, yards.
        radius: f32,
        /// Falloff ring as a fraction of the radius.
        falloff: f32,
    },
    /// A horizontal plane disc (flatten / water level).
    Plane {
        /// Centre (z = plane height).
        center: [f32; 3],
        /// Radius, yards.
        radius: f32,
    },
    /// A closed outline (restore-point radius, selection, polygon).
    Outline {
        /// Points, WoW yards.
        points: Vec<[f32; 3]>,
    },
    /// Overlay cells to tint.
    Cells {
        /// Layer.
        layer: EmeOverlayLayer,
        /// Cell coordinates.
        cells: Vec<(i32, i32)>,
        /// Tint level (water) or `None`.
        level: Option<f32>,
    },
}

/// One EME tool. Implement this in one file and register it; tools never reference each other.
pub trait EmeTool: Send + Sync + 'static {
    /// Stable id.
    fn id(&self) -> EmeToolId;
    /// Lowest rank that may use the tool: 3 for every mutating tool, 2 for read-only ones.
    fn min_rank(&self) -> u8;
    /// Handle one frame of input; emit commands into `out`.
    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink);
    /// The option panel model.
    fn options_ui_model(&self) -> EmeOptions;
    /// The user changed option `key`.
    fn set_option(&mut self, _key: &str, _value: EmeOptionValue) {}
    /// What to draw in the world this frame.
    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview>;
    /// The tool became active.
    fn on_activate(&mut self) {}
    /// The tool stopped being active (end any held stroke here).
    fn on_deactivate(&mut self, _out: &mut EmeCommandSink) {}
}

/// Why a registry call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmeRegistryError {
    /// Another tool already has this id.
    DuplicateId(EmeToolId),
    /// No tool with this id.
    Unknown,
    /// The caller's rank (or the server's tool list) does not allow the tool.
    NotPermitted,
}

impl fmt::Display for EmeRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(f, "duplicate EME tool id {id}"),
            Self::Unknown => f.write_str("unknown EME tool"),
            Self::NotPermitted => f.write_str("EME tool not permitted"),
        }
    }
}

impl std::error::Error for EmeRegistryError {}

/// Every registered tool, in registration order (= toolbar order), and the active one.
#[derive(Default)]
pub struct EmeToolRegistry {
    tools: Vec<Box<dyn EmeTool>>,
    active: Option<usize>,
}

impl fmt::Debug for EmeToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmeToolRegistry")
            .field(
                "tools",
                &self.tools.iter().map(|t| t.id()).collect::<Vec<_>>(),
            )
            .field("active", &self.active.map(|i| self.tools[i].id()))
            .finish()
    }
}

impl EmeToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool. Ids are unique.
    pub fn register(&mut self, tool: Box<dyn EmeTool>) -> Result<(), EmeRegistryError> {
        let id = tool.id();
        if self.tools.iter().any(|t| t.id() == id) {
            return Err(EmeRegistryError::DuplicateId(id));
        }
        self.tools.push(tool);
        Ok(())
    }

    /// Number of tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tool is registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Every tool id, in toolbar order.
    pub fn ids(&self) -> Vec<EmeToolId> {
        self.tools.iter().map(|t| t.id()).collect()
    }

    /// A tool by id.
    pub fn get(&self, id: &str) -> Option<&dyn EmeTool> {
        self.tools
            .iter()
            .find(|t| t.id().as_str() == id)
            .map(|t| t.as_ref())
    }

    /// A tool by id, mutably.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut (dyn EmeTool + 'static)> {
        self.tools
            .iter_mut()
            .find(|t| t.id().as_str() == id)
            .map(|t| t.as_mut())
    }

    /// Whether `tool` is usable: rank high enough and, once caps are known, listed by the server.
    pub fn is_permitted(
        tool: &dyn EmeTool,
        perms: &EmePermissions,
        caps: Option<&EmeCaps>,
    ) -> bool {
        perms.allows_rank(tool.min_rank()) && caps.is_none_or(|c| c.offers_tool(tool.id().as_str()))
    }

    /// The toolbar: usable tools in registration order.
    pub fn visible(&self, perms: &EmePermissions, caps: Option<&EmeCaps>) -> Vec<EmeToolId> {
        self.tools
            .iter()
            .filter(|t| Self::is_permitted(t.as_ref(), perms, caps))
            .map(|t| t.id())
            .collect()
    }

    /// The active tool's id.
    pub fn active_id(&self) -> Option<EmeToolId> {
        self.active.map(|i| self.tools[i].id())
    }

    /// The active tool, mutably.
    pub fn active_mut(&mut self) -> Option<&mut (dyn EmeTool + 'static)> {
        let i = self.active?;
        Some(self.tools[i].as_mut())
    }

    /// Switch tools. The old tool's `on_deactivate` may emit commands (e.g. end a stroke).
    pub fn activate(
        &mut self,
        id: &str,
        perms: &EmePermissions,
        caps: Option<&EmeCaps>,
        out: &mut EmeCommandSink,
    ) -> Result<(), EmeRegistryError> {
        let i = self
            .tools
            .iter()
            .position(|t| t.id().as_str() == id)
            .ok_or(EmeRegistryError::Unknown)?;
        if !Self::is_permitted(self.tools[i].as_ref(), perms, caps) {
            return Err(EmeRegistryError::NotPermitted);
        }
        if self.active == Some(i) {
            return Ok(());
        }
        self.deactivate(out);
        self.active = Some(i);
        self.tools[i].on_activate();
        Ok(())
    }

    /// No active tool.
    pub fn deactivate(&mut self, out: &mut EmeCommandSink) {
        if let Some(old) = self.active.take() {
            self.tools[old].on_deactivate(out);
        }
    }

    /// Feed a frame of input to the active tool; returns false when no tool is active or the
    /// caller may not use it (rank dropped since activation).
    pub fn feed(
        &mut self,
        ctx: &EmeInputCtx,
        caps: Option<&EmeCaps>,
        out: &mut EmeCommandSink,
    ) -> bool {
        let Some(i) = self.active else { return false };
        if !Self::is_permitted(self.tools[i].as_ref(), &ctx.perms, caps) {
            self.deactivate(out);
            return false;
        }
        let before = out.len();
        self.tools[i].on_input(ctx, out);
        let id = self.tools[i].id();
        out.stamp_origin(before, id);
        true
    }

    /// Emit a raw command on behalf of tool `id` (e.g. a panel button).
    pub fn emit(&self, id: EmeToolId, command: EmeCommand, out: &mut EmeCommandSink) {
        out.push_from(id, command);
    }
}

//! `flatten` (T3): level the ground toward a plane. The plane is sampled at the press (the
//! clicked height plus the `offset` option) and every edit of the stroke carries it, so dragging
//! across a slope cuts and fills it to ONE height. Options: `radius`, `strength`, `offset`.
//!
//! It is the sculpt brush with its mode locked to flatten (same stroke builder, same one-Ctrl+Z
//! stroke), registered as its own tool because the server offers it under its own name
//! (`caps.tools`) and a GM thinks of it as one tool.

use eme_core::command::EmeCommandSink;
use eme_core::stroke::EmeStrokeIds;
use eme_core::tool::{EmeInputCtx, EmeOptionValue, EmeOptions, EmePreview, EmeTool, EmeToolId};

use super::sculpt::EmeSculptTool;

/// The tool id (also the server's name for it in `caps.tools`).
pub const FLATTEN: EmeToolId = EmeToolId("flatten");

/// The flatten tool.
#[derive(Debug)]
pub struct EmeFlattenTool(EmeSculptTool);

impl EmeFlattenTool {
    /// A flatten tool numbering its strokes from the shared `ids` counter.
    pub fn new(ids: EmeStrokeIds) -> Self {
        Self(EmeSculptTool::new_flatten(ids, FLATTEN))
    }

    /// The stroke being held, if any.
    pub fn active_stroke(&self) -> Option<u16> {
        self.0.active_stroke()
    }
}

impl EmeTool for EmeFlattenTool {
    fn id(&self) -> EmeToolId {
        FLATTEN
    }
    fn min_rank(&self) -> u8 {
        3
    }
    fn on_input(&mut self, ctx: &EmeInputCtx, out: &mut EmeCommandSink) {
        self.0.on_input(ctx, out);
    }
    fn options_ui_model(&self) -> EmeOptions {
        self.0.options_ui_model()
    }
    fn set_option(&mut self, key: &str, value: EmeOptionValue) {
        self.0.set_option(key, value);
    }
    fn preview(&self, ctx: &EmeInputCtx) -> Option<EmePreview> {
        self.0.preview(ctx)
    }
    fn on_activate(&mut self) {
        self.0.on_activate();
    }
    fn on_deactivate(&mut self, out: &mut EmeCommandSink) {
        self.0.on_deactivate(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eme_core::command::EmeCommand;
    use eme_core::perm::{EmePermissions, EmeRank};
    use eme_core::tool::{EmeButton, EmeHit};
    use eme_core::transport::mode;

    #[test]
    fn flatten_is_its_own_tool_with_no_mode_option() {
        let t = EmeFlattenTool::new(EmeStrokeIds::default());
        assert_eq!(t.id().as_str(), "flatten");
        let keys: Vec<_> = t.options_ui_model().iter().map(|f| f.key).collect();
        assert_eq!(keys, ["radius", "strength", "offset"]);
    }

    #[test]
    fn flatten_strokes_carry_the_plane_and_mode_three() {
        let mut t = EmeFlattenTool::new(EmeStrokeIds::default());
        t.set_option("mode", EmeOptionValue::Choice(0)); // locked: ignored
        t.set_option("offset", EmeOptionValue::Number(1.0));
        let mut sink = EmeCommandSink::new();
        let mut ctx = EmeInputCtx::idle(0, 0, EmePermissions::from_rank(EmeRank::EDIT));
        ctx.hit = Some(EmeHit {
            pos: [0.0, 0.0, 10.0],
            normal: [0.0, 0.0, 1.0],
            object: None,
        });
        ctx.primary = EmeButton {
            pressed: true,
            held: true,
            released: false,
        };
        t.on_input(&ctx, &mut sink);
        let op = sink
            .drain()
            .into_iter()
            .find_map(|i| match i.command {
                EmeCommand::VoxelStroke { ops, .. } => ops.first().copied(),
                _ => None,
            })
            .expect("a flatten edit");
        assert_eq!(op.mode, mode::FLATTEN);
        assert_eq!(op.param as i16, 100); // plane 11 over z 10
    }
}

//! `EmeTheme`: dark, high contrast. State is never colour alone: every coloured mark also has a
//! glyph and a word (see [`crate::shell`]).

use bevy::prelude::Resource;
use bevy_egui::egui::{self, Color32, CornerRadius, Stroke};

/// The shell's palette and metrics.
#[derive(Resource, Debug, Clone)]
pub struct EmeTheme {
    /// Panel background.
    pub panel: Color32,
    /// Raised widget background.
    pub widget: Color32,
    /// Hovered widget.
    pub widget_hover: Color32,
    /// Active (selected) widget.
    pub accent: Color32,
    /// Body text.
    pub text: Color32,
    /// Dim text (still readable on `panel`).
    pub text_dim: Color32,
    /// Warning banner / needs-confirm.
    pub warn: Color32,
    /// Error.
    pub error: Color32,
    /// OK.
    pub ok: Color32,
}

impl Default for EmeTheme {
    fn default() -> Self {
        Self {
            panel: Color32::from_rgb(18, 18, 22),
            widget: Color32::from_rgb(44, 44, 54),
            widget_hover: Color32::from_rgb(66, 66, 82),
            accent: Color32::from_rgb(255, 200, 64),
            text: Color32::from_rgb(244, 244, 248),
            text_dim: Color32::from_rgb(178, 178, 190),
            warn: Color32::from_rgb(255, 176, 32),
            error: Color32::from_rgb(255, 96, 96),
            ok: Color32::from_rgb(108, 224, 128),
        }
    }
}

impl EmeTheme {
    /// Applies the theme to an egui context.
    pub fn apply(&self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        let mut v = egui::Visuals::dark();
        v.panel_fill = self.panel;
        v.window_fill = self.panel;
        v.extreme_bg_color = Color32::from_rgb(8, 8, 10);
        v.override_text_color = Some(self.text);
        v.selection.bg_fill = self.accent.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.5_f32, self.accent);
        let r = CornerRadius::same(3);
        for w in [
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.noninteractive,
        ] {
            w.corner_radius = r;
        }
        v.widgets.inactive.bg_fill = self.widget;
        v.widgets.inactive.weak_bg_fill = self.widget;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, self.text_dim);
        v.widgets.hovered.bg_fill = self.widget_hover;
        v.widgets.hovered.weak_bg_fill = self.widget_hover;
        v.widgets.hovered.bg_stroke = Stroke::new(1.5_f32, self.accent);
        v.widgets.active.bg_stroke = Stroke::new(2.0_f32, self.accent);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(70, 70, 84));
        style.visuals = v;
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        ctx.set_style(style);
    }
}

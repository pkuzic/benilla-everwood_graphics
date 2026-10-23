//! Built-in tools. Each tool is one file implementing [`eme_core::EmeTool`], registered with
//! [`crate::EmeAppExt::register_eme_tool`]; tools never reference each other.

pub mod area;
pub mod flatten;
pub mod sculpt;
pub mod water;

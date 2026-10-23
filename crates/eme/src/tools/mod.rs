//! Built-in tools. Each tool is one file implementing [`eme_core::EmeTool`], registered with
//! [`crate::EmeAppExt::register_eme_tool`]; tools never reference each other.

pub mod sculpt;

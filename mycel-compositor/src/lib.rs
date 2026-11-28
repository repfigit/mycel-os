//! Mycel OS standalone thin Wayland compositor built on Smithay 0.7.

pub mod compositor;
#[cfg(feature = "drm")]
pub mod drm_backend;
pub mod shell_proc;
pub mod state;

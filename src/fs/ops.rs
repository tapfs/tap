//! FUSE operation handlers -- thin conversion layer.
//!
//! All filesystem logic now lives in `crate::vfs::core::VirtualFs`.
//! This module is retained for backward compatibility but delegates
//! everything to VirtualFs.

pub use crate::vfs::core::AGENT_MD_CONTENT;

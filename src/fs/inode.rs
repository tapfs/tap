//! Inode table.
//!
//! Re-exports from `vfs` for backward compatibility.
//! New code should use `NodeTable` and `NodeKind` from `crate::vfs` directly.

pub use crate::vfs::core::NodeTable;
pub use crate::vfs::types::NodeKind;
pub use crate::vfs::types::ResourceVariant;

/// Type alias for backward compatibility.
pub type InodeTable = NodeTable;

/// Type alias for backward compatibility.
pub type InodeEntry = NodeKind;

/// Type alias for backward compatibility.
pub type PathVariant = ResourceVariant;

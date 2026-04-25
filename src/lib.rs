//! tapfs — mount enterprise REST APIs as agent-readable files.
//!
//! tapfs is a FUSE/NFS filesystem that exposes enterprise APIs (Salesforce,
//! Google Workspace, Jira, etc.) as plain files. Agents interact with APIs
//! using standard filesystem operations: `ls`, `cat`, `grep`, and `echo`.
//!
//! # Architecture
//!
//! - [`vfs`] — platform-agnostic virtual filesystem core
//! - [`connector`] — pluggable API connectors (REST, Google, Jira, Confluence)
//! - [`draft`] — sandboxed copy-on-write drafts
//! - [`version`] — immutable resource snapshots
//! - [`governance`] — audit logging and approval gates
//! - [`cache`] — TTL-based response cache
//! - [`ffi`] — C FFI bridge for non-Rust consumers

pub mod cache;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod cli;
pub mod config;
pub mod connector;
pub mod credentials;
pub mod draft;
pub mod ffi;
#[cfg(feature = "fuse")]
pub mod fs;
pub mod governance;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod ipc;
#[cfg(feature = "nfs")]
pub mod nfs;
pub mod path;
pub mod version;
pub mod vfs;

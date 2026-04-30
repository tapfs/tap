//! C-compatible FFI bridge for the tapfs virtual filesystem.
//!
//! This module exposes VirtualFs operations through `extern "C"` functions that
//! Swift (and other C-ABI callers) can invoke.  A global [`TapFsHandle`] holds
//! both the VirtualFs instance and a tokio runtime so that async connector
//! operations can be driven from synchronous FFI calls.
//!
//! # Memory ownership
//!
//! Heap-allocated return values ([`FfiDirList`], [`FfiData`]) must be freed by
//! the caller via the corresponding `tapfs_free_*` function.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::cache::store::Cache;
use crate::connector::registry::ConnectorRegistry;
use crate::connector::rest::RestConnector;
use crate::connector::spec::ConnectorSpec;
use crate::draft::store::DraftStore;
use crate::governance::audit::AuditLogger;
use crate::governance::interceptor::AuditedConnector;
use crate::version::store::VersionStore;
use crate::vfs::core::VirtualFs;
use crate::vfs::types::*;

// ---------------------------------------------------------------------------
// C-compatible types
// ---------------------------------------------------------------------------

/// Opaque handle that Swift holds.  Contains the VirtualFs and a tokio runtime.
pub struct TapFsHandle {
    vfs: Arc<VirtualFs>,
    rt: tokio::runtime::Runtime,
}

/// File/directory attributes returned across the FFI boundary.
#[repr(C)]
pub struct FfiAttr {
    /// Node ID (inode).  0 signals an error / not-found.
    pub id: u64,
    /// File size in bytes.
    pub size: u64,
    /// 0 = directory, 1 = regular file.
    pub file_type: u8,
    /// POSIX permission bits (e.g. 0o755, 0o644).
    pub perm: u16,
}

/// A single directory entry returned across the FFI boundary.
#[repr(C)]
pub struct FfiDirEntry {
    /// Heap-allocated, NUL-terminated name.  Freed as part of [`FfiDirList`].
    pub name: *mut c_char,
    /// Node ID.
    pub id: u64,
    /// 0 = directory, 1 = regular file.
    pub file_type: u8,
}

/// A list of directory entries.
#[repr(C)]
pub struct FfiDirList {
    /// Pointer to a heap-allocated array of [`FfiDirEntry`].
    pub entries: *mut FfiDirEntry,
    /// Number of entries in the array.
    pub count: u32,
}

/// A buffer of bytes returned across the FFI boundary.
#[repr(C)]
pub struct FfiData {
    /// Pointer to a heap-allocated byte buffer.
    pub ptr: *mut u8,
    /// Length of the buffer in bytes.
    pub len: u32,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a `VfsFileType` to the FFI u8 encoding.
fn vfs_file_type_to_u8(ft: VfsFileType) -> u8 {
    match ft {
        VfsFileType::Directory => 0,
        VfsFileType::RegularFile => 1,
    }
}

/// Build an [`FfiAttr`] from a [`VfsAttr`].
fn vfs_attr_to_ffi(attr: &VfsAttr) -> FfiAttr {
    FfiAttr {
        id: attr.id,
        size: attr.size,
        file_type: vfs_file_type_to_u8(attr.file_type),
        perm: attr.perm,
    }
}

/// Return an [`FfiAttr`] with `id = 0`, indicating an error.
fn ffi_attr_error() -> FfiAttr {
    FfiAttr {
        id: 0,
        size: 0,
        file_type: 0,
        perm: 0,
    }
}

/// Safely convert a C string pointer to a `&str`.  Returns `None` on null
/// pointer or invalid UTF-8.
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

// ---------------------------------------------------------------------------
// FFI functions
// ---------------------------------------------------------------------------

/// Initialize tapfs from a YAML spec string and a data directory path.
///
/// Returns an opaque [`TapFsHandle`] pointer that must be passed to every
/// subsequent FFI call, and eventually freed with [`tapfs_free`].
///
/// Returns a null pointer on failure.
///
/// # Safety
///
/// `spec_yaml` and `data_dir` must be valid, null-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn tapfs_init(
    spec_yaml: *const c_char,
    data_dir: *const c_char,
) -> *mut TapFsHandle {
    let yaml = match cstr_to_str(spec_yaml) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };

    let data_dir = match cstr_to_str(data_dir) {
        Some(s) => PathBuf::from(s),
        None => return std::ptr::null_mut(),
    };

    // Parse the connector spec.
    let spec = match ConnectorSpec::from_yaml(yaml) {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };

    // Create the tokio runtime used by this handle.
    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(_) => return std::ptr::null_mut(),
    };

    // Ensure data directories exist.
    let drafts_dir = data_dir.join("drafts");
    let versions_dir = data_dir.join("versions");
    let audit_log = data_dir.join("audit.log");
    let _ = std::fs::create_dir_all(&data_dir);
    let _ = std::fs::create_dir_all(&drafts_dir);
    let _ = std::fs::create_dir_all(&versions_dir);

    // Build subsystems.
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(10)
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(30))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default();
    let rest = RestConnector::new(spec, client);

    let audit = match AuditLogger::new(audit_log) {
        Ok(a) => Arc::new(a),
        Err(_) => return std::ptr::null_mut(),
    };

    let inner: Arc<dyn crate::connector::traits::Connector> = Arc::new(rest);
    let audited: Arc<dyn crate::connector::traits::Connector> =
        Arc::new(AuditedConnector::new(inner, audit.clone()));

    let registry = ConnectorRegistry::new();
    registry.register(audited);
    let registry = Arc::new(registry);

    let cache = Arc::new(Cache::new(Duration::from_secs(60)));

    let drafts = match DraftStore::new(drafts_dir) {
        Ok(d) => Arc::new(d),
        Err(_) => return std::ptr::null_mut(),
    };

    let versions = match VersionStore::new(versions_dir) {
        Ok(v) => Arc::new(v),
        Err(_) => return std::ptr::null_mut(),
    };

    let vfs = Arc::new(VirtualFs::new(registry, cache, drafts, versions, audit));

    let handle = Box::new(TapFsHandle { vfs, rt });
    Box::into_raw(handle)
}

/// Free a [`TapFsHandle`] previously returned by [`tapfs_init`].
///
/// After this call the pointer is invalid and must not be reused.
///
/// # Safety
///
/// `handle` must be a pointer returned by [`tapfs_init`], or null.
/// Must not be called more than once for the same handle.
#[no_mangle]
pub unsafe extern "C" fn tapfs_free(handle: *mut TapFsHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// Look up a child node by parent ID and name.
///
/// Returns an [`FfiAttr`] with `id = 0` on error or not-found.
///
/// # Safety
///
/// `handle` must be a valid pointer from [`tapfs_init`].
/// `name` must be a valid, null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn tapfs_lookup(
    handle: *const TapFsHandle,
    parent_id: u64,
    name: *const c_char,
) -> FfiAttr {
    let handle = match handle.as_ref() {
        Some(h) => h,
        None => return ffi_attr_error(),
    };
    let name = match cstr_to_str(name) {
        Some(n) => n,
        None => return ffi_attr_error(),
    };

    match handle.vfs.lookup(handle.rt.handle(), parent_id, name) {
        Ok(attr) => vfs_attr_to_ffi(&attr),
        Err(_) => ffi_attr_error(),
    }
}

/// Get attributes of a node by its ID.
///
/// Returns an [`FfiAttr`] with `id = 0` on error.
///
/// # Safety
///
/// `handle` must be a valid pointer from [`tapfs_init`].
#[no_mangle]
pub unsafe extern "C" fn tapfs_getattr(handle: *const TapFsHandle, id: u64) -> FfiAttr {
    let handle = match handle.as_ref() {
        Some(h) => h,
        None => return ffi_attr_error(),
    };

    match handle.vfs.getattr(id) {
        Ok(attr) => vfs_attr_to_ffi(&attr),
        Err(_) => ffi_attr_error(),
    }
}

/// Read directory contents.
///
/// Returns an [`FfiDirList`].  The caller **must** free the returned value
/// with [`tapfs_free_dir_list`].  On error, `count` is 0 and `entries` is
/// null.
///
/// # Safety
///
/// `handle` must be a valid pointer from [`tapfs_init`].
#[no_mangle]
pub unsafe extern "C" fn tapfs_readdir(handle: *const TapFsHandle, id: u64) -> FfiDirList {
    let handle = match handle.as_ref() {
        Some(h) => h,
        None => {
            return FfiDirList {
                entries: std::ptr::null_mut(),
                count: 0,
            }
        }
    };

    let entries = match handle.vfs.readdir(handle.rt.handle(), id) {
        Ok(e) => e,
        Err(_) => {
            return FfiDirList {
                entries: std::ptr::null_mut(),
                count: 0,
            }
        }
    };

    let count = entries.len() as u32;
    let mut ffi_entries: Vec<FfiDirEntry> = entries
        .into_iter()
        .map(|e| {
            let name = CString::new(e.name).unwrap_or_default();
            FfiDirEntry {
                name: name.into_raw(),
                id: e.id,
                file_type: vfs_file_type_to_u8(e.file_type),
            }
        })
        .collect();

    let ptr = ffi_entries.as_mut_ptr();
    std::mem::forget(ffi_entries);

    FfiDirList {
        entries: ptr,
        count,
    }
}

/// Free an [`FfiDirList`] previously returned by [`tapfs_readdir`].
///
/// # Safety
///
/// `list` must be a value returned by [`tapfs_readdir`].
/// Must not be called more than once for the same list.
#[no_mangle]
pub unsafe extern "C" fn tapfs_free_dir_list(list: FfiDirList) {
    if list.entries.is_null() || list.count == 0 {
        return;
    }

    let entries = Vec::from_raw_parts(list.entries, list.count as usize, list.count as usize);
    for entry in entries {
        if !entry.name.is_null() {
            drop(CString::from_raw(entry.name));
        }
    }
}

/// Read file content at the given offset.
///
/// Returns an [`FfiData`].  The caller **must** free it with
/// [`tapfs_free_data`].  On error, `len` is 0 and `ptr` is null.
///
/// # Safety
///
/// `handle` must be a valid pointer from [`tapfs_init`].
#[no_mangle]
pub unsafe extern "C" fn tapfs_read(
    handle: *const TapFsHandle,
    id: u64,
    offset: u64,
    size: u32,
) -> FfiData {
    let handle = match handle.as_ref() {
        Some(h) => h,
        None => {
            return FfiData {
                ptr: std::ptr::null_mut(),
                len: 0,
            }
        }
    };

    let data = match handle.vfs.read(handle.rt.handle(), id, offset, size) {
        Ok(d) => d,
        Err(_) => {
            return FfiData {
                ptr: std::ptr::null_mut(),
                len: 0,
            }
        }
    };

    let len = data.len() as u32;
    let mut boxed = data.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);

    FfiData { ptr, len }
}

/// Free an [`FfiData`] buffer previously returned by [`tapfs_read`].
///
/// # Safety
///
/// `data` must be a value returned by [`tapfs_read`].
/// Must not be called more than once for the same data.
#[no_mangle]
pub unsafe extern "C" fn tapfs_free_data(data: FfiData) {
    if !data.ptr.is_null() && data.len > 0 {
        drop(Vec::from_raw_parts(
            data.ptr,
            data.len as usize,
            data.len as usize,
        ));
    }
}

/// Write data to a file at the given offset.
///
/// Returns the number of bytes written, or -1 on error.
///
/// # Safety
///
/// `handle` must be a valid pointer from [`tapfs_init`].
/// `data` must point to at least `len` readable bytes, or be null if `len` is 0.
#[no_mangle]
pub unsafe extern "C" fn tapfs_write(
    handle: *const TapFsHandle,
    id: u64,
    offset: u64,
    data: *const u8,
    len: u32,
) -> i64 {
    let handle = match handle.as_ref() {
        Some(h) => h,
        None => return -1,
    };

    if data.is_null() && len > 0 {
        return -1;
    }

    let slice = if len > 0 {
        std::slice::from_raw_parts(data, len as usize)
    } else {
        &[]
    };

    match handle.vfs.write(id, offset, slice) {
        Ok(written) => written as i64,
        Err(_) => -1,
    }
}

/// Create a file (draft or lock) under the given parent directory.
///
/// Returns an [`FfiAttr`] for the newly created node, or an [`FfiAttr`] with
/// `id = 0` on error.
///
/// # Safety
///
/// `handle` must be a valid pointer from [`tapfs_init`].
/// `name` must be a valid, null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn tapfs_create(
    handle: *const TapFsHandle,
    parent_id: u64,
    name: *const c_char,
) -> FfiAttr {
    let handle = match handle.as_ref() {
        Some(h) => h,
        None => return ffi_attr_error(),
    };
    let name = match cstr_to_str(name) {
        Some(n) => n,
        None => return ffi_attr_error(),
    };

    match handle.vfs.create(parent_id, name) {
        Ok(attr) => vfs_attr_to_ffi(&attr),
        Err(_) => ffi_attr_error(),
    }
}

/// Rename a file (e.g. promote a draft to live).
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `handle` must be a valid pointer from [`tapfs_init`].
/// `old_name` and `new_name` must be valid, null-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn tapfs_rename(
    handle: *const TapFsHandle,
    parent_id: u64,
    old_name: *const c_char,
    new_parent_id: u64,
    new_name: *const c_char,
) -> i32 {
    let handle = match handle.as_ref() {
        Some(h) => h,
        None => return -1,
    };
    let old_name = match cstr_to_str(old_name) {
        Some(n) => n,
        None => return -1,
    };
    let new_name = match cstr_to_str(new_name) {
        Some(n) => n,
        None => return -1,
    };

    match handle.vfs.rename(
        handle.rt.handle(),
        parent_id,
        old_name,
        new_parent_id,
        new_name,
    ) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Delete a file (draft or lock).
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `handle` must be a valid pointer from [`tapfs_init`].
/// `name` must be a valid, null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn tapfs_unlink(
    handle: *const TapFsHandle,
    parent_id: u64,
    name: *const c_char,
) -> i32 {
    let handle = match handle.as_ref() {
        Some(h) => h,
        None => return -1,
    };
    let name = match cstr_to_str(name) {
        Some(n) => n,
        None => return -1,
    };

    match handle.vfs.unlink(handle.rt.handle(), parent_id, name) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

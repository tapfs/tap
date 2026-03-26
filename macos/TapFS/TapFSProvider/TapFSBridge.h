//
//  TapFSBridge.h
//  Bridging header for the tapfs Rust FFI.
//
//  Declares all C-compatible functions and types exported by the tapfs cdylib
//  (libtapfs.dylib).  Swift files in the TapFSProvider target import this
//  header automatically via the bridging-header build setting.
//

#ifndef TapFSBridge_h
#define TapFSBridge_h

#include <stdint.h>

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Opaque handle to the Rust VirtualFs + tokio runtime.
typedef struct TapFsHandle TapFsHandle;

/// File / directory attributes.
typedef struct {
    uint64_t id;         // Node ID (inode). 0 means error / not-found.
    uint64_t size;       // File size in bytes.
    uint8_t  file_type;  // 0 = directory, 1 = regular file.
    uint16_t perm;       // POSIX permission bits (e.g. 0755).
} FfiAttr;

/// A single directory entry.
typedef struct {
    char    *name;       // Heap-allocated, NUL-terminated name.
    uint64_t id;         // Node ID.
    uint8_t  file_type;  // 0 = directory, 1 = regular file.
} FfiDirEntry;

/// A list of directory entries.
typedef struct {
    FfiDirEntry *entries; // Heap-allocated array.
    uint32_t     count;   // Number of entries.
} FfiDirList;

/// A byte buffer returned from Rust.
typedef struct {
    uint8_t *ptr;  // Heap-allocated byte buffer.
    uint32_t len;  // Length in bytes.
} FfiData;

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Initialize tapfs from a YAML spec string and a data-directory path.
/// Returns an opaque handle (NULL on failure).
TapFsHandle *tapfs_init(const char *spec_yaml, const char *data_dir);

/// Free a handle previously returned by tapfs_init.
void tapfs_free(TapFsHandle *handle);

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// Look up a child by parent ID and name.
FfiAttr tapfs_lookup(const TapFsHandle *handle, uint64_t parent_id, const char *name);

/// Get attributes of a node by its ID.
FfiAttr tapfs_getattr(const TapFsHandle *handle, uint64_t id);

// ---------------------------------------------------------------------------
// Directory listing
// ---------------------------------------------------------------------------

/// Read directory contents.  Caller must free with tapfs_free_dir_list.
FfiDirList tapfs_readdir(const TapFsHandle *handle, uint64_t id);

/// Free an FfiDirList previously returned by tapfs_readdir.
void tapfs_free_dir_list(FfiDirList list);

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Read file content.  Caller must free with tapfs_free_data.
FfiData tapfs_read(const TapFsHandle *handle, uint64_t id, uint64_t offset, uint32_t size);

/// Free an FfiData buffer previously returned by tapfs_read.
void tapfs_free_data(FfiData data);

/// Write data to a file.  Returns bytes written or -1 on error.
int64_t tapfs_write(const TapFsHandle *handle, uint64_t id, uint64_t offset,
                    const uint8_t *data, uint32_t len);

// ---------------------------------------------------------------------------
// Mutation
// ---------------------------------------------------------------------------

/// Create a file (draft/lock) under the given parent.
FfiAttr tapfs_create(const TapFsHandle *handle, uint64_t parent_id, const char *name);

/// Rename a file.  Returns 0 on success, -1 on error.
int32_t tapfs_rename(const TapFsHandle *handle, uint64_t parent_id, const char *old_name,
                     uint64_t new_parent_id, const char *new_name);

/// Delete a file (draft/lock).  Returns 0 on success, -1 on error.
int32_t tapfs_unlink(const TapFsHandle *handle, uint64_t parent_id, const char *name);

#endif /* TapFSBridge_h */

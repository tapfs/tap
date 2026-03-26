//! FUSE adapter for the platform-agnostic VirtualFs.
//!
//! `TapFs` implements the `fuser::Filesystem` trait, delegating all logic to
//! [`VirtualFs`] and converting between VFS types and fuser types.

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyWrite, ReplyXattr, Request,
};

use crate::vfs::core::VirtualFs;
use crate::vfs::types::*;

/// Default TTL for FUSE attribute caches.
const TTL: Duration = Duration::from_secs(1);

/// The tapfs FUSE filesystem.
///
/// This is a thin adapter that wraps [`VirtualFs`] and converts between
/// VFS types and fuser types. All filesystem logic lives in VirtualFs.
pub struct TapFs {
    pub vfs: Arc<VirtualFs>,
    pub rt: tokio::runtime::Handle,
}

// ---------------------------------------------------------------------------
// Type conversion helpers
// ---------------------------------------------------------------------------

/// Convert a [`VfsAttr`] to a [`fuser::FileAttr`].
fn to_fuse_attr(attr: &VfsAttr) -> fuser::FileAttr {
    let now = SystemTime::now();
    let kind = match attr.file_type {
        VfsFileType::Directory => FileType::Directory,
        VfsFileType::RegularFile => FileType::RegularFile,
    };
    let nlink = match attr.file_type {
        VfsFileType::Directory => 2,
        VfsFileType::RegularFile => 1,
    };
    fuser::FileAttr {
        ino: attr.id,
        size: attr.size,
        blocks: (attr.size + 511) / 512,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind,
        perm: attr.perm,
        nlink,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// Convert a [`VfsFileType`] to a [`fuser::FileType`].
fn to_fuse_file_type(ft: VfsFileType) -> FileType {
    match ft {
        VfsFileType::Directory => FileType::Directory,
        VfsFileType::RegularFile => FileType::RegularFile,
    }
}

/// Convert a [`VfsError`] to a libc errno.
fn to_errno(err: VfsError) -> i32 {
    match err {
        VfsError::NotFound => libc::ENOENT,
        VfsError::NotDirectory => libc::ENOTDIR,
        VfsError::IsDirectory => libc::EISDIR,
        VfsError::PermissionDenied => libc::EACCES,
        VfsError::AlreadyExists => libc::EEXIST,
        VfsError::CrossDevice => libc::EXDEV,
        VfsError::NotSupported => libc::ENOTSUP,
        VfsError::IoError(_) => libc::EIO,
    }
}

impl Filesystem for TapFs {
    // -----------------------------------------------------------------------
    // lookup
    // -----------------------------------------------------------------------

    fn lookup(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::ENOENT); return; }
        };
        match self.vfs.lookup(&self.rt, parent, name_str) {
            Ok(attr) => reply.entry(&TTL, &to_fuse_attr(&attr), 0),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // getattr
    // -----------------------------------------------------------------------

    fn getattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        reply: ReplyAttr,
    ) {
        match self.vfs.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &to_fuse_attr(&attr)),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // readdir
    // -----------------------------------------------------------------------

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        match self.vfs.readdir(&self.rt, ino) {
            Ok(entries) => {
                for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                    if reply.add(
                        entry.id,
                        (i + 1) as i64,
                        to_fuse_file_type(entry.file_type),
                        &entry.name,
                    ) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // read
    // -----------------------------------------------------------------------

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.vfs.read(&self.rt, ino, offset as u64, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // write
    // -----------------------------------------------------------------------

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.vfs.write(ino, offset as u64, data) {
            Ok(written) => reply.written(written),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // create
    // -----------------------------------------------------------------------

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };
        match self.vfs.create(parent, name_str) {
            Ok(attr) => {
                let ttl = Duration::from_secs(1);
                reply.created(&ttl, &to_fuse_attr(&attr), 0, 0, 0);
            }
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // rename
    // -----------------------------------------------------------------------

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let old_name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };
        let new_name_str = match new_name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };
        match self.vfs.rename(&self.rt, parent, old_name_str, new_parent, new_name_str) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // unlink
    // -----------------------------------------------------------------------

    fn unlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: ReplyEmpty,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };
        match self.vfs.unlink(parent, name_str) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // statfs
    // -----------------------------------------------------------------------

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        reply.statfs(
            1_000_000,   // blocks
            500_000,     // bfree
            500_000,     // bavail
            1_000_000,   // files
            500_000,     // ffree
            4096,        // bsize
            255,         // namelen
            4096,        // frsize
        );
    }

    // -----------------------------------------------------------------------
    // xattr operations -- not supported
    // -----------------------------------------------------------------------

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(libc::ENOTSUP);
    }

    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(libc::ENOTSUP);
    }

    fn listxattr(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(libc::ENOTSUP);
    }

    // -----------------------------------------------------------------------
    // open -- allow any open, no file handle tracking
    // -----------------------------------------------------------------------

    fn open(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _flags: i32,
        reply: fuser::ReplyOpen,
    ) {
        reply.opened(0, 0);
    }

    // -----------------------------------------------------------------------
    // opendir -- allow any directory open
    // -----------------------------------------------------------------------

    fn opendir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _flags: i32,
        reply: fuser::ReplyOpen,
    ) {
        reply.opened(0, 0);
    }

    // -----------------------------------------------------------------------
    // flush -- no-op (required to suppress "Not Implemented" warnings)
    // -----------------------------------------------------------------------

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    // -----------------------------------------------------------------------
    // release / releasedir -- no-op
    // -----------------------------------------------------------------------

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    // -----------------------------------------------------------------------
    // setattr -- accept timestamp changes (no-op, virtual filesystem)
    // -----------------------------------------------------------------------

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // Return current attrs unchanged — we accept but ignore attribute changes.
        match self.vfs.getattr(ino) {
            Ok(attr) => reply.attr(&TTL, &to_fuse_attr(&attr)),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    // -----------------------------------------------------------------------
    // access -- allow everything
    // -----------------------------------------------------------------------

    fn access(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _mask: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}

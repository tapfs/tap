//! NFS adapter for the platform-agnostic VirtualFs.
//!
//! Implements `nfsserve::vfs::NFSFileSystem` by delegating to [`VirtualFs`].
//! This is the macOS transport layer — no kernel extensions, no drivers,
//! just a localhost NFSv3 server mounted via the built-in `mount_nfs`.

use std::sync::Arc;

use async_trait::async_trait;
use nfsserve::nfs::*;
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::vfs::core::VirtualFs;
use crate::vfs::types::*;

/// NFS adapter wrapping VirtualFs.
pub struct TapNfs {
    pub vfs: Arc<VirtualFs>,
    pub rt: tokio::runtime::Handle,
}

impl TapNfs {
    fn vfs_attr_to_fattr(&self, attr: &VfsAttr) -> fattr3 {
        let ftype = match attr.file_type {
            VfsFileType::Directory => ftype3::NF3DIR,
            VfsFileType::RegularFile => ftype3::NF3REG,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let ts = nfstime3 {
            seconds: now.as_secs() as u32,
            nseconds: now.subsec_nanos(),
        };

        fattr3 {
            ftype,
            mode: attr.perm as u32,
            nlink: if attr.file_type == VfsFileType::Directory { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            size: attr.size,
            used: attr.size,
            rdev: specdata3 {
                specdata1: 0,
                specdata2: 0,
            },
            fsid: 1,
            fileid: attr.id,
            atime: ts,
            mtime: ts,
            ctime: ts,
        }
    }

    fn vfs_err_to_nfs(e: VfsError) -> nfsstat3 {
        match e {
            VfsError::NotFound => nfsstat3::NFS3ERR_NOENT,
            VfsError::NotDirectory => nfsstat3::NFS3ERR_NOTDIR,
            VfsError::IsDirectory => nfsstat3::NFS3ERR_ISDIR,
            VfsError::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
            VfsError::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
            VfsError::CrossDevice => nfsstat3::NFS3ERR_XDEV,
            VfsError::NotSupported => nfsstat3::NFS3ERR_NOTSUPP,
            VfsError::IoError(_) => nfsstat3::NFS3ERR_IO,
        }
    }
}

#[async_trait]
impl NFSFileSystem for TapNfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        1
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let name = name.to_string();
        let vfs = Arc::clone(&self.vfs);
        let rt = self.rt.clone();
        let attr = tokio::task::spawn_blocking(move || {
            vfs.lookup(&rt, dirid, &name)
        })
        .await
        .map_err(|_| nfsstat3::NFS3ERR_IO)?
        .map_err(Self::vfs_err_to_nfs)?;
        Ok(attr.id)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let attr = self.vfs.getattr(id).map_err(Self::vfs_err_to_nfs)?;
        Ok(self.vfs_attr_to_fattr(&attr))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        // Handle truncation
        if let set_size3::size(new_size) = setattr.size {
            self.vfs
                .truncate(id, new_size)
                .map_err(Self::vfs_err_to_nfs)?;
        }
        // Return current attrs (we don't support changing mode/uid/gid)
        self.getattr(id).await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let vfs = Arc::clone(&self.vfs);
        let rt = self.rt.clone();
        let data = tokio::task::spawn_blocking(move || {
            vfs.read(&rt, id, offset, count)
        })
        .await
        .map_err(|_| nfsstat3::NFS3ERR_IO)?
        .map_err(Self::vfs_err_to_nfs)?;
        let eof = data.len() < count as usize;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.vfs
            .write(id, offset, data)
            .map_err(Self::vfs_err_to_nfs)?;
        self.getattr(id).await
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self
            .vfs
            .create(dirid, name)
            .map_err(Self::vfs_err_to_nfs)?;
        let fattr = self.vfs_attr_to_fattr(&attr);
        Ok((attr.id, fattr))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self
            .vfs
            .create(dirid, name)
            .map_err(Self::vfs_err_to_nfs)?;
        Ok(attr.id)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = std::str::from_utf8(dirname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self
            .vfs
            .mkdir(dirid, name)
            .map_err(Self::vfs_err_to_nfs)?;
        let fattr = self.vfs_attr_to_fattr(&attr);
        Ok((attr.id, fattr))
    }

    async fn remove(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        self.vfs
            .unlink(dirid, name)
            .map_err(Self::vfs_err_to_nfs)
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let old_name = std::str::from_utf8(from_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?.to_string();
        let new_name = std::str::from_utf8(to_filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?.to_string();
        let vfs = Arc::clone(&self.vfs);
        let rt = self.rt.clone();
        tokio::task::spawn_blocking(move || {
            vfs.rename(&rt, from_dirid, &old_name, to_dirid, &new_name)
        })
        .await
        .map_err(|_| nfsstat3::NFS3ERR_IO)?
        .map_err(Self::vfs_err_to_nfs)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let vfs = Arc::clone(&self.vfs);
        let rt = self.rt.clone();
        let entries = tokio::task::spawn_blocking(move || {
            vfs.readdir(&rt, dirid)
        })
        .await
        .map_err(|_| nfsstat3::NFS3ERR_IO)?
        .map_err(Self::vfs_err_to_nfs)?;

        // Filter out . and .., convert to NFS DirEntry, handle pagination
        let mut nfs_entries: Vec<DirEntry> = Vec::new();
        let mut past_start = start_after == 0;

        for entry in &entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            if !past_start {
                if entry.id == start_after {
                    past_start = true;
                }
                continue;
            }
            if nfs_entries.len() >= max_entries {
                return Ok(ReadDirResult {
                    entries: nfs_entries,
                    end: false,
                });
            }

            let attr = self.vfs.getattr(entry.id).map_err(Self::vfs_err_to_nfs)?;
            nfs_entries.push(DirEntry {
                fileid: entry.id,
                name: nfsserve::nfs::nfsstring(entry.name.as_bytes().to_vec()),
                attr: self.vfs_attr_to_fattr(&attr),
            });
        }

        Ok(ReadDirResult {
            entries: nfs_entries,
            end: true,
        })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

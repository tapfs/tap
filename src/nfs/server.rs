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
    uid: u32,
    gid: u32,
}

impl TapNfs {
    pub fn new(vfs: Arc<VirtualFs>, rt: tokio::runtime::Handle) -> Self {
        Self {
            vfs,
            rt,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn vfs_attr_to_fattr(&self, attr: &VfsAttr) -> fattr3 {
        let ftype = match attr.file_type {
            VfsFileType::Directory => ftype3::NF3DIR,
            VfsFileType::RegularFile => ftype3::NF3REG,
        };
        let ts = if let Some(ref mtime_str) = attr.mtime {
            chrono::DateTime::parse_from_rfc3339(mtime_str)
                .map(|dt| nfstime3 {
                    seconds: dt.timestamp() as u32,
                    nseconds: dt.timestamp_subsec_nanos(),
                })
                .unwrap_or_else(|_| Self::now_nfstime())
        } else {
            nfstime3 {
                seconds: 0,
                nseconds: 0,
            }
        };

        fattr3 {
            ftype,
            mode: match attr.file_type {
                VfsFileType::Directory => u32::from(libc::S_IFDIR) | (attr.perm as u32),
                VfsFileType::RegularFile => u32::from(libc::S_IFREG) | (attr.perm as u32),
            },
            nlink: if attr.file_type == VfsFileType::Directory {
                2
            } else {
                1
            },
            uid: self.uid,
            gid: self.gid,
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

    fn now_nfstime() -> nfstime3 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        nfstime3 {
            seconds: now.as_secs() as u32,
            nseconds: now.subsec_nanos(),
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
        let attr = tokio::task::spawn_blocking(move || vfs.lookup(&rt, dirid, &name))
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
        let data = tokio::task::spawn_blocking(move || vfs.read(&rt, id, offset, count))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map_err(Self::vfs_err_to_nfs)?;
        let eof = data.len() < count as usize;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let vfs = Arc::clone(&self.vfs);
        let rt = self.rt.clone();
        let data_owned = data.to_vec();
        tokio::task::spawn_blocking(move || {
            vfs.write(id, offset, &data_owned)
                .and_then(|_| vfs.flush(&rt, id))
        })
        .await
        .map_err(|_| nfsstat3::NFS3ERR_IO)?
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
        let attr = self.vfs.create(dirid, name).map_err(Self::vfs_err_to_nfs)?;
        let fattr = self.vfs_attr_to_fattr(&attr);
        Ok((attr.id, fattr))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self.vfs.create(dirid, name).map_err(Self::vfs_err_to_nfs)?;
        Ok(attr.id)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = std::str::from_utf8(dirname).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = self.vfs.mkdir(dirid, name).map_err(Self::vfs_err_to_nfs)?;
        let fattr = self.vfs_attr_to_fattr(&attr);
        Ok((attr.id, fattr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = std::str::from_utf8(filename).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        self.vfs
            .unlink(&self.rt, dirid, name)
            .map_err(Self::vfs_err_to_nfs)
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let old_name = std::str::from_utf8(from_filename)
            .map_err(|_| nfsstat3::NFS3ERR_INVAL)?
            .to_string();
        let new_name = std::str::from_utf8(to_filename)
            .map_err(|_| nfsstat3::NFS3ERR_INVAL)?
            .to_string();
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
        let entries = tokio::task::spawn_blocking(move || vfs.readdir(&rt, dirid))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::types::{VfsAttr, VfsFileType};

    fn dummy_tapnfs() -> TapNfs {
        // Minimal construction just to call vfs_attr_to_fattr.
        // The vfs field is never accessed in these tests.
        use crate::cache::store::Cache;
        use crate::connector::registry::ConnectorRegistry;
        use crate::draft::store::DraftStore;
        use crate::governance::audit::AuditLogger;
        use crate::version::store::VersionStore;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let registry = Arc::new(ConnectorRegistry::new());
        let cache = Arc::new(Cache::new(Duration::from_secs(60)));
        let drafts = Arc::new(DraftStore::new(tmp.path().join("d")).unwrap());
        let versions = Arc::new(VersionStore::new(tmp.path().join("v")).unwrap());
        let audit = Arc::new(AuditLogger::new(tmp.path().join("a.log")).unwrap());
        let vfs = Arc::new(crate::vfs::core::VirtualFs::new(
            registry, cache, drafts, versions, audit,
        ));
        TapNfs {
            vfs,
            rt: tokio::runtime::Handle::current(),
            uid: 0,
            gid: 0,
        }
    }

    /// vfs_attr_to_fattr must include S_IFDIR / S_IFREG in the mode field.
    /// Without these bits macOS NFS client misidentifies file types and
    /// returns EPERM when re-validating directory attributes.
    #[test]
    fn mode_includes_file_type_bits() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let nfs = dummy_tapnfs();

        let dir_attr = VfsAttr {
            id: 1,
            size: 0,
            file_type: VfsFileType::Directory,
            perm: 0o755,
            mtime: None,
        };
        let fattr = nfs.vfs_attr_to_fattr(&dir_attr);
        assert_eq!(
            fattr.mode,
            u32::from(libc::S_IFDIR) | 0o755,
            "directory mode must contain S_IFDIR"
        );

        let file_attr = VfsAttr {
            id: 2,
            size: 100,
            file_type: VfsFileType::RegularFile,
            perm: 0o644,
            mtime: None,
        };
        let fattr = nfs.vfs_attr_to_fattr(&file_attr);
        assert_eq!(
            fattr.mode,
            u32::from(libc::S_IFREG) | 0o644,
            "regular file mode must contain S_IFREG"
        );
    }

    /// Nodes with no real mtime must emit epoch-0, not the current wall clock.
    /// A changing mtime causes macOS NFS client to re-validate aggressively,
    /// eventually triggering a strict mode-bit check and returning EPERM.
    #[test]
    fn stable_mtime_for_nodes_without_mtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let nfs = dummy_tapnfs();

        let attr = VfsAttr {
            id: 1,
            size: 0,
            file_type: VfsFileType::Directory,
            perm: 0o755,
            mtime: None,
        };
        let fattr = nfs.vfs_attr_to_fattr(&attr);
        assert_eq!(
            fattr.mtime.seconds, 0,
            "mtime must be epoch-0 when node has no real mtime"
        );
        assert_eq!(fattr.mtime.nseconds, 0);
    }
}

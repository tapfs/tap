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

// libc::S_IFDIR / S_IFREG are u16 on macOS and u32 on Linux.
// Define u32 constants once so call sites stay lint-free on both platforms.
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
const MODE_IFDIR: u32 = libc::S_IFDIR as u32;
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
const MODE_IFREG: u32 = libc::S_IFREG as u32;

/// Default maximum in-flight NFS handlers that hit the connector side.
///
/// NFS clients (especially macOS) retry aggressively on RPC timeout — a flaky
/// upstream API can otherwise grow our blocking-task pool without bound and
/// exhaust tokio's blocking thread pool (default 512). 64 in-flight is a
/// conservative ceiling that still gives plenty of parallelism for normal
/// bursty access. Override via `TAPFS_MAX_CONCURRENT_REQUESTS`.
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 64;

fn max_concurrent_requests() -> usize {
    std::env::var("TAPFS_MAX_CONCURRENT_REQUESTS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_REQUESTS)
}

/// Clamp a Unix timestamp (signed seconds-since-epoch) into the u32 slot
/// that NFSv3's `nfstime3.seconds` requires. The previous `as u32` cast
/// silently truncated past 2038-01-19 03:14:07 UTC (and wrapped negatives
/// to huge positives) — by 2038 a stat() on any old file would report a
/// nonsensical mtime and macOS NFS client behavior depends on plausible
/// timestamps for cache validation.
///
/// Strategy: clamp to `[0, u32::MAX]`. Pre-1970 dates become epoch-0 (we
/// don't have a way to represent them in NFSv3 anyway); post-2106 dates
/// pin to u32::MAX. Both end-points get logged once per process via
/// `tracing::warn` (rate-limited inside chrono parsing pathways).
pub(crate) fn clamp_to_u32_seconds(seconds: i64) -> u32 {
    if seconds < 0 {
        tracing::debug!(seconds, "pre-epoch timestamp clamped to 0 for nfstime3");
        0
    } else if seconds > u32::MAX as i64 {
        tracing::warn!(
            seconds,
            "post-2106 timestamp clamped to u32::MAX — fattr3 cannot represent it"
        );
        u32::MAX
    } else {
        seconds as u32
    }
}

/// NFS adapter wrapping VirtualFs.
pub struct TapNfs {
    pub vfs: Arc<VirtualFs>,
    pub rt: tokio::runtime::Handle,
    uid: u32,
    gid: u32,
    /// Bounds in-flight handlers that dispatch into the connector side
    /// (`spawn_blocking` + sync VFS that may `rt.block_on` an HTTP call).
    /// When the semaphore has no permits, handlers return `NFS3ERR_JUKEBOX`
    /// so the kernel client backs off — that's NFSv3's standard "transient,
    /// try later" code, exactly what JUKEBOX was added to the protocol for.
    request_semaphore: Arc<tokio::sync::Semaphore>,
}

impl TapNfs {
    pub fn new(vfs: Arc<VirtualFs>, rt: tokio::runtime::Handle) -> Self {
        Self::new_with_concurrency(vfs, rt, max_concurrent_requests())
    }

    pub fn new_with_concurrency(
        vfs: Arc<VirtualFs>,
        rt: tokio::runtime::Handle,
        max_concurrent: usize,
    ) -> Self {
        Self {
            vfs,
            rt,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            request_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
        }
    }

    /// Acquire an owned permit for an in-flight NFS handler. Returns
    /// `NFS3ERR_JUKEBOX` when no permit is available so the kernel backs off
    /// instead of piling on retries (which would just deepen the pile-up).
    fn acquire_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, nfsstat3> {
        Arc::clone(&self.request_semaphore)
            .try_acquire_owned()
            .map_err(|_| {
                tracing::warn!(
                    in_flight = self.request_semaphore.available_permits(),
                    "NFS request rejected — semaphore exhausted, returning JUKEBOX"
                );
                nfsstat3::NFS3ERR_JUKEBOX
            })
    }

    fn vfs_attr_to_fattr(&self, attr: &VfsAttr) -> fattr3 {
        let ftype = match attr.file_type {
            VfsFileType::Directory => ftype3::NF3DIR,
            VfsFileType::RegularFile => ftype3::NF3REG,
        };
        let ts = if let Some(ref mtime_str) = attr.mtime {
            chrono::DateTime::parse_from_rfc3339(mtime_str)
                .map(|dt| nfstime3 {
                    seconds: clamp_to_u32_seconds(dt.timestamp()),
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
                VfsFileType::Directory => MODE_IFDIR | (attr.perm as u32),
                VfsFileType::RegularFile => MODE_IFREG | (attr.perm as u32),
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
            seconds: clamp_to_u32_seconds(now.as_secs() as i64),
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
            // JUKEBOX is the NFSv3 protocol's "transient, retry me" code.
            // Both Busy (in-flight POST) and RateLimited (upstream 429) are
            // surfaced as JUKEBOX so a Linux/macOS NFS client backs off
            // instead of treating them as hard failures.
            VfsError::Busy => nfsstat3::NFS3ERR_JUKEBOX,
            VfsError::RateLimited(_) => nfsstat3::NFS3ERR_JUKEBOX,
            VfsError::StaleHandle => nfsstat3::NFS3ERR_STALE,
            VfsError::NoSpace => nfsstat3::NFS3ERR_NOSPC,
            // PartialFlush and DraftCorrupted both indicate something is
            // genuinely wrong (data is in an inconsistent state). Surface as
            // EIO so the client doesn't loop on retries.
            VfsError::PartialFlush(_) => nfsstat3::NFS3ERR_IO,
            VfsError::DraftCorrupted(_) => nfsstat3::NFS3ERR_IO,
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
        let _permit = self.acquire_permit()?;
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
        let _permit = self.acquire_permit()?;
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
        let _permit = self.acquire_permit()?;
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
        let _permit = self.acquire_permit()?;
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
        let _permit = self.acquire_permit()?;
        let vfs = Arc::clone(&self.vfs);
        let rt = self.rt.clone();
        let entries = tokio::task::spawn_blocking(move || vfs.readdir(&rt, dirid))
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?
            .map_err(Self::vfs_err_to_nfs)?;

        // Filter out . and .., convert to NFS DirEntry, handle pagination.
        //
        // Cookie validity check: NFSv3 lets clients paginate by passing back
        // the fileid of the last entry from the previous batch. If the
        // directory's contents changed between calls and that fileid is no
        // longer here, walking past every entry would silently return an
        // empty result — the client never learns the directory shifted under
        // it. Walk first to detect the missing-cookie case, then return
        // NFS3ERR_BAD_COOKIE so the client restarts from the beginning.
        if start_after != 0 {
            let cookie_present = entries
                .iter()
                .any(|e| e.id == start_after && e.name != "." && e.name != "..");
            if !cookie_present {
                tracing::debug!(
                    dirid,
                    cookie = start_after,
                    "readdir cookie no longer in directory — directory shifted; \
                     returning BAD_COOKIE so client restarts from beginning"
                );
                return Err(nfsstat3::NFS3ERR_BAD_COOKIE);
            }
        }

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

    /// Override `fsinfo` to advertise tapfs-specific characteristics.
    ///
    /// The nfsserve default reports nanosecond `time_delta`, which is wrong
    /// for us — most of our mtimes come from API timestamps that round to
    /// the second (and synthetic nodes report epoch-0 per CLAUDE.md). Telling
    /// the kernel we have nanosecond precision causes it to expect sub-second
    /// mtime ordering and produce confusing "file changed" warnings in
    /// editors when nothing actually changed.
    ///
    /// Other tunings:
    /// - `maxfilesize`: kept generous (128 GB) — most resources are tiny but
    ///   a future binary-blob backend shouldn't trip on this.
    /// - rt/wt sizes: kept at defaults (1 MB) — fine for our workload.
    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
        let dir_attr = match self.getattr(root_fileid).await {
            Ok(v) => post_op_attr::attributes(v),
            Err(_) => post_op_attr::Void,
        };
        Ok(fsinfo3 {
            obj_attributes: dir_attr,
            rtmax: 1024 * 1024,
            rtpref: 1024 * 124,
            rtmult: 1024 * 1024,
            wtmax: 1024 * 1024,
            wtpref: 1024 * 1024,
            wtmult: 1024 * 1024,
            dtpref: 1024 * 1024,
            maxfilesize: 128 * 1024 * 1024 * 1024,
            // 1-second granularity: matches the precision of API mtimes and
            // the epoch-0 stable mtime contract from CLAUDE.md. Tells the
            // kernel not to expect sub-second mtime ordering.
            time_delta: nfstime3 {
                seconds: 1,
                nseconds: 0,
            },
            properties: FSF_HOMOGENEOUS | FSF_CANSETTIME,
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
            request_semaphore: Arc::new(tokio::sync::Semaphore::new(64)),
        }
    }

    fn dummy_tapnfs_with_concurrency(max: usize) -> TapNfs {
        let mut nfs = dummy_tapnfs();
        nfs.request_semaphore = Arc::new(tokio::sync::Semaphore::new(max));
        nfs
    }

    #[test]
    fn acquire_permit_returns_jukebox_when_exhausted() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        // Capacity 1: take the only permit, then a second acquire must
        // surface JUKEBOX rather than block (which would queue forever
        // under retry storms).
        let nfs = dummy_tapnfs_with_concurrency(1);
        let _held = nfs.acquire_permit().expect("first permit should succeed");
        let err = nfs
            .acquire_permit()
            .expect_err("second permit should be rejected");
        assert!(matches!(err, nfsstat3::NFS3ERR_JUKEBOX));
    }

    #[test]
    fn acquire_permit_replenishes_after_drop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let nfs = dummy_tapnfs_with_concurrency(1);
        {
            let _held = nfs.acquire_permit().expect("first permit");
            // Permit goes out of scope at end of block.
        }
        // Now the permit is back, the next acquire should succeed.
        let _again = nfs
            .acquire_permit()
            .expect("permit should be available again");
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
            MODE_IFDIR | 0o755,
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
            MODE_IFREG | 0o644,
            "regular file mode must contain S_IFREG"
        );
    }

    #[test]
    fn typed_vfs_errors_map_to_correct_nfsstat3() {
        // The kernel uses the nfsstat3 code to decide whether to retry,
        // back off, surface to the user, etc. Mapping these correctly is
        // the whole point of typed VfsError variants.
        use std::time::Duration;
        assert!(matches!(
            TapNfs::vfs_err_to_nfs(VfsError::Busy),
            nfsstat3::NFS3ERR_JUKEBOX
        ));
        assert!(matches!(
            TapNfs::vfs_err_to_nfs(VfsError::RateLimited(Duration::from_secs(5))),
            nfsstat3::NFS3ERR_JUKEBOX
        ));
        assert!(matches!(
            TapNfs::vfs_err_to_nfs(VfsError::StaleHandle),
            nfsstat3::NFS3ERR_STALE
        ));
        assert!(matches!(
            TapNfs::vfs_err_to_nfs(VfsError::NoSpace),
            nfsstat3::NFS3ERR_NOSPC
        ));
        assert!(matches!(
            TapNfs::vfs_err_to_nfs(VfsError::PartialFlush("upstream timed out".into())),
            nfsstat3::NFS3ERR_IO
        ));
        assert!(matches!(
            TapNfs::vfs_err_to_nfs(VfsError::DraftCorrupted("bad frontmatter".into())),
            nfsstat3::NFS3ERR_IO
        ));
    }

    #[test]
    fn connector_rate_limited_propagates_through_vfs_error() {
        // When a connector returns ConnectorError::RateLimited, the
        // From<anyhow::Error> impl on VfsError needs to preserve it as
        // RateLimited (not collapse to IoError). Otherwise the NFS layer
        // can't tell the kernel to back off via JUKEBOX.
        use crate::connector::traits::ConnectorError;
        use std::time::Duration;

        let upstream: anyhow::Error = ConnectorError::RateLimited {
            message: "429".into(),
            retry_after: Some(Duration::from_secs(7)),
        }
        .into();
        let vfs_err: VfsError = upstream.into();
        match vfs_err {
            VfsError::RateLimited(d) => assert_eq!(d, Duration::from_secs(7)),
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fsinfo_advertises_one_second_time_delta() {
        // The nfsserve default reports nanosecond precision; we override
        // because our mtime sources (API timestamps, epoch-0 synthetic nodes)
        // round to the second. Telling the kernel "1-sec precision" prevents
        // editors from claiming the file changed when nothing did.
        let nfs = dummy_tapnfs();
        let info = nfs.fsinfo(nfs.root_dir()).await.unwrap();
        assert_eq!(info.time_delta.seconds, 1);
        assert_eq!(info.time_delta.nseconds, 0);
    }

    #[test]
    fn clamp_rejects_negative_seconds() {
        // Pre-1970 dates can't be represented in nfstime3. Clamp to 0 rather
        // than letting `as u32` wrap them to wildly future timestamps that
        // confuse macOS NFS attribute caching.
        assert_eq!(clamp_to_u32_seconds(-1), 0);
        assert_eq!(clamp_to_u32_seconds(i64::MIN), 0);
    }

    #[test]
    fn clamp_caps_post_2106_seconds() {
        // u32::MAX seconds = 2106-02-07 06:28:15 UTC. Anything past that
        // pins to the cap; previously `as u32` silently truncated to
        // bogus low values (e.g. 2106 + 1 sec → 0, the Unix epoch).
        let post_2106 = (u32::MAX as i64) + 1;
        assert_eq!(clamp_to_u32_seconds(post_2106), u32::MAX);
        assert_eq!(clamp_to_u32_seconds(i64::MAX), u32::MAX);
    }

    #[test]
    fn clamp_passes_through_in_range() {
        // Sanity: sane modern timestamp survives the clamp unchanged.
        let now = 1_777_000_000; // ~2026
        assert_eq!(clamp_to_u32_seconds(now), now as u32);
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

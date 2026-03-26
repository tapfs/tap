use anyhow::{Context, Result};
use std::path::Path;

/// Unmount a tapfs FUSE filesystem at the given mount point.
pub fn run(mount_point: &Path) -> Result<()> {
    if !mount_point.exists() {
        anyhow::bail!("mount point does not exist: {}", mount_point.display());
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("umount")
            .arg(mount_point)
            .status()
            .context("failed to execute umount")?;
        if !status.success() {
            anyhow::bail!("umount failed with exit code {:?}", status.code());
        }
    }

    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("fusermount")
            .arg("-u")
            .arg(mount_point)
            .status()
            .context("failed to execute fusermount -u")?;
        if !status.success() {
            anyhow::bail!("fusermount -u failed with exit code {:?}", status.code());
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("unmount is not supported on this platform");
    }

    println!("Unmounted {}", mount_point.display());

    // Try to clean up the mounts.json status file
    let data_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".tapfs");
    let mounts_path = data_dir.join("mounts.json");
    if mounts_path.exists() {
        let _ = std::fs::remove_file(&mounts_path);
    }

    Ok(())
}

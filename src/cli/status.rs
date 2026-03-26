use anyhow::Result;
use std::path::PathBuf;

/// Show the current mount status.
pub fn run(data_dir: PathBuf) -> Result<()> {
    let mounts_path = data_dir.join("mounts.json");

    if !mounts_path.exists() {
        println!("No active tapfs mounts.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&mounts_path)?;
    let info: serde_json::Value = serde_json::from_str(&content)?;

    println!("Active tapfs mount:");
    println!(
        "  Connector:   {}",
        info["connector"].as_str().unwrap_or("?")
    );
    println!(
        "  Mount point: {}",
        info["mount_point"].as_str().unwrap_or("?")
    );
    println!("  PID:         {}", info["pid"]);
    println!(
        "  Started at:  {}",
        info["started_at"].as_str().unwrap_or("?")
    );

    // Check if the process is still alive
    if let Some(pid) = info["pid"].as_u64() {
        let pid = pid as i32;
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if alive {
            println!("  Status:      running");
        } else {
            println!("  Status:      stale (process {} not found)", pid);
        }
    }

    // Show audit log stats if available
    let audit_path = data_dir.join("audit.log");
    if audit_path.exists() {
        if let Ok(meta) = std::fs::metadata(&audit_path) {
            println!(
                "  Audit log:   {} ({} bytes)",
                audit_path.display(),
                meta.len()
            );
        }
    }

    Ok(())
}

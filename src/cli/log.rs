use anyhow::Result;
use std::path::PathBuf;

use crate::governance::audit::AuditLogger;

/// Read and display audit log entries.
pub fn run(data_dir: PathBuf, limit: Option<usize>, connector: Option<String>) -> Result<()> {
    let log_path = data_dir.join("audit.log");

    if !log_path.exists() {
        println!("No audit log found at {}", log_path.display());
        return Ok(());
    }

    let logger = AuditLogger::new(log_path)?;
    let entries = logger.read_entries(limit, connector.as_deref())?;

    if entries.is_empty() {
        println!("No audit entries found.");
        return Ok(());
    }

    for entry in &entries {
        let collection = entry.collection.as_deref().unwrap_or("-");
        let resource = entry.resource.as_deref().unwrap_or("-");
        let detail = entry.detail.as_deref().unwrap_or("");

        println!(
            "{} [{:>7}] {} {}/{} {} {}",
            entry.timestamp,
            entry.outcome,
            entry.connector,
            collection,
            resource,
            entry.operation,
            detail,
        );
    }

    println!("\n({} entries shown)", entries.len());
    Ok(())
}

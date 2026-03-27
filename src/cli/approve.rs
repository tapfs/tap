use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct PendingChange {
    pub connector: String,
    pub collection: String,
    pub resource: String,
    pub content_path: String,
    pub created_at: String,
    pub size: u64,
}

/// List all pending changes
pub fn run_pending(data_dir: &Path) -> Result<()> {
    let pending_dir = data_dir.join("pending");
    if !pending_dir.exists() {
        println!("No pending changes.");
        return Ok(());
    }

    let mut found = false;
    for entry in walkdir(&pending_dir)? {
        if entry.extension().map(|e| e == "json").unwrap_or(false) {
            let content = std::fs::read_to_string(&entry)?;
            let change: PendingChange = serde_json::from_str(&content)?;
            println!("  {} {}/{}/{} ({} bytes, {})",
                "PENDING",
                change.connector,
                change.collection,
                change.resource,
                change.size,
                change.created_at,
            );
            found = true;
        }
    }

    if !found {
        println!("No pending changes.");
    }

    Ok(())
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path)?);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// Approve a pending change by path
pub fn run_approve(path: &Path, data_dir: &Path) -> Result<()> {
    let (connector, collection, resource, _) = crate::cli::versions::parse_tapfs_path(path)?;

    let pending_json = data_dir
        .join("pending")
        .join(&connector)
        .join(&collection)
        .join(format!("{}.json", resource));

    if !pending_json.exists() {
        return Err(anyhow!("no pending change for {}/{}/{}", connector, collection, resource));
    }

    let meta: PendingChange = serde_json::from_str(&std::fs::read_to_string(&pending_json)?)?;
    let content = std::fs::read(&meta.content_path)?;

    println!("Approving: {}/{}/{} ({} bytes)", connector, collection, resource, content.len());
    println!("Content will be pushed to API.");

    // The actual push happens here - but we need a runtime and connector.
    // For now, just move the content to the draft store so next flush promotes it.
    let draft_store = crate::draft::store::DraftStore::new(data_dir.join("drafts"))?;
    draft_store.create_draft(&connector, &collection, &resource, &content)?;

    // Clean up pending
    let _ = std::fs::remove_file(&pending_json);
    let _ = std::fs::remove_file(&meta.content_path);

    println!("Approved. Draft created — will push to API on next mount/flush.");

    Ok(())
}

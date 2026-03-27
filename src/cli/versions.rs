use anyhow::{anyhow, Result};
use std::path::Path;

use crate::version::store::VersionStore;

/// Parse a tapfs path into (connector, collection, resource, version).
///
/// Examples:
///   /mnt/tap/google/drive/test.md       → ("google", "drive", "test", None)
///   /mnt/tap/google/drive/test@v3.md    → ("google", "drive", "test", Some(3))
///   /mnt/tap/rest/items/item-1.md       → ("rest", "items", "item-1", None)
pub fn parse_tapfs_path(path: &Path) -> Result<(String, String, String, Option<u32>)> {
    // Walk up from the path to find the structure:
    // <mount_point>/<connector>/<collection>/<resource>.md
    // We need at least 3 meaningful components after the mount point.
    // Strategy: take the last 3 components (connector/collection/resource).
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    if components.len() < 3 {
        return Err(anyhow!(
            "invalid tapfs path: need at least <connector>/<collection>/<resource>.md, got: {}",
            path.display()
        ));
    }

    // Last 3 components
    let resource_file = components[components.len() - 1];
    let collection = components[components.len() - 2].to_string();
    let connector = components[components.len() - 3].to_string();

    // Parse resource name: strip .md, extract @vN if present
    let name = resource_file
        .strip_suffix(".md")
        .or_else(|| resource_file.strip_suffix(".draft.md"))
        .unwrap_or(resource_file);

    let (resource, version) = if let Some(at_pos) = name.rfind("@v") {
        let ver_str = &name[at_pos + 2..];
        match ver_str.parse::<u32>() {
            Ok(v) => (name[..at_pos].to_string(), Some(v)),
            Err(_) => (name.to_string(), None),
        }
    } else {
        (name.to_string(), None)
    };

    Ok((connector, collection, resource, version))
}

/// `tap versions /mnt/tap/google/drive/test.md` — list all versions.
pub fn run_versions(path: &Path, data_dir: &Path) -> Result<()> {
    let (connector, collection, resource, _) = parse_tapfs_path(path)?;

    let store = VersionStore::new(data_dir.join("versions"))?;
    let versions = store.list_versions(&connector, &collection, &resource)?;

    if versions.is_empty() {
        println!("No versions for {}/{}/{}", connector, collection, resource);
        return Ok(());
    }

    println!(
        "Versions of {}/{}/{}:",
        connector, collection, resource
    );
    for v in &versions {
        let file = data_dir
            .join("versions")
            .join(&connector)
            .join(&collection)
            .join(&resource)
            .join(format!("v{}", v));
        let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        let modified = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| {
                        chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                            .unwrap_or_default()
                    })
            })
            .unwrap_or_default();

        println!("  @v{}  {} bytes  {}", v, size, modified);
    }

    // Show how to read or rollback
    let latest = versions.last().unwrap();
    println!();
    println!("Read a version:    cat {}", path.display().to_string().replace(".md", &format!("@v{}.md", latest)));
    println!("Rollback to v{}:   tap rollback {}", latest, path.display().to_string().replace(".md", &format!("@v{}.md", latest)));

    Ok(())
}

/// `tap rollback /mnt/tap/google/drive/test@v3.md` — restore a version as current.
pub fn run_rollback(path: &Path, data_dir: &Path) -> Result<()> {
    let (connector, collection, resource, version) = parse_tapfs_path(path)?;

    let version = version.ok_or_else(|| {
        anyhow!(
            "no version specified. Use @vN in the path, e.g.: {}",
            path.display().to_string().replace(".md", "@v3.md")
        )
    })?;

    let store = VersionStore::new(data_dir.join("versions"))?;

    let content = store
        .read_version(&connector, &collection, &resource, version)?
        .ok_or_else(|| {
            anyhow!(
                "version @v{} not found for {}/{}/{}",
                version,
                connector,
                collection,
                resource
            )
        })?;

    // Save the current content as a new version (so rollback is reversible)
    // Read current from the latest version or live content
    let versions = store.list_versions(&connector, &collection, &resource)?;
    let latest = versions.last().copied().unwrap_or(0);
    if latest != version {
        // The rollback target isn't the latest — save current as new version first
        if let Some(current) = store.read_version(&connector, &collection, &resource, latest)? {
            let saved = store.save_snapshot(&connector, &collection, &resource, &current)?;
            println!(
                "Saved current (v{}) as v{} before rollback",
                latest, saved
            );
        }
    }

    // Write the old version content as a draft so it gets promoted on next flush
    let drafts_dir = data_dir.join("drafts");
    let draft_store = crate::draft::store::DraftStore::new(drafts_dir)?;
    draft_store.create_draft(&connector, &collection, &resource, &content)?;

    println!(
        "Rolled back {}/{}/{} to @v{} ({} bytes)",
        connector,
        collection,
        resource,
        version,
        content.len()
    );
    println!("Draft created. To push to API:");
    println!(
        "  mv {}.draft.md {}.md",
        path.display().to_string().replace(".md", "").replace(&format!("@v{}", version), ""),
        path.display().to_string().replace(".md", "").replace(&format!("@v{}", version), ""),
    );
    println!("Or if auto-promote is enabled, the content will push on next write/close.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_simple_path() {
        let p = PathBuf::from("/mnt/tap/google/drive/test.md");
        let (c, col, r, v) = parse_tapfs_path(&p).unwrap();
        assert_eq!(c, "google");
        assert_eq!(col, "drive");
        assert_eq!(r, "test");
        assert_eq!(v, None);
    }

    #[test]
    fn parse_versioned_path() {
        let p = PathBuf::from("/mnt/tap/google/drive/test@v3.md");
        let (c, col, r, v) = parse_tapfs_path(&p).unwrap();
        assert_eq!(c, "google");
        assert_eq!(col, "drive");
        assert_eq!(r, "test");
        assert_eq!(v, Some(3));
    }

    #[test]
    fn parse_rest_path() {
        let p = PathBuf::from("/tmp/tap/rest/items/item-1.md");
        let (c, col, r, v) = parse_tapfs_path(&p).unwrap();
        assert_eq!(c, "rest");
        assert_eq!(col, "items");
        assert_eq!(r, "item-1");
        assert_eq!(v, None);
    }

    #[test]
    fn parse_high_version() {
        let p = PathBuf::from("/mnt/tap/google/gmail/msg@v42.md");
        let (_, _, r, v) = parse_tapfs_path(&p).unwrap();
        assert_eq!(r, "msg");
        assert_eq!(v, Some(42));
    }

    #[test]
    fn parse_too_short_fails() {
        let p = PathBuf::from("/mnt/tap");
        assert!(parse_tapfs_path(&p).is_err());
    }
}

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// Install a connector from a Git repository or GitHub shorthand.
///
/// Examples:
///   tap install tapfs/salesforce           -> git clone https://github.com/tapfs/salesforce
///   tap install https://github.com/foo/bar -> git clone as-is
///   tap install ./local-connector          -> copy from local path
pub fn run_install(source: &str, data_dir: &Path) -> Result<()> {
    let connectors_dir = data_dir.join("connectors");
    std::fs::create_dir_all(&connectors_dir)?;

    if source.starts_with("./") || source.starts_with('/') {
        // Local path
        let src_path = PathBuf::from(source);
        let name = src_path.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("invalid path: {}", source))?;
        let dest = connectors_dir.join(name);
        copy_dir(&src_path, &dest)?;
        println!("Installed {} from local path", name);
    } else if source.starts_with("http://") || source.starts_with("https://") || source.starts_with("git@") {
        // Full Git URL
        let name = source.rsplit('/')
            .next()
            .unwrap_or("connector")
            .trim_end_matches(".git");
        let dest = connectors_dir.join(name);
        git_clone(source, &dest)?;
        println!("Installed {} from {}", name, source);
    } else {
        // GitHub shorthand: org/repo
        let url = format!("https://github.com/{}.git", source);
        let name = source.rsplit('/').next().unwrap_or("connector");
        let dest = connectors_dir.join(name);
        git_clone(&url, &dest)?;
        println!("Installed {} from {}", name, url);
    }

    Ok(())
}

/// List installed connectors
pub fn run_list_connectors(data_dir: &Path) -> Result<()> {
    let connectors_dir = data_dir.join("connectors");
    if !connectors_dir.exists() {
        println!("No connectors installed.");
        return Ok(());
    }

    println!("Installed connectors:");
    for entry in std::fs::read_dir(&connectors_dir)? {
        let entry = entry?;
        if entry.path().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Check for tap.yaml or connector.yaml
            let has_spec = entry.path().join("tap.yaml").exists()
                || entry.path().join("connector.yaml").exists();
            let marker = if has_spec { "[ok]" } else { "[?]" };
            println!("  {} {}", marker, name);
        }
    }

    Ok(())
}

/// Remove an installed connector
pub fn run_remove(name: &str, data_dir: &Path) -> Result<()> {
    let dest = data_dir.join("connectors").join(name);
    if !dest.exists() {
        return Err(anyhow!("connector '{}' is not installed", name));
    }
    std::fs::remove_dir_all(&dest)?;
    println!("Removed {}", name);
    Ok(())
}

/// Update an installed connector (git pull)
pub fn run_update(name: &str, data_dir: &Path) -> Result<()> {
    let dest = data_dir.join("connectors").join(name);
    if !dest.exists() {
        return Err(anyhow!("connector '{}' is not installed", name));
    }

    let status = std::process::Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(&dest)
        .status()
        .context("failed to run git pull")?;

    if !status.success() {
        return Err(anyhow!("git pull failed for {}", name));
    }

    println!("Updated {}", name);
    Ok(())
}

fn git_clone(url: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        return Err(anyhow!("already installed at {}", dest.display()));
    }

    let status = std::process::Command::new("git")
        .args(["clone", "--depth", "1", url, &dest.to_string_lossy()])
        .status()
        .context("failed to run git clone")?;

    if !status.success() {
        return Err(anyhow!("git clone failed for {}", url));
    }

    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    if !src.exists() {
        return Err(anyhow!("source not found: {}", src.display()));
    }
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

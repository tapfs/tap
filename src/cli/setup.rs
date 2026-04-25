use anyhow::{Context, Result};
use std::path::Path;

/// Generate a CLAUDE.md snippet for Claude Code integration.
///
/// Reads mounts.json to discover what connectors are currently mounted,
/// and produces a markdown snippet that teaches Claude Code how to use tapfs.
pub fn run_setup_claude(data_dir: &Path, append: bool) -> Result<()> {
    let mount_point = discover_mount_point(data_dir);
    let connector = discover_connector(data_dir);
    let snippet = generate_snippet(&mount_point, connector.as_deref());

    if append {
        let claude_md = Path::new("CLAUDE.md");
        if claude_md.exists() {
            let existing = std::fs::read_to_string(claude_md)?;
            if existing.contains("tapfs is mounted") {
                println!("CLAUDE.md already contains tapfs configuration.");
                return Ok(());
            }
            std::fs::write(claude_md, format!("{}\n{}", existing.trim_end(), snippet))
                .context("writing CLAUDE.md")?;
        } else {
            std::fs::write(claude_md, &snippet).context("creating CLAUDE.md")?;
        }
        println!("Appended tapfs configuration to ./CLAUDE.md");
    } else {
        print!("{}", snippet);
    }

    Ok(())
}

fn discover_mount_point(data_dir: &Path) -> String {
    let mounts_path = data_dir.join("mounts.json");
    if let Ok(contents) = std::fs::read_to_string(&mounts_path) {
        if let Ok(info) = serde_json::from_str::<serde_json::Value>(&contents) {
            if let Some(mp) = info.get("mount_point").and_then(|v| v.as_str()) {
                return mp.to_string();
            }
        }
    }
    "/tmp/tap".to_string()
}

fn discover_connector(data_dir: &Path) -> Option<String> {
    let mounts_path = data_dir.join("mounts.json");
    if let Ok(contents) = std::fs::read_to_string(&mounts_path) {
        if let Ok(info) = serde_json::from_str::<serde_json::Value>(&contents) {
            return info
                .get("connector")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

fn generate_snippet(mount_point: &str, connector: Option<&str>) -> String {
    let mut out = String::new();

    out.push_str("\n# tapfs\n\n");
    out.push_str(&format!(
        "Enterprise API data is mounted at {} as a live filesystem.\n\n",
        mount_point
    ));

    // Currently mounted
    if let Some(name) = connector {
        out.push_str(&format!("Currently mounted: **{}**\n\n", name));
    }

    out.push_str("## How to use\n\n");
    out.push_str(&format!(
        "1. Read the guide: `cat {}/agent.md`\n",
        mount_point
    ));
    out.push_str(&format!("2. List services: `ls {}/`\n", mount_point));
    out.push_str(&format!(
        "3. Browse collections: `ls {}/<service>/`\n",
        mount_point
    ));
    out.push_str(&format!(
        "4. Read a resource: `cat {}/<service>/<collection>/<name>.md`\n",
        mount_point
    ));
    out.push_str(&format!(
        "5. Search: `grep -r \"keyword\" {}/<service>/`\n",
        mount_point
    ));

    out.push_str("\n## Making changes\n\n");
    out.push_str("- Write to `<name>.draft.md` to stage changes safely\n");
    out.push_str("- Rename `.draft.md` to `.md` to publish changes to the API\n");
    out.push_str("- Create `<name>.lock` before editing to prevent conflicts\n");

    out
}

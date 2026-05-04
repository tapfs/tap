//! `agent.md` generators for the three levels at which the file appears:
//! root (`/agent.md`), per-connector (`/<connector>/agent.md`), and
//! per-collection (`/<connector>/<collection>/agent.md`).
//!
//! These run in `read()` for the synthetic AgentMd nodes. They're free of
//! mutation, but they consume the connector spec + listings cache, so they
//! live as additional `impl VirtualFs` methods (Rust allows multiple impl
//! blocks across files in the same crate).

use super::core::VirtualFs;

impl VirtualFs {
    pub(crate) fn generate_root_agent_md(&self) -> String {
        let connectors = self.registry.list();
        let mut out = String::new();
        out.push_str("---\ntitle: tapfs\n---\n\n");

        // Connected services
        out.push_str("# Connected services\n\n");
        if connectors.is_empty() {
            out.push_str("No services connected.\n");
        } else {
            for name in &connectors {
                out.push_str(&format!("- **{}/**\n", name));
            }
        }

        // How to use — this is the skill definition for any agent
        out.push_str("\n# How to use this filesystem\n\n");
        out.push_str("Enterprise data is mounted here as plain files. ");
        out.push_str("Use standard commands to explore and modify it.\n\n");

        out.push_str("## Reading data\n\n");
        out.push_str("- `ls <service>/` — list collections (issues, repos, etc.)\n");
        out.push_str("- `ls <service>/<collection>/` — list resources\n");
        out.push_str("- `cat <resource>.md` — read a resource\n");
        out.push_str("- `grep -r \"keyword\" <service>/` — search across resources\n");

        out.push_str("\n## Making changes\n\n");
        out.push_str("- Write to `<name>.draft.md` to stage changes safely\n");
        out.push_str("- Rename `.draft.md` to `.md` to publish changes\n");
        out.push_str("- Create `<name>.lock` before editing to prevent conflicts\n");

        out.push_str("\n## Tips\n\n");
        out.push_str("- Each service directory has its own `agent.md` with details\n");
        out.push_str("- Each collection directory has an `agent.md` listing available resources\n");
        out.push_str("- `.md` files are live data — reading fetches the latest from the API\n");
        out.push_str("- `.draft.md` files are local only until promoted\n");

        out
    }

    pub(crate) fn generate_connector_agent_md(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
    ) -> String {
        let spec_owned = self.registry.get_spec(connector);
        let spec = spec_owned.as_ref();
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("connector: {}\n", connector));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", connector));

        // Connector description from spec
        if let Some(desc) = spec.and_then(|s| s.description.as_ref()) {
            out.push_str(desc);
            out.push_str("\n\n");
        }

        // List collections with descriptions from spec
        if let Ok(collections) = self.get_collections_cached(rt, connector) {
            out.push_str("## Collections\n\n");
            for col in &collections {
                out.push_str(&format!("- **{}/**", col.name));
                // Prefer description from spec (richer), fall back to trait
                let spec_desc = spec
                    .and_then(|s| s.collections.iter().find(|c| c.name == col.name))
                    .and_then(|c| c.description.as_ref());
                if let Some(desc) = spec_desc.or(col.description.as_ref()) {
                    out.push_str(&format!(" — {}", desc));
                }
                // Show slug hint if available
                if let Some(hint) = spec
                    .and_then(|s| s.collections.iter().find(|c| c.name == col.name))
                    .and_then(|c| c.slug_hint.as_ref())
                {
                    out.push_str(&format!(" (filenames: {})", hint));
                }
                out.push('\n');
            }
        }

        // Capabilities from spec
        if let Some(caps) = spec.and_then(|s| s.capabilities.as_ref()) {
            out.push_str("\n## Capabilities\n\n");
            let mut cap_list = Vec::new();
            if caps.read.unwrap_or(true) {
                cap_list.push("read");
            }
            if caps.write.unwrap_or(false) {
                cap_list.push("write");
            }
            if caps.create.unwrap_or(false) {
                cap_list.push("create");
            }
            if caps.delete.unwrap_or(false) {
                cap_list.push("delete");
            }
            if caps.drafts.unwrap_or(true) {
                cap_list.push("drafts");
            }
            if caps.versions.unwrap_or(false) {
                cap_list.push("versions");
            }
            if !cap_list.is_empty() {
                out.push_str(&format!("Supported: {}\n", cap_list.join(", ")));
            }
            if let Some(ref rl) = caps.rate_limit {
                if let Some(rpm) = rl.requests_per_minute {
                    out.push_str(&format!("\nRate limit: {} requests/min\n", rpm));
                }
            }
        }

        // Agent tips from spec
        if let Some(tips) = spec
            .and_then(|s| s.agent.as_ref())
            .and_then(|a| a.tips.as_ref())
        {
            if !tips.is_empty() {
                out.push_str("\n## Tips\n\n");
                for tip in tips {
                    out.push_str(&format!("- {}\n", tip));
                }
            }
        }

        // Relationships from spec
        if let Some(rels) = spec
            .and_then(|s| s.agent.as_ref())
            .and_then(|a| a.relationships.as_ref())
        {
            if !rels.is_empty() {
                out.push_str("\n## Relationships\n\n");
                for rel in rels {
                    out.push_str(&format!("- {}\n", rel));
                }
            }
        }

        out.push_str("\n## Usage\n\n");
        out.push_str(&format!("- `ls {}/` — list collections\n", connector));
        out.push_str(&format!(
            "- `ls {}/<collection>/` — list resources\n",
            connector
        ));
        out.push_str(&format!(
            "- `cat {}/<collection>/<resource>.md` — read a resource\n",
            connector
        ));

        out
    }

    pub(crate) fn generate_collection_agent_md(
        &self,
        rt: &tokio::runtime::Handle,
        connector: &str,
        collection: &str,
    ) -> String {
        let spec_owned = self.registry.get_spec(connector);
        let spec = spec_owned.as_ref();
        let col_spec = spec.and_then(|s| s.collections.iter().find(|c| c.name == collection));

        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("connector: {}\n", connector));
        out.push_str(&format!("collection: {}\n", collection));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}/{}\n\n", connector, collection));

        // Collection description from spec
        if let Some(desc) = col_spec.and_then(|c| c.description.as_ref()) {
            out.push_str(desc);
            out.push_str("\n\n");
        }

        // Operations supported
        if let Some(ops) = col_spec.and_then(|c| c.operations.as_ref()) {
            if !ops.is_empty() {
                out.push_str(&format!("**Operations:** {}\n\n", ops.join(", ")));
            }
        }

        // Slug hint
        if let Some(hint) = col_spec.and_then(|c| c.slug_hint.as_ref()) {
            out.push_str(&format!("**Filenames:** {}\n\n", hint));
        }

        // List some resources
        if let Ok(resources) = self.get_resources_cached(rt, connector, collection) {
            out.push_str(&format!("**{} resources available.**\n\n", resources.len()));
            out.push_str("## Sample resources\n\n");
            for res in resources.iter().take(10) {
                out.push_str(&format!("- `{}.md`", res.slug));
                if let Some(ref title) = res.title {
                    out.push_str(&format!(" — {}", title));
                }
                out.push('\n');
            }
            if resources.len() > 10 {
                out.push_str(&format!(
                    "\n... and {} more. Use `ls` to see all.\n",
                    resources.len() - 10
                ));
            }
        }

        // Collection-level relationships
        if let Some(rels) = col_spec.and_then(|c| c.relationships.as_ref()) {
            if !rels.is_empty() {
                out.push_str("\n## Related collections\n\n");
                for rel in rels {
                    out.push_str(&format!("- **{}/**", rel.target));
                    if let Some(ref desc) = rel.description {
                        out.push_str(&format!(" — {}", desc));
                    }
                    out.push('\n');
                }
            }
        }

        out
    }
}

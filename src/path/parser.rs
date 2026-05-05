/// Path convention parser for tapfs.
///
/// Parses filesystem paths (relative to the mount root) into structured
/// representations that the rest of the system can act on.
///
/// Conventions:
///   `/connector/collection/resource.md`         → Live resource
///   `/connector/collection/resource.draft.md`   → Draft (local edits)
///   `/connector/collection/resource.lock`        → Lock file
///   `/connector/collection/resource@v3.md`       → Version 3
///   `/connector/AGENTS.md` or `/AGENTS.md`         → Agent help file

#[derive(Debug, Clone, PartialEq)]
pub enum PathVariant {
    Live,
    Draft,
    Lock,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPath {
    pub connector: String,
    pub collection: Option<String>,
    pub resource: Option<String>,
    pub variant: PathVariant,
    pub version: Option<u32>,
    pub is_agent_md: bool,
}

impl ParsedPath {
    /// Parse a path relative to the mount root.
    ///
    /// The incoming `path` should already have leading `/` stripped and contain
    /// no trailing slash. Empty strings are rejected (returns `None`).
    ///
    /// # Examples
    /// ```
    /// use tapfs::path::parser::{ParsedPath, PathVariant};
    ///
    /// let p = ParsedPath::parse("rest/items/item-1.md").unwrap();
    /// assert_eq!(p.connector, "rest");
    /// assert_eq!(p.collection.as_deref(), Some("items"));
    /// assert_eq!(p.resource.as_deref(), Some("item-1"));
    /// assert_eq!(p.variant, PathVariant::Live);
    /// assert_eq!(p.version, None);
    /// assert!(!p.is_agent_md);
    /// ```
    pub fn parse(path: &str) -> Option<Self> {
        // Normalise: strip leading/trailing slashes
        let path = path.trim_matches('/');
        if path.is_empty() {
            return None;
        }

        let segments: Vec<&str> = path.split('/').collect();

        match segments.len() {
            // ── 1 segment ────────────────────────────────────────────
            // "AGENTS.md" → root-level agent file
            // "rest"     → connector root directory
            1 => {
                let seg = segments[0];
                if seg == "AGENTS.md" {
                    Some(Self {
                        connector: String::new(),
                        collection: None,
                        resource: None,
                        variant: PathVariant::Live,
                        version: None,
                        is_agent_md: true,
                    })
                } else {
                    Some(Self {
                        connector: seg.to_string(),
                        collection: None,
                        resource: None,
                        variant: PathVariant::Live,
                        version: None,
                        is_agent_md: false,
                    })
                }
            }

            // ── 2 segments ───────────────────────────────────────────
            // "rest/AGENTS.md" → connector-level agent file
            // "rest/items"    → collection directory
            2 => {
                let connector = segments[0].to_string();
                let second = segments[1];
                if second == "AGENTS.md" {
                    Some(Self {
                        connector,
                        collection: None,
                        resource: None,
                        variant: PathVariant::Live,
                        version: None,
                        is_agent_md: true,
                    })
                } else {
                    Some(Self {
                        connector,
                        collection: Some(second.to_string()),
                        resource: None,
                        variant: PathVariant::Live,
                        version: None,
                        is_agent_md: false,
                    })
                }
            }

            // ── 3 segments ───────────────────────────────────────────
            // "rest/items/item-1.md"        → live
            // "rest/items/item-1.draft.md"  → draft
            // "rest/items/item-1.lock"      → lock
            // "rest/items/item-1@v3.md"     → version 3
            3 => {
                let connector = segments[0].to_string();
                let collection = segments[1].to_string();
                let filename = segments[2];

                let (resource, variant, version) = Self::parse_filename(filename)?;

                Some(Self {
                    connector,
                    collection: Some(collection),
                    resource: Some(resource),
                    variant,
                    version,
                    is_agent_md: false,
                })
            }

            // Deeper paths are not supported.
            _ => None,
        }
    }

    // ── private helpers ──────────────────────────────────────────────

    /// Parse a filename into (resource_slug, variant, optional_version).
    ///
    /// Supported patterns:
    ///   `name.lock`        → Lock
    ///   `name.draft.md`    → Draft
    ///   `name@v3.md`       → Live + version 3
    ///   `name.md`          → Live
    ///
    /// The `.md` extension is stripped from the resource slug.
    /// For lock files the `.lock` extension is stripped.
    fn parse_filename(filename: &str) -> Option<(String, PathVariant, Option<u32>)> {
        // ── Lock file ────────────────────────────────────────────
        if let Some(base) = filename.strip_suffix(".lock") {
            if base.is_empty() {
                return None;
            }
            return Some((base.to_string(), PathVariant::Lock, None));
        }

        // Everything else must end with `.md`
        let without_md = filename.strip_suffix(".md")?;
        if without_md.is_empty() {
            return None;
        }

        // ── Draft file ───────────────────────────────────────────
        if let Some(base) = without_md.strip_suffix(".draft") {
            if base.is_empty() {
                return None;
            }
            return Some((base.to_string(), PathVariant::Draft, None));
        }

        // ── Versioned file ───────────────────────────────────────
        // Look for `@vN` where N is a positive integer.
        if let Some(at_pos) = without_md.rfind('@') {
            let before_at = &without_md[..at_pos];
            let version_tag = &without_md[at_pos + 1..]; // e.g. "v3"
            if let Some(num_str) = version_tag.strip_prefix('v') {
                if let Ok(ver) = num_str.parse::<u32>() {
                    if !before_at.is_empty() && ver > 0 {
                        return Some((before_at.to_string(), PathVariant::Live, Some(ver)));
                    }
                }
            }
        }

        // ── Plain live file ──────────────────────────────────────
        Some((without_md.to_string(), PathVariant::Live, None))
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    // ── Basic live path ──────────────────────────────────────────

    #[test]
    fn basic_live_resource() {
        let p = ParsedPath::parse("rest/items/item-1.md").unwrap();
        assert_eq!(p.connector, "rest");
        assert_eq!(p.collection.as_deref(), Some("items"));
        assert_eq!(p.resource.as_deref(), Some("item-1"));
        assert_eq!(p.variant, PathVariant::Live);
        assert_eq!(p.version, None);
        assert!(!p.is_agent_md);
    }

    #[test]
    fn live_resource_with_leading_slash() {
        let p = ParsedPath::parse("/rest/items/item-1.md").unwrap();
        assert_eq!(p.connector, "rest");
        assert_eq!(p.resource.as_deref(), Some("item-1"));
        assert_eq!(p.variant, PathVariant::Live);
    }

    // ── Draft path ───────────────────────────────────────────────

    #[test]
    fn draft_path() {
        let p = ParsedPath::parse("rest/items/item-1.draft.md").unwrap();
        assert_eq!(p.resource.as_deref(), Some("item-1"));
        assert_eq!(p.variant, PathVariant::Draft);
        assert_eq!(p.version, None);
    }

    // ── Lock path ────────────────────────────────────────────────

    #[test]
    fn lock_path() {
        let p = ParsedPath::parse("rest/items/item-1.lock").unwrap();
        assert_eq!(p.resource.as_deref(), Some("item-1"));
        assert_eq!(p.variant, PathVariant::Lock);
        assert_eq!(p.version, None);
    }

    // ── Version path ─────────────────────────────────────────────

    #[test]
    fn version_path_v3() {
        let p = ParsedPath::parse("rest/items/item-1@v3.md").unwrap();
        assert_eq!(p.resource.as_deref(), Some("item-1"));
        assert_eq!(p.variant, PathVariant::Live);
        assert_eq!(p.version, Some(3));
    }

    #[test]
    fn version_path_v100() {
        let p = ParsedPath::parse("rest/items/report@v100.md").unwrap();
        assert_eq!(p.resource.as_deref(), Some("report"));
        assert_eq!(p.version, Some(100));
    }

    // ── Agent.md ─────────────────────────────────────────────────

    #[test]
    fn root_agent_md() {
        let p = ParsedPath::parse("AGENTS.md").unwrap();
        assert!(p.is_agent_md);
        assert_eq!(p.connector, "");
        assert_eq!(p.collection, None);
        assert_eq!(p.resource, None);
    }

    #[test]
    fn connector_agent_md() {
        let p = ParsedPath::parse("rest/AGENTS.md").unwrap();
        assert!(p.is_agent_md);
        assert_eq!(p.connector, "rest");
        assert_eq!(p.collection, None);
    }

    // ── Connector root / collection ──────────────────────────────

    #[test]
    fn connector_root() {
        let p = ParsedPath::parse("rest").unwrap();
        assert_eq!(p.connector, "rest");
        assert_eq!(p.collection, None);
        assert_eq!(p.resource, None);
        assert!(!p.is_agent_md);
    }

    #[test]
    fn collection_dir() {
        let p = ParsedPath::parse("rest/items").unwrap();
        assert_eq!(p.connector, "rest");
        assert_eq!(p.collection.as_deref(), Some("items"));
        assert_eq!(p.resource, None);
    }

    // ── Edge cases ───────────────────────────────────────────────

    #[test]
    fn dots_in_resource_name() {
        let p = ParsedPath::parse("rest/items/my.cool.resource.md").unwrap();
        assert_eq!(p.resource.as_deref(), Some("my.cool.resource"));
        assert_eq!(p.variant, PathVariant::Live);
    }

    #[test]
    fn dots_in_resource_name_draft() {
        let p = ParsedPath::parse("rest/items/my.cool.resource.draft.md").unwrap();
        assert_eq!(p.resource.as_deref(), Some("my.cool.resource"));
        assert_eq!(p.variant, PathVariant::Draft);
    }

    #[test]
    fn unicode_slug() {
        let p = ParsedPath::parse("rest/items/données-été.md").unwrap();
        assert_eq!(p.resource.as_deref(), Some("données-été"));
        assert_eq!(p.variant, PathVariant::Live);
    }

    #[test]
    fn unicode_connector_and_collection() {
        let p = ParsedPath::parse("接続/コレクション/リソース.md").unwrap();
        assert_eq!(p.connector, "接続");
        assert_eq!(p.collection.as_deref(), Some("コレクション"));
        assert_eq!(p.resource.as_deref(), Some("リソース"));
    }

    // ── Missing / invalid ────────────────────────────────────────

    #[test]
    fn empty_string() {
        assert!(ParsedPath::parse("").is_none());
    }

    #[test]
    fn only_slashes() {
        assert!(ParsedPath::parse("///").is_none());
    }

    #[test]
    fn too_many_segments() {
        assert!(ParsedPath::parse("a/b/c/d").is_none());
    }

    #[test]
    fn bare_md_extension() {
        // ".md" with no resource name
        assert!(ParsedPath::parse("rest/items/.md").is_none());
    }

    #[test]
    fn bare_lock_extension() {
        assert!(ParsedPath::parse("rest/items/.lock").is_none());
    }

    #[test]
    fn version_zero_treated_as_literal() {
        // @v0 is not a valid version (versions start at 1), so the
        // entire "item@v0" is treated as the resource slug.
        let p = ParsedPath::parse("rest/items/item@v0.md").unwrap();
        assert_eq!(p.resource.as_deref(), Some("item@v0"));
        assert_eq!(p.version, None);
        assert_eq!(p.variant, PathVariant::Live);
    }

    #[test]
    fn version_missing_number() {
        // @v with no digits → treated as a normal resource name
        let p = ParsedPath::parse("rest/items/item@v.md").unwrap();
        assert_eq!(p.resource.as_deref(), Some("item@v"));
        assert_eq!(p.version, None);
    }

    #[test]
    fn trailing_slash_stripped() {
        let p = ParsedPath::parse("rest/items/").unwrap();
        assert_eq!(p.connector, "rest");
        assert_eq!(p.collection.as_deref(), Some("items"));
        assert_eq!(p.resource, None);
    }
}

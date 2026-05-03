//! Confluence Cloud connector for tapfs.
//!
//! Exposes Confluence spaces and pages as filesystem collections.
//! Uses shared Atlassian auth (same as Jira).

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use dashmap::DashMap;

use crate::connector::atlassian_auth::{
    escape_yaml, sanitize_slug, strip_frontmatter_str, AtlassianAuth,
};
use crate::connector::traits::{CollectionInfo, Connector, Resource, ResourceMeta, VersionInfo};

pub struct ConfluenceConnector {
    auth: AtlassianAuth,
    slug_to_id: DashMap<String, String>,
}

impl ConfluenceConnector {
    pub fn new() -> Result<Self> {
        let auth =
            AtlassianAuth::from_env().context("initializing Atlassian auth for Confluence")?;
        tracing::info!(base_url = %auth.base_url, "Confluence connector initialized");
        Ok(Self {
            auth,
            slug_to_id: DashMap::new(),
        })
    }

    fn cache_slug(&self, collection: &str, slug: &str, id: &str) {
        let key = format!("{}/{}", collection, slug);
        self.slug_to_id.insert(key, id.to_string());
    }

    fn resolve_id(&self, collection: &str, slug: &str) -> String {
        let key = format!("{}/{}", collection, slug);
        self.slug_to_id
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_else(|| slug.to_string())
    }

    // -----------------------------------------------------------------------
    // Spaces
    // -----------------------------------------------------------------------

    async fn list_spaces(&self) -> Result<Vec<ResourceMeta>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut url = format!(
                "{}/wiki/api/v2/spaces?limit=100&sort=name",
                self.auth.base_url
            );
            if let Some(ref c) = cursor {
                url.push_str(&format!("&cursor={}", c));
            }

            tracing::debug!(url = %url, "confluence: listing spaces");
            let json = self.auth.get_json(&url).await?;

            if let Some(results) = json.get("results").and_then(|v| v.as_array()) {
                for space in results {
                    let id = space.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    let key = space
                        .get("key")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let name = space.get("name").and_then(|v| v.as_str()).unwrap_or(key);

                    all.push(ResourceMeta {
                        id: id.to_string(),
                        slug: sanitize_slug(key),
                        title: Some(name.to_string()),
                        updated_at: None,
                        content_type: Some("confluence/space".to_string()),
                        group: None,
                    });
                }
            }

            // Pagination
            cursor = json
                .pointer("/_links/next")
                .and_then(|v| v.as_str())
                .and_then(|next| {
                    // Extract cursor from next URL
                    next.split("cursor=")
                        .nth(1)
                        .map(|c| c.split('&').next().unwrap_or(c).to_string())
                });

            if cursor.is_none() {
                break;
            }
        }

        Ok(all)
    }

    async fn read_space(&self, space_key: &str) -> Result<Resource> {
        // Get space info
        let url = format!(
            "{}/wiki/api/v2/spaces?keys={}&limit=1",
            self.auth.base_url, space_key
        );
        let json = self.auth.get_json(&url).await?;
        let space = json
            .get("results")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("space '{}' not found", space_key))?;

        let name = space
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(space_key);
        let description = space
            .pointer("/description/plain/value")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // List top pages in this space
        let pages_url = format!(
            "{}/wiki/api/v2/spaces/{}/pages?limit=25&sort=-modified-date",
            self.auth.base_url,
            space
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(space_key)
        );
        let pages_json = self.auth.get_json(&pages_url).await.ok();

        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("key: \"{}\"\n", space_key));
        out.push_str(&format!("name: \"{}\"\n", escape_yaml(name)));
        out.push_str("type: \"confluence/space\"\n");
        out.push_str("operations: [read]\n");
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", name));

        if !description.is_empty() {
            out.push_str(description);
            out.push_str("\n\n");
        }

        if let Some(pj) = pages_json {
            if let Some(pages) = pj.get("results").and_then(|v| v.as_array()) {
                out.push_str("## Pages\n\n");
                for page in pages {
                    let title = page
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Untitled");
                    let status = page
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("current");
                    out.push_str(&format!("- **{}** ({})\n", title, status));
                }
            }
        }

        let meta = ResourceMeta {
            id: space_key.to_string(),
            slug: sanitize_slug(space_key),
            title: Some(name.to_string()),
            updated_at: None,
            content_type: Some("confluence/space".to_string()),
            group: None,
        };

        Ok(Resource {
            meta,
            content: out.into_bytes(),
            raw_json: None,
        })
    }

    // -----------------------------------------------------------------------
    // Pages
    // -----------------------------------------------------------------------

    async fn list_pages(&self) -> Result<Vec<ResourceMeta>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut url = format!(
                "{}/wiki/api/v2/pages?limit=100&sort=-modified-date",
                self.auth.base_url
            );
            if let Some(ref c) = cursor {
                url.push_str(&format!("&cursor={}", c));
            }

            tracing::debug!(url = %url, "confluence: listing pages");
            let json = self.auth.get_json(&url).await?;

            if let Some(results) = json.get("results").and_then(|v| v.as_array()) {
                for page in results {
                    let id = page.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    let title = page
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Untitled");
                    let space_id = page.get("spaceId").and_then(|v| v.as_str());
                    let status = page.get("status").and_then(|v| v.as_str());
                    let version = page.pointer("/version/number");
                    let modified = page.pointer("/version/createdAt").and_then(|v| v.as_str());

                    let slug = sanitize_slug(title);

                    all.push(ResourceMeta {
                        id: id.to_string(),
                        slug,
                        title: Some(title.to_string()),
                        updated_at: modified.map(|s| s.to_string()),
                        content_type: Some(format!(
                            "confluence/page{}{}",
                            space_id
                                .map(|s| format!(";space={}", s))
                                .unwrap_or_default(),
                            status.map(|s| format!(";status={}", s)).unwrap_or_default(),
                        )),
                        group: None,
                    });

                    // Track version for potential version history
                    let _ = version;
                }
            }

            cursor = json
                .pointer("/_links/next")
                .and_then(|v| v.as_str())
                .and_then(|next| {
                    next.split("cursor=")
                        .nth(1)
                        .map(|c| c.split('&').next().unwrap_or(c).to_string())
                });

            if cursor.is_none() {
                break;
            }
        }

        Ok(all)
    }

    async fn read_page(&self, id: &str) -> Result<Resource> {
        let url = format!(
            "{}/wiki/api/v2/pages/{}?body-format=storage",
            self.auth.base_url, id
        );
        tracing::debug!(url = %url, "confluence: reading page");
        let json = self.auth.get_json(&url).await?;

        let title = json
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled");
        let status = json
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("current");
        let space_id = json.get("spaceId").and_then(|v| v.as_str()).unwrap_or("");
        let version_num = json.pointer("/version/number").and_then(|v| v.as_u64());
        let author = json
            .pointer("/version/authorId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let created = json.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
        let modified = json
            .pointer("/version/createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Get the storage format body
        let body_storage = json
            .pointer("/body/storage/value")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let body_markdown = storage_to_markdown(body_storage);

        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: \"{}\"\n", id));
        out.push_str(&format!("title: \"{}\"\n", escape_yaml(title)));
        out.push_str(&format!("space: \"{}\"\n", space_id));
        out.push_str(&format!("status: \"{}\"\n", status));
        out.push_str(&format!("author: \"{}\"\n", author));
        out.push_str(&format!("created: \"{}\"\n", created));
        out.push_str(&format!("updated: \"{}\"\n", modified));
        if let Some(v) = version_num {
            out.push_str(&format!("version: {}\n", v));
        }
        out.push_str("operations: [read, write, draft]\n");
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", title));
        out.push_str(&body_markdown);
        out.push('\n');

        let meta = ResourceMeta {
            id: id.to_string(),
            slug: sanitize_slug(title),
            title: Some(title.to_string()),
            updated_at: Some(modified.to_string()),
            content_type: Some("confluence/page".to_string()),
            group: None,
        };

        Ok(Resource {
            meta,
            content: out.into_bytes(),
            raw_json: None,
        })
    }

    async fn write_page(&self, id: &str, content: &[u8]) -> Result<()> {
        let text = String::from_utf8_lossy(content);
        let body_text = strip_frontmatter_str(&text);

        // Convert markdown back to storage format
        let storage = markdown_to_storage(body_text);

        // Get current page to find version number
        let get_url = format!("{}/wiki/api/v2/pages/{}", self.auth.base_url, id);
        let current = self.auth.get_json(&get_url).await?;
        let current_version = current
            .pointer("/version/number")
            .and_then(|v| v.as_u64())
            .unwrap_or(1);
        let title = current
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled");
        let status = current
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("current");

        let update_body = serde_json::json!({
            "id": id,
            "status": status,
            "title": title,
            "body": {
                "representation": "storage",
                "value": storage
            },
            "version": {
                "number": current_version + 1,
                "message": "Updated via tapfs"
            }
        });

        let url = format!("{}/wiki/api/v2/pages/{}", self.auth.base_url, id);
        self.auth.put_json(&url, &update_body).await?;

        tracing::info!(page_id = %id, "updated Confluence page");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Page versions
    // -----------------------------------------------------------------------

    async fn list_page_versions(&self, page_id: &str) -> Result<Vec<VersionInfo>> {
        let url = format!(
            "{}/wiki/api/v2/pages/{}/versions?limit=25&sort=-modified-date",
            self.auth.base_url, page_id
        );
        let json = self.auth.get_json(&url).await?;

        let mut versions = Vec::new();
        if let Some(results) = json.get("results").and_then(|v| v.as_array()) {
            for v in results {
                let number = v.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
                let created = v
                    .get("createdAt")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();

                versions.push(VersionInfo {
                    version: number as u32,
                    created_at: created,
                    size: 0,
                });
            }
        }

        Ok(versions)
    }

    async fn read_page_version(&self, page_id: &str, version: u32) -> Result<Resource> {
        let url = format!(
            "{}/wiki/api/v2/pages/{}?body-format=storage&version={}",
            self.auth.base_url, page_id, version
        );
        let json = self.auth.get_json(&url).await?;

        let title = json
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled");
        let body_storage = json
            .pointer("/body/storage/value")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let body_markdown = storage_to_markdown(body_storage);

        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: \"{}\"\n", page_id));
        out.push_str(&format!("title: \"{}\"\n", escape_yaml(title)));
        out.push_str(&format!("version: {}\n", version));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", title));
        out.push_str(&body_markdown);
        out.push('\n');

        let meta = ResourceMeta {
            id: page_id.to_string(),
            slug: sanitize_slug(title),
            title: Some(title.to_string()),
            updated_at: None,
            content_type: Some("confluence/page".to_string()),
            group: None,
        };

        Ok(Resource {
            meta,
            content: out.into_bytes(),
            raw_json: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Connector trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Connector for ConfluenceConnector {
    fn name(&self) -> &str {
        "confluence"
    }

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        Ok(vec![
            CollectionInfo {
                name: "pages".to_string(),
                description: Some("Confluence pages (recently modified)".to_string()),
            },
            CollectionInfo {
                name: "spaces".to_string(),
                description: Some("Confluence spaces".to_string()),
            },
        ])
    }

    async fn list_resources(&self, collection: &str) -> Result<Vec<ResourceMeta>> {
        let resources = match collection {
            "pages" => self.list_pages().await?,
            "spaces" => self.list_spaces().await?,
            _ => return Err(anyhow!("unknown collection: '{}'", collection)),
        };

        for r in &resources {
            self.cache_slug(collection, &r.slug, &r.id);
        }

        Ok(resources)
    }

    async fn read_resource(&self, collection: &str, id: &str) -> Result<Resource> {
        let resolved = self.resolve_id(collection, id);
        match collection {
            "pages" => self.read_page(&resolved).await,
            "spaces" => self.read_space(&resolved).await,
            _ => Err(anyhow!("unknown collection: '{}'", collection)),
        }
    }

    async fn write_resource(&self, collection: &str, id: &str, content: &[u8]) -> Result<()> {
        let resolved = self.resolve_id(collection, id);
        match collection {
            "pages" => self.write_page(&resolved, content).await,
            _ => Err(anyhow!(
                "write not supported for collection '{}'",
                collection
            )),
        }
    }

    async fn resource_versions(&self, collection: &str, id: &str) -> Result<Vec<VersionInfo>> {
        let resolved = self.resolve_id(collection, id);
        match collection {
            "pages" => self.list_page_versions(&resolved).await,
            _ => Ok(vec![]),
        }
    }

    async fn read_version(&self, collection: &str, id: &str, version: u32) -> Result<Resource> {
        let resolved = self.resolve_id(collection, id);
        match collection {
            "pages" => self.read_page_version(&resolved, version).await,
            _ => Err(anyhow!(
                "versions not supported for collection '{}'",
                collection
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Confluence storage format → Markdown conversion
// ---------------------------------------------------------------------------

/// Convert Confluence storage format (XHTML-like) to Markdown.
fn storage_to_markdown(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut in_code_block = false;

    while let Some(c) = chars.next() {
        if c == '<' {
            // Read the tag
            let mut tag = String::new();
            for tc in chars.by_ref() {
                if tc == '>' {
                    break;
                }
                tag.push(tc);
            }

            let tag_lower = tag.to_lowercase();
            let is_closing = tag_lower.starts_with('/');
            let tag_name = if is_closing {
                tag_lower[1..].split_whitespace().next().unwrap_or("")
            } else {
                tag_lower.split_whitespace().next().unwrap_or("")
            };

            match tag_name {
                "h1" if !is_closing => out.push_str("# "),
                "h2" if !is_closing => out.push_str("## "),
                "h3" if !is_closing => out.push_str("### "),
                "h4" if !is_closing => out.push_str("#### "),
                "h5" if !is_closing => out.push_str("##### "),
                "h6" if !is_closing => out.push_str("###### "),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if is_closing => {
                    out.push_str("\n\n");
                }
                "p" if is_closing => out.push_str("\n\n"),
                "br" | "br/" => out.push('\n'),
                "strong" | "b" if !is_closing => out.push_str("**"),
                "strong" | "b" if is_closing => out.push_str("**"),
                "em" | "i" if !is_closing => out.push('*'),
                "em" | "i" if is_closing => out.push('*'),
                "code" if !is_closing && !in_code_block => out.push('`'),
                "code" if is_closing && !in_code_block => out.push('`'),
                "li" if !is_closing => out.push_str("- "),
                "li" if is_closing => out.push('\n'),
                "blockquote" if !is_closing => out.push_str("> "),
                "blockquote" if is_closing => out.push('\n'),
                "hr" | "hr/" => out.push_str("\n---\n\n"),
                "a" if !is_closing => {
                    // Extract href
                    if let Some(href_start) = tag.find("href=\"") {
                        let href = &tag[href_start + 6..];
                        if let Some(href_end) = href.find('"') {
                            out.push('[');
                            // Read until </a>, capture text
                            let mut link_text = String::new();
                            for lc in chars.by_ref() {
                                if lc == '<' {
                                    // Consume </a>
                                    for tc in chars.by_ref() {
                                        if tc == '>' {
                                            break;
                                        }
                                    }
                                    break;
                                }
                                link_text.push(lc);
                            }
                            out.push_str(&link_text);
                            out.push_str("](");
                            out.push_str(&href[..href_end]);
                            out.push(')');
                        }
                    }
                }
                _ if tag_lower.contains("ac:name=\"code\"") => {
                    in_code_block = true;
                    out.push_str("\n```\n");
                }
                "ac:structured-macro" if is_closing && in_code_block => {
                    in_code_block = false;
                    out.push_str("\n```\n\n");
                }
                "ac:plain-text-body" | "ac:rich-text-body" => {
                    // Content container, skip tag
                }
                "table" if !is_closing => out.push('\n'),
                "tr" if is_closing => out.push_str("|\n"),
                "th" | "td" if !is_closing => out.push_str("| "),
                "th" | "td" if is_closing => out.push(' '),
                "img" => {
                    if let Some(alt_start) = tag.find("alt=\"") {
                        let alt = &tag[alt_start + 4..];
                        if let Some(alt_end) = alt.find('"') {
                            out.push_str(&format!("![{}]", &alt[..alt_end]));
                        }
                    }
                    if let Some(src_start) = tag.find("src=\"") {
                        let src = &tag[src_start + 4..];
                        if let Some(src_end) = src.find('"') {
                            out.push_str(&format!("({})", &src[..src_end]));
                        }
                    }
                }
                _ => {
                    // Skip unknown tags
                }
            }
        } else {
            // Decode HTML entities
            if c == '&' {
                let mut entity = String::new();
                for ec in chars.by_ref() {
                    if ec == ';' {
                        break;
                    }
                    entity.push(ec);
                }
                match entity.as_str() {
                    "amp" => out.push('&'),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "quot" => out.push('"'),
                    "apos" => out.push('\''),
                    "nbsp" => out.push(' '),
                    _ => {
                        out.push('&');
                        out.push_str(&entity);
                        out.push(';');
                    }
                }
            } else {
                out.push(c);
            }
        }
    }

    // Clean up excessive newlines
    let mut cleaned = String::with_capacity(out.len());
    let mut consecutive_newlines = 0;
    for c in out.chars() {
        if c == '\n' {
            consecutive_newlines += 1;
            if consecutive_newlines <= 2 {
                cleaned.push(c);
            }
        } else {
            consecutive_newlines = 0;
            cleaned.push(c);
        }
    }

    cleaned.trim().to_string()
}

/// Convert Markdown to Confluence storage format (simple conversion).
fn markdown_to_storage(markdown: &str) -> String {
    let mut out = String::new();
    let mut in_code_block = false;

    for line in markdown.lines() {
        if line.starts_with("```") {
            if in_code_block {
                out.push_str("</ac:plain-text-body></ac:structured-macro>");
                in_code_block = false;
            } else {
                out.push_str("<ac:structured-macro ac:name=\"code\"><ac:plain-text-body><![CDATA[");
                in_code_block = true;
            }
            continue;
        }

        if in_code_block {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if line.starts_with("######") {
            out.push_str(&format!("<h6>{}</h6>", escape_html(line[7..].trim())));
        } else if line.starts_with("#####") {
            out.push_str(&format!("<h5>{}</h5>", escape_html(line[6..].trim())));
        } else if line.starts_with("####") {
            out.push_str(&format!("<h4>{}</h4>", escape_html(line[5..].trim())));
        } else if line.starts_with("###") {
            out.push_str(&format!("<h3>{}</h3>", escape_html(line[4..].trim())));
        } else if line.starts_with("##") {
            out.push_str(&format!("<h2>{}</h2>", escape_html(line[3..].trim())));
        } else if line.starts_with('#') {
            out.push_str(&format!("<h1>{}</h1>", escape_html(line[2..].trim())));
        } else if let Some(rest) = line.strip_prefix("- ") {
            out.push_str(&format!("<ul><li>{}</li></ul>", escape_html(rest)));
        } else if let Some(rest) = line.strip_prefix("> ") {
            out.push_str(&format!(
                "<blockquote><p>{}</p></blockquote>",
                escape_html(rest)
            ));
        } else if line.starts_with("---") {
            out.push_str("<hr />");
        } else if line.is_empty() {
            // Skip empty lines (they become paragraph breaks)
        } else {
            // Inline formatting
            let formatted = line
                .replace("**", "<strong>") // simplified — doesn't pair open/close
                .replace('*', "<em>");
            out.push_str(&format!("<p>{}</p>", formatted));
        }
    }

    if in_code_block {
        out.push_str("]]></ac:plain-text-body></ac:structured-macro>");
    }

    out
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_to_markdown_basic() {
        let html = "<h1>Title</h1><p>Hello <strong>world</strong></p>";
        let md = storage_to_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("**world**"));
    }

    #[test]
    fn test_storage_to_markdown_list() {
        let html = "<ul><li>Item 1</li><li>Item 2</li></ul>";
        let md = storage_to_markdown(html);
        assert!(md.contains("- Item 1"));
        assert!(md.contains("- Item 2"));
    }

    #[test]
    fn test_storage_to_markdown_entities() {
        let html = "<p>A &amp; B &lt; C</p>";
        let md = storage_to_markdown(html);
        assert!(md.contains("A & B < C"));
    }

    #[test]
    fn test_markdown_to_storage() {
        let md = "# Title\n\nHello world\n\n- Item 1\n- Item 2";
        let storage = markdown_to_storage(md);
        assert!(storage.contains("<h1>Title</h1>"));
        assert!(storage.contains("<li>Item 1</li>"));
    }

    #[test]
    fn test_escape_html() {
        assert_eq!(escape_html("a < b & c"), "a &lt; b &amp; c");
    }
}

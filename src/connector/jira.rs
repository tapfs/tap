//! Jira Cloud connector for tapfs.
//!
//! Exposes Jira projects, issues, and boards as filesystem collections.
//! Authentication uses Atlassian Cloud Basic auth (shared with Confluence).

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;

use crate::connector::atlassian_auth::{
    escape_yaml, extract_frontmatter, sanitize_slug, strip_frontmatter_str, AtlassianAuth,
};
use crate::connector::traits::{CollectionInfo, Connector, Resource, ResourceMeta, VersionInfo};

// ---------------------------------------------------------------------------
// Jira connector
// ---------------------------------------------------------------------------

pub struct JiraConnector {
    auth: AtlassianAuth,
    /// Maps "collection/slug" -> API resource ID.
    slug_to_id: DashMap<String, String>,
}

impl JiraConnector {
    pub fn new(creds: &crate::credentials::CredentialStore) -> Result<Self> {
        Self::new_with_overrides(creds, None, None)
    }

    /// Construct with declarative-config overrides for base_url and/or token,
    /// applied on top of `creds`. Sourced from a `service.yaml` entry by the
    /// factory.
    pub fn new_with_overrides(
        creds: &crate::credentials::CredentialStore,
        base_url_override: Option<&str>,
        token_override: Option<&str>,
    ) -> Result<Self> {
        let auth =
            AtlassianAuth::load_with_overrides("jira", creds, base_url_override, token_override)
                .context("loading Atlassian auth for Jira")?;
        tracing::info!(base_url = %auth.base_url, "Jira connector initialized");
        Ok(Self {
            auth,
            slug_to_id: DashMap::new(),
        })
    }

    /// Cache a slug -> ID mapping.
    fn cache_slug(&self, collection: &str, slug: &str, id: &str) {
        let key = format!("{}/{}", collection, slug);
        self.slug_to_id.insert(key, id.to_string());
    }

    /// Resolve a slug to its API ID. Falls back to the slug itself.
    fn resolve_id(&self, collection: &str, slug: &str) -> String {
        let key = format!("{}/{}", collection, slug);
        self.slug_to_id
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_else(|| slug.to_string())
    }

    // -----------------------------------------------------------------------
    // my-issues
    // -----------------------------------------------------------------------

    async fn list_my_issues(&self) -> Result<Vec<ResourceMeta>> {
        let url = format!(
            "{}/rest/api/3/search/jql?jql=assignee=currentUser()+ORDER+BY+updated+DESC\
             &maxResults=100\
             &fields=key,summary,status,assignee,priority,updated",
            self.auth.base_url
        );

        let json = self.auth.get_json(&url).await?;
        let issues = json
            .get("issues")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut resources = Vec::new();
        for issue in &issues {
            let key = issue
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let fields = issue.get("fields").unwrap_or(&Value::Null);
            let summary = fields
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("(no summary)");
            let updated = fields
                .get("updated")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let slug = key.to_string();
            self.cache_slug("my-issues", &slug, key);

            resources.push(ResourceMeta {
                id: key.to_string(),
                slug,
                title: Some(format!("{}: {}", key, summary)),
                updated_at: updated,
                content_type: Some("text/markdown".to_string()),
                group: None,
            });
        }

        Ok(resources)
    }

    async fn read_issue(&self, key: &str) -> Result<Resource> {
        let url = format!(
            "{}/rest/api/3/issue/{}?expand=renderedFields",
            self.auth.base_url, key
        );
        let json = self.auth.get_json(&url).await?;

        let fields = json.get("fields").unwrap_or(&Value::Null);

        let summary = fields
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("(no summary)");
        let status = fields
            .get("status")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        let priority = fields
            .get("priority")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("None");
        let issue_type = fields
            .get("issuetype")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Task");
        let assignee = fields
            .get("assignee")
            .and_then(|v| v.get("emailAddress"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unassigned");
        let reporter = fields
            .get("reporter")
            .and_then(|v| v.get("emailAddress"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        let project_key = fields
            .get("project")
            .and_then(|v| v.get("key"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let created = fields.get("created").and_then(|v| v.as_str()).unwrap_or("");
        let updated = fields.get("updated").and_then(|v| v.as_str()).unwrap_or("");

        // Convert ADF description to Markdown
        let description = fields
            .get("description")
            .map(adf_to_markdown)
            .unwrap_or_default();

        // Fetch comments
        let comments_md = self.fetch_comments_markdown(key).await?;

        // Build markdown output
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("key: \"{}\"\n", key));
        out.push_str(&format!("summary: \"{}\"\n", escape_yaml(summary)));
        out.push_str(&format!("status: \"{}\"\n", escape_yaml(status)));
        out.push_str(&format!("assignee: \"{}\"\n", escape_yaml(assignee)));
        out.push_str(&format!("priority: \"{}\"\n", escape_yaml(priority)));
        out.push_str(&format!("project: \"{}\"\n", project_key));
        out.push_str(&format!("type: \"{}\"\n", escape_yaml(issue_type)));
        out.push_str(&format!("created: \"{}\"\n", created));
        out.push_str(&format!("updated: \"{}\"\n", updated));
        out.push_str("operations: [read, write, draft, lock]\n");
        out.push_str("---\n\n");

        out.push_str(&format!("# {}: {}\n\n", key, summary));
        out.push_str(&format!(
            "**Status:** {} | **Priority:** {} | **Type:** {}\n",
            status, priority, issue_type
        ));
        out.push_str(&format!(
            "**Assignee:** {} | **Reporter:** {}\n\n",
            assignee, reporter
        ));

        if !description.trim().is_empty() {
            out.push_str("## Description\n\n");
            out.push_str(&description);
            out.push_str("\n\n");
        }

        if !comments_md.is_empty() {
            out.push_str("## Comments\n\n");
            out.push_str(&comments_md);
        }

        let meta = ResourceMeta {
            id: key.to_string(),
            slug: key.to_string(),
            title: Some(format!("{}: {}", key, summary)),
            updated_at: Some(updated.to_string()),
            content_type: Some("text/markdown".to_string()),
            group: None,
        };

        Ok(Resource {
            meta,
            content: out.into_bytes(),
            raw_json: None,
        })
    }

    async fn fetch_comments_markdown(&self, key: &str) -> Result<String> {
        let url = format!(
            "{}/rest/api/3/issue/{}/comment?orderBy=-created&maxResults=50",
            self.auth.base_url, key
        );
        let json = self.auth.get_json(&url).await?;

        let comments = json
            .get("comments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut out = String::new();
        for comment in &comments {
            let author = comment
                .get("author")
                .and_then(|v| v.get("emailAddress"))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    comment
                        .get("author")
                        .and_then(|v| v.get("displayName"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("Unknown");
            let created = comment
                .get("created")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Format the date nicely
            let date_display = if created.len() >= 16 {
                created[..16].replace('T', " ")
            } else {
                created.to_string()
            };

            let body = comment.get("body").map(adf_to_markdown).unwrap_or_default();

            out.push_str(&format!("**{}** ({}):\n", author, date_display));
            // Indent comment body as blockquote
            for line in body.lines() {
                out.push_str(&format!("> {}\n", line));
            }
            out.push('\n');
        }

        Ok(out)
    }

    async fn write_issue(&self, key: &str, content: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(content).context("content is not valid UTF-8")?;

        // Check if content has a new comment section
        let body_text = strip_frontmatter_str(text);

        // Try to parse frontmatter to update fields
        if let Some(frontmatter) = extract_frontmatter(text) {
            let mut update_fields = serde_json::Map::new();

            // Update summary if changed
            if let Some(summary) = frontmatter.get("summary").and_then(|v| v.as_str()) {
                update_fields.insert("summary".to_string(), Value::String(summary.to_string()));
            }

            // Extract description from body text
            let description = extract_description_from_body(body_text);
            if !description.is_empty() {
                update_fields.insert("description".to_string(), markdown_to_adf(&description));
            }

            if !update_fields.is_empty() {
                let update_body = serde_json::json!({
                    "fields": update_fields
                });
                let url = format!("{}/rest/api/3/issue/{}", self.auth.base_url, key);
                self.auth.put_json(&url, &update_body).await?;
            }

            // Handle status transitions
            if let Some(status) = frontmatter.get("status").and_then(|v| v.as_str()) {
                let _ = self.transition_issue(key, status).await;
            }
        }

        // Check for new comments (lines after "## Comments" that are not blockquoted)
        let new_comment = extract_new_comment(body_text);
        if let Some(comment_text) = new_comment {
            self.add_comment(key, &comment_text).await?;
        }

        Ok(())
    }

    async fn transition_issue(&self, key: &str, target_status: &str) -> Result<()> {
        // Get available transitions
        let url = format!(
            "{}/rest/api/3/issue/{}/transitions",
            self.auth.base_url, key
        );
        let json = self.auth.get_json(&url).await?;

        let transitions = json
            .get("transitions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Find the matching transition
        let target_lower = target_status.to_lowercase();
        for transition in &transitions {
            let name = transition
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let to_name = transition
                .get("to")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if name.to_lowercase() == target_lower || to_name.to_lowercase() == target_lower {
                let transition_id = transition
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();

                let body = serde_json::json!({
                    "transition": { "id": transition_id }
                });
                let transition_url = format!(
                    "{}/rest/api/3/issue/{}/transitions",
                    self.auth.base_url, key
                );
                self.auth.post_json(&transition_url, &body).await?;
                tracing::info!(key = %key, status = %target_status, "transitioned issue");
                return Ok(());
            }
        }

        tracing::warn!(
            key = %key,
            status = %target_status,
            "no matching transition found"
        );
        Ok(())
    }

    async fn add_comment(&self, key: &str, text: &str) -> Result<()> {
        let body = serde_json::json!({
            "body": markdown_to_adf(text)
        });
        let url = format!("{}/rest/api/3/issue/{}/comment", self.auth.base_url, key);
        self.auth.post_json(&url, &body).await?;
        tracing::info!(key = %key, "added comment");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // projects
    // -----------------------------------------------------------------------

    async fn list_projects(&self) -> Result<Vec<ResourceMeta>> {
        let url = format!("{}/rest/api/3/project?maxResults=100", self.auth.base_url);
        let json = self.auth.get_json(&url).await?;

        // The response is an array directly
        let projects = json.as_array().cloned().unwrap_or_default();

        let mut resources = Vec::new();
        for project in &projects {
            let key = project
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let name = project
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("(no name)");
            let id = project
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            let slug = key.to_string();
            self.cache_slug("projects", &slug, id);

            resources.push(ResourceMeta {
                id: id.to_string(),
                slug,
                title: Some(format!("{} ({})", name, key)),
                updated_at: None,
                content_type: Some("text/markdown".to_string()),
                group: None,
            });
        }

        Ok(resources)
    }

    async fn read_project(&self, key: &str) -> Result<Resource> {
        let url = format!("{}/rest/api/3/project/{}", self.auth.base_url, key);
        let json = self.auth.get_json(&url).await?;

        let name = json
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("(no name)");
        let project_key = json.get("key").and_then(|v| v.as_str()).unwrap_or(key);
        let description_text = json
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lead = json
            .get("lead")
            .and_then(|v| v.get("emailAddress"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                json.get("lead")
                    .and_then(|v| v.get("displayName"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("Unknown");
        let project_type = json
            .get("projectTypeKey")
            .and_then(|v| v.as_str())
            .unwrap_or("software");

        // Fetch recent issues for this project
        let issues_url = format!(
            "{}/rest/api/3/search/jql?jql=project={}+ORDER+BY+updated+DESC\
             &maxResults=20&fields=key,summary,status,updated",
            self.auth.base_url, project_key
        );
        let issues_json = self.auth.get_json(&issues_url).await.unwrap_or(Value::Null);
        let issues = issues_json
            .get("issues")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Build output
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("key: \"{}\"\n", project_key));
        out.push_str(&format!("name: \"{}\"\n", escape_yaml(name)));
        out.push_str(&format!("type: \"{}\"\n", project_type));
        out.push_str(&format!("lead: \"{}\"\n", escape_yaml(lead)));
        out.push_str("operations: [read]\n");
        out.push_str("---\n\n");

        out.push_str(&format!("# {} ({})\n\n", name, project_key));
        out.push_str(&format!(
            "**Lead:** {} | **Type:** {}\n\n",
            lead, project_type
        ));

        if !description_text.is_empty() {
            out.push_str("## Description\n\n");
            out.push_str(description_text);
            out.push_str("\n\n");
        }

        if !issues.is_empty() {
            out.push_str("## Recent Issues\n\n");
            out.push_str("| Key | Summary | Status | Updated |\n");
            out.push_str("|-----|---------|--------|----------|\n");
            for issue in &issues {
                let ikey = issue.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let fields = issue.get("fields").unwrap_or(&Value::Null);
                let summary = fields.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                let status = fields
                    .get("status")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let updated = fields.get("updated").and_then(|v| v.as_str()).unwrap_or("");
                let date_display = if updated.len() >= 10 {
                    &updated[..10]
                } else {
                    updated
                };
                out.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ikey, summary, status, date_display
                ));
            }
        }

        let id = json.get("id").and_then(|v| v.as_str()).unwrap_or(key);

        let meta = ResourceMeta {
            id: id.to_string(),
            slug: project_key.to_string(),
            title: Some(format!("{} ({})", name, project_key)),
            updated_at: None,
            content_type: Some("text/markdown".to_string()),
            group: None,
        };

        Ok(Resource {
            meta,
            content: out.into_bytes(),
            raw_json: None,
        })
    }

    // -----------------------------------------------------------------------
    // boards
    // -----------------------------------------------------------------------

    async fn list_boards(&self) -> Result<Vec<ResourceMeta>> {
        let url = format!("{}/rest/agile/1.0/board?maxResults=50", self.auth.base_url);
        let json = self.auth.get_json(&url).await?;

        let boards = json
            .get("values")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut resources = Vec::new();
        for board in &boards {
            let id = board
                .get("id")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string())
                .unwrap_or_default();
            let name = board
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("(no name)");
            let board_type = board
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            let slug = sanitize_slug(name);
            self.cache_slug("boards", &slug, &id);

            resources.push(ResourceMeta {
                id: id.clone(),
                slug,
                title: Some(format!("{} ({})", name, board_type)),
                updated_at: None,
                content_type: Some("text/markdown".to_string()),
                group: None,
            });
        }

        Ok(resources)
    }

    async fn read_board(&self, id: &str) -> Result<Resource> {
        let url = format!("{}/rest/agile/1.0/board/{}", self.auth.base_url, id);
        let json = self.auth.get_json(&url).await?;

        let name = json
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("(no name)");
        let board_type = json
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        // Try to get active sprint(s)
        let sprints_url = format!(
            "{}/rest/agile/1.0/board/{}/sprint?state=active&maxResults=5",
            self.auth.base_url, id
        );
        let sprints_json = self
            .auth
            .get_json(&sprints_url)
            .await
            .unwrap_or(Value::Null);
        let sprints = sprints_json
            .get("values")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: \"{}\"\n", id));
        out.push_str(&format!("name: \"{}\"\n", escape_yaml(name)));
        out.push_str(&format!("type: \"{}\"\n", board_type));
        out.push_str("operations: [read]\n");
        out.push_str("---\n\n");

        out.push_str(&format!("# {}\n\n", name));
        out.push_str(&format!("**Type:** {}\n\n", board_type));

        if !sprints.is_empty() {
            out.push_str("## Active Sprints\n\n");
            for sprint in &sprints {
                let sprint_name = sprint
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unnamed)");
                let sprint_state = sprint.get("state").and_then(|v| v.as_str()).unwrap_or("");
                let start_date = sprint
                    .get("startDate")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let end_date = sprint.get("endDate").and_then(|v| v.as_str()).unwrap_or("");
                let goal = sprint.get("goal").and_then(|v| v.as_str()).unwrap_or("");

                out.push_str(&format!("### {}\n\n", sprint_name));
                out.push_str(&format!(
                    "**State:** {} | **Start:** {} | **End:** {}\n",
                    sprint_state,
                    if start_date.len() >= 10 {
                        &start_date[..10]
                    } else {
                        start_date
                    },
                    if end_date.len() >= 10 {
                        &end_date[..10]
                    } else {
                        end_date
                    }
                ));
                if !goal.is_empty() {
                    out.push_str(&format!("**Goal:** {}\n", goal));
                }
                out.push('\n');

                // Fetch sprint issues
                let sprint_id = sprint
                    .get("id")
                    .and_then(|v| v.as_u64())
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                if !sprint_id.is_empty() {
                    let issues_url = format!(
                        "{}/rest/agile/1.0/sprint/{}/issue?maxResults=50&fields=key,summary,status,assignee",
                        self.auth.base_url, sprint_id
                    );
                    if let Ok(issues_json) = self.auth.get_json(&issues_url).await {
                        let issues = issues_json
                            .get("issues")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();

                        if !issues.is_empty() {
                            out.push_str("| Key | Summary | Status | Assignee |\n");
                            out.push_str("|-----|---------|--------|----------|\n");
                            for issue in &issues {
                                let ikey = issue.get("key").and_then(|v| v.as_str()).unwrap_or("");
                                let fields = issue.get("fields").unwrap_or(&Value::Null);
                                let summary =
                                    fields.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                                let status = fields
                                    .get("status")
                                    .and_then(|v| v.get("name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let assignee = fields
                                    .get("assignee")
                                    .and_then(|v| v.get("displayName"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Unassigned");
                                out.push_str(&format!(
                                    "| {} | {} | {} | {} |\n",
                                    ikey, summary, status, assignee
                                ));
                            }
                            out.push('\n');
                        }
                    }
                }
            }
        }

        let slug = sanitize_slug(name);
        let meta = ResourceMeta {
            id: id.to_string(),
            slug,
            title: Some(format!("{} ({})", name, board_type)),
            updated_at: None,
            content_type: Some("text/markdown".to_string()),
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
impl Connector for JiraConnector {
    fn name(&self) -> &str {
        "jira"
    }

    async fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        Ok(vec![
            CollectionInfo {
                name: "my-issues".to_string(),
                description: Some("Issues assigned to you".to_string()),
            },
            CollectionInfo {
                name: "projects".to_string(),
                description: Some("Jira projects".to_string()),
            },
            CollectionInfo {
                name: "boards".to_string(),
                description: Some("Agile boards".to_string()),
            },
        ])
    }

    async fn list_resources(&self, collection: &str) -> Result<Vec<ResourceMeta>> {
        match collection {
            "my-issues" => self.list_my_issues().await,
            "projects" => self.list_projects().await,
            "boards" => self.list_boards().await,
            _ => Err(anyhow!("unknown collection: '{}'", collection)),
        }
    }

    async fn read_resource(&self, collection: &str, id: &str) -> Result<Resource> {
        let resolved = self.resolve_id(collection, id);
        match collection {
            "my-issues" => self.read_issue(&resolved).await,
            "projects" => self.read_project(&resolved).await,
            "boards" => self.read_board(&resolved).await,
            _ => Err(anyhow!("unknown collection: '{}'", collection)),
        }
    }

    async fn write_resource(&self, collection: &str, id: &str, content: &[u8]) -> Result<()> {
        let resolved = self.resolve_id(collection, id);
        match collection {
            "my-issues" => self.write_issue(&resolved, content).await,
            _ => Err(anyhow!(
                "write is not supported for collection '{}'",
                collection
            )),
        }
    }

    async fn resource_versions(&self, _collection: &str, _id: &str) -> Result<Vec<VersionInfo>> {
        // Jira doesn't have a simple version history API for issues
        Ok(vec![])
    }

    async fn read_version(&self, collection: &str, id: &str, version: u32) -> Result<Resource> {
        if version == 0 {
            return self.read_resource(collection, id).await;
        }
        Err(anyhow!("versioned reads are not supported for Jira"))
    }
}

// ---------------------------------------------------------------------------
// ADF (Atlassian Document Format) <-> Markdown conversion
// ---------------------------------------------------------------------------

/// Convert ADF JSON to Markdown text.
fn adf_to_markdown(adf: &Value) -> String {
    let mut out = String::new();
    adf_node_to_markdown(adf, &mut out, 0);
    out
}

fn adf_node_to_markdown(node: &Value, out: &mut String, list_depth: usize) {
    let node_type = node.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match node_type {
        "doc" => {
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    adf_node_to_markdown(child, out, list_depth);
                }
            }
        }
        "paragraph" => {
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    adf_node_to_markdown(child, out, list_depth);
                }
            }
            out.push_str("\n\n");
        }
        "heading" => {
            let level = node
                .get("attrs")
                .and_then(|v| v.get("level"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as usize;
            for _ in 0..level {
                out.push('#');
            }
            out.push(' ');
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    adf_node_to_markdown(child, out, list_depth);
                }
            }
            out.push_str("\n\n");
        }
        "text" => {
            let text = node.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let marks = node
                .get("marks")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let mut prefix = String::new();
            let mut suffix = String::new();
            for mark in &marks {
                let mark_type = mark.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match mark_type {
                    "strong" => {
                        prefix.push_str("**");
                        suffix.insert_str(0, "**");
                    }
                    "em" => {
                        prefix.push('*');
                        suffix.insert(0, '*');
                    }
                    "code" => {
                        prefix.push('`');
                        suffix.insert(0, '`');
                    }
                    "strike" => {
                        prefix.push_str("~~");
                        suffix.insert_str(0, "~~");
                    }
                    "link" => {
                        if let Some(href) = mark
                            .get("attrs")
                            .and_then(|v| v.get("href"))
                            .and_then(|v| v.as_str())
                        {
                            prefix.push('[');
                            suffix.insert_str(0, &format!("]({})", href));
                        }
                    }
                    _ => {}
                }
            }
            out.push_str(&prefix);
            out.push_str(text);
            out.push_str(&suffix);
        }
        "hardBreak" => {
            out.push('\n');
        }
        "bulletList" => {
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    adf_node_to_markdown(child, out, list_depth);
                }
            }
        }
        "orderedList" => {
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for (i, child) in content.iter().enumerate() {
                    // Temporarily store the index for ordered items
                    let indent = "  ".repeat(list_depth);
                    out.push_str(&format!("{}{}. ", indent, i + 1));
                    if let Some(item_content) = child.get("content").and_then(|v| v.as_array()) {
                        for sub in item_content {
                            adf_node_to_markdown(sub, out, list_depth + 1);
                        }
                    }
                }
            }
        }
        "listItem" => {
            let indent = "  ".repeat(list_depth);
            out.push_str(&format!("{}- ", indent));
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    // For paragraphs inside list items, don't add extra newlines
                    let child_type = child.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if child_type == "paragraph" {
                        if let Some(para_content) = child.get("content").and_then(|v| v.as_array())
                        {
                            for sub in para_content {
                                adf_node_to_markdown(sub, out, list_depth + 1);
                            }
                        }
                        out.push('\n');
                    } else {
                        adf_node_to_markdown(child, out, list_depth + 1);
                    }
                }
            }
        }
        "codeBlock" => {
            let language = node
                .get("attrs")
                .and_then(|v| v.get("language"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            out.push_str(&format!("```{}\n", language));
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    let text = child.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    out.push_str(text);
                }
            }
            out.push_str("\n```\n\n");
        }
        "blockquote" => {
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    let mut block_content = String::new();
                    adf_node_to_markdown(child, &mut block_content, list_depth);
                    for line in block_content.lines() {
                        out.push_str(&format!("> {}\n", line));
                    }
                }
            }
            out.push('\n');
        }
        "rule" => {
            out.push_str("---\n\n");
        }
        "table" => {
            if let Some(rows) = node.get("content").and_then(|v| v.as_array()) {
                for (i, row) in rows.iter().enumerate() {
                    let cells = row
                        .get("content")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    out.push('|');
                    for cell in &cells {
                        out.push(' ');
                        let mut cell_text = String::new();
                        if let Some(cell_content) = cell.get("content").and_then(|v| v.as_array()) {
                            for child in cell_content {
                                adf_node_to_markdown(child, &mut cell_text, list_depth);
                            }
                        }
                        out.push_str(cell_text.trim());
                        out.push_str(" |");
                    }
                    out.push('\n');
                    // Add separator after header row
                    if i == 0 {
                        out.push('|');
                        for _ in &cells {
                            out.push_str("---|");
                        }
                        out.push('\n');
                    }
                }
                out.push('\n');
            }
        }
        "mediaSingle" | "mediaGroup" => {
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    adf_node_to_markdown(child, out, list_depth);
                }
            }
        }
        "media" => {
            let media_id = node
                .get("attrs")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("attachment");
            out.push_str(&format!("![{}](attachment:{})\n\n", media_id, media_id));
        }
        "emoji" => {
            let short_name = node
                .get("attrs")
                .and_then(|v| v.get("shortName"))
                .and_then(|v| v.as_str())
                .unwrap_or(":emoji:");
            out.push_str(short_name);
        }
        "mention" => {
            let text_val = node
                .get("attrs")
                .and_then(|v| v.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("@someone");
            out.push_str(text_val);
        }
        "inlineCard" | "blockCard" | "embedCard" => {
            let card_url = node
                .get("attrs")
                .and_then(|v| v.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !card_url.is_empty() {
                out.push_str(&format!("[{}]({})", card_url, card_url));
            }
        }
        _ => {
            // Unknown node type: try to recurse into content
            if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
                for child in content {
                    adf_node_to_markdown(child, out, list_depth);
                }
            }
        }
    }
}

/// Convert simple Markdown text to Atlassian Document Format (ADF).
///
/// This is a simplified conversion that handles the most common cases:
/// paragraphs, headings, bold, italic, code, links, lists, and code blocks.
fn markdown_to_adf(markdown: &str) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // Code blocks
        if line.starts_with("```") {
            let language = line.trim_start_matches('`').trim();
            let mut code_lines = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].starts_with("```") {
                code_lines.push(lines[i]);
                i += 1;
            }
            i += 1; // skip closing ```
            let code_text = code_lines.join("\n");
            let mut attrs = serde_json::Map::new();
            if !language.is_empty() {
                attrs.insert("language".to_string(), Value::String(language.to_string()));
            }
            content.push(serde_json::json!({
                "type": "codeBlock",
                "attrs": attrs,
                "content": [{ "type": "text", "text": code_text }]
            }));
            continue;
        }

        // Headings
        if line.starts_with('#') {
            let level = line.chars().take_while(|c| *c == '#').count();
            let text = line[level..].trim();
            if (1..=6).contains(&level) && !text.is_empty() {
                content.push(serde_json::json!({
                    "type": "heading",
                    "attrs": { "level": level },
                    "content": inline_markdown_to_adf(text)
                }));
                i += 1;
                continue;
            }
        }

        // Bullet lists
        if line.starts_with("- ") || line.starts_with("* ") {
            let mut items = Vec::new();
            while i < lines.len() && (lines[i].starts_with("- ") || lines[i].starts_with("* ")) {
                let item_text = &lines[i][2..];
                items.push(serde_json::json!({
                    "type": "listItem",
                    "content": [{
                        "type": "paragraph",
                        "content": inline_markdown_to_adf(item_text)
                    }]
                }));
                i += 1;
            }
            content.push(serde_json::json!({
                "type": "bulletList",
                "content": items
            }));
            continue;
        }

        // Blockquotes
        if line.starts_with("> ") {
            let mut quote_lines = Vec::new();
            while i < lines.len() && lines[i].starts_with("> ") {
                quote_lines.push(&lines[i][2..]);
                i += 1;
            }
            let quote_text = quote_lines.join("\n");
            content.push(serde_json::json!({
                "type": "blockquote",
                "content": [{
                    "type": "paragraph",
                    "content": inline_markdown_to_adf(&quote_text)
                }]
            }));
            continue;
        }

        // Horizontal rule
        if line == "---" || line == "***" || line == "___" {
            content.push(serde_json::json!({ "type": "rule" }));
            i += 1;
            continue;
        }

        // Empty lines — skip
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        // Regular paragraph
        content.push(serde_json::json!({
            "type": "paragraph",
            "content": inline_markdown_to_adf(line)
        }));
        i += 1;
    }

    if content.is_empty() {
        content.push(serde_json::json!({
            "type": "paragraph",
            "content": [{ "type": "text", "text": markdown }]
        }));
    }

    serde_json::json!({
        "version": 1,
        "type": "doc",
        "content": content
    })
}

/// Convert inline markdown to ADF inline nodes (text with marks).
fn inline_markdown_to_adf(text: &str) -> Vec<Value> {
    let mut nodes: Vec<Value> = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        // Bold: **text**
        if remaining.starts_with("**") {
            if let Some(end) = remaining[2..].find("**") {
                let bold_text = &remaining[2..2 + end];
                nodes.push(serde_json::json!({
                    "type": "text",
                    "text": bold_text,
                    "marks": [{ "type": "strong" }]
                }));
                remaining = &remaining[2 + end + 2..];
                continue;
            }
        }

        // Italic: *text*
        if remaining.starts_with('*') && !remaining.starts_with("**") {
            if let Some(end) = remaining[1..].find('*') {
                let italic_text = &remaining[1..1 + end];
                nodes.push(serde_json::json!({
                    "type": "text",
                    "text": italic_text,
                    "marks": [{ "type": "em" }]
                }));
                remaining = &remaining[1 + end + 1..];
                continue;
            }
        }

        // Inline code: `text`
        if remaining.starts_with('`') && !remaining.starts_with("```") {
            if let Some(end) = remaining[1..].find('`') {
                let code_text = &remaining[1..1 + end];
                nodes.push(serde_json::json!({
                    "type": "text",
                    "text": code_text,
                    "marks": [{ "type": "code" }]
                }));
                remaining = &remaining[1 + end + 1..];
                continue;
            }
        }

        // Link: [text](url)
        if remaining.starts_with('[') {
            if let Some(bracket_end) = remaining[1..].find("](") {
                let link_text = &remaining[1..1 + bracket_end];
                let after_paren = &remaining[1 + bracket_end + 2..];
                if let Some(paren_end) = after_paren.find(')') {
                    let url = &after_paren[..paren_end];
                    nodes.push(serde_json::json!({
                        "type": "text",
                        "text": link_text,
                        "marks": [{ "type": "link", "attrs": { "href": url } }]
                    }));
                    remaining = &after_paren[paren_end + 1..];
                    continue;
                }
            }
        }

        // Plain text: consume until the next special character
        let next_special = remaining.find(['*', '`', '[']).unwrap_or(remaining.len());
        if next_special == 0 {
            // The special char itself didn't match a pattern, consume one char
            nodes.push(serde_json::json!({
                "type": "text",
                "text": &remaining[..1]
            }));
            remaining = &remaining[1..];
        } else {
            nodes.push(serde_json::json!({
                "type": "text",
                "text": &remaining[..next_special]
            }));
            remaining = &remaining[next_special..];
        }
    }

    if nodes.is_empty() {
        nodes.push(serde_json::json!({
            "type": "text",
            "text": text
        }));
    }

    nodes
}

// ---------------------------------------------------------------------------
// Body parsing helpers
// ---------------------------------------------------------------------------

/// Extract the description section from the markdown body (everything between
/// "## Description" and the next "##" heading or "## Comments").
fn extract_description_from_body(body: &str) -> String {
    let mut in_description = false;
    let mut description_lines = Vec::new();

    for line in body.lines() {
        if line.starts_with("## Description") {
            in_description = true;
            continue;
        }
        if in_description && line.starts_with("## ") {
            break;
        }
        if in_description {
            description_lines.push(line);
        }
    }

    description_lines.join("\n").trim().to_string()
}

/// Extract a new comment from the body. A new comment is any non-blockquoted
/// text that appears after the "## Comments" heading and after all existing
/// blockquoted comments.
fn extract_new_comment(body: &str) -> Option<String> {
    let mut in_comments = false;
    let mut past_existing = false;
    let mut new_comment_lines = Vec::new();

    for line in body.lines() {
        if line.starts_with("## Comments") {
            in_comments = true;
            continue;
        }
        if !in_comments {
            continue;
        }
        // We're in the comments section
        if line.starts_with("## ") {
            break; // Next section
        }

        // Existing comments are blockquoted or are author lines (**author** (date):)
        if line.starts_with("> ") || line.starts_with("**") {
            past_existing = true;
            continue;
        }

        if past_existing && !line.trim().is_empty() {
            new_comment_lines.push(line);
        }
    }

    if new_comment_lines.is_empty() {
        None
    } else {
        Some(new_comment_lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adf_to_markdown_simple_paragraph() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [{
                    "type": "text",
                    "text": "Hello world"
                }]
            }]
        });
        let md = adf_to_markdown(&adf);
        assert_eq!(md.trim(), "Hello world");
    }

    #[test]
    fn test_adf_to_markdown_heading() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "heading",
                "attrs": { "level": 2 },
                "content": [{
                    "type": "text",
                    "text": "Section Title"
                }]
            }]
        });
        let md = adf_to_markdown(&adf);
        assert!(md.contains("## Section Title"));
    }

    #[test]
    fn test_adf_to_markdown_bold_and_italic() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [
                    { "type": "text", "text": "normal " },
                    { "type": "text", "text": "bold", "marks": [{ "type": "strong" }] },
                    { "type": "text", "text": " and " },
                    { "type": "text", "text": "italic", "marks": [{ "type": "em" }] }
                ]
            }]
        });
        let md = adf_to_markdown(&adf);
        assert!(md.contains("**bold**"));
        assert!(md.contains("*italic*"));
    }

    #[test]
    fn test_adf_to_markdown_bullet_list() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "bulletList",
                "content": [
                    {
                        "type": "listItem",
                        "content": [{
                            "type": "paragraph",
                            "content": [{ "type": "text", "text": "Item 1" }]
                        }]
                    },
                    {
                        "type": "listItem",
                        "content": [{
                            "type": "paragraph",
                            "content": [{ "type": "text", "text": "Item 2" }]
                        }]
                    }
                ]
            }]
        });
        let md = adf_to_markdown(&adf);
        assert!(md.contains("- Item 1"));
        assert!(md.contains("- Item 2"));
    }

    #[test]
    fn test_adf_to_markdown_code_block() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "codeBlock",
                "attrs": { "language": "rust" },
                "content": [{
                    "type": "text",
                    "text": "fn main() {}"
                }]
            }]
        });
        let md = adf_to_markdown(&adf);
        assert!(md.contains("```rust"));
        assert!(md.contains("fn main() {}"));
        assert!(md.contains("```"));
    }

    #[test]
    fn test_adf_to_markdown_link() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [{
                    "type": "text",
                    "text": "click here",
                    "marks": [{
                        "type": "link",
                        "attrs": { "href": "https://example.com" }
                    }]
                }]
            }]
        });
        let md = adf_to_markdown(&adf);
        assert!(md.contains("[click here](https://example.com)"));
    }

    #[test]
    fn test_markdown_to_adf_paragraph() {
        let adf = markdown_to_adf("Hello world");
        assert_eq!(adf["type"], "doc");
        assert_eq!(adf["content"][0]["type"], "paragraph");
        assert_eq!(adf["content"][0]["content"][0]["text"], "Hello world");
    }

    #[test]
    fn test_markdown_to_adf_heading() {
        let adf = markdown_to_adf("## My Heading");
        assert_eq!(adf["content"][0]["type"], "heading");
        assert_eq!(adf["content"][0]["attrs"]["level"], 2);
    }

    #[test]
    fn test_markdown_to_adf_code_block() {
        let adf = markdown_to_adf("```rust\nfn main() {}\n```");
        assert_eq!(adf["content"][0]["type"], "codeBlock");
        assert_eq!(adf["content"][0]["attrs"]["language"], "rust");
        assert_eq!(adf["content"][0]["content"][0]["text"], "fn main() {}");
    }

    #[test]
    fn test_markdown_to_adf_bullet_list() {
        let adf = markdown_to_adf("- Item 1\n- Item 2");
        assert_eq!(adf["content"][0]["type"], "bulletList");
        let items = adf["content"][0]["content"].as_array().unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_markdown_to_adf_bold() {
        let adf = markdown_to_adf("This is **bold** text");
        let content = &adf["content"][0]["content"];
        // Should have: "This is ", bold "bold", " text"
        let items = content.as_array().unwrap();
        assert!(items.len() >= 2);
        // Find the bold node
        let bold_node = items.iter().find(|n| {
            n.get("marks")
                .and_then(|m| m.as_array())
                .map(|m| m.iter().any(|mark| mark["type"] == "strong"))
                .unwrap_or(false)
        });
        assert!(bold_node.is_some());
        assert_eq!(bold_node.unwrap()["text"], "bold");
    }

    #[test]
    fn test_extract_description_from_body() {
        let body = "# PROJ-123: Test\n\n## Description\n\nThis is the description.\n\n## Comments\n\nSome comments";
        let desc = extract_description_from_body(body);
        assert_eq!(desc, "This is the description.");
    }

    #[test]
    fn test_extract_new_comment() {
        let body = "## Comments\n\n**john@example.com** (2026-03-25 14:00):\n> Existing comment\n\nThis is a new comment";
        let comment = extract_new_comment(body);
        assert_eq!(comment.unwrap(), "This is a new comment");
    }

    #[test]
    fn test_extract_new_comment_none() {
        let body = "## Comments\n\n**john@example.com** (2026-03-25 14:00):\n> Existing comment\n";
        let comment = extract_new_comment(body);
        assert!(comment.is_none());
    }
}

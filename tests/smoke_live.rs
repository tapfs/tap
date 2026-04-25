//! Live smoke tests for connector specs against real APIs.
//!
//! These tests validate that connector YAML specs produce working HTTP requests
//! and that responses parse correctly through the RestConnector pipeline.
//!
//! - `jsonplaceholder` always runs (public API, no auth)
//! - All other connectors run only when their env var is set
//!
//! Run all available:
//!   cargo test --no-default-features --features nfs --test smoke_live -- --nocapture
//!
//! Run just jsonplaceholder:
//!   cargo test --no-default-features --features nfs --test smoke_live jsonplaceholder -- --nocapture
//!
//! Run with specific credentials:
//!   GITHUB_TOKEN=ghp_xxx cargo test --test smoke_live github -- --nocapture

use tapfs::connector::builtin::builtin_spec;
use tapfs::connector::rest::RestConnector;
use tapfs::connector::spec::ConnectorSpec;
use tapfs::connector::traits::Connector;

/// Create a RestConnector from a builtin spec, optionally with a token override.
fn make_connector(name: &str, token: Option<String>) -> RestConnector {
    let yaml = builtin_spec(name).unwrap_or_else(|| panic!("no builtin spec for {name}"));
    let spec = ConnectorSpec::from_yaml(yaml)
        .unwrap_or_else(|e| panic!("failed to parse spec '{name}': {e}"));
    let client = reqwest::Client::new();
    RestConnector::new_with_token(spec, client, token)
}

/// Smoke test: list_collections + list_resources on first collection + read first resource.
async fn smoke_test_connector(name: &str, token: Option<String>) {
    println!("\n============================================================");
    println!("  SMOKE TEST: {name}");
    println!("============================================================");

    let connector = make_connector(name, token);

    // 1. list_collections
    let collections = connector.list_collections().await
        .unwrap_or_else(|e| panic!("[{name}] list_collections failed: {e}"));
    assert!(!collections.is_empty(), "[{name}] no collections returned");
    println!("  [OK] list_collections: {} collections", collections.len());
    for c in &collections {
        println!("        - {}", c.name);
    }

    // 2. list_resources on first collection
    let first_coll = &collections[0].name;
    let resources = match connector.list_resources(first_coll).await {
        Ok(r) => r,
        Err(e) => {
            let err_str = format!("{e}");
            // Auth errors are expected when testing — the endpoint exists, auth just failed
            if err_str.contains("401") || err_str.contains("403") {
                println!("  [AUTH] list_resources({first_coll}): auth error (endpoint exists, token invalid or missing scope)");
                println!("         {e}");
                return;
            }
            panic!("[{name}] list_resources({first_coll}) failed: {e}");
        }
    };
    println!("  [OK] list_resources({first_coll}): {} resources", resources.len());

    // 3. Read first resource (if any exist)
    if let Some(first) = resources.first() {
        println!("        first resource: id={}, slug={}, title={:?}",
            first.id, first.slug, first.title);

        match connector.read_resource(first_coll, &first.slug).await {
            Ok(resource) => {
                let content = String::from_utf8_lossy(&resource.content);
                let lines: Vec<&str> = content.lines().collect();
                let preview_lines = lines.len().min(15);
                println!("  [OK] read_resource({first_coll}, {}): {} bytes, {} lines",
                    first.slug, resource.content.len(), lines.len());
                println!("        --- preview ---");
                for line in &lines[..preview_lines] {
                    println!("        {line}");
                }
                if lines.len() > preview_lines {
                    println!("        ... ({} more lines)", lines.len() - preview_lines);
                }
                println!("        --- end preview ---");

                // Verify content has frontmatter
                assert!(content.starts_with("---\n"),
                    "[{name}] rendered content should start with YAML frontmatter");
                // Verify raw_json is populated
                assert!(resource.raw_json.is_some(),
                    "[{name}] raw_json should be populated");
            }
            Err(e) => {
                println!("  [WARN] read_resource({first_coll}, {}) failed: {e}", first.slug);
            }
        }
    } else {
        println!("  [SKIP] no resources in {first_coll} to read");
    }

    println!("  PASSED: {name}");
}

// ---------------------------------------------------------------
// Always-on test: JSONPlaceholder (no auth required)
// ---------------------------------------------------------------
#[tokio::test]
async fn jsonplaceholder_smoke() {
    smoke_test_connector("jsonplaceholder", None).await;
}

// ---------------------------------------------------------------
// Conditional tests: only run when env var is set
// ---------------------------------------------------------------

macro_rules! conditional_smoke_test {
    ($test_name:ident, $connector:literal, $env_var:literal) => {
        #[tokio::test]
        async fn $test_name() {
            let token = match std::env::var($env_var) {
                Ok(t) if !t.is_empty() => t,
                _ => {
                    println!("SKIPPED: {} not set", $env_var);
                    return;
                }
            };
            smoke_test_connector($connector, Some(token)).await;
        }
    };
}

conditional_smoke_test!(github_smoke, "github", "GITHUB_TOKEN");
conditional_smoke_test!(slack_smoke, "slack", "SLACK_BOT_TOKEN");
conditional_smoke_test!(stripe_smoke, "stripe", "STRIPE_API_KEY");
conditional_smoke_test!(linear_smoke, "linear", "LINEAR_API_TOKEN");
conditional_smoke_test!(notion_smoke, "notion", "NOTION_API_TOKEN");
conditional_smoke_test!(hubspot_smoke, "hubspot", "HUBSPOT_API_TOKEN");
conditional_smoke_test!(pagerduty_smoke, "pagerduty", "PAGERDUTY_API_TOKEN");
conditional_smoke_test!(gitlab_smoke, "gitlab", "GITLAB_TOKEN");
conditional_smoke_test!(asana_smoke, "asana", "ASANA_TOKEN");
conditional_smoke_test!(clickup_smoke, "clickup", "CLICKUP_TOKEN");
conditional_smoke_test!(discord_smoke, "discord", "DISCORD_BOT_TOKEN");
conditional_smoke_test!(sendgrid_smoke, "sendgrid", "SENDGRID_API_KEY");
conditional_smoke_test!(cloudflare_smoke, "cloudflare", "CLOUDFLARE_TOKEN");

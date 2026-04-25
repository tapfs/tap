//! `tap search` — fan a query out across registered search providers.
//!
//! For now this command runs out-of-process: it instantiates the requested
//! connector (if any) directly via [`create_connector`], builds a
//! [`SearchRegistry`] from `~/.tapfs/search.yaml` (falling back to the
//! default `upstream`-only registry), and prints fused results.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::connector::factory::create_connector;
use crate::connector::registry::ConnectorRegistry;
use crate::credentials::CredentialStore;
use crate::governance::audit::AuditLogger;
use crate::search::factory::{build_registry, default_registry};
use crate::search::spec::SearchConfig;
use crate::search::traits::{Query, QueryMode, SearchScope};

pub struct SearchArgs {
    /// `connector` or `connector/collection`, or empty for global search.
    pub target: Option<String>,
    pub query: String,
    pub limit: usize,
    pub json: bool,
    pub timeout_secs: u64,
}

pub async fn run(args: SearchArgs, data_dir: &Path) -> Result<()> {
    let (connector_name, collection) = parse_target(args.target.as_deref())?;

    let audit =
        Arc::new(AuditLogger::new(data_dir.join("audit.log")).context("creating audit logger")?);

    // Build a connector registry containing only the connector we need (if any).
    // Global queries don't need any connectors registered — they only run
    // against indexed/vector providers, none of which are wired in this PR.
    let connector_registry = Arc::new(ConnectorRegistry::new());
    if let Some(ref name) = connector_name {
        let creds = CredentialStore::load(data_dir)?;
        let (c, spec) = create_connector(name, &audit, &creds)
            .with_context(|| format!("loading connector '{}'", name))?;
        if let Some(s) = spec {
            connector_registry.register_with_spec(c, s);
        } else {
            connector_registry.register(c);
        }
    }

    let cfg_path = data_dir.join("search.yaml");
    let registry = if cfg_path.exists() {
        let yaml = std::fs::read_to_string(&cfg_path)
            .with_context(|| format!("reading {}", cfg_path.display()))?;
        let cfg = SearchConfig::from_yaml(&yaml)?;
        build_registry(&cfg, connector_registry.clone(), audit.clone())?
    } else {
        default_registry(connector_registry.clone(), audit.clone())
    };

    let scope = match (connector_name.as_deref(), collection.as_deref()) {
        (None, _) => SearchScope::Global,
        (Some(c), None) => SearchScope::Connector {
            connector: c.to_string(),
        },
        (Some(c), Some(col)) => SearchScope::Collection {
            connector: c.to_string(),
            collection: col.to_string(),
        },
    };

    let q = Query {
        text: args.query,
        k: args.limit,
        deadline: Some(Duration::from_secs(args.timeout_secs)),
        mode: QueryMode::Hybrid,
        filters: Default::default(),
    };

    let result = registry.query(scope, q).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        if result.hits.is_empty() {
            println!("(no results)");
        }
        for hit in &result.hits {
            println!("{:>7.4}  {}  [{}]", hit.score, hit.tap_path, hit.provider);
            if let Some(t) = &hit.title {
                println!("         {}", t);
            }
            if let Some(s) = &hit.snippet {
                println!("         {}", s);
            }
        }
        for w in &result.warnings {
            eprintln!("warning: {}", w);
        }
    }
    Ok(())
}

/// Parse `connector` or `connector/collection`. An empty target (`None` or
/// empty string) yields a global search.
fn parse_target(target: Option<&str>) -> Result<(Option<String>, Option<String>)> {
    let Some(t) = target.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok((None, None));
    };
    let mut parts = t.splitn(2, '/');
    let connector = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("invalid target: {}", t))?
        .to_string();
    let collection = parts
        .next()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    Ok((Some(connector), collection))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parser() {
        assert_eq!(parse_target(None).unwrap(), (None, None));
        assert_eq!(parse_target(Some("")).unwrap(), (None, None));
        assert_eq!(
            parse_target(Some("github")).unwrap(),
            (Some("github".into()), None)
        );
        assert_eq!(
            parse_target(Some("github/issues")).unwrap(),
            (Some("github".into()), Some("issues".into()))
        );
    }
}

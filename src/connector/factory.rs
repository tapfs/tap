use crate::connector::builtin::builtin_spec;
use crate::connector::confluence::ConfluenceConnector;
use crate::connector::google::GoogleWorkspaceConnector;
use crate::connector::jira::JiraConnector;
use crate::connector::rest::{OAuth2Config, RestConnector};
use crate::connector::spec::ConnectorSpec;
use crate::connector::traits::Connector;
use crate::credentials::CredentialStore;
use crate::governance::audit::AuditLogger;
use crate::governance::interceptor::AuditedConnector;
use std::sync::Arc;
use std::time::Duration;

/// Error indicating that interactive authentication is required before the
/// connector can be created.
#[derive(Debug)]
pub struct AuthRequired {
    pub connector_name: String,
    pub spec: Option<ConnectorSpec>,
}

impl std::fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "connector '{}' requires authentication",
            self.connector_name
        )
    }
}

impl std::error::Error for AuthRequired {}

pub fn create_connector(
    name: &str,
    audit: &Arc<AuditLogger>,
    creds: &CredentialStore,
) -> anyhow::Result<(Arc<dyn Connector>, Option<ConnectorSpec>)> {
    let audited: Arc<dyn Connector>;
    let spec: Option<ConnectorSpec>;

    if name == "google" {
        // Check if Google credentials exist before creating the connector
        let has_google_creds = std::env::var("GOOGLE_ACCESS_TOKEN").is_ok()
            || std::env::var("GOOGLE_CREDENTIALS_FILE")
                .ok()
                .map(|p| std::path::Path::new(&p).exists())
                .unwrap_or(false)
            || dirs::home_dir()
                .map(|h| h.join(".config/gws/credentials.json").exists())
                .unwrap_or(false)
            || dirs::home_dir()
                .map(|h| {
                    h.join(".config/gcloud/application_default_credentials.json")
                        .exists()
                })
                .unwrap_or(false)
            || creds.token("google").is_some()
            || creds
                .get("google")
                .and_then(|c| c.refresh_token.as_ref())
                .is_some();

        if !has_google_creds {
            return Err(AuthRequired {
                connector_name: name.to_string(),
                spec: None,
            }
            .into());
        }

        match GoogleWorkspaceConnector::new() {
            Ok(connector) => {
                let inner: Arc<dyn Connector> = Arc::new(connector);
                audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
                spec = None;
            }
            Err(_) => {
                return Err(AuthRequired {
                    connector_name: name.to_string(),
                    spec: None,
                }
                .into());
            }
        }
    } else if name == "jira" {
        if !crate::connector::atlassian_auth::AtlassianAuth::credentials_present(name, creds) {
            return Err(AuthRequired {
                connector_name: name.to_string(),
                spec: None,
            }
            .into());
        }
        let inner: Arc<dyn Connector> = Arc::new(JiraConnector::new(creds)?);
        audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        spec = None;
    } else if name == "confluence" {
        if !crate::connector::atlassian_auth::AtlassianAuth::credentials_present(name, creds) {
            return Err(AuthRequired {
                connector_name: name.to_string(),
                spec: None,
            }
            .into());
        }
        let inner: Arc<dyn Connector> = Arc::new(ConfluenceConnector::new(creds)?);
        audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        spec = None;
    } else {
        let yaml =
            builtin_spec(name).ok_or_else(|| anyhow::anyhow!("unknown connector: {}", name))?;
        let mut parsed = ConnectorSpec::from_yaml(yaml)?;
        if let Some(url) = creds.base_url(name) {
            parsed.base_url = url;
        }

        let is_oauth2 = parsed.auth.as_ref().map(|a| a.auth_type.as_str()) == Some("oauth2");

        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;

        let cred = creds.get(name);
        let token = creds.token(name);
        let refresh_token = cred.and_then(|c| c.refresh_token.clone());

        if let (true, Some(rt)) = (is_oauth2, refresh_token) {
            // Full OAuth2 with refresh token — use auto-refresh connector
            let auth_spec = parsed.auth.as_ref().unwrap();
            let oauth2_config = OAuth2Config {
                token_url: auth_spec.token_url.clone().unwrap_or_default(),
                client_id: auth_spec.client_id.clone().unwrap_or_default(),
                client_secret: auth_spec.client_secret.clone().unwrap_or_default(),
                refresh_token: rt,
                expiry: std::sync::RwLock::new(None),
            };

            let rest = RestConnector::new_with_oauth2(parsed.clone(), client, token, oauth2_config);
            let inner: Arc<dyn Connector> = Arc::new(rest);
            audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        } else if token.is_some() {
            // Have a token (from credentials.yaml, env var, or device flow) — use as bearer
            let rest = RestConnector::new_with_token(parsed.clone(), client, token);
            let inner: Arc<dyn Connector> = Arc::new(rest);
            audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        } else if parsed.auth.is_some() {
            // Auth required but no credentials at all — trigger interactive auth
            return Err(AuthRequired {
                connector_name: name.to_string(),
                spec: Some(parsed),
            }
            .into());
        } else {
            // No auth needed
            let rest = RestConnector::new_with_token(parsed.clone(), client, None);
            let inner: Arc<dyn Connector> = Arc::new(rest);
            audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        }
        spec = Some(parsed);
    }

    Ok((audited, spec))
}

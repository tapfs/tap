use crate::connector::builtin::builtin_spec;
use crate::connector::confluence::ConfluenceConnector;
use crate::connector::google::GoogleWorkspaceConnector;
use crate::connector::jira::JiraConnector;
use crate::connector::rest::RestConnector;
use crate::connector::spec::ConnectorSpec;
use crate::connector::traits::Connector;
use crate::credentials::CredentialStore;
use crate::governance::audit::AuditLogger;
use crate::governance::interceptor::AuditedConnector;
use std::sync::Arc;
use std::time::Duration;

pub fn create_connector(
    name: &str,
    audit: &Arc<AuditLogger>,
    creds: &CredentialStore,
) -> anyhow::Result<(Arc<dyn Connector>, Option<ConnectorSpec>)> {
    let audited: Arc<dyn Connector>;
    let spec: Option<ConnectorSpec>;

    if name == "google" {
        let inner: Arc<dyn Connector> = Arc::new(GoogleWorkspaceConnector::new()?);
        audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        spec = None;
    } else if name == "jira" {
        let inner: Arc<dyn Connector> = Arc::new(JiraConnector::new()?);
        audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        spec = None;
    } else if name == "confluence" {
        let inner: Arc<dyn Connector> = Arc::new(ConfluenceConnector::new()?);
        audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        spec = None;
    } else {
        let yaml = builtin_spec(name)
            .ok_or_else(|| anyhow::anyhow!("unknown connector: {}", name))?;
        let mut parsed = ConnectorSpec::from_yaml(yaml)?;
        if let Some(url) = creds.base_url(name) {
            parsed.base_url = url;
        }
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;
        let token = creds.token(name);
        let rest = RestConnector::new_with_token(parsed.clone(), client, token);
        let inner: Arc<dyn Connector> = Arc::new(rest);
        audited = Arc::new(AuditedConnector::new(inner, audit.clone()));
        spec = Some(parsed);
    }

    Ok((audited, spec))
}

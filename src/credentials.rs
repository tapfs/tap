//! Credential resolution for connectors.
//!
//! Secrets (token / refresh_token / client_secret) are stored in the OS keychain
//! under the service `tapfs`, with one entry per connector. Non-secret metadata
//! (email, base_url, client_id) and an entry-per-connector index live in
//! `~/.tapfs/credentials.yaml` (mode `0o600`).
//!
//! When the keychain is unavailable (headless Linux without Secret Service, CI),
//! set `TAPFS_NO_KEYCHAIN=1` to keep secrets in the YAML file too.
//!
//! ```yaml
//! github: {}              # token lives in keychain
//! jira:
//!   email: user@company.com
//!   base_url: https://myorg.atlassian.net
//! ```
//!
//! ## Backend injection
//!
//! `KeychainBackend` is a trait with one prod impl (`OsKeychain`) and a test
//! impl (`MockKeychain`). Tests construct a per-test `Arc<dyn KeychainBackend>`
//! and pass it via the `_with_backend` constructors — no global state, no
//! cross-test contamination.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

const KEYCHAIN_SERVICE: &str = "tapfs";

/// Credentials for a single connector.
///
/// Secret fields are redacted by the manual `Debug` impl so that
/// `tracing::debug!(?creds)` and similar accidents can never leak them.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ConnectorCredentials {
    pub token: Option<String>,
    pub email: Option<String>,
    pub base_url: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

struct Redacted<'a>(&'a Option<String>);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(_) => f.write_str("Some(<redacted>)"),
            None => f.write_str("None"),
        }
    }
}

impl fmt::Debug for ConnectorCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectorCredentials")
            .field("token", &Redacted(&self.token))
            .field("email", &self.email)
            .field("base_url", &self.base_url)
            .field("refresh_token", &Redacted(&self.refresh_token))
            .field("client_id", &self.client_id)
            .field("client_secret", &Redacted(&self.client_secret))
            .finish()
    }
}

/// Secret fields stored in the OS keychain as a single JSON blob per connector.
#[derive(Clone, Serialize, Deserialize, Default)]
struct KeychainSecret {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
}

impl fmt::Debug for KeychainSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeychainSecret")
            .field("token", &Redacted(&self.token))
            .field("refresh_token", &Redacted(&self.refresh_token))
            .field("client_secret", &Redacted(&self.client_secret))
            .finish()
    }
}

impl KeychainSecret {
    fn is_empty(&self) -> bool {
        self.token.is_none() && self.refresh_token.is_none() && self.client_secret.is_none()
    }
}

/// All credentials keyed by connector name.
#[derive(Debug, Default)]
pub struct CredentialStore {
    entries: HashMap<String, ConnectorCredentials>,
}

fn keychain_disabled() -> bool {
    std::env::var("TAPFS_NO_KEYCHAIN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Pluggable keychain backend.
///
/// `pub(crate)` so the test module can implement it. Production code only ever
/// touches `OsKeychain`; alternative backends are constructed by tests and
/// passed via the explicit `_with_backend` entry points.
pub(crate) trait KeychainBackend: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<String>>;
    fn set(&self, name: &str, value: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

pub(crate) struct OsKeychain;

impl KeychainBackend for OsKeychain {
    fn get(&self, name: &str) -> Result<Option<String>> {
        let entry =
            keyring::Entry::new(KEYCHAIN_SERVICE, name).context("opening keychain entry")?;
        match entry.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context("reading keychain entry")),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<()> {
        let entry =
            keyring::Entry::new(KEYCHAIN_SERVICE, name).context("opening keychain entry")?;
        entry.set_password(value).context("writing keychain entry")?;
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<()> {
        let entry =
            keyring::Entry::new(KEYCHAIN_SERVICE, name).context("opening keychain entry")?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::Error::new(e).context("deleting keychain entry")),
        }
    }
}

/// Construct the default backend honoring `TAPFS_NO_KEYCHAIN`.
fn default_backend() -> Option<Arc<dyn KeychainBackend>> {
    if keychain_disabled() {
        None
    } else {
        Some(Arc::new(OsKeychain) as Arc<dyn KeychainBackend>)
    }
}

fn keychain_get(backend: &dyn KeychainBackend, connector: &str) -> Result<Option<KeychainSecret>> {
    match backend.get(connector)? {
        Some(json) => Ok(Some(
            serde_json::from_str(&json).context("parsing keychain JSON")?,
        )),
        None => Ok(None),
    }
}

fn keychain_set(
    backend: &dyn KeychainBackend,
    connector: &str,
    secret: &KeychainSecret,
) -> Result<()> {
    if secret.is_empty() {
        backend.delete(connector)
    } else {
        let json = serde_json::to_string(secret).context("serializing keychain secret")?;
        backend.set(connector, &json)
    }
}

/// Read the `credentials.yaml` index without touching the keychain.
pub(crate) fn read_yaml_index(data_dir: &Path) -> Result<HashMap<String, ConnectorCredentials>> {
    let path = data_dir.join("credentials.yaml");
    if !path.exists() {
        return Ok(HashMap::new());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(&path)
            .context("reading credentials file metadata")?
            .permissions();
        let mode = perms.mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{:o}", mode),
                "credentials file has overly permissive permissions — should be 0600"
            );
        }
    }

    let content = std::fs::read_to_string(&path).context("reading credentials file")?;
    let entries: HashMap<String, ConnectorCredentials> =
        serde_yaml::from_str(&content).context("parsing credentials YAML")?;
    Ok(entries)
}

/// Write the YAML index back, atomically and with 0600 permissions.
pub(crate) fn write_yaml_index(
    data_dir: &Path,
    entries: &HashMap<String, ConnectorCredentials>,
) -> Result<()> {
    let path = data_dir.join("credentials.yaml");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let yaml = serde_yaml::to_string(entries).context("serializing credentials")?;

    let tmp = data_dir.join("credentials.yaml.tmp");
    std::fs::write(&tmp, yaml).context("writing credentials tempfile")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .context("setting credentials tempfile permissions")?;
    }
    std::fs::rename(&tmp, &path).context("renaming credentials tempfile into place")?;
    Ok(())
}

impl CredentialStore {
    /// Load credentials. Secrets come from the keychain unless
    /// `TAPFS_NO_KEYCHAIN=1` is set, in which case they come from the YAML
    /// index alongside the metadata.
    pub fn load(data_dir: &Path) -> Result<Self> {
        Self::load_with_backend(data_dir, default_backend().as_deref())
    }

    /// Lower-level entry point used by tests to inject a per-test backend.
    /// `backend = None` is the YAML-only mode (TAPFS_NO_KEYCHAIN=1).
    pub(crate) fn load_with_backend(
        data_dir: &Path,
        backend: Option<&dyn KeychainBackend>,
    ) -> Result<Self> {
        let mut entries = read_yaml_index(data_dir)?;

        if let Some(backend) = backend {
            // With the keychain enabled, the YAML index never holds secrets.
            // Overlay each connector's secrets from the keychain.
            for (name, creds) in entries.iter_mut() {
                match keychain_get(backend, name) {
                    Ok(Some(secret)) => {
                        creds.token = secret.token;
                        creds.refresh_token = secret.refresh_token;
                        creds.client_secret = secret.client_secret;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            connector = %name,
                            error = %e,
                            "keychain lookup failed; secrets unavailable for this connector — \
                             rerun `tap mount {}` to re-authenticate, or set TAPFS_NO_KEYCHAIN=1 \
                             to read secrets from credentials.yaml",
                            name
                        );
                    }
                }
            }
        }

        if !entries.is_empty() {
            tracing::info!(
                count = entries.len(),
                "loaded credentials for {} connector(s)",
                entries.len()
            );
        }

        Ok(Self { entries })
    }

    pub fn get(&self, connector_name: &str) -> Option<&ConnectorCredentials> {
        self.entries.get(connector_name)
    }

    pub fn token(&self, connector_name: &str) -> Option<String> {
        self.entries
            .get(connector_name)
            .and_then(|c| c.token.clone())
    }

    pub fn base_url(&self, connector_name: &str) -> Option<String> {
        self.entries
            .get(connector_name)
            .and_then(|c| c.base_url.clone())
    }

    /// Save a token for a connector. Writes to the OS keychain unless
    /// `TAPFS_NO_KEYCHAIN=1` is set, in which case the token lands in the YAML
    /// index. Either way, the YAML index gains an entry for the connector.
    pub fn save_token(data_dir: &Path, connector_name: &str, token: &str) -> Result<()> {
        Self::save_token_with_backend(data_dir, connector_name, token, default_backend().as_deref())
    }

    pub(crate) fn save_token_with_backend(
        data_dir: &Path,
        connector_name: &str,
        token: &str,
        backend: Option<&dyn KeychainBackend>,
    ) -> Result<()> {
        let mut entries = read_yaml_index(data_dir)?;
        let entry = entries.entry(connector_name.to_string()).or_default();

        if let Some(backend) = backend {
            let mut secret = keychain_get(backend, connector_name)
                .ok()
                .flatten()
                .unwrap_or_default();
            secret.token = Some(token.to_string());
            keychain_set(backend, connector_name, &secret)?;
            entry.token = None;
        } else {
            entry.token = Some(token.to_string());
        }

        write_yaml_index(data_dir, &entries)?;
        Ok(())
    }

    /// Save OAuth2 credentials. Token / refresh_token / client_secret go to the
    /// keychain; the non-secret `client_id` always stays in the YAML index.
    pub fn save_oauth2(
        data_dir: &Path,
        connector_name: &str,
        token: &str,
        refresh_token: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<()> {
        Self::save_oauth2_with_backend(
            data_dir,
            connector_name,
            token,
            refresh_token,
            client_id,
            client_secret,
            default_backend().as_deref(),
        )
    }

    pub(crate) fn save_oauth2_with_backend(
        data_dir: &Path,
        connector_name: &str,
        token: &str,
        refresh_token: &str,
        client_id: &str,
        client_secret: &str,
        backend: Option<&dyn KeychainBackend>,
    ) -> Result<()> {
        let mut entries = read_yaml_index(data_dir)?;
        let entry = entries.entry(connector_name.to_string()).or_default();
        entry.client_id = Some(client_id.to_string());

        if let Some(backend) = backend {
            let secret = KeychainSecret {
                token: Some(token.to_string()),
                refresh_token: Some(refresh_token.to_string()),
                client_secret: Some(client_secret.to_string()),
            };
            keychain_set(backend, connector_name, &secret)?;
            entry.token = None;
            entry.refresh_token = None;
            entry.client_secret = None;
        } else {
            entry.token = Some(token.to_string());
            entry.refresh_token = Some(refresh_token.to_string());
            entry.client_secret = Some(client_secret.to_string());
        }

        write_yaml_index(data_dir, &entries)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    pub(crate) struct MockKeychain {
        store: Mutex<HashMap<String, String>>,
    }

    impl MockKeychain {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                store: Mutex::new(HashMap::new()),
            })
        }
    }

    impl KeychainBackend for MockKeychain {
        fn get(&self, name: &str) -> Result<Option<String>> {
            Ok(self.store.lock().unwrap().get(name).cloned())
        }
        fn set(&self, name: &str, value: &str) -> Result<()> {
            self.store
                .lock()
                .unwrap()
                .insert(name.to_string(), value.to_string());
            Ok(())
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.store.lock().unwrap().remove(name);
            Ok(())
        }
    }

    #[test]
    fn save_and_load_token_via_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let kc = MockKeychain::new();

        CredentialStore::save_token_with_backend(
            dir.path(),
            "github",
            "ghp_secret",
            Some(kc.as_ref()),
        )
        .unwrap();

        let yaml = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(
            !yaml.contains("ghp_secret"),
            "YAML should not contain plaintext token: {}",
            yaml
        );

        let store = CredentialStore::load_with_backend(dir.path(), Some(kc.as_ref())).unwrap();
        assert_eq!(store.token("github").as_deref(), Some("ghp_secret"));
    }

    #[test]
    fn save_oauth2_separates_secrets_from_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let kc = MockKeychain::new();

        CredentialStore::save_oauth2_with_backend(
            dir.path(),
            "google",
            "access-tok",
            "refresh-tok",
            "client-id-123",
            "client-secret-xyz",
            Some(kc.as_ref()),
        )
        .unwrap();

        let yaml = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(yaml.contains("client-id-123"));
        assert!(!yaml.contains("access-tok"));
        assert!(!yaml.contains("refresh-tok"));
        assert!(!yaml.contains("client-secret-xyz"));

        let store = CredentialStore::load_with_backend(dir.path(), Some(kc.as_ref())).unwrap();
        let creds = store.get("google").expect("entry");
        assert_eq!(creds.token.as_deref(), Some("access-tok"));
        assert_eq!(creds.refresh_token.as_deref(), Some("refresh-tok"));
        assert_eq!(creds.client_secret.as_deref(), Some("client-secret-xyz"));
        assert_eq!(creds.client_id.as_deref(), Some("client-id-123"));
    }

    #[test]
    fn no_keychain_writes_token_to_yaml() {
        let dir = tempfile::tempdir().unwrap();
        CredentialStore::save_token_with_backend(dir.path(), "x", "yaml-only-tok", None).unwrap();
        let yaml = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(yaml.contains("yaml-only-tok"));

        let store = CredentialStore::load_with_backend(dir.path(), None).unwrap();
        assert_eq!(store.token("x").as_deref(), Some("yaml-only-tok"));
    }

    #[test]
    fn keychain_present_but_yaml_token_field_ignored() {
        // If somehow the YAML index has a `token` field for a connector that
        // also has a keychain entry, the keychain wins. We no longer have a
        // migration path that reads YAML secrets — that's gone.
        let dir = tempfile::tempdir().unwrap();
        let kc = MockKeychain::new();

        let mut entries = HashMap::new();
        entries.insert(
            "github".to_string(),
            ConnectorCredentials {
                token: Some("stale-yaml-tok".to_string()),
                ..Default::default()
            },
        );
        write_yaml_index(dir.path(), &entries).unwrap();
        kc.set(
            "github",
            r#"{"token":"fresh-keychain-tok"}"#,
        )
        .unwrap();

        let store = CredentialStore::load_with_backend(dir.path(), Some(kc.as_ref())).unwrap();
        assert_eq!(store.token("github").as_deref(), Some("fresh-keychain-tok"));
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::load_with_backend(dir.path(), None).unwrap();
        assert!(store.get("anything").is_none());
        assert!(store.token("anything").is_none());
    }

    #[test]
    fn debug_impl_redacts_secrets() {
        let creds = ConnectorCredentials {
            token: Some("ghp_super_secret".to_string()),
            refresh_token: Some("rt_also_secret".to_string()),
            client_secret: Some("cs_secret".to_string()),
            email: Some("user@example.com".to_string()),
            base_url: Some("https://api.example.com".to_string()),
            client_id: Some("public-client-id".to_string()),
        };
        let dbg = format!("{:?}", creds);
        assert!(!dbg.contains("ghp_super_secret"), "debug leaked token: {}", dbg);
        assert!(!dbg.contains("rt_also_secret"), "debug leaked refresh: {}", dbg);
        assert!(!dbg.contains("cs_secret"), "debug leaked client_secret: {}", dbg);
        assert!(dbg.contains("user@example.com"));
        assert!(dbg.contains("public-client-id"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn save_token_propagates_corrupt_yaml_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("credentials.yaml"),
            "::: not valid yaml :::\n",
        )
        .unwrap();

        let err = CredentialStore::save_token_with_backend(dir.path(), "github", "ghp_secret", None)
            .expect_err("expected error on corrupt YAML");
        assert!(format!("{:#}", err).contains("parsing credentials YAML"));
        let on_disk = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(on_disk.contains("not valid yaml"));
    }

    #[test]
    fn save_oauth2_propagates_corrupt_yaml_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("credentials.yaml"), "{ unterminated\n").unwrap();
        let err = CredentialStore::save_oauth2_with_backend(
            dir.path(),
            "google",
            "tok",
            "refresh",
            "client",
            "secret",
            None,
        )
        .expect_err("expected error on corrupt YAML");
        assert!(format!("{:#}", err).contains("parsing credentials YAML"));
    }

    #[test]
    fn write_yaml_index_is_atomic_no_tempfile_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = HashMap::new();
        entries.insert(
            "x".to_string(),
            ConnectorCredentials {
                email: Some("a@b".to_string()),
                ..Default::default()
            },
        );
        write_yaml_index(dir.path(), &entries).unwrap();
        assert!(dir.path().join("credentials.yaml").exists());
        assert!(!dir.path().join("credentials.yaml.tmp").exists());
    }

    #[test]
    fn parallel_test_instances_do_not_share_keychain() {
        // Two MockKeychain instances are independent. Previously a OnceLock
        // global meant the first installed mock won and contaminated all
        // subsequent tests in the process.
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let kc_a = MockKeychain::new();
        let kc_b = MockKeychain::new();

        CredentialStore::save_token_with_backend(dir_a.path(), "x", "tok-a", Some(kc_a.as_ref()))
            .unwrap();
        CredentialStore::save_token_with_backend(dir_b.path(), "x", "tok-b", Some(kc_b.as_ref()))
            .unwrap();

        let a = CredentialStore::load_with_backend(dir_a.path(), Some(kc_a.as_ref())).unwrap();
        let b = CredentialStore::load_with_backend(dir_b.path(), Some(kc_b.as_ref())).unwrap();
        assert_eq!(a.token("x").as_deref(), Some("tok-a"));
        assert_eq!(b.token("x").as_deref(), Some("tok-b"));
    }
}

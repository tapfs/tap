//! Credential resolution for connectors.
//!
//! Secrets (token / refresh_token / client_secret) are stored in the OS keychain
//! under the service `tapfs`, with one entry per connector. Non-secret metadata
//! (email, base_url, client_id) and an entry-per-connector index live in
//! `~/.tapfs/credentials.yaml` (mode `0o600`).
//!
//! When the keychain is unavailable (headless Linux without Secret Service, CI),
//! set `TAPFS_NO_KEYCHAIN=1` to force everything — including secrets — into the
//! YAML file.
//!
//! ```yaml
//! github: {}              # token lives in keychain
//! jira:
//!   email: user@company.com
//!   base_url: https://myorg.atlassian.net
//! ```

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

const KEYCHAIN_SERVICE: &str = "tapfs";

/// Credentials for a single connector.
///
/// Secret fields (`token`, `refresh_token`, `client_secret`) are redacted by the
/// manual `Debug` impl so that `tracing::debug!(?creds)` and similar accidents
/// can never leak them into logs.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ConnectorCredentials {
    pub token: Option<String>,
    pub email: Option<String>,
    pub base_url: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

/// Wrapper that lets `Debug` print presence-or-absence of a secret without
/// revealing its value.
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

/// Pluggable keychain backend so we can test against an in-memory store
/// without touching the real OS keychain (the `keyring` crate's mock has
/// `EntryOnly` persistence and doesn't survive across `Entry::new` calls).
trait KeychainBackend: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<String>>;
    fn set(&self, name: &str, value: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

struct OsKeychain;

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
        entry
            .set_password(value)
            .context("writing keychain entry")?;
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

static KEYCHAIN: std::sync::OnceLock<Box<dyn KeychainBackend>> = std::sync::OnceLock::new();

fn keychain() -> &'static dyn KeychainBackend {
    KEYCHAIN
        .get_or_init(|| Box::new(OsKeychain) as Box<dyn KeychainBackend>)
        .as_ref()
}

fn keychain_get(connector: &str) -> Result<Option<KeychainSecret>> {
    match keychain().get(connector)? {
        Some(json) => Ok(Some(
            serde_json::from_str(&json).context("parsing keychain JSON")?,
        )),
        None => Ok(None),
    }
}

fn keychain_set(connector: &str, secret: &KeychainSecret) -> Result<()> {
    if secret.is_empty() {
        keychain().delete(connector)
    } else {
        let json = serde_json::to_string(secret).context("serializing keychain secret")?;
        keychain().set(connector, &json)
    }
}

/// Read the `credentials.yaml` index without touching the keychain.
///
/// `pub(crate)` so other connector-side code (e.g. `atlassian_auth`) can reuse
/// the same parser instead of duplicating it (and re-introducing the silent
/// `unwrap_or_default()` swallow that drops parse errors).
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
            // By default we warn loudly. Users running in a security-sensitive
            // environment can opt into a hard error via TAPFS_STRICT_PERMS=1
            // so a misconfigured deploy fails closed instead of surfacing a
            // log line that nobody reads.
            let strict = std::env::var("TAPFS_STRICT_PERMS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            if strict {
                anyhow::bail!(
                    "credentials file {} has overly permissive permissions ({:o}) — \
                     refusing to load with TAPFS_STRICT_PERMS=1 set. Run `chmod 600 {}`",
                    path.display(),
                    mode,
                    path.display()
                );
            }
            tracing::warn!(
                path = %path.display(),
                mode = format!("{:o}", mode),
                "credentials file has overly permissive permissions — should be 0600 \
                 (set TAPFS_STRICT_PERMS=1 to fail-closed instead of warn)"
            );
        }
    }

    let content = std::fs::read_to_string(&path).context("reading credentials file")?;
    let entries: HashMap<String, ConnectorCredentials> =
        serde_yaml::from_str(&content).context("parsing credentials YAML")?;
    Ok(entries)
}

/// Write the YAML index back, atomically and with 0600 permissions.
///
/// Atomicity is critical here: the previous implementation used `std::fs::write`,
/// which truncates the destination *before* writing. A crash mid-write left a
/// truncated `credentials.yaml`, losing every connector's metadata. We instead
/// write to a sibling tempfile, set its mode, then `rename(2)` it over the
/// destination — atomic on POSIX same-filesystem.
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
    /// Load credentials from `~/.tapfs/credentials.yaml` and the OS keychain.
    ///
    /// Returns an empty store if neither source has any data.
    /// Migrates plaintext YAML secrets into the keychain on first run; the YAML
    /// values are left in place so users can downgrade or audit.
    pub fn load(data_dir: &Path) -> Result<Self> {
        Self::load_with_keychain(data_dir, !keychain_disabled())
    }

    pub fn load_with_keychain(data_dir: &Path, use_keychain: bool) -> Result<Self> {
        let mut entries = read_yaml_index(data_dir)?;

        if use_keychain {
            // For each connector listed in YAML, overlay keychain secrets.
            // Migrate YAML-only secrets to keychain on first observation.
            for (name, creds) in entries.iter_mut() {
                match keychain_get(name) {
                    Ok(Some(secret)) => {
                        if secret.token.is_some() {
                            creds.token = secret.token;
                        }
                        if secret.refresh_token.is_some() {
                            creds.refresh_token = secret.refresh_token;
                        }
                        if secret.client_secret.is_some() {
                            creds.client_secret = secret.client_secret;
                        }
                    }
                    Ok(None) => {
                        let migrated = KeychainSecret {
                            token: creds.token.clone(),
                            refresh_token: creds.refresh_token.clone(),
                            client_secret: creds.client_secret.clone(),
                        };
                        if !migrated.is_empty() {
                            match keychain_set(name, &migrated) {
                                Ok(()) => tracing::info!(
                                    connector = %name,
                                    "migrated credentials from credentials.yaml to OS keychain"
                                ),
                                Err(e) => tracing::warn!(
                                    connector = %name,
                                    error = %e,
                                    "keychain migration failed; using YAML credentials"
                                ),
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            connector = %name,
                            error = %e,
                            "keychain lookup failed; using YAML credentials"
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

    /// Save a token for a connector. Writes to OS keychain when available,
    /// falling back to `credentials.yaml`. The YAML is always touched to
    /// register the connector in the index, even when the secret lives in
    /// the keychain.
    pub fn save_token(data_dir: &Path, connector_name: &str, token: &str) -> Result<()> {
        Self::save_token_with_keychain(data_dir, connector_name, token, !keychain_disabled())
    }

    pub fn save_token_with_keychain(
        data_dir: &Path,
        connector_name: &str,
        token: &str,
        use_keychain: bool,
    ) -> Result<()> {
        // Propagate parse errors instead of silently overwriting a corrupted
        // YAML file with a single-entry map (which would lose every other
        // connector's metadata).
        let mut entries = read_yaml_index(data_dir)?;
        let entry = entries.entry(connector_name.to_string()).or_default();

        let mut wrote_to_keychain = false;
        if use_keychain {
            let mut secret = keychain_get(connector_name)
                .ok()
                .flatten()
                .unwrap_or_default();
            secret.token = Some(token.to_string());
            match keychain_set(connector_name, &secret) {
                Ok(()) => {
                    wrote_to_keychain = true;
                    entry.token = None;
                }
                Err(e) => {
                    tracing::warn!(
                        connector = %connector_name,
                        error = %e,
                        "keychain save failed; falling back to credentials.yaml"
                    );
                }
            }
        }

        if !wrote_to_keychain {
            entry.token = Some(token.to_string());
        }

        write_yaml_index(data_dir, &entries)?;
        Ok(())
    }

    /// Save OAuth2 credentials for a connector. Secrets (token, refresh_token,
    /// client_secret) go to the keychain; non-secret fields (client_id) stay
    /// in `credentials.yaml`.
    pub fn save_oauth2(
        data_dir: &Path,
        connector_name: &str,
        token: &str,
        refresh_token: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<()> {
        Self::save_oauth2_with_keychain(
            data_dir,
            connector_name,
            token,
            refresh_token,
            client_id,
            client_secret,
            !keychain_disabled(),
        )
    }

    pub fn save_oauth2_with_keychain(
        data_dir: &Path,
        connector_name: &str,
        token: &str,
        refresh_token: &str,
        client_id: &str,
        client_secret: &str,
        use_keychain: bool,
    ) -> Result<()> {
        let mut entries = read_yaml_index(data_dir)?;
        let entry = entries.entry(connector_name.to_string()).or_default();
        entry.client_id = Some(client_id.to_string());

        let mut wrote_to_keychain = false;
        if use_keychain {
            let secret = KeychainSecret {
                token: Some(token.to_string()),
                refresh_token: Some(refresh_token.to_string()),
                client_secret: Some(client_secret.to_string()),
            };
            match keychain_set(connector_name, &secret) {
                Ok(()) => {
                    wrote_to_keychain = true;
                    entry.token = None;
                    entry.refresh_token = None;
                    entry.client_secret = None;
                }
                Err(e) => {
                    tracing::warn!(
                        connector = %connector_name,
                        error = %e,
                        "keychain save failed; falling back to credentials.yaml"
                    );
                }
            }
        }

        if !wrote_to_keychain {
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
    use std::sync::{Mutex, Once};

    struct MockKeychain {
        store: Mutex<HashMap<String, String>>,
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

    static INIT_MOCK: Once = Once::new();

    fn install_mock_keychain() {
        INIT_MOCK.call_once(|| {
            let _ = KEYCHAIN.set(Box::new(MockKeychain {
                store: Mutex::new(HashMap::new()),
            }) as Box<dyn KeychainBackend>);
        });
    }

    /// Generate a unique connector name per test to avoid cross-test contamination
    /// in the in-process mock keychain.
    fn unique_name(prefix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "test-{}-{}-{}",
            prefix,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        )
    }

    #[test]
    fn save_and_load_token_via_keychain() {
        install_mock_keychain();
        let dir = tempfile::tempdir().unwrap();
        let name = unique_name("github");

        CredentialStore::save_token_with_keychain(dir.path(), &name, "ghp_secret", true).unwrap();

        let yaml = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(
            !yaml.contains("ghp_secret"),
            "YAML should not contain plaintext token: {}",
            yaml
        );

        let store = CredentialStore::load_with_keychain(dir.path(), true).unwrap();
        assert_eq!(store.token(&name).as_deref(), Some("ghp_secret"));
    }

    #[test]
    fn save_oauth2_separates_secrets_from_metadata() {
        install_mock_keychain();
        let dir = tempfile::tempdir().unwrap();
        let name = unique_name("oauth");

        CredentialStore::save_oauth2_with_keychain(
            dir.path(),
            &name,
            "access-tok",
            "refresh-tok",
            "client-id-123",
            "client-secret-xyz",
            true,
        )
        .unwrap();

        let yaml = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(yaml.contains("client-id-123"));
        assert!(!yaml.contains("access-tok"));
        assert!(!yaml.contains("refresh-tok"));
        assert!(!yaml.contains("client-secret-xyz"));

        let store = CredentialStore::load_with_keychain(dir.path(), true).unwrap();
        let creds = store.get(&name).expect("entry");
        assert_eq!(creds.token.as_deref(), Some("access-tok"));
        assert_eq!(creds.refresh_token.as_deref(), Some("refresh-tok"));
        assert_eq!(creds.client_secret.as_deref(), Some("client-secret-xyz"));
        assert_eq!(creds.client_id.as_deref(), Some("client-id-123"));
    }

    #[test]
    fn migration_moves_yaml_secrets_into_keychain() {
        install_mock_keychain();
        let dir = tempfile::tempdir().unwrap();
        let name = unique_name("migrate");

        let mut entries = HashMap::new();
        entries.insert(
            name.clone(),
            ConnectorCredentials {
                token: Some("legacy-tok".to_string()),
                ..Default::default()
            },
        );
        write_yaml_index(dir.path(), &entries).unwrap();

        let store = CredentialStore::load_with_keychain(dir.path(), true).unwrap();
        assert_eq!(store.token(&name).as_deref(), Some("legacy-tok"));

        let secret = keychain_get(&name).unwrap().unwrap();
        assert_eq!(secret.token.as_deref(), Some("legacy-tok"));

        let yaml = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(yaml.contains("legacy-tok"));
    }

    #[test]
    fn no_keychain_writes_token_to_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let name = unique_name("nokeychain");

        CredentialStore::save_token_with_keychain(dir.path(), &name, "yaml-only-tok", false)
            .unwrap();
        let yaml = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(yaml.contains("yaml-only-tok"));

        let store = CredentialStore::load_with_keychain(dir.path(), false).unwrap();
        assert_eq!(store.token(&name).as_deref(), Some("yaml-only-tok"));
    }

    #[test]
    fn load_missing_file_returns_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::load(dir.path()).unwrap();
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
        assert!(
            !dbg.contains("ghp_super_secret"),
            "debug leaked token: {}",
            dbg
        );
        assert!(
            !dbg.contains("rt_also_secret"),
            "debug leaked refresh: {}",
            dbg
        );
        assert!(
            !dbg.contains("cs_secret"),
            "debug leaked client_secret: {}",
            dbg
        );
        assert!(
            dbg.contains("user@example.com"),
            "non-secret email should remain: {}",
            dbg
        );
        assert!(
            dbg.contains("public-client-id"),
            "non-secret client_id should remain: {}",
            dbg
        );
        assert!(
            dbg.contains("<redacted>"),
            "should annotate redaction: {}",
            dbg
        );
    }

    #[test]
    fn save_token_propagates_corrupt_yaml_error() {
        // The previous unwrap_or_default() path would silently overwrite a
        // corrupt credentials.yaml with a single-entry map, losing every other
        // connector's metadata. Now it must error.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("credentials.yaml"),
            "::: not valid yaml :::\n",
        )
        .unwrap();

        let err =
            CredentialStore::save_token_with_keychain(dir.path(), "github", "ghp_secret", false)
                .expect_err("expected error on corrupt YAML");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("parsing credentials YAML"),
            "unexpected error: {}",
            msg
        );

        // Confirm the corrupt file was not overwritten.
        let on_disk = std::fs::read_to_string(dir.path().join("credentials.yaml")).unwrap();
        assert!(on_disk.contains("not valid yaml"));
    }

    #[test]
    fn save_oauth2_propagates_corrupt_yaml_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("credentials.yaml"), "{ unterminated\n").unwrap();

        let err = CredentialStore::save_oauth2_with_keychain(
            dir.path(),
            "google",
            "tok",
            "refresh",
            "client",
            "secret",
            false,
        )
        .expect_err("expected error on corrupt YAML");
        assert!(format!("{:#}", err).contains("parsing credentials YAML"));
    }

    #[test]
    fn write_yaml_index_is_atomic_no_tempfile_left_behind() {
        // tempfile + rename means after a successful write the .tmp sidecar is
        // gone. (Crash-safety we can't directly test in-process — the rename
        // call itself is what gives us atomicity on POSIX same-fs.)
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
        assert!(
            !dir.path().join("credentials.yaml.tmp").exists(),
            "tempfile should have been renamed away"
        );
    }

    #[cfg(unix)]
    #[test]
    fn strict_perms_env_promotes_warn_to_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.yaml");
        std::fs::write(&path, "github: {}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Setting + unsetting an env var inside a parallel test runner is
        // racy, but read_yaml_index reads it once per call so the window
        // is tiny.
        std::env::set_var("TAPFS_STRICT_PERMS", "1");
        let result = read_yaml_index(dir.path());
        std::env::remove_var("TAPFS_STRICT_PERMS");

        let err = result.expect_err("expected hard error with TAPFS_STRICT_PERMS=1");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("overly permissive permissions"),
            "unexpected error: {}",
            msg
        );
    }
}

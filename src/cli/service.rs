use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    #[serde(default)]
    pub connectors: Vec<ConnectorEntry>,
    #[serde(default = "default_mount_point")]
    pub mount_point: PathBuf,
}

/// A connector entry in service.yaml.
///
/// Two YAML shapes are accepted (both parse into this enum via `untagged`):
///
/// ```yaml
/// connectors:
///   - github                            # bare-string form (auto-managed by `tap mount <name>`)
///   - name: jira                        # detailed form (human-editable overrides)
///     base_url: https://x.atlassian.net
///     auth_token_env: JIRA_TOKEN
/// ```
///
/// `add_connector(name)` always appends the bare-string form; the detailed
/// form is only produced when a human edits service.yaml directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConnectorEntry {
    Name(String),
    Detailed {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_token_env: Option<String>,
    },
}

impl ConnectorEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Name(s) => s,
            Self::Detailed { name, .. } => name,
        }
    }

    pub fn base_url(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::Detailed { base_url, .. } => base_url.as_deref(),
        }
    }

    pub fn auth_token_env(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::Detailed { auth_token_env, .. } => auth_token_env.as_deref(),
        }
    }
}

fn default_mount_point() -> PathBuf {
    PathBuf::from("/tmp/tap")
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            connectors: vec![],
            mount_point: default_mount_point(),
        }
    }
}

impl ServiceConfig {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("service.yaml")
    }

    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents =
            std::fs::read_to_string(&path).with_context(|| format!("reading {:?}", path))?;
        serde_yaml::from_str(&contents).with_context(|| format!("parsing {:?}", path))
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        let yaml = serde_yaml::to_string(self)?;
        std::fs::write(&path, yaml).with_context(|| format!("writing {:?}", path))
    }

    pub fn add_connector(&mut self, name: &str) -> bool {
        if self.connectors.iter().any(|c| c.name() == name) {
            return false; // already present — preserve existing entry (incl. overrides)
        }
        self.connectors.push(ConnectorEntry::Name(name.to_string()));
        true
    }

    pub fn remove_connector(&mut self, name: &str) -> bool {
        let len = self.connectors.len();
        self.connectors.retain(|c| c.name() != name);
        self.connectors.len() < len
    }

    pub fn get_connector(&self, name: &str) -> Option<&ConnectorEntry> {
        self.connectors.iter().find(|c| c.name() == name)
    }
}

// --- Service management ---

const PLIST_LABEL: &str = "dev.tapfs.agent";
const SYSTEMD_UNIT: &str = "tapfs.service";

pub fn plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", PLIST_LABEL))
}

pub fn systemd_unit_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/systemd/user")
        .join(SYSTEMD_UNIT)
}

/// Detect which service manager is available.
pub enum ServiceManager {
    Launchd,
    Systemd,
    None,
}

pub fn detect_service_manager() -> ServiceManager {
    if cfg!(target_os = "macos") {
        ServiceManager::Launchd
    } else if std::process::Command::new("systemctl")
        .arg("--user")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        ServiceManager::Systemd
    } else {
        ServiceManager::None
    }
}

fn tap_binary_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tap"))
}

fn logs_dir() -> PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tapfs/logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn generate_plist() -> String {
    let tap = tap_binary_path();
    let logs = logs_dir();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{tap}</string>
        <string>mount</string>
        <string>--daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{logs}/tapfs.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{logs}/tapfs.stderr.log</string>
</dict>
</plist>"#,
        label = PLIST_LABEL,
        tap = tap.display(),
        logs = logs.display(),
    )
}

pub fn generate_systemd_unit() -> String {
    let tap = tap_binary_path();
    format!(
        r#"[Unit]
Description=tapfs - mount REST APIs as files
After=network-online.target

[Service]
Type=simple
ExecStart={tap} mount --daemon
Restart=always
RestartSec=5

[Install]
WantedBy=default.target"#,
        tap = tap.display(),
    )
}

// --- Service commands ---

pub fn install() -> Result<()> {
    match detect_service_manager() {
        ServiceManager::Launchd => {
            let path = plist_path();
            let parent = path.parent().unwrap();
            std::fs::create_dir_all(parent)?;
            std::fs::write(&path, generate_plist())?;
            let status = std::process::Command::new("launchctl")
                .args(["load", "-w"])
                .arg(&path)
                .status()?;
            if status.success() {
                println!("Service installed: {}", path.display());
            } else {
                anyhow::bail!("launchctl load failed");
            }
        }
        ServiceManager::Systemd => {
            let path = systemd_unit_path();
            let parent = path.parent().unwrap();
            std::fs::create_dir_all(parent)?;
            std::fs::write(&path, generate_systemd_unit())?;
            std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status()?;
            std::process::Command::new("systemctl")
                .args(["--user", "enable", SYSTEMD_UNIT])
                .status()?;
            println!("Service installed: {}", path.display());
        }
        ServiceManager::None => {
            anyhow::bail!("No service manager found (need launchd or systemd)");
        }
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    match detect_service_manager() {
        ServiceManager::Launchd => {
            let path = plist_path();
            if path.exists() {
                let _ = std::process::Command::new("launchctl")
                    .args(["unload"])
                    .arg(&path)
                    .status();
                std::fs::remove_file(&path)?;
                println!("Service uninstalled");
            } else {
                println!("Service not installed");
            }
        }
        ServiceManager::Systemd => {
            std::process::Command::new("systemctl")
                .args(["--user", "stop", SYSTEMD_UNIT])
                .status()?;
            std::process::Command::new("systemctl")
                .args(["--user", "disable", SYSTEMD_UNIT])
                .status()?;
            let path = systemd_unit_path();
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status()?;
            println!("Service uninstalled");
        }
        ServiceManager::None => {
            anyhow::bail!("No service manager found");
        }
    }
    Ok(())
}

pub fn start() -> Result<()> {
    match detect_service_manager() {
        ServiceManager::Launchd => {
            let status = std::process::Command::new("launchctl")
                .args(["start", PLIST_LABEL])
                .status()?;
            if !status.success() {
                anyhow::bail!("launchctl start failed");
            }
            println!("Service started");
        }
        ServiceManager::Systemd => {
            let status = std::process::Command::new("systemctl")
                .args(["--user", "start", SYSTEMD_UNIT])
                .status()?;
            if !status.success() {
                anyhow::bail!("systemctl start failed");
            }
            println!("Service started");
        }
        ServiceManager::None => anyhow::bail!("No service manager found"),
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    match detect_service_manager() {
        ServiceManager::Launchd => {
            let _ = std::process::Command::new("launchctl")
                .args(["stop", PLIST_LABEL])
                .status();
            println!("Service stopped");
        }
        ServiceManager::Systemd => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "stop", SYSTEMD_UNIT])
                .status();
            println!("Service stopped");
        }
        ServiceManager::None => anyhow::bail!("No service manager found"),
    }
    Ok(())
}

pub fn restart() -> Result<()> {
    stop().ok();
    start()
}

pub async fn status() -> Result<()> {
    let socket_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tapfs/tap.sock");

    match crate::ipc::send_request(&socket_path, &serde_json::json!({"cmd": "list_connectors"}))
        .await
    {
        Ok(resp) => {
            println!("tapfs is running");
            if let Some(connectors) = resp.get("connectors").and_then(|v| v.as_array()) {
                println!("Mounted connectors:");
                for c in connectors {
                    if let Some(name) = c.as_str() {
                        println!("  - {}", name);
                    }
                }
            }
        }
        Err(_) => {
            println!("tapfs is not running");
        }
    }
    Ok(())
}

pub fn logs() -> Result<()> {
    let log_path = logs_dir().join("tapfs.stderr.log");
    if !log_path.exists() {
        anyhow::bail!("No log file at {}", log_path.display());
    }
    let status = std::process::Command::new("tail")
        .args(["-f", "-n", "100"])
        .arg(&log_path)
        .status()?;
    if !status.success() {
        anyhow::bail!("tail failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_string_form() {
        let yaml = "connectors:\n  - github\n  - linear\n";
        let cfg: ServiceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.connectors.len(), 2);
        assert_eq!(cfg.connectors[0].name(), "github");
        assert_eq!(cfg.connectors[0].base_url(), None);
    }

    #[test]
    fn parses_detailed_form_with_overrides() {
        let yaml = r#"
connectors:
  - name: jira
    base_url: https://acme.atlassian.net
    auth_token_env: JIRA_TOKEN
  - github
"#;
        let cfg: ServiceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.connectors[0].name(), "jira");
        assert_eq!(
            cfg.connectors[0].base_url(),
            Some("https://acme.atlassian.net")
        );
        assert_eq!(cfg.connectors[0].auth_token_env(), Some("JIRA_TOKEN"));
        assert_eq!(cfg.connectors[1].name(), "github");
        assert_eq!(cfg.connectors[1].base_url(), None);
    }

    #[test]
    fn add_connector_preserves_existing_detailed_entry() {
        let yaml = r#"
connectors:
  - name: jira
    base_url: https://acme.atlassian.net
"#;
        let mut cfg: ServiceConfig = serde_yaml::from_str(yaml).unwrap();
        let added = cfg.add_connector("jira");
        assert!(!added, "duplicate add must be a no-op");
        // Existing detailed entry must remain intact.
        assert_eq!(
            cfg.connectors[0].base_url(),
            Some("https://acme.atlassian.net")
        );
    }

    #[test]
    fn round_trip_preserves_both_forms() {
        let yaml = r#"
connectors:
  - github
  - name: jira
    base_url: https://acme.atlassian.net
mount_point: /mnt/tap
"#;
        let cfg: ServiceConfig = serde_yaml::from_str(yaml).unwrap();
        let serialized = serde_yaml::to_string(&cfg).unwrap();
        let reparsed: ServiceConfig = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.connectors.len(), 2);
        assert_eq!(reparsed.connectors[0].name(), "github");
        assert_eq!(reparsed.connectors[1].name(), "jira");
        assert_eq!(
            reparsed.connectors[1].base_url(),
            Some("https://acme.atlassian.net")
        );
    }

    #[test]
    fn remove_connector_works_on_detailed_entry() {
        let yaml = r#"
connectors:
  - name: jira
    base_url: https://acme.atlassian.net
  - github
"#;
        let mut cfg: ServiceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.remove_connector("jira"));
        assert_eq!(cfg.connectors.len(), 1);
        assert_eq!(cfg.connectors[0].name(), "github");
    }
}

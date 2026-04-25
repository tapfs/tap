use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    #[serde(default)]
    pub connectors: Vec<String>,
    #[serde(default = "default_mount_point")]
    pub mount_point: PathBuf,
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
        if self.connectors.iter().any(|c| c == name) {
            return false; // already present
        }
        self.connectors.push(name.to_string());
        true
    }

    pub fn remove_connector(&mut self, name: &str) -> bool {
        let len = self.connectors.len();
        self.connectors.retain(|c| c != name);
        self.connectors.len() < len
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

    match crate::ipc::send_request(
        &socket_path,
        &serde_json::json!({"cmd": "list_connectors"}),
    )
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

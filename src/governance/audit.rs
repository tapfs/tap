use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: String,
    /// Operation kind: "read", "write", "list", "create_draft", "promote", "lock", "unlock", "delete"
    pub operation: String,
    pub connector: String,
    pub collection: Option<String>,
    pub resource: Option<String>,
    /// Outcome: "success" or "error"
    pub outcome: String,
    pub detail: Option<String>,
}

/// NDJSON audit logger. Appends JSON lines to a log file.
pub struct AuditLogger {
    log_path: PathBuf,
    writer: Mutex<BufWriter<std::fs::File>>,
}

impl AuditLogger {
    /// Create a new audit logger that writes to the given path.
    /// Parent directories are created if they do not exist.
    pub fn new(log_path: PathBuf) -> Result<Self> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let writer = Mutex::new(BufWriter::new(file));
        Ok(Self { log_path, writer })
    }

    /// Serialize entry to a JSON line and append it to the log file.
    pub fn log(&self, entry: AuditEntry) -> Result<()> {
        let line = serde_json::to_string(&entry)?;
        let mut w = self.writer.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        writeln!(w, "{}", line)?;
        w.flush()?;
        Ok(())
    }

    /// Convenience method to build and log an entry in one call.
    pub fn record(
        &self,
        operation: &str,
        connector: &str,
        collection: Option<&str>,
        resource: Option<&str>,
        outcome: &str,
        detail: Option<String>,
    ) -> Result<()> {
        let entry = AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            operation: operation.to_string(),
            connector: connector.to_string(),
            collection: collection.map(|s| s.to_string()),
            resource: resource.map(|s| s.to_string()),
            outcome: outcome.to_string(),
            detail,
        };
        tracing::info!(
            op = %entry.operation,
            connector = %entry.connector,
            outcome = %entry.outcome,
            "audit"
        );
        self.log(entry)
    }

    /// Read entries from the log file, optionally filtering by connector name
    /// and limiting the number of results returned (most recent first).
    pub fn read_entries(
        &self,
        limit: Option<usize>,
        connector_filter: Option<&str>,
    ) -> Result<Vec<AuditEntry>> {
        let file = OpenOptions::new().read(true).open(&self.log_path)?;
        let reader = BufReader::new(file);

        let mut entries: Vec<AuditEntry> = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<AuditEntry>(trimmed) {
                Ok(entry) => {
                    if let Some(filter) = connector_filter {
                        if entry.connector != filter {
                            continue;
                        }
                    }
                    entries.push(entry);
                }
                Err(e) => {
                    tracing::warn!("skipping malformed audit line: {}", e);
                }
            }
        }

        // Return most recent entries first.
        entries.reverse();

        if let Some(n) = limit {
            entries.truncate(n);
        }

        Ok(entries)
    }

    /// Return the path to the log file.
    pub fn log_path(&self) -> &PathBuf {
        &self.log_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let logger = AuditLogger::new(path).unwrap();

        logger
            .record("read", "rest", Some("posts"), Some("1"), "success", None)
            .unwrap();
        logger
            .record(
                "write",
                "rest",
                Some("posts"),
                Some("2"),
                "error",
                Some("403 forbidden".into()),
            )
            .unwrap();

        let entries = logger.read_entries(None, None).unwrap();
        assert_eq!(entries.len(), 2);
        // Most recent first
        assert_eq!(entries[0].operation, "write");
        assert_eq!(entries[1].operation, "read");

        // Filter by connector
        let filtered = logger
            .read_entries(None, Some("nonexistent"))
            .unwrap();
        assert!(filtered.is_empty());

        // Limit
        let limited = logger.read_entries(Some(1), None).unwrap();
        assert_eq!(limited.len(), 1);
    }
}

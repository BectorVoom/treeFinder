//! Append-only JSONL audit log. Arguments are sanitized before logging:
//! content-bearing and secret-looking fields are replaced with size or
//! redaction markers, so the audit stream never contains document text.

use crate::domain::{AuditEvent, HdsResult};
use std::io::Write;
use std::path::PathBuf;

const CONTENT_FIELDS: &[&str] = &["content", "patch", "text", "body"];
const SECRET_MARKERS: &[&str] = &[
    "secret",
    "token",
    "password",
    "api_key",
    "apikey",
    "authorization",
];

pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        AuditLog { path }
    }

    pub fn append(&self, event: &AuditEvent) -> HdsResult<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    /// Replace content-bearing values with `{len}` markers and secret-looking
    /// values with `[redacted]`, recursively.
    pub fn sanitize_arguments(args: &serde_json::Value) -> serde_json::Value {
        match args {
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, v) in map {
                    let kl = k.to_ascii_lowercase();
                    if SECRET_MARKERS.iter().any(|m| kl.contains(m)) {
                        out.insert(k.clone(), serde_json::json!("[redacted]"));
                    } else if CONTENT_FIELDS.contains(&kl.as_str()) {
                        let size = match v {
                            serde_json::Value::String(s) => s.len(),
                            other => other.to_string().len(),
                        };
                        out.insert(k.clone(), serde_json::json!({ "bytes": size }));
                    } else {
                        out.insert(k.clone(), Self::sanitize_arguments(v));
                    }
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(Self::sanitize_arguments).collect())
            }
            other => other.clone(),
        }
    }
}

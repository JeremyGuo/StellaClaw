use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::Mutex,
};

use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Debug)]
pub struct StellaclawLogger {
    file: Mutex<File>,
    mirror_stdout: bool,
}

impl StellaclawLogger {
    /// Open a logger under root/.stellaclaw/log/ (for conversation-level logs).
    pub fn open_under_stellaclaw(root: &Path, name: &str) -> Result<Self, String> {
        let dir = root.join(".stellaclaw").join("log");
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
        let path = dir.join(name);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
            mirror_stdout: stdout_logging_enabled(),
        })
    }

    /// Open a logger under root/.log/stellaclaw/ (for host-level logs).
    pub fn open_under(root: &Path, name: &str) -> Result<Self, String> {
        let dir = root.join(".log").join("stellaclaw");
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
        let path = dir.join(name);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
            mirror_stdout: stdout_logging_enabled(),
        })
    }

    pub fn info(&self, event: &str, data: Value) {
        self.write("info", event, data);
    }

    pub fn warn(&self, event: &str, data: Value) {
        self.write("warn", event, data);
    }

    pub fn error(&self, event: &str, data: Value) {
        self.write("error", event, data);
    }

    fn write(&self, level: &str, event: &str, data: Value) {
        let ts = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
        let line = json!({
            "ts": ts,
            "level": level,
            "target": "stellaclaw",
            "event": event,
            "data": data,
        });
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
        if self.mirror_stdout {
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
        }
    }
}

fn stdout_logging_enabled() -> bool {
    std::env::var("STELLACLAW_LOG_STDOUT")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
}

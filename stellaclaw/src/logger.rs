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
    warn_file: Option<Mutex<File>>,
    error_file: Option<Mutex<File>>,
    mirror_stdout: bool,
}

impl StellaclawLogger {
    /// Open a logger under root/.stellaclaw/ (for host/workdir-level logs).
    pub fn open_under(root: &Path, name: &str) -> Result<Self, String> {
        let dir = root.join(".stellaclaw");
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
        let path = dir.join(name);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        let warn_file = open_workdir_level_log(root, "warn")?;
        let error_file = open_workdir_level_log(root, "error")?;
        Ok(Self {
            file: Mutex::new(file),
            warn_file: Some(Mutex::new(warn_file)),
            error_file: Some(Mutex::new(error_file)),
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
        let line = log_line(level, event, data);
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
        match level {
            "warn" => {
                if let Some(file) = &self.warn_file {
                    write_line(file, &line);
                }
            }
            "error" => {
                if let Some(file) = &self.error_file {
                    write_line(file, &line);
                }
            }
            _ => {}
        }
        if self.mirror_stdout {
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
        }
    }
}

pub fn append_workdir_level_log(
    workdir: &Path,
    level: &str,
    event: &str,
    data: Value,
) -> Result<(), String> {
    let line = log_line(level, event, data);
    let file = open_workdir_level_log(workdir, level)?;
    let file = Mutex::new(file);
    write_line(&file, &line);
    Ok(())
}

fn log_line(level: &str, event: &str, data: Value) -> Value {
    let ts = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    json!({
        "ts": ts,
        "level": level,
        "target": "stellaclaw",
        "event": event,
        "data": data,
    })
}

fn open_workdir_level_log(root: &Path, level: &str) -> Result<File, String> {
    let dir = root.join("logs");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    let path = dir.join(format!("{level}.log"));
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))
}

fn write_line(file: &Mutex<File>, line: &Value) {
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

fn stdout_logging_enabled() -> bool {
    std::env::var("STELLACLAW_LOG_STDOUT")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn host_logger_mirrors_warn_and_error_to_workdir_logs() {
        let root = test_root("level_logs");
        let logger = StellaclawLogger::open_under(&root, "host.log").expect("logger opens");

        logger.info("info_event", json!({}));
        logger.warn("warn_event", json!({"detail": "careful"}));
        logger.error("error_event", json!({"detail": "bad"}));

        let warn_log =
            fs::read_to_string(root.join("logs").join("warn.log")).expect("warn log is written");
        let error_log =
            fs::read_to_string(root.join("logs").join("error.log")).expect("error log is written");

        assert!(warn_log.contains("warn_event"));
        assert!(!warn_log.contains("info_event"));
        assert!(error_log.contains("error_event"));
        assert!(!error_log.contains("warn_event"));
    }

    #[test]
    fn append_workdir_level_log_writes_requested_level_file() {
        let root = test_root("append_level_log");
        append_workdir_level_log(
            &root,
            "error",
            "conversation_kernel_failed",
            json!({"conversation_id": "c1"}),
        )
        .expect("level log appends");

        let error_log =
            fs::read_to_string(root.join("logs").join("error.log")).expect("error log is written");
        assert!(error_log.contains("conversation_kernel_failed"));
        assert!(error_log.contains("c1"));
    }

    fn test_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock works")
            .as_nanos();
        env::temp_dir().join(format!("stellaclaw-logger-{name}-{unique}"))
    }
}

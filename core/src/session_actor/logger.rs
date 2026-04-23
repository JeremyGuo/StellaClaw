use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Debug)]
pub struct SessionActorLogger {
    path: PathBuf,
    file: Mutex<File>,
}

impl SessionActorLogger {
    pub fn open_default(session_id: &str) -> Result<Self, String> {
        let cwd =
            std::env::current_dir().map_err(|error| format!("failed to resolve cwd: {error}"))?;
        Self::open_under(cwd, session_id)
    }

    pub fn open_under(root: impl AsRef<Path>, session_id: &str) -> Result<Self, String> {
        let safe_session_id = sanitize_session_id(session_id);
        let dir = root
            .as_ref()
            .join(".log")
            .join("stellaclaw")
            .join(safe_session_id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;

        let path = dir.join("actor.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;

        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
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
            "target": "session_actor",
            "event": event,
            "data": data,
        });
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

fn sanitize_session_id(session_id: &str) -> String {
    let safe = session_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => ch,
            _ => '_',
        })
        .collect::<String>();
    if safe.trim_matches('_').is_empty() || safe == "." || safe == ".." {
        "session".to_string()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sanitizes_session_id_for_log_path() {
        assert_eq!(sanitize_session_id("../abc/def"), ".._abc_def");
        assert_eq!(sanitize_session_id(".."), "session");
        assert_eq!(sanitize_session_id(""), "session");
    }

    #[test]
    fn writes_actor_log_under_stellaclaw_session_dir() {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("stellaclaw_logger_test_{id}"));
        let logger = SessionActorLogger::open_under(&root, "session/1").expect("logger opens");

        logger.info("demo_event", serde_json::json!({"ok": true}));

        let log_path = root
            .join(".log")
            .join("stellaclaw")
            .join("session_1")
            .join("actor.log");
        assert_eq!(logger.path(), log_path.as_path());
        let content = std::fs::read_to_string(log_path).expect("log is readable");
        assert!(content.contains("\"event\":\"demo_event\""));
    }
}

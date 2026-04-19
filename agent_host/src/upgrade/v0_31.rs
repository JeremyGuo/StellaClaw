use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.30"
    }

    fn to_version(&self) -> &'static str {
        "0.31"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        create_transcripts(&workdir.join("sessions"), "session.json")
    }
}

fn create_transcripts(root: &Path, marker_file: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry_root = entry?.path();
        if !entry_root.join(marker_file).is_file() {
            continue;
        }
        create_transcript(&entry_root)?;
    }
    Ok(())
}

fn create_transcript(root: &Path) -> Result<()> {
    let transcript_path = root.join("transcript.jsonl");
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&transcript_path)
        .with_context(|| format!("failed to create {}", transcript_path.display()))?;
    Ok(())
}

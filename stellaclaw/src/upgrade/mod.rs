use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::config::StellaclawConfig;

mod v0_1;
mod v0_10;
mod v0_11;
mod v0_12;
mod v0_13;
mod v0_14;
mod v0_15;
mod v0_16;
mod v0_17;
mod v0_18;
mod v0_19;
mod v0_2;
mod v0_20;
mod v0_21;
mod v0_22;
mod v0_23;
mod v0_24;
mod v0_3;
mod v0_4;
mod v0_5;
mod v0_6;
mod v0_7;
mod v0_8;
mod v0_9;

pub const LEGACY_WORKDIR_VERSION: &str = "0.1";
pub const WORKDIR_VERSION_0_2: &str = "0.2";
pub const WORKDIR_VERSION_0_3: &str = "0.3";
pub const WORKDIR_VERSION_0_4: &str = "0.4";
pub const WORKDIR_VERSION_0_5: &str = "0.5";
pub const WORKDIR_VERSION_0_6: &str = "0.6";
pub const WORKDIR_VERSION_0_7: &str = "0.7";
pub const WORKDIR_VERSION_0_8: &str = "0.8";
pub const WORKDIR_VERSION_0_9: &str = "0.9";
pub const WORKDIR_VERSION_0_10: &str = "0.10";
pub const WORKDIR_VERSION_0_11: &str = "0.11";
pub const WORKDIR_VERSION_0_12: &str = "0.12";
pub const WORKDIR_VERSION_0_13: &str = "0.13";
pub const WORKDIR_VERSION_0_14: &str = "0.14";
pub const WORKDIR_VERSION_0_15: &str = "0.15";
pub const WORKDIR_VERSION_0_16: &str = "0.16";
pub const WORKDIR_VERSION_0_17: &str = "0.17";
pub const WORKDIR_VERSION_0_18: &str = "0.18";
pub const WORKDIR_VERSION_0_19: &str = "0.19";
pub const WORKDIR_VERSION_0_20: &str = "0.20";
pub const WORKDIR_VERSION_0_21: &str = "0.21";
pub const WORKDIR_VERSION_0_22: &str = "0.22";
pub const WORKDIR_VERSION_0_23: &str = "0.23";
pub const WORKDIR_VERSION_0_24: &str = "0.24";
pub const LATEST_WORKDIR_VERSION: &str = "0.25";
pub const PARTYCLAW_LATEST_WORKDIR_VERSION: &str = "0.39";

const WORKDIR_VERSION_FILE: &str = "STELLA_VERSION";
const LEGACY_WORKDIR_VERSION_FILE: &str = "VERSION";

trait WorkdirUpgrader {
    fn from_version(&self) -> &'static str;
    fn to_version(&self) -> &'static str;
    fn upgrade(&self, workdir: &Path, config: &StellaclawConfig) -> Result<()>;
}

pub fn upgrade_workdir(workdir: &Path, config: &StellaclawConfig) -> Result<bool> {
    fs::create_dir_all(workdir)
        .with_context(|| format!("failed to create workdir {}", workdir.display()))?;
    let version_path = workdir.join(WORKDIR_VERSION_FILE);
    let legacy_version_path = workdir.join(LEGACY_WORKDIR_VERSION_FILE);
    let mut current = read_workdir_version(&version_path, &legacy_version_path)?;
    let mut upgraded = false;
    let upgraders: [&dyn WorkdirUpgrader; 25] = [
        &v0_1::LegacyUpgrade,
        &v0_1::PartyClawUpgrade,
        &v0_2::ChatMessageReasoningUpgrade,
        &v0_3::ModelSelectionUpgrade,
        &v0_4::SkillUpstreamUpgrade,
        &v0_5::TokenUsageCostUpgrade,
        &v0_6::CronScriptUpgrade,
        &v0_7::CronScriptFieldRenameUpgrade,
        &v0_8::RuntimeCacheDirectoryUpgrade,
        &v0_9::ConversationNicknameUpgrade,
        &v0_10::ChannelStateDirectoryUpgrade,
        &v0_11::StellaclawWorkdirDirectoryUpgrade,
        &v0_12::SshfsWorkspaceMaterializeUpgrade,
        &v0_13::StellaclawConversationSpecialPathUpgrade,
        &v0_14::StaleSpecialLinkRepairUpgrade,
        &v0_15::MemoryV1DirectoryUpgrade,
        &v0_16::MemoryV1UsageLogUpgrade,
        &v0_17::ToolResultStructuredContentUpgrade,
        &v0_18::ConversationServiceStateUpgrade,
        &v0_19::ReasoningSummaryPartsUpgrade,
        &v0_20::WebSeenStateUpgrade,
        &v0_21::RemoveStatusServiceUpgrade,
        &v0_22::RuntimeConfigIdleCompactUpgrade,
        &v0_23::ChatMessageCompactionItemUpgrade,
        &v0_24::ToolCallItemIdUpgrade,
    ];

    while current != LATEST_WORKDIR_VERSION {
        let upgrader = upgraders
            .iter()
            .find(|item| item.from_version() == current)
            .copied()
            .ok_or_else(|| anyhow!("unsupported workdir version '{}'", current))?;
        upgrader.upgrade(workdir, config)?;
        current = upgrader.to_version();
        write_workdir_version(&version_path, current)?;
        upgraded = true;
    }

    if !version_path.exists() {
        write_workdir_version(&version_path, current)?;
        remove_legacy_version_file_if_present(&legacy_version_path)?;
        upgraded = true;
    }

    Ok(upgraded)
}

fn read_workdir_version(version_path: &Path, legacy_version_path: &Path) -> Result<&'static str> {
    if !version_path.exists() {
        if legacy_version_path.exists() {
            return read_version_file(legacy_version_path);
        }
        return Ok(LEGACY_WORKDIR_VERSION);
    }
    read_version_file(version_path)
}

fn read_version_file(version_path: &Path) -> Result<&'static str> {
    let raw = fs::read_to_string(version_path)
        .with_context(|| format!("failed to read {}", version_path.display()))?;
    match raw.trim() {
        LEGACY_WORKDIR_VERSION => Ok(LEGACY_WORKDIR_VERSION),
        PARTYCLAW_LATEST_WORKDIR_VERSION => Ok(PARTYCLAW_LATEST_WORKDIR_VERSION),
        WORKDIR_VERSION_0_2 => Ok(WORKDIR_VERSION_0_2),
        WORKDIR_VERSION_0_3 => Ok(WORKDIR_VERSION_0_3),
        WORKDIR_VERSION_0_4 => Ok(WORKDIR_VERSION_0_4),
        WORKDIR_VERSION_0_5 => Ok(WORKDIR_VERSION_0_5),
        WORKDIR_VERSION_0_6 => Ok(WORKDIR_VERSION_0_6),
        WORKDIR_VERSION_0_7 => Ok(WORKDIR_VERSION_0_7),
        WORKDIR_VERSION_0_8 => Ok(WORKDIR_VERSION_0_8),
        WORKDIR_VERSION_0_9 => Ok(WORKDIR_VERSION_0_9),
        WORKDIR_VERSION_0_10 => Ok(WORKDIR_VERSION_0_10),
        WORKDIR_VERSION_0_11 => Ok(WORKDIR_VERSION_0_11),
        WORKDIR_VERSION_0_12 => Ok(WORKDIR_VERSION_0_12),
        WORKDIR_VERSION_0_13 => Ok(WORKDIR_VERSION_0_13),
        WORKDIR_VERSION_0_14 => Ok(WORKDIR_VERSION_0_14),
        WORKDIR_VERSION_0_15 => Ok(WORKDIR_VERSION_0_15),
        WORKDIR_VERSION_0_16 => Ok(WORKDIR_VERSION_0_16),
        WORKDIR_VERSION_0_17 => Ok(WORKDIR_VERSION_0_17),
        WORKDIR_VERSION_0_18 => Ok(WORKDIR_VERSION_0_18),
        WORKDIR_VERSION_0_19 => Ok(WORKDIR_VERSION_0_19),
        WORKDIR_VERSION_0_20 => Ok(WORKDIR_VERSION_0_20),
        WORKDIR_VERSION_0_21 => Ok(WORKDIR_VERSION_0_21),
        WORKDIR_VERSION_0_22 => Ok(WORKDIR_VERSION_0_22),
        WORKDIR_VERSION_0_23 => Ok(WORKDIR_VERSION_0_23),
        WORKDIR_VERSION_0_24 => Ok(WORKDIR_VERSION_0_24),
        LATEST_WORKDIR_VERSION => Ok(LATEST_WORKDIR_VERSION),
        other => Err(anyhow!("unsupported workdir version '{}'", other)),
    }
}

fn write_workdir_version(version_path: &Path, version: &str) -> Result<()> {
    fs::write(version_path, format!("{version}\n"))
        .with_context(|| format!("failed to write {}", version_path.display()))
}

fn remove_legacy_version_file_if_present(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove legacy version file {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use std::collections::BTreeMap;

    #[test]
    fn upgrade_chain_runs_v0_12_key_path_materialization_to_latest() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-upgrade-chain-v0_12-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let conversations = root.join("conversations");
        let sshfs = conversations.join("sshfs-web-main-000001");
        let local = conversations.join("web-main-000001");
        fs::create_dir_all(sshfs.join("attachments")).unwrap();
        fs::create_dir_all(sshfs.join("src")).unwrap();
        fs::create_dir_all(&local).unwrap();
        fs::write(
            sshfs.join("attachments").join("report.txt"),
            "remote attachment",
        )
        .unwrap();
        fs::write(sshfs.join("src").join("main.rs"), "remote source").unwrap();
        fs::write(
            root.join(WORKDIR_VERSION_FILE),
            format!("{WORKDIR_VERSION_0_12}\n"),
        )
        .unwrap();

        let upgraded = upgrade_workdir(&root, &test_config()).unwrap();

        assert!(upgraded);
        assert_eq!(
            fs::read_to_string(root.join(WORKDIR_VERSION_FILE)).unwrap(),
            format!("{LATEST_WORKDIR_VERSION}\n")
        );
        assert_eq!(
            fs::read_to_string(local.join(".stellaclaw/attachments").join("report.txt")).unwrap(),
            "remote attachment"
        );
        assert!(!local.join("src").join("main.rs").exists());
        let compaction_status =
            fs::read_to_string(root.join("rundir/memory_v1/user/compaction.json")).unwrap();
        let compaction_status: serde_json::Value =
            serde_json::from_str(&compaction_status).unwrap();
        assert_eq!(compaction_status["state"], "idle");
        assert_eq!(compaction_status["attempts"], 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn upgrade_chain_accepts_v0_22_workdirs() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-upgrade-chain-v0_22-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(WORKDIR_VERSION_FILE),
            format!("{WORKDIR_VERSION_0_22}\n"),
        )
        .unwrap();

        let upgraded = upgrade_workdir(&root, &test_config()).unwrap();

        assert!(upgraded);
        assert_eq!(
            fs::read_to_string(root.join(WORKDIR_VERSION_FILE)).unwrap(),
            format!("{LATEST_WORKDIR_VERSION}\n")
        );
        let _ = fs::remove_dir_all(root);
    }

    fn test_config() -> StellaclawConfig {
        StellaclawConfig {
            version: LATEST_CONFIG_VERSION.to_string(),
            agent_server: AgentServerConfig::default(),
            default_profile: None,
            channels: Vec::new(),
            models: BTreeMap::new(),
            session_defaults: SessionDefaults::default(),
            memory: crate::config::MemoryConfig::default(),
            sandbox: SandboxConfig::default(),
            skill_sync: Vec::new(),
            available_agent_models: Vec::new(),
        }
    }
}

use crate::domain::ChannelAddress;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationApprovalState {
    Pending,
    Approved,
    Rejected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationApprovalRecord {
    pub conversation_id: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    pub state: ConversationApprovalState,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ChannelAuthorizationRecord {
    #[serde(default)]
    pub admin_user_id: Option<String>,
    #[serde(default)]
    pub admin_display_name: Option<String>,
    #[serde(default)]
    pub admin_private_conversation_id: Option<String>,
    #[serde(default)]
    pub conversations: HashMap<String, ConversationApprovalRecord>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedChannelAuthorizationStore {
    #[serde(default)]
    channels: HashMap<String, ChannelAuthorizationRecord>,
}

#[derive(Clone, Debug)]
pub struct ChannelAdminSnapshot {
    pub user_id: String,
    pub display_name: Option<String>,
    pub private_conversation_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum AdminAuthorizeOutcome {
    Authorized(ChannelAdminSnapshot),
    AlreadyAuthorized(ChannelAdminSnapshot),
    OwnedByAnotherAdmin(ChannelAdminSnapshot),
}

#[derive(Clone, Debug)]
pub struct ConversationApprovalSnapshot {
    pub conversation_id: String,
    pub user_id: Option<String>,
    pub display_name: Option<String>,
    pub state: ConversationApprovalState,
    pub updated_at: DateTime<Utc>,
}

pub struct ChannelAuthorizationManager {
    state_path: PathBuf,
    channels: HashMap<String, ChannelAuthorizationRecord>,
}

impl ChannelAuthorizationManager {
    pub fn new(workdir: impl AsRef<Path>) -> Result<Self> {
        let root = workdir.as_ref().join("channel_auth");
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        let state_path = root.join("authorizations.json");
        let channels = if state_path.is_file() {
            let raw = fs::read_to_string(&state_path)
                .with_context(|| format!("failed to read {}", state_path.display()))?;
            serde_json::from_str::<PersistedChannelAuthorizationStore>(&raw)
                .context("failed to parse channel authorization store")?
                .channels
        } else {
            HashMap::new()
        };
        Ok(Self {
            state_path,
            channels,
        })
    }

    pub fn admin_for_channel(&self, channel_id: &str) -> Option<ChannelAdminSnapshot> {
        let record = self.channels.get(channel_id)?;
        let user_id = record.admin_user_id.clone()?;
        Some(ChannelAdminSnapshot {
            user_id,
            display_name: record.admin_display_name.clone(),
            private_conversation_id: record.admin_private_conversation_id.clone(),
        })
    }

    pub fn authorize_admin(&mut self, address: &ChannelAddress) -> Result<AdminAuthorizeOutcome> {
        let user_id = address
            .user_id
            .clone()
            .ok_or_else(|| anyhow!("admin authorization requires a known user id"))?;
        let channel = self.channels.entry(address.channel_id.clone()).or_default();
        let existing = channel.admin_user_id.clone();

        let snapshot =
            |channel: &ChannelAuthorizationRecord, user_id: String| ChannelAdminSnapshot {
                user_id,
                display_name: channel.admin_display_name.clone(),
                private_conversation_id: channel.admin_private_conversation_id.clone(),
            };

        match existing {
            Some(existing_user_id) if existing_user_id == user_id => {
                channel.admin_display_name = address.display_name.clone();
                channel.admin_private_conversation_id = Some(address.conversation_id.clone());
                channel.conversations.insert(
                    address.conversation_id.clone(),
                    ConversationApprovalRecord {
                        conversation_id: address.conversation_id.clone(),
                        user_id: address.user_id.clone(),
                        display_name: address.display_name.clone(),
                        state: ConversationApprovalState::Approved,
                        updated_at: Utc::now(),
                    },
                );
                let snapshot = snapshot(channel, user_id);
                self.persist()?;
                Ok(AdminAuthorizeOutcome::AlreadyAuthorized(snapshot))
            }
            Some(existing_user_id) => Ok(AdminAuthorizeOutcome::OwnedByAnotherAdmin(snapshot(
                channel,
                existing_user_id,
            ))),
            None => {
                channel.admin_user_id = Some(user_id.clone());
                channel.admin_display_name = address.display_name.clone();
                channel.admin_private_conversation_id = Some(address.conversation_id.clone());
                channel.conversations.insert(
                    address.conversation_id.clone(),
                    ConversationApprovalRecord {
                        conversation_id: address.conversation_id.clone(),
                        user_id: address.user_id.clone(),
                        display_name: address.display_name.clone(),
                        state: ConversationApprovalState::Approved,
                        updated_at: Utc::now(),
                    },
                );
                let snapshot = snapshot(channel, user_id);
                self.persist()?;
                Ok(AdminAuthorizeOutcome::Authorized(snapshot))
            }
        }
    }

    pub fn is_channel_admin(&self, address: &ChannelAddress) -> bool {
        let Some(user_id) = address.user_id.as_deref() else {
            return false;
        };
        self.channels
            .get(&address.channel_id)
            .and_then(|record| record.admin_user_id.as_deref())
            == Some(user_id)
    }

    pub fn current_conversation_state(
        &self,
        address: &ChannelAddress,
    ) -> Option<ConversationApprovalState> {
        self.channels
            .get(&address.channel_id)
            .and_then(|channel| channel.conversations.get(&address.conversation_id))
            .map(|record| record.state.clone())
    }

    pub fn ensure_pending_conversation(
        &mut self,
        address: &ChannelAddress,
    ) -> Result<ConversationApprovalState> {
        let channel = self.channels.entry(address.channel_id.clone()).or_default();
        let entry = channel
            .conversations
            .entry(address.conversation_id.clone())
            .or_insert_with(|| ConversationApprovalRecord {
                conversation_id: address.conversation_id.clone(),
                user_id: address.user_id.clone(),
                display_name: address.display_name.clone(),
                state: ConversationApprovalState::Pending,
                updated_at: Utc::now(),
            });
        if entry.user_id.is_none() {
            entry.user_id = address.user_id.clone();
        }
        if entry.display_name.is_none() {
            entry.display_name = address.display_name.clone();
        }
        entry.updated_at = Utc::now();
        let state = entry.state.clone();
        self.persist()?;
        Ok(state)
    }

    pub fn approve_conversation(
        &mut self,
        channel_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationApprovalSnapshot> {
        self.set_conversation_state(
            channel_id,
            conversation_id,
            ConversationApprovalState::Approved,
        )
    }

    pub fn reject_conversation(
        &mut self,
        channel_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationApprovalSnapshot> {
        self.set_conversation_state(
            channel_id,
            conversation_id,
            ConversationApprovalState::Rejected,
        )
    }

    pub fn list_conversations(&self, channel_id: &str) -> Vec<ConversationApprovalSnapshot> {
        let Some(channel) = self.channels.get(channel_id) else {
            return Vec::new();
        };
        let mut items = channel
            .conversations
            .values()
            .map(|record| ConversationApprovalSnapshot {
                conversation_id: record.conversation_id.clone(),
                user_id: record.user_id.clone(),
                display_name: record.display_name.clone(),
                state: record.state.clone(),
                updated_at: record.updated_at,
            })
            .collect::<Vec<_>>();
        items.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.conversation_id.cmp(&b.conversation_id))
        });
        items
    }

    fn set_conversation_state(
        &mut self,
        channel_id: &str,
        conversation_id: &str,
        state: ConversationApprovalState,
    ) -> Result<ConversationApprovalSnapshot> {
        let channel = self
            .channels
            .get_mut(channel_id)
            .ok_or_else(|| anyhow!("channel {} has not been authorized yet", channel_id))?;
        let record = channel
            .conversations
            .entry(conversation_id.to_string())
            .or_insert_with(|| ConversationApprovalRecord {
                conversation_id: conversation_id.to_string(),
                user_id: None,
                display_name: None,
                state: state.clone(),
                updated_at: Utc::now(),
            });
        record.state = state;
        record.updated_at = Utc::now();
        let snapshot = ConversationApprovalSnapshot {
            conversation_id: record.conversation_id.clone(),
            user_id: record.user_id.clone(),
            display_name: record.display_name.clone(),
            state: record.state.clone(),
            updated_at: record.updated_at,
        };
        self.persist()?;
        Ok(snapshot)
    }

    fn persist(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&PersistedChannelAuthorizationStore {
            channels: self.channels.clone(),
        })
        .context("failed to serialize channel authorization store")?;
        fs::write(&self.state_path, raw)
            .with_context(|| format!("failed to write {}", self.state_path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::{AdminAuthorizeOutcome, ChannelAuthorizationManager, ConversationApprovalState};
    use crate::domain::ChannelAddress;
    use tempfile::TempDir;

    fn group_address() -> ChannelAddress {
        ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "-100123".to_string(),
            user_id: Some("42".to_string()),
            display_name: Some("Group User".to_string()),
        }
    }

    fn private_address() -> ChannelAddress {
        ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "42".to_string(),
            user_id: Some("42".to_string()),
            display_name: Some("Admin".to_string()),
        }
    }

    #[test]
    fn first_private_authorize_becomes_admin_and_is_auto_approved() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = ChannelAuthorizationManager::new(temp_dir.path()).unwrap();
        let outcome = manager.authorize_admin(&private_address()).unwrap();
        assert!(matches!(outcome, AdminAuthorizeOutcome::Authorized(_)));
        assert!(manager.is_channel_admin(&private_address()));
        assert_eq!(
            manager.current_conversation_state(&private_address()),
            Some(ConversationApprovalState::Approved)
        );
    }

    #[test]
    fn new_group_becomes_pending_until_approved() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = ChannelAuthorizationManager::new(temp_dir.path()).unwrap();
        manager.authorize_admin(&private_address()).unwrap();
        let state = manager
            .ensure_pending_conversation(&group_address())
            .unwrap();
        assert_eq!(state, ConversationApprovalState::Pending);
        let approved = manager
            .approve_conversation("telegram-main", "-100123")
            .unwrap();
        assert_eq!(approved.state, ConversationApprovalState::Approved);
    }
}

use crate::channel::Channel;
use crate::domain::{ChannelAddress, OutgoingMessage};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SinkTarget {
    Direct(ChannelAddress),
    Multi(Vec<SinkTarget>),
    Broadcast(String),
}

pub struct SinkRouter {
    broadcast_groups: HashMap<String, Vec<ChannelAddress>>,
}

impl SinkRouter {
    pub fn new() -> Self {
        Self {
            broadcast_groups: HashMap::new(),
        }
    }

    pub fn subscribe(&mut self, topic: impl Into<String>, address: ChannelAddress) {
        self.broadcast_groups
            .entry(topic.into())
            .or_default()
            .push(address);
    }

    pub async fn dispatch(
        &self,
        channels: &HashMap<String, Arc<dyn Channel>>,
        target: &SinkTarget,
        message: OutgoingMessage,
    ) -> Result<()> {
        for address in self.resolve_targets(target) {
            tracing::debug!(
                log_stream = "channel",
                log_key = %address.channel_id,
                kind = "sink_dispatch",
                conversation_id = %address.conversation_id,
                has_text = message.text.is_some(),
                image_count = message.images.len() as u64,
                attachment_count = message.attachments.len() as u64,
                "dispatching outgoing message through sink"
            );
            let channel = channels
                .get(&address.channel_id)
                .ok_or_else(|| anyhow!("unknown channel {}", address.channel_id))?;
            channel.send(&address, message.clone()).await?;
        }
        Ok(())
    }

    fn resolve_targets(&self, target: &SinkTarget) -> Vec<ChannelAddress> {
        let mut resolved = Vec::new();
        self.collect_targets(target, &mut resolved);
        resolved
    }

    fn collect_targets(&self, target: &SinkTarget, resolved: &mut Vec<ChannelAddress>) {
        match target {
            SinkTarget::Direct(address) => resolved.push(address.clone()),
            SinkTarget::Multi(targets) => {
                for nested in targets {
                    self.collect_targets(nested, resolved);
                }
            }
            SinkTarget::Broadcast(topic) => {
                if let Some(targets) = self.broadcast_groups.get(topic) {
                    resolved.extend(targets.iter().cloned());
                }
            }
        }
    }
}

impl Default for SinkRouter {
    fn default() -> Self {
        Self::new()
    }
}

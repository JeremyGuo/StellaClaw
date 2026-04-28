use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossbeam_channel::Sender;

use crate::{conversation_id_manager::ConversationIdManager, logger::StellaclawLogger};

pub mod telegram;
pub mod types;
pub mod web;
mod web_terminal;

pub use telegram::TelegramChannel;
pub use types::{
    ChannelEvent, IncomingDispatch, OutgoingDelivery, OutgoingError, OutgoingProgressFeedback,
    OutgoingStatus, ProcessingState,
};
pub use web::WebChannel;

pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    fn send_delivery(&self, delivery: &OutgoingDelivery) -> Result<()>;
    fn send_status(&self, status: &OutgoingStatus) -> Result<()>;
    fn send_event(&self, event: &ChannelEvent) -> Result<()> {
        match event {
            ChannelEvent::Delivery(delivery) => self.send_delivery(delivery),
            ChannelEvent::Processing(processing) => {
                self.set_processing(&processing.platform_chat_id, processing.state)
            }
            ChannelEvent::ProgressFeedback(feedback) => self.update_progress_feedback(feedback),
            ChannelEvent::Status(status) => self.send_status(status),
            ChannelEvent::Error(error) => self.send_error(error),
        }
    }
    fn send_error(&self, error: &OutgoingError) -> Result<()> {
        let mut text = error.message.clone();
        if let Some(action) = error
            .suggested_action
            .as_deref()
            .filter(|action| !action.trim().is_empty())
        {
            text.push('\n');
            text.push_str(action);
        }
        self.send_delivery(&OutgoingDelivery {
            channel_id: error.channel_id.clone(),
            platform_chat_id: error.platform_chat_id.clone(),
            text,
            attachments: Vec::new(),
            options: None,
        })
    }
    fn set_processing(&self, _platform_chat_id: &str, _state: ProcessingState) -> Result<()> {
        Ok(())
    }
    fn update_progress_feedback(&self, _feedback: &OutgoingProgressFeedback) -> Result<()> {
        Ok(())
    }
    fn spawn_ingress(
        self: Arc<Self>,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
        logger: Arc<StellaclawLogger>,
    ) where
        Self: Sized;
}

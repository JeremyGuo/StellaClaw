use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossbeam_channel::Sender;

use crate::{conversation_id_manager::ConversationIdManager, logger::StellaclawLogger};

pub mod telegram;
pub mod types;
pub mod web;

pub use telegram::TelegramChannel;
pub use types::{
    ChannelEvent, IncomingDispatch, OutgoingDelivery, OutgoingError, OutgoingMessageAppended,
    OutgoingSessionStream, ProcessingState,
};
pub use web::WebChannel;

pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    fn send_delivery(&self, delivery: &OutgoingDelivery) -> Result<()>;
    fn send_event(&self, event: &ChannelEvent) -> Result<()> {
        match event {
            ChannelEvent::Delivery(delivery) => self.send_delivery(delivery),
            ChannelEvent::MessageAppended(appended) => self.message_appended(appended),
            ChannelEvent::SessionStream(stream) => self.session_stream(stream),
            ChannelEvent::Processing(processing) => {
                self.set_processing(&processing.platform_chat_id, processing.state)
            }
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
            conversation_id: error.conversation_id.clone(),
            session_id: None,
            message: None,
            text,
            attachments: Vec::new(),
            options: None,
        })
    }
    fn set_processing(&self, _platform_chat_id: &str, _state: ProcessingState) -> Result<()> {
        Ok(())
    }
    fn message_appended(&self, _appended: &OutgoingMessageAppended) -> Result<()> {
        Ok(())
    }
    fn session_stream(&self, _stream: &OutgoingSessionStream) -> Result<()> {
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

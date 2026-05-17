use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossbeam_channel::Sender;

use crate::{conversation_id_manager::ConversationIdManager, logger::StellaclawLogger};

pub mod telegram;
pub mod types;
pub mod web;

pub use telegram::TelegramChannel;
pub use types::{
    ChannelEvent, IncomingDispatch, OutgoingError, OutgoingMessageAppended, OutgoingSessionStream,
    ProcessingState,
};
pub use web::WebChannel;

pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    fn send_event(&self, event: &ChannelEvent) -> Result<()> {
        match event {
            ChannelEvent::Home(home) => self.home_event(&home.payload),
            ChannelEvent::MessageAppended(appended) => self.message_appended(appended),
            ChannelEvent::SessionStream(stream) => self.session_stream(stream),
            ChannelEvent::Processing(processing) => {
                self.set_processing(&processing.platform_chat_id, processing.state)
            }
            ChannelEvent::Error(error) => self.send_error(error),
        }
    }
    fn send_error(&self, _error: &OutgoingError) -> Result<()> {
        Ok(())
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
    fn home_event(&self, _payload: &serde_json::Value) -> Result<()> {
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

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
    IncomingDispatch, OutgoingDelivery, OutgoingProgressFeedback, OutgoingStatus, ProcessingState,
};
pub use web::WebChannel;

pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    fn send_delivery(&self, delivery: &OutgoingDelivery) -> Result<()>;
    fn send_status(&self, status: &OutgoingStatus) -> Result<()>;
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

use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossbeam_channel::Sender;

use crate::{conversation_id_manager::ConversationIdManager, logger::StellaclawLogger};

pub mod telegram;
pub mod types;

pub use telegram::TelegramChannel;
pub use types::{IncomingDispatch, OutgoingDelivery};

pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    fn send_delivery(&self, delivery: &OutgoingDelivery) -> Result<()>;
    fn spawn_ingress(
        self: Arc<Self>,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
        logger: Arc<StellaclawLogger>,
    ) where
        Self: Sized;
}

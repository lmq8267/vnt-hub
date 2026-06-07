use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

use crate::config::Args;
use crate::db::Database;
use crate::models::ServerMessage;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub args: Args,
    pub clients: Arc<RwLock<HashMap<String, mpsc::UnboundedSender<ServerMessage>>>>,
    pub events: broadcast::Sender<String>,
    pub login_failures: Arc<Mutex<HashMap<String, LoginFailure>>>,
}

#[derive(Debug, Clone)]
pub struct LoginFailure {
    pub count: u32,
    pub locked_until: i64,
}

impl AppState {
    pub fn new(db: Database, args: Args) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            db,
            args,
            clients: Arc::new(RwLock::new(HashMap::new())),
            events,
            login_failures: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

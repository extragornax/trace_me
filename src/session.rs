use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::broadcast;

use crate::db::Ping;

pub struct Channels {
    inner: Mutex<HashMap<String, broadcast::Sender<Ping>>>,
}

impl Channels {
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
    }

    pub fn get_or_create(&self, session_id: &str) -> broadcast::Sender<Ping> {
        let mut map = self.inner.lock().unwrap();
        map.entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    pub fn subscribe(&self, session_id: &str) -> Option<broadcast::Receiver<Ping>> {
        let map = self.inner.lock().unwrap();
        map.get(session_id).map(|tx| tx.subscribe())
    }

    pub fn remove_if_empty(&self, session_id: &str) {
        let mut map = self.inner.lock().unwrap();
        if let Some(tx) = map.get(session_id) {
            if tx.receiver_count() == 0 {
                map.remove(session_id);
            }
        }
    }
}

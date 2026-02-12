use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aprs_proto::client::Bounce;
use aprs_proto::primitives::{SlotId, TeamId};
use aprs_server_core::bounce_matches;
use aprs_server_core::traits::{GetGame, GetSlotId, GetTeamId, HasTag};
use rand::Rng;
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tungstenite::Bytes;

use crate::config::DeathlinkProbability;

pub type ClientId = u64;

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(0);

pub enum ClientResponse {
    Values(Vec<Value>),
    Raw(Arc<str>),
    Pong(Bytes),
}

pub struct ClientEntry {
    pub slot: SlotId,
    pub team: TeamId,
    pub game: String,
    pub tags: HashSet<String>,
    pub sender: mpsc::Sender<ClientResponse>,
}

impl GetSlotId for ClientEntry {
    fn get_slot_id(&self) -> SlotId {
        self.slot
    }
}

impl GetTeamId for ClientEntry {
    fn get_team_id(&self) -> TeamId {
        self.team
    }
}

impl GetGame for ClientEntry {
    fn get_game(&self) -> &str {
        &self.game
    }
}

impl HasTag for ClientEntry {
    fn has_tag(&self, tag: &str) -> bool {
        self.tags.contains(tag)
    }
}

pub struct ClientRegistry {
    clients: RwLock<HashMap<ClientId, ClientEntry>>,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
        }
    }

    pub fn allocate_id() -> ClientId {
        NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn register(&self, id: ClientId, entry: ClientEntry) {
        self.clients.write().await.insert(id, entry);
    }

    pub async fn deregister(&self, id: ClientId) {
        self.clients.write().await.remove(&id);
    }

    pub async fn update_tags(&self, id: ClientId, tags: HashSet<String>) {
        if let Some(entry) = self.clients.write().await.get_mut(&id) {
            entry.tags = tags;
        }
    }

    pub async fn route_bounce(
        &self,
        sender_id: ClientId,
        bounce_value: &Value,
        deathlink_exclusions: &HashSet<SlotId>,
        deathlink_probability: &DeathlinkProbability,
        room_id: &str,
    ) {
        let Ok(bounce) = serde_json::from_value::<Bounce>(bounce_value.clone()) else {
            log::warn!("Failed to parse Bounce message for routing");
            return;
        };

        let is_deathlink = bounce.tags.iter().any(|t| t == "DeathLink");

        // Build bounced message from raw JSON to preserve data exactly
        let mut bounced = bounce_value.clone();
        if let Some(obj) = bounced.as_object_mut() {
            obj.insert("cmd".into(), Value::String("Bounced".into()));
        }

        let Ok(serialized) = serde_json::to_string(&[&bounced]) else {
            log::warn!("Failed to serialize Bounced message for routing");
            return;
        };
        let serialized: Arc<str> = serialized.into();

        let clients = self.clients.read().await;
        let Some(sender) = clients.get(&sender_id) else {
            return;
        };
        let sender_team = sender.team;

        for (_id, client) in clients.iter() {
            if !bounce_matches(&bounce, sender_team, client) {
                continue;
            }

            if is_deathlink {
                if deathlink_exclusions.contains(&client.slot) {
                    continue;
                }
                let probability = deathlink_probability.get();
                if probability < 1.0 {
                    let roll: f64 = rand::rng().random();
                    if roll >= probability {
                        continue;
                    }
                }
            }

            if client
                .sender
                .try_send(ClientResponse::Raw(Arc::clone(&serialized)))
                .is_ok()
            {
                crate::metrics::record_message(
                    room_id,
                    client.slot,
                    "Bounced",
                    "upstream_to_client",
                );
            }
        }
    }
}

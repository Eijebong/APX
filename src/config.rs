use anyhow::{Context, Result};
use reqwest::Url;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct Config {
    pub lobby_root_url: Url,
    pub lobby_api_key: String,
    pub db_url: String,
    pub apx_api_key: String,
    pub room_id: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Config {
            lobby_root_url: std::env::var("LOBBY_ROOT_URL")
                .context("LOBBY_ROOT_URL")?
                .parse()
                .context("LOBBY_ROOT_URL")?,
            lobby_api_key: std::env::var("LOBBY_API_KEY").context("LOBBY_API_KEY")?,
            db_url: std::env::var("DATABASE_URL").context("DATABASE_URL")?,
            apx_api_key: std::env::var("APX_API_KEY").context("APX_API_KEY")?,
            room_id: std::env::var("LOBBY_ROOM_ID").context("LOBBY_ROOM_ID")?,
        })
    }
}

pub struct AppState {
    pub config: Config,
    pub passwords: Arc<RwLock<HashMap<u32, String>>>,
}

pub enum Signal {
    DeathLink,
}

use anyhow::{Result, bail};
use aprs_proto::primitives::SlotId;
use std::collections::HashMap;

use crate::config::Config;
use crate::proto::SlotPasswordInfo;

pub async fn refresh_login_info(config: &Config) -> Result<HashMap<SlotId, String>> {
    let url = config
        .lobby_root_url
        .join(&format!("/api/room/{}/slots_passwords", config.room_id))?;

    log::info!("Fetching slot passwords from {}", url);

    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .header("X-Api-Key", &config.lobby_api_key)
        .send()
        .await?;

    if !response.status().is_success() {
        bail!("Failed to fetch slot passwords: HTTP {}", response.status());
    }

    let slots: Vec<SlotPasswordInfo> = response.json().await?;

    let mut password_map = HashMap::new();
    for slot_info in slots {
        let password = slot_info.password.unwrap_or_default();
        if password.is_empty() {
            log::debug!(
                "Slot {} ({}) has no password",
                slot_info.slot_number,
                slot_info.player_name
            );
        } else {
            log::debug!(
                "Loaded password for slot {} ({})",
                slot_info.slot_number,
                slot_info.player_name
            );
        }
        password_map.insert(SlotId(slot_info.slot_number as i64), password);
    }

    log::info!("Loaded passwords for {} slots", password_map.len());
    Ok(password_map)
}

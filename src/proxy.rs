use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::Sender;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::{connect_async_with_config, tungstenite::Message};
use tungstenite::extensions::compression::deflate::DeflateConfig;
use tungstenite::protocol::WebSocketConfig;

const AUTH_TIMEOUT: Duration = Duration::from_secs(60);

use aprs_proto::primitives::SlotId;

use crate::DataPackageCache;
use crate::config::{DeathlinkProbability, Signal};
use crate::metrics;
use crate::proto::{Bounced, ConnectUpdate, Connected, GetDataPackage, PrintJSON, RoomInfo, Say};
use crate::registry::{ClientEntry, ClientRegistry, ClientResponse};

const MAX_MESSAGE_SIZE: usize = 15 * 1024 * 1024; // 15 MB
const MAX_SAY_LENGTH: usize = 2000;

#[derive(Clone, Debug)]
pub enum ConnectionState {
    WaitingForRoomInfo,
    WaitingForConnect,
    WaitingForConnected {
        password: String,
        tags: Vec<String>,
        game: String,
    },
    LoggedIn,
}

enum MessageDecision {
    Forward,
    ForwardWithRegistration {
        registration: RegistrationData,
        inject_response: Option<Value>,
    },
    Modified,
    Drop,
    DropAndRoute,
    DropWithResponse(Value),
    DropWithRawResponse(Arc<str>),
    SendConnectionRefused,
}

struct RegistrationData {
    slot: SlotId,
    team: aprs_proto::primitives::TeamId,
    game: String,
    tags: Vec<String>,
}

#[derive(Default)]
struct ClientHandlerResult {
    modified: bool,
    response: Option<ClientResponse>,
    bounces_to_route: Vec<Value>,
    tag_update: Option<HashSet<String>>,
}

enum UpstreamResult {
    Continue {
        modified: bool,
        inject_response: Option<Value>,
        registration: Option<RegistrationData>,
    },
    SendConnectionRefused,
}

pub async fn handle_client<S>(
    socket: S,
    upstream_url: &str,
    signal_sender: Sender<Signal>,
    passwords: Arc<RwLock<HashMap<SlotId, String>>>,
    deathlink_exclusions: Arc<RwLock<HashSet<SlotId>>>,
    deathlink_probability: Arc<DeathlinkProbability>,
    datapackage_cache: Arc<DataPackageCache>,
    room_id: String,
    inject_notext: bool,
    client_registry: Arc<ClientRegistry>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let state = Arc::new(Mutex::new(ConnectionState::WaitingForRoomInfo));
    let slot_info = Arc::new(Mutex::new(None::<(SlotId, String)>));
    let mut config = WebSocketConfig::default();
    config.extensions.permessage_deflate = Some(DeflateConfig::default());

    let client_ws = accept_async_with_config(socket, Some(config)).await?;
    let (upstream_ws, _) = connect_async_with_config(upstream_url, Some(config), false).await?;

    let (mut upstream_write, mut upstream_read) = upstream_ws.split();
    let (mut client_write, mut client_read) = client_ws.split();

    // Channel for sending responses back to client
    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<ClientResponse>(32);
    let response_tx_for_registry = response_tx.clone();
    let client_id = ClientRegistry::allocate_id();

    let state_client = state.clone();
    let slot_info_client = slot_info.clone();
    let signal_sender_client = signal_sender.clone();
    let deathlink_exclusions_client = deathlink_exclusions.clone();
    let deathlink_probability_client = deathlink_probability.clone();
    let datapackage_cache_client = datapackage_cache.clone();
    let room_id_client = room_id.clone();
    let client_registry_client = client_registry.clone();
    let client_to_upstream = async move {
        while let Some(msg) = client_read.next().await {
            let msg = match msg {
                Ok(msg) => msg,
                Err(_) => break,
            };

            // Handle ping frames directly. Respond with pong without forwarding to upstream
            // This should keep clients alive even when the upstream AP server is slow/overloaded
            if let Message::Ping(data) = &msg {
                log::trace!("Responding to client ping directly");
                let _ = response_tx.send(ClientResponse::Pong(data.clone())).await;
                continue;
            }

            let Message::Text(text) = msg else {
                if msg.len() > MAX_MESSAGE_SIZE {
                    log::warn!(
                        "Dropping oversized non-text message from client ({} bytes)",
                        msg.len()
                    );
                    continue;
                }
                if upstream_write.send(msg).await.is_err() {
                    log::error!("Error while writing non text message");
                    break;
                }
                continue;
            };

            if text.len() > MAX_MESSAGE_SIZE {
                log::warn!(
                    "Dropping oversized message from client ({} bytes)",
                    text.len()
                );
                continue;
            }

            let Ok(mut commands) = parse_message(&text) else {
                log::error!("Invalid JSON received from client, closing connection");
                break;
            };

            let handler_result = {
                let mut state = state_client.lock().await;
                let slot_info = slot_info_client.lock().await;
                let exclusions = deathlink_exclusions_client.read().await;
                match handle_client_messages(
                    &mut state,
                    &mut commands,
                    &slot_info,
                    &signal_sender_client,
                    &exclusions,
                    &datapackage_cache_client,
                    inject_notext,
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        log::error!("Error while handling message from client: {}", e);
                        break;
                    }
                }
            };

            let slot_info_snapshot = slot_info_client.lock().await.clone();

            for bounce in &handler_result.bounces_to_route {
                if let Some((slot, _)) = &slot_info_snapshot {
                    metrics::record_message(&room_id_client, *slot, "Bounce", "client_to_upstream");
                }
                let exclusions = deathlink_exclusions_client.read().await;
                client_registry_client
                    .route_bounce(
                        client_id,
                        bounce,
                        &exclusions,
                        &deathlink_probability_client,
                        &room_id_client,
                    )
                    .await;
            }

            if let Some(tags) = handler_result.tag_update {
                client_registry_client.update_tags(client_id, tags).await;
            }

            if let Some(response) = handler_result.response {
                if response_tx.send(response).await.is_err() {
                    break;
                }
            }

            // Don't send if all messages were filtered out
            if commands.is_empty() {
                continue;
            }

            if let Some((slot, name)) = &slot_info_snapshot {
                log::debug!(
                    "[slot {} ({})] Forwarding {} ({:?}) commands to upstream (modified: {})",
                    slot.0,
                    name,
                    commands.len(),
                    CommandList(&commands),
                    handler_result.modified
                );

                // Record metrics for each command
                for cmd in &commands {
                    if let Some(cmd_type) = get_cmd(cmd) {
                        metrics::record_message(
                            &room_id_client,
                            *slot,
                            cmd_type,
                            "client_to_upstream",
                        );
                    }
                }
            } else {
                log::debug!(
                    "Forwarding {} ({:?}) commands to upstream (modified: {})",
                    commands.len(),
                    CommandList(&commands),
                    handler_result.modified
                );
            }

            let msg_to_send = if handler_result.modified {
                let Ok(serialized) = serde_json::to_string(&commands) else {
                    log::error!("Error while reserializing commands");
                    break;
                };
                Message::Text(serialized.into())
            } else {
                Message::Text(text)
            };

            if upstream_write.send(msg_to_send).await.is_err() {
                break;
            }
        }
    };

    let state_upstream = state.clone();
    let passwords_upstream = passwords.clone();
    let deathlink_exclusions_upstream = deathlink_exclusions.clone();
    let deathlink_probability_upstream = deathlink_probability.clone();
    let slot_info_upstream = slot_info.clone();
    let room_id_upstream = room_id.clone();
    let inject_notext_upstream = inject_notext;
    let client_registry_cleanup = client_registry.clone();
    let upstream_to_client = async move {
        loop {
            tokio::select! {
                msg = upstream_read.next() => {
                    let Some(msg) = msg else {
                        break;
                    };
                    let msg = match msg {
                        Ok(msg) => msg,
                        Err(_) => break,
                    };

                    let Message::Text(text) = msg else {
                        if msg.len() > MAX_MESSAGE_SIZE {
                            log::warn!(
                                "Dropping oversized non-text message from upstream ({} bytes)",
                                msg.len()
                            );
                            continue;
                        }
                        if client_write.send(msg).await.is_err() {
                            log::error!("Error while writing non text message");
                            break;
                        }
                        continue;
                    };

                    if text.len() > MAX_MESSAGE_SIZE {
                        log::warn!(
                            "Dropping oversized message from upstream ({} bytes)",
                            text.len()
                        );
                        continue;
                    }

                    let Ok(mut commands) = parse_message(&text) else {
                        log::error!("Invalid JSON received from upstream, closing connection");
                        break;
                    };

                    // Extract slot info from Connected message
                    for cmd in &commands {
                        if get_cmd(cmd) == Some("Connected")
                            && let Ok(connected) = parse_as::<Connected>(cmd)
                        {
                            let player_name = connected
                                .players
                                .iter()
                                .find(|p| p.slot == connected.slot)
                                .map(|p| p.name.clone())
                                .unwrap_or_else(|| format!("Unknown-{}", connected.slot.0));

                            let mut info = slot_info_upstream.lock().await;
                            *info = Some((connected.slot, player_name));
                            break;
                        }
                    }

                    let result = {
                        let mut state = state_upstream.lock().await;
                        let passwords_read = passwords_upstream.read().await;
                        let exclusions = deathlink_exclusions_upstream.read().await;
                        let slot_info_read = slot_info_upstream.lock().await;
                        match handle_upstream_messages(
                            &mut state,
                            &mut commands,
                            &passwords_read,
                            &exclusions,
                            &slot_info_read,
                            &deathlink_probability_upstream,
                            inject_notext_upstream,
                        ) {
                            Ok(result) => result,
                            Err(e) => {
                                log::error!("Error while validating upstream message: {}", e);
                                break;
                            }
                        }
                    };

                    let (mut modified, inject_response, registration) = match result {
                        UpstreamResult::Continue { modified, inject_response, registration } => (modified, inject_response, registration),
                        UpstreamResult::SendConnectionRefused => {
                            // Send ConnectionRefused to client and revert state to allow retry
                            let refused = serde_json::json!({
                                "cmd": "ConnectionRefused",
                                "errors": ["InvalidPassword"]
                            });
                            let refused_msg =
                                Message::Text(serde_json::to_string(&[refused]).unwrap().into());
                            if client_write.send(refused_msg).await.is_err() {
                                break;
                            }

                            // Revert state back to WaitingForConnect to allow retry
                            let mut state = state_upstream.lock().await;
                            *state = ConnectionState::WaitingForConnect;
                            continue;
                        }
                    };

                    if let Some(response) = inject_response {
                        commands.push(response);
                        modified = true;
                    }

                    if let Some(reg) = registration {
                        client_registry.register(
                            client_id,
                            ClientEntry {
                                slot: reg.slot,
                                team: reg.team,
                                game: reg.game,
                                tags: reg.tags.into_iter().collect(),
                                sender: response_tx_for_registry.clone(),
                            },
                        ).await;
                    }

                    // Get slot info once for logging and metrics
                    let slot_info_snapshot = slot_info_upstream.lock().await.clone();

                    if let Some((slot, name)) = &slot_info_snapshot {
                        log::debug!(
                            "[slot {} ({})] Forwarding {} ({:?}) commands to client (modified: {})",
                            slot.0,
                            name,
                            commands.len(),
                            CommandList(&commands),
                            modified
                        );

                        // Record metrics for each command
                        for cmd in &commands {
                            if let Some(cmd_type) = get_cmd(cmd) {
                                metrics::record_message(
                                    &room_id_upstream,
                                    *slot,
                                    cmd_type,
                                    "upstream_to_client",
                                );
                            }
                        }
                    } else {
                        log::debug!(
                            "Forwarding {} ({:?}) commands to client (modified: {})",
                            commands.len(),
                            CommandList(&commands),
                            modified
                        );
                    }

                    if commands.is_empty() {
                        continue;
                    }

                    let msg_to_send = if modified {
                        let Ok(serialized) = serde_json::to_string(&commands) else {
                            log::error!("Error while reserializing commands");
                            break;
                        };
                        Message::Text(serialized.into())
                    } else {
                        Message::Text(text)
                    };

                    if client_write.send(msg_to_send).await.is_err() {
                        break;
                    }
                }
                Some(response) = response_rx.recv() => {
                    let response_msg = match response {
                        ClientResponse::Values(values) => {
                            Message::Text(serde_json::to_string(&values).unwrap().into())
                        }
                        ClientResponse::Raw(raw) => {
                            Message::Text((*raw).into())
                        }
                        ClientResponse::Pong(data) => {
                            Message::Pong(data)
                        }
                    };
                    if client_write.send(response_msg).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    let state_timeout = state.clone();
    let auth_timeout = async move {
        tokio::time::sleep(AUTH_TIMEOUT).await;
        let state = state_timeout.lock().await;
        if !matches!(*state, ConnectionState::LoggedIn) {
            log::warn!(
                "Client failed to authenticate within {:?}, closing connection",
                AUTH_TIMEOUT
            );
            true
        } else {
            drop(state);
            std::future::pending::<bool>().await
        }
    };

    tokio::select! {
        _ = client_to_upstream => log::debug!("Client connection closed"),
        _ = upstream_to_client => log::debug!("Upstream connection closed"),
        timed_out = auth_timeout => {
            if timed_out {
                log::debug!("Connection closed due to auth timeout");
            }
        }
    }

    client_registry_cleanup.deregister(client_id).await;
    Ok(())
}

async fn handle_client_messages(
    state: &mut ConnectionState,
    messages: &mut Vec<Value>,
    slot_info: &Option<(SlotId, String)>,
    signal_sender: &Sender<Signal>,
    deathlink_exclusions: &HashSet<SlotId>,
    datapackage_cache: &Arc<DataPackageCache>,
    inject_notext: bool,
) -> Result<ClientHandlerResult> {
    let mut result = ClientHandlerResult::default();
    let mut error = None;

    messages.retain_mut(|message| {
        if error.is_some() {
            return false;
        }

        let decision = match handle_client_message(
            state,
            message,
            slot_info,
            signal_sender,
            deathlink_exclusions,
            datapackage_cache,
            inject_notext,
        ) {
            Ok(decision) => decision,
            Err(e) => {
                error = Some(e);
                return false;
            }
        };

        match decision {
            MessageDecision::Drop => {
                result.modified = true;
                false
            }
            MessageDecision::DropAndRoute => {
                result.bounces_to_route.push(message.clone());
                result.modified = true;
                false
            }
            MessageDecision::DropWithResponse(value) => {
                result.response = Some(ClientResponse::Values(vec![value]));
                result.modified = true;
                false
            }
            MessageDecision::DropWithRawResponse(raw) => {
                result.response = Some(ClientResponse::Raw(raw));
                result.modified = true;
                false
            }
            MessageDecision::Forward | MessageDecision::Modified => {
                if get_cmd(message) == Some("ConnectUpdate") {
                    if let Ok(update) = parse_as::<ConnectUpdate>(message) {
                        result.tag_update = Some(update.tags.into_iter().collect());
                    }
                }
                if matches!(decision, MessageDecision::Modified) {
                    result.modified = true;
                }
                true
            }
            MessageDecision::ForwardWithRegistration { .. }
            | MessageDecision::SendConnectionRefused => {
                unreachable!(
                    "Client messages should never return ForwardWithRegistration or SendConnectionRefused"
                )
            }
        }
    });

    if let Some(e) = error {
        return Err(e);
    }

    Ok(result)
}

fn handle_client_message(
    state: &mut ConnectionState,
    cmd: &mut Value,
    slot_info: &Option<(SlotId, String)>,
    signal_sender: &Sender<Signal>,
    deathlink_exclusions: &HashSet<SlotId>,
    datapackage_cache: &Arc<DataPackageCache>,
    inject_notext: bool,
) -> Result<MessageDecision> {
    let cmd_type = get_cmd(cmd);

    if cmd_type == Some("GetDataPackage") {
        if let Ok(request) = parse_as::<GetDataPackage>(cmd) {
            if !request.games.is_empty() {
                log::debug!(
                    "Serving DataPackage from cache for games: {:?}",
                    request.games
                );
                return Ok(MessageDecision::DropWithRawResponse(
                    datapackage_cache.response_for_games(&request.games),
                ));
            }
            if !request.exclusions.is_empty() {
                log::debug!(
                    "Serving DataPackage from cache excluding: {:?}",
                    request.exclusions
                );
                return Ok(MessageDecision::DropWithRawResponse(
                    datapackage_cache.response_excluding_games(&request.exclusions),
                ));
            }
            log::debug!("Serving full DataPackage from cache");
            return Ok(MessageDecision::DropWithRawResponse(Arc::clone(
                datapackage_cache.full_response(),
            )));
        }
        log::warn!("Unreadable datapackage request, forwarding to upstream");
        return Ok(MessageDecision::Forward);
    }

    if cmd_type == Some("Bounce") {
        if let Ok(bounced) = parse_as::<Bounced>(cmd) {
            if bounced.tags.contains(&"DeathLink".to_string()) {
                if let Some((slot, name)) = slot_info {
                    if deathlink_exclusions.contains(slot) {
                        log::info!(
                            "Dropping outgoing DeathLink from excluded slot {} ({})",
                            slot.0,
                            name
                        );
                        return Ok(MessageDecision::Drop);
                    }

                    let source = bounced
                        .data
                        .get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown")
                        .to_string();
                    let cause = bounced
                        .data
                        .get("cause")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    log::info!(
                        "DeathLink sent from slot {} ({}): source={}, cause={:?}",
                        slot.0,
                        name,
                        source,
                        cause
                    );
                    let _ = signal_sender.try_send(Signal::DeathLink {
                        slot: *slot,
                        source,
                        cause,
                    });
                }
            }
        }
        return Ok(MessageDecision::DropAndRoute);
    }

    if cmd_type == Some("Say") {
        if let Ok(say) = parse_as::<Say>(cmd) {
            if say.text.len() > MAX_SAY_LENGTH {
                log::warn!("Dropping oversized Say message ({} chars)", say.text.len());
                let denial =
                    PrintJSON::with_color("Your message is too long. Please reconsider.", "red");
                let denial_value = serde_json::to_value(denial).unwrap();
                return Ok(MessageDecision::DropWithResponse(denial_value));
            }

            if is_command(&say.text, "countdown") {
                if let Some((slot, name)) = slot_info {
                    log::info!("Intercepted !countdown from slot {} ({})", slot.0, name);
                    let _ = signal_sender.try_send(Signal::CountdownInit { slot: *slot });
                } else {
                    log::warn!("Received !countdown but slot info not available yet");
                }

                let denial = PrintJSON::with_color(
                    "Starting countdowns is not allowed. This attempt has been logged.",
                    "red",
                );
                let denial_value = serde_json::to_value(denial).unwrap();
                return Ok(MessageDecision::DropWithResponse(denial_value));
            }
        }
    }

    match state {
        ConnectionState::WaitingForRoomInfo => {
            bail!("Received message from client while waiting for RoomInfo. This is a client bug.")
        }
        ConnectionState::WaitingForConnect => {
            if cmd_type != Some("Connect") {
                log::debug!(
                    "Received non Connect ({:?}) client message while waiting for connect, dropping it.",
                    cmd_type
                );
                return Ok(MessageDecision::Drop);
            }

            log::debug!("Intercepted Connect packet");

            let password = cmd
                .get("password")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let game = cmd
                .get("game")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if let Some(obj) = cmd.as_object_mut() {
                // Empty the password before forwarding to upstream
                obj.insert(
                    "password".to_string(),
                    serde_json::Value::String("".to_string()),
                );

                if inject_notext {
                    let tags = obj
                        .entry("tags")
                        .or_insert_with(|| serde_json::Value::Array(vec![]));
                    if let Some(tags_array) = tags.as_array_mut() {
                        if !tags_array.contains(&serde_json::Value::String("NoText".to_string())) {
                            tags_array.push(serde_json::Value::String("NoText".to_string()));
                        }
                        log::debug!("Injected NoText tag into Connect");
                    }
                }
            }

            // Extract tags after NoText injection so they reflect actual state
            let tags: Vec<String> = cmd
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            *state = ConnectionState::WaitingForConnected {
                password,
                tags,
                game,
            };
            Ok(MessageDecision::Modified)
        }
        ConnectionState::WaitingForConnected { .. } => {
            log::debug!(
                "Dropping client message {:?} while waiting for authentication",
                cmd_type
            );
            Ok(MessageDecision::Drop)
        }
        ConnectionState::LoggedIn => {
            if inject_notext && cmd_type == Some("ConnectUpdate") {
                if let Ok(mut update) = parse_as::<ConnectUpdate>(cmd) {
                    if !update.tags.contains(&"NoText".to_string()) {
                        log::debug!("Injecting NoText tag into ConnectUpdate");
                        update.tags.push("NoText".to_string());
                        *cmd = serde_json::to_value(update)?;
                        return Ok(MessageDecision::Modified);
                    }
                }
            }
            Ok(MessageDecision::Forward)
        }
    }
}

fn handle_upstream_messages(
    state: &mut ConnectionState,
    messages: &mut Vec<Value>,
    login_info: &HashMap<SlotId, String>,
    deathlink_exclusions: &HashSet<SlotId>,
    slot_info: &Option<(SlotId, String)>,
    deathlink_probability: &DeathlinkProbability,
    inject_notext: bool,
) -> Result<UpstreamResult> {
    let mut modified = false;
    let mut send_refused = false;
    let mut error = None;
    let mut inject_response = None;
    let mut registration = None;

    messages.retain_mut(|message| {
        if send_refused || error.is_some() {
            return false;
        }

        let decision = match handle_upstream_message(
            state,
            message,
            login_info,
            deathlink_exclusions,
            slot_info,
            deathlink_probability,
            inject_notext,
        ) {
            Ok(decision) => decision,
            Err(e) => {
                error = Some(e);
                return false;
            }
        };

        match decision {
            MessageDecision::Drop => {
                modified = true;
                false
            }
            MessageDecision::DropWithResponse(_)
            | MessageDecision::DropWithRawResponse(_)
            | MessageDecision::DropAndRoute => {
                unreachable!(
                    "Upstream messages should never return DropWithResponse or DropAndRoute"
                )
            }
            MessageDecision::Forward => true,
            MessageDecision::ForwardWithRegistration {
                registration: reg,
                inject_response: resp,
            } => {
                registration = Some(reg);
                if let Some(resp) = resp {
                    inject_response = Some(resp);
                }
                true
            }
            MessageDecision::Modified => {
                modified = true;
                true
            }
            MessageDecision::SendConnectionRefused => {
                send_refused = true;
                false
            }
        }
    });

    if let Some(e) = error {
        return Err(e);
    }

    if send_refused {
        return Ok(UpstreamResult::SendConnectionRefused);
    }

    Ok(UpstreamResult::Continue {
        modified,
        inject_response,
        registration,
    })
}

fn handle_upstream_message(
    state: &mut ConnectionState,
    cmd: &mut Value,
    login_info: &HashMap<SlotId, String>,
    deathlink_exclusions: &HashSet<SlotId>,
    slot_info: &Option<(SlotId, String)>,
    deathlink_probability: &DeathlinkProbability,
    inject_notext: bool,
) -> Result<MessageDecision> {
    let cmd_type = get_cmd(cmd);

    if cmd_type == Some("Bounced") {
        log::warn!(
            "Received unexpected Bounced from upstream (bounces should be handled by proxy)"
        );
        if let Ok(bounced) = parse_as::<Bounced>(cmd) {
            if bounced.tags.contains(&"DeathLink".to_string()) {
                if let Some((slot, name)) = slot_info {
                    if deathlink_exclusions.contains(slot) {
                        log::info!(
                            "Dropping incoming DeathLink for excluded slot {} ({})",
                            slot.0,
                            name
                        );
                        return Ok(MessageDecision::Drop);
                    }

                    let probability = deathlink_probability.get();
                    if probability < 1.0 {
                        let roll: f64 = rand::rng().random();
                        if roll >= probability {
                            log::info!(
                                "Dropping incoming DeathLink for slot {} ({}) due to probability filter ({:.1}% chance, rolled {:.3})",
                                slot.0,
                                name,
                                probability * 100.0,
                                roll
                            );
                            return Ok(MessageDecision::Drop);
                        }
                    }
                }
            }
        }
    }

    // Hide Admin client connections/disconnections
    // Also hide cheat console as that is used by admins
    if cmd_type == Some("PrintJSON") {
        if let Ok(print_json) = parse_as::<PrintJSON>(cmd) {
            match print_json.type_.as_deref() {
                Some("Join") if print_json.tags.contains(&"Admin".to_string()) => {
                    log::debug!("Hiding Admin client join message");
                    return Ok(MessageDecision::Drop);
                }
                Some("Part") => {
                    // Part messages don't have tags field, but the text includes the tags. It's
                    // brittle but it's all we have :/
                    let text: String = print_json.data.iter().map(|p| p.text.as_str()).collect();
                    if text.contains("'Admin'") {
                        log::debug!("Hiding Admin client part message");
                        return Ok(MessageDecision::Drop);
                    }
                }
                Some("ItemCheat") => return Ok(MessageDecision::Drop),
                None => {
                    let is_cheat_console = print_json
                        .data
                        .iter()
                        .any(|data| data.text.contains("Cheat console"));
                    if is_cheat_console {
                        return Ok(MessageDecision::Drop);
                    }
                }
                _ => {}
            }
        }
    }

    match state {
        ConnectionState::WaitingForRoomInfo => {
            if get_cmd(cmd) != Some("RoomInfo") {
                bail!("Received non RoomInfo as the first upstream message, this is a bug")
            }

            let mut room_info = parse_as::<RoomInfo>(cmd)?;
            log::debug!("Intercepted RoomInfo packet");

            room_info.set_password(true);
            *cmd = serde_json::to_value(room_info)?;
            *state = ConnectionState::WaitingForConnect;
            Ok(MessageDecision::Modified)
        }
        ConnectionState::WaitingForConnect => {
            log::debug!(
                "Dropping upstream message {:?} while waiting for Connect",
                cmd_type
            );
            Ok(MessageDecision::Drop)
        }
        ConnectionState::WaitingForConnected {
            password,
            tags,
            game,
        } => {
            let cmd_type = get_cmd(cmd);
            let password = password.clone();
            let connect_tags = tags.clone();
            let connect_game = game.clone();

            if cmd_type == Some("Connected") {
                let connected = parse_as::<Connected>(cmd)?;
                log::debug!("Intercepted Connected packet for slot {}", connected.slot.0);

                let expected_password = login_info.get(&connected.slot);

                match expected_password {
                    Some(expected) if !expected.is_empty() => {
                        if password != *expected {
                            log::warn!("Invalid password provided for slot {}", connected.slot.0);
                            return Ok(MessageDecision::SendConnectionRefused);
                        }
                        log::info!(
                            "Password validated successfully for slot {} (notext: {})",
                            connected.slot.0,
                            inject_notext
                        );
                    }
                    Some(_) | None => {
                        log::info!(
                            "No password required for slot {}, allowing connection (notext: {})",
                            connected.slot.0,
                            inject_notext
                        );
                    }
                }

                let registration = RegistrationData {
                    slot: connected.slot,
                    team: connected.team,
                    game: connect_game,
                    tags: connect_tags,
                };

                *state = ConnectionState::LoggedIn;
                let inject_response = if inject_notext {
                    let confirmation =
                        PrintJSON::with_color("Connected to APX proxy (NoText mode)", "green");
                    Some(serde_json::to_value(confirmation)?)
                } else {
                    None
                };
                Ok(MessageDecision::ForwardWithRegistration {
                    registration,
                    inject_response,
                })
            } else if cmd_type == Some("ConnectionRefused") {
                log::debug!("Connection refused by upstream");
                Ok(MessageDecision::Forward)
            } else {
                bail!(
                    "Expected Connected, ConnectionRefused, or DataPackage, got {:?}",
                    cmd_type
                );
            }
        }
        ConnectionState::LoggedIn => Ok(MessageDecision::Forward),
    }
}

fn get_cmd(value: &serde_json::Value) -> Option<&str> {
    value.get("cmd").and_then(|v| v.as_str())
}

struct CommandList<'a>(&'a [serde_json::Value]);

impl<'a> std::fmt::Debug for CommandList<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|cmd| get_cmd(cmd).unwrap_or("Unknown")))
            .finish()
    }
}

fn parse_as<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T> {
    T::deserialize(value).map_err(Into::into)
}

fn parse_message(text: &str) -> Result<Vec<serde_json::Value>> {
    if let Ok(commands) = serde_json::from_str::<Vec<serde_json::Value>>(text) {
        return Ok(commands);
    }

    if let Ok(single) = serde_json::from_str::<serde_json::Value>(text) {
        return Ok(vec![single]);
    }

    bail!("Could not parse message as JSON")
}

fn is_command(text: &str, command_name: &str) -> bool {
    // This matches as best we can the way archipelago does command parsing
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    // try:
    //     command = shlex.split(raw, comments=False)
    // except ValueError:  # most likely: "ValueError: No closing quotation"
    //     command = raw.split()
    let parts = match shlex::split(trimmed) {
        Some(parts) => parts,
        None => trimmed.split_whitespace().map(String::from).collect(),
    };

    if parts.is_empty() {
        return false;
    }

    // basecommand = command[0]
    // if basecommand[0] == self.marker:
    //     method = self.commands.get(basecommand[1:].lower(), None)
    let first = &parts[0];
    first.starts_with('!') && first[1..].eq_ignore_ascii_case(command_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_command_basic() {
        assert!(is_command("!countdown", "countdown"));
        assert!(is_command("  !countdown", "countdown"));
        assert!(is_command("!countdown 10", "countdown"));
        assert!(is_command("!COUNTDOWN", "countdown"));
        assert!(is_command("\"!COUNTDOWN\"", "countdown"));
        assert!(is_command("'!COUNTDOWN'", "countdown"));
        assert!(is_command("  !countdown  ", "countdown"));
        assert!(is_command("  !\\countdown  ", "countdown"));
        assert!(is_command("  !\"\"countdown  ", "countdown"));
    }

    #[test]
    fn test_is_command_with_args() {
        assert!(is_command("!countdown 30", "countdown"));
        assert!(is_command("!countdown 30 message", "countdown"));
        assert!(is_command("!countdown 10 '", "countdown"));
    }

    #[test]
    fn test_is_command_with_quotes() {
        assert!(is_command("!countdown \"some message\"", "countdown"));
        assert!(is_command("!countdown 'message with spaces'", "countdown"));
    }

    #[test]
    fn test_is_command_not_matching() {
        assert!(!is_command("countdown", "countdown"));
        assert!(!is_command("!other", "countdown"));
        assert!(!is_command("!countdownfoo", "countdown"));
        assert!(!is_command("", "countdown"));
        assert!(!is_command("  ", "countdown"));
    }

    #[test]
    fn test_is_command_embedded_in_text() {
        assert!(is_command("!countdown", "countdown"));
        assert!(!is_command("!countdowntest", "countdown"));
    }

    #[test]
    fn test_is_command_case_insensitive() {
        assert!(is_command("!CoUnTdOwN", "countdown"));
        assert!(is_command("!COUNTDOWN", "countdown"));
        assert!(is_command("!countdown", "COUNTDOWN"));
    }
}

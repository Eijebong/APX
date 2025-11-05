use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::Sender;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::{connect_async_with_config, tungstenite::Message};
use tungstenite::extensions::compression::deflate::DeflateConfig;
use tungstenite::protocol::WebSocketConfig;

use crate::config::Signal;
use crate::metrics;
use crate::proto::{Connected, PrintJSON, RoomInfo, Say};

#[derive(Clone, Debug)]
pub enum ConnectionState {
    WaitingForRoomInfo,
    WaitingForConnect,
    WaitingForConnected { password: String },
    LoggedIn,
}

enum MessageDecision {
    Forward,
    Modified,
    Drop,
    DropWithResponse(Value),
    SendConnectionRefused,
}

enum UpstreamResult {
    Continue { modified: bool },
    SendConnectionRefused,
}

pub async fn handle_client<S>(
    socket: S,
    upstream_url: &str,
    signal_sender: Sender<Signal>,
    passwords: Arc<RwLock<HashMap<u32, String>>>,
    room_id: String,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let state = Arc::new(Mutex::new(ConnectionState::WaitingForRoomInfo));
    let slot_info = Arc::new(Mutex::new(None::<(u32, String)>));
    let mut config = WebSocketConfig::default();
    config.extensions.permessage_deflate = Some(DeflateConfig::default());

    let client_ws = accept_async_with_config(socket, Some(config)).await?;
    let (upstream_ws, _) = connect_async_with_config(upstream_url, Some(config), false).await?;

    let (mut upstream_write, mut upstream_read) = upstream_ws.split();
    let (mut client_write, mut client_read) = client_ws.split();

    // Channel for sending responses back to client
    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<Vec<Value>>(32);

    let state_client = state.clone();
    let slot_info_client = slot_info.clone();
    let signal_sender_client = signal_sender.clone();
    let room_id_client = room_id.clone();
    let client_to_upstream = async move {
        while let Some(msg) = client_read.next().await {
            let msg = match msg {
                Ok(msg) => msg,
                Err(_) => break,
            };

            let Message::Text(text) = msg else {
                if upstream_write.send(msg).await.is_err() {
                    log::error!("Error while writing non text message");
                    break;
                }
                continue;
            };

            let Ok(mut commands) = parse_message(&text) else {
                log::error!("Invalid JSON received from client, closing connection");
                break;
            };

            let (modified, responses) = {
                let mut state = state_client.lock().await;
                let slot_info = slot_info_client.lock().await;
                match handle_client_messages(
                    &mut state,
                    &mut commands,
                    &slot_info,
                    &signal_sender_client,
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

            if !responses.is_empty() && response_tx.send(responses).await.is_err() {
                break;
            }

            // Don't send if all messages were filtered out
            if commands.is_empty() {
                continue;
            }

            // Get slot info once for logging and metrics
            let slot_info_snapshot = slot_info_client.lock().await.clone();

            if let Some((slot, name)) = &slot_info_snapshot {
                log::debug!(
                    "[slot {} ({})] Forwarding {} ({:?}) commands to upstream (modified: {})",
                    slot,
                    name,
                    commands.len(),
                    CommandList(&commands),
                    modified
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
                    modified
                );
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

            if upstream_write.send(msg_to_send).await.is_err() {
                break;
            }
        }
    };

    let state_upstream = state.clone();
    let passwords_upstream = passwords.clone();
    let slot_info_upstream = slot_info.clone();
    let room_id_upstream = room_id.clone();
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
                if client_write.send(msg).await.is_err() {
                    log::error!("Error while writing non text message");
                    break;
                }
                continue;
            };

            let Ok(mut commands) = parse_message(&text) else {
                log::error!("Invalid JSON received from upstream, closing connection");
                break;
            };

            // Extract slot info from Connected message
            for cmd in &commands {
                if get_cmd(cmd) == Some("Connected")
                    && let Ok(connected) = parse_as::<Connected>(cmd) {
                        let player_name = connected
                            .players
                            .iter()
                            .find(|p| p.slot == connected.slot)
                            .map(|p| p.name.clone())
                            .unwrap_or_else(|| format!("Unknown-{}", connected.slot));

                        let mut info = slot_info_upstream.lock().await;
                        *info = Some((connected.slot, player_name));
                        break;
                    }
            }

            let result = {
                let mut state = state_upstream.lock().await;
                let passwords_read = passwords_upstream.read().await;
                match handle_upstream_messages(&mut state, &mut commands, &passwords_read) {
                    Ok(result) => result,
                    Err(e) => {
                        log::error!("Error while validating upstream message: {}", e);
                        break;
                    }
                }
            };

            let modified = match result {
                UpstreamResult::Continue { modified } => modified,
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

            // Get slot info once for logging and metrics
            let slot_info_snapshot = slot_info_upstream.lock().await.clone();

            if let Some((slot, name)) = &slot_info_snapshot {
                log::debug!(
                    "[slot {} ({})] Forwarding {} ({:?}) commands to client (modified: {})",
                    slot,
                    name,
                    commands.len(),
                    CommandList(&commands),
                    modified
                );

                // Record metrics for each command
                for cmd in &commands {
                    if let Some(cmd_type) = get_cmd(cmd) {
                        metrics::record_message(&room_id_upstream, *slot, cmd_type, "upstream_to_client");
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
                Some(responses) = response_rx.recv() => {
                    let response_msg = Message::Text(serde_json::to_string(&responses).unwrap().into());
                    if client_write.send(response_msg).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = client_to_upstream => log::debug!("Client connection closed"),
        _ = upstream_to_client => log::debug!("Upstream connection closed"),
    }

    Ok(())
}

async fn handle_client_messages(
    state: &mut ConnectionState,
    messages: &mut Vec<Value>,
    slot_info: &Option<(u32, String)>,
    signal_sender: &Sender<Signal>,
) -> Result<(bool, Vec<Value>)> {
    let mut modified = false;
    let mut error = None;
    let mut responses = Vec::new();

    messages.retain_mut(|message| {
        if error.is_some() {
            return false;
        }

        let decision = match handle_client_message(state, message, slot_info, signal_sender) {
            Ok(decision) => decision,
            Err(e) => {
                error = Some(e);
                return false;
            }
        };

        match decision {
            MessageDecision::Drop => false,
            MessageDecision::DropWithResponse(response) => {
                responses.push(response);
                false
            }
            MessageDecision::Forward => true,
            MessageDecision::Modified => {
                modified = true;
                true
            }
            MessageDecision::SendConnectionRefused => {
                unreachable!("Client messages should never return SendConnectionRefused")
            }
        }
    });

    if let Some(e) = error {
        return Err(e);
    }

    Ok((modified, responses))
}

fn handle_client_message(
    state: &mut ConnectionState,
    cmd: &mut Value,
    slot_info: &Option<(u32, String)>,
    signal_sender: &Sender<Signal>,
) -> Result<MessageDecision> {
    let cmd_type = get_cmd(cmd);

    if cmd_type == Some("Say")
        && let Ok(say) = parse_as::<Say>(cmd)
        && say.text.starts_with("!countdown")
    {
        if let Some((slot, name)) = slot_info {
            log::info!("Intercepted !countdown from slot {} ({})", slot, name);
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

    match state {
        ConnectionState::WaitingForRoomInfo => {
            bail!("Received message from client while waiting for RoomInfo. This is a client bug.")
        }
        ConnectionState::WaitingForConnect => {
            if cmd_type == Some("GetDataPackage") {
                log::debug!("Received data package request, letting it through");
                return Ok(MessageDecision::Forward);
            }

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

            // Empty the password before forwarding to upstream
            if let Some(obj) = cmd.as_object_mut() {
                obj.insert(
                    "password".to_string(),
                    serde_json::Value::String("".to_string()),
                );
            }

            *state = ConnectionState::WaitingForConnected { password };
            Ok(MessageDecision::Modified)
        }
        ConnectionState::WaitingForConnected { .. } => {
            if cmd_type != Some("GetDataPackage") {
                log::debug!(
                    "Dropping client message {:?} while waiting for authentication",
                    cmd_type
                );
                return Ok(MessageDecision::Drop);
            }
            Ok(MessageDecision::Forward)
        }
        ConnectionState::LoggedIn => Ok(MessageDecision::Forward),
    }
}

fn handle_upstream_messages(
    state: &mut ConnectionState,
    messages: &mut Vec<Value>,
    login_info: &HashMap<u32, String>,
) -> Result<UpstreamResult> {
    let mut modified = false;
    let mut send_refused = false;
    let mut error = None;

    messages.retain_mut(|message| {
        if send_refused || error.is_some() {
            return false;
        }

        let decision = match handle_upstream_message(state, message, login_info) {
            Ok(decision) => decision,
            Err(e) => {
                error = Some(e);
                return false;
            }
        };

        match decision {
            MessageDecision::Drop => false,
            MessageDecision::DropWithResponse(_) => {
                unreachable!("Upstream messages should never return DropWithResponse")
            }
            MessageDecision::Forward => true,
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

    Ok(UpstreamResult::Continue { modified })
}

fn handle_upstream_message(
    state: &mut ConnectionState,
    cmd: &mut Value,
    login_info: &HashMap<u32, String>,
) -> Result<MessageDecision> {
    let cmd_type = get_cmd(cmd);

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
            if cmd_type == Some("DataPackage") {
                log::debug!("Allowing DataPackage response through while waiting for Connect");
                return Ok(MessageDecision::Forward);
            }

            // Drop any other messages - they might be responses to a previous failed auth attempt
            log::debug!(
                "Dropping upstream message {:?} while waiting for Connect",
                cmd_type
            );
            Ok(MessageDecision::Drop)
        }
        ConnectionState::WaitingForConnected { password } => {
            let cmd_type = get_cmd(cmd);

            if cmd_type == Some("DataPackage") {
                log::debug!("Allowing DataPackage response through while waiting for Connected");
                return Ok(MessageDecision::Forward);
            }

            let password = password.clone();

            if cmd_type == Some("Connected") {
                let connected = parse_as::<Connected>(cmd)?;
                log::debug!("Intercepted Connected packet for slot {}", connected.slot);

                let expected_password = login_info.get(&connected.slot);

                match expected_password {
                    Some(expected) if !expected.is_empty() => {
                        // Slot has a password, validate it
                        if password == *expected {
                            log::info!(
                                "Password validated successfully for slot {}",
                                connected.slot
                            );
                            *state = ConnectionState::LoggedIn;
                        } else {
                            log::warn!("Invalid password provided for slot {}", connected.slot);
                            return Ok(MessageDecision::SendConnectionRefused);
                        }
                    }
                    Some(_) | None => {
                        // Slot has no password (empty string) or not found - allow connection
                        log::info!(
                            "No password required for slot {}, allowing connection",
                            connected.slot
                        );
                        *state = ConnectionState::LoggedIn;
                    }
                }
                Ok(MessageDecision::Forward)
            } else if cmd_type == Some("ConnectionRefused") {
                // Connection was refused by upstream, just forward it
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
    serde_json::from_value(value.clone()).map_err(Into::into)
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

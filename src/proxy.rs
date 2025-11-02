use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};

use crate::config::Signal;
use crate::proto::{Connected, RoomInfo};

#[derive(Clone, Debug)]
pub enum ConnectionState {
    WaitingForRoomInfo,
    WaitingForConnect,
    WaitingForConnected { password: String },
    LoggedIn,
}

pub async fn handle_client(
    socket: tokio::net::TcpStream,
    upstream_url: &str,
    _signal_sender: Sender<Signal>,
    passwords: Arc<RwLock<HashMap<u32, String>>>,
) -> Result<()> {
    let state = Arc::new(Mutex::new(ConnectionState::WaitingForRoomInfo));

    let client_ws = accept_async(socket).await?;
    let (upstream_ws, _) = connect_async(upstream_url).await?;

    let (mut upstream_write, mut upstream_read) = upstream_ws.split();
    let (mut client_write, mut client_read) = client_ws.split();

    let state_client = state.clone();
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

            let modified = {
                let mut state = state_client.lock().await;
                match handle_client_message(&mut *state, &mut commands) {
                    Ok(modified) => modified,
                    Err(e) => {
                        log::error!("Error while handling message from client: {}", e);
                        break;
                    }
                }
            };

            let msg_to_send = if modified {
                let Ok(serialized) = serde_json::to_string(&commands) else {
                    log::error!("Error while reserializing commands");
                    break;
                };
                Message::Text(serialized)
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
    let upstream_to_client = async move {
        while let Some(msg) = upstream_read.next().await {
            let msg = match msg {
                Ok(msg) => msg,
                Err(_) => break,
            };

            let Message::Text(text) = msg else {
                if client_write.send(msg).await.is_err() {
                    log::error!("Error while writing non text message");
                    break;
                }
                break;
            };

            let Ok(mut commands) = parse_message(&text) else {
                log::error!("Invalid JSON received from upstream, closing connection");
                break;
            };

            let modified = {
                let mut state = state_upstream.lock().await;
                let passwords_read = passwords_upstream.read().await;
                match handle_upstream_message(&mut *state, &mut commands, &passwords_read) {
                    Ok(modified) => modified,
                    Err(e) => {
                        log::error!("Error while handling message from upstream: {}", e);
                        break;
                    }
                }
            };

            let msg_to_send = if modified {
                let Ok(serialized) = serde_json::to_string(&commands) else {
                    log::error!("Error while reserializing commands");
                    break;
                };
                Message::Text(serialized)
            } else {
                Message::Text(text)
            };

            if client_write.send(msg_to_send).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = client_to_upstream => log::debug!("Client connection closed"),
        _ = upstream_to_client => log::debug!("Upstream connection closed"),
    }

    Ok(())
}

fn handle_client_message(state: &mut ConnectionState, messages: &mut [Value]) -> Result<bool> {
    let mut modified = false;

    match state {
        ConnectionState::WaitingForRoomInfo => {
            bail!("Received message from client while waiting for RoomInfo. This is a client bug.")
        }
        ConnectionState::WaitingForConnect => {
            if messages.is_empty() {
                bail!(
                    "Got empty messages while waiting for connect, this is a bug"
                );
            }

            for cmd in messages {
                if get_cmd(cmd) == Some("GetDataPackage") {
                    log::debug!("Received data package request, letting it through");
                    continue
                }

                if get_cmd(cmd) != Some("Connect") {
                    bail!("Received non Connect as the first client message, this is a bug")
                }

                log::debug!("Intercepted Connect packet");

                let password = cmd
                    .get("password")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                // Empty the password before forwarding to upstream
                if let Some(obj) = cmd.as_object_mut() {
                    obj.insert("password".to_string(), serde_json::Value::String("".to_string()));
                }

                *state = ConnectionState::WaitingForConnected { password };
                modified = true;
            }
        }
        ConnectionState::WaitingForConnected { .. } => {
            bail!(
                "Client should not send any message while waiting for the `Connected` response from upstream"
            )
        }
        ConnectionState::LoggedIn => {}
    }

    Ok(modified)
}

fn handle_upstream_message(
    state: &mut ConnectionState,
    messages: &mut [Value],
    login_info: &HashMap<u32, String>,
) -> Result<bool> {
    let mut modified = false;

    match state {
        ConnectionState::WaitingForRoomInfo => {
            if messages.len() != 1 {
                bail!(
                    "Received message that contained more than the room info from upstream, this is a bug"
                );
            }
            let cmd = &mut messages[0];
            if get_cmd(cmd) != Some("RoomInfo") {
                bail!("Received non RoomInfo as the first upstream message, this is a bug")
            }
            let mut room_info = parse_as::<RoomInfo>(cmd)?;
            log::debug!("Intercepted RoomInfo packet");

            room_info.set_password(true);
            *cmd = serde_json::to_value(room_info)?;
            *state = ConnectionState::WaitingForConnect;
            modified = true;
        }
        ConnectionState::WaitingForConnect => {
            bail!("Upstream should not send any message while waiting for the `Connect` package")
        }
        ConnectionState::WaitingForConnected { password } => {
            // We're waiting for Connected or ConnectionRefused from upstream
            if messages.len() != 1 {
                bail!(
                    "Received more messages than expected from upstream while waiting for connection result."
                );
            }

            let cmd = &messages[0];
            let cmd_type = get_cmd(cmd);

            if cmd_type == Some("Connected") {
                let connected = parse_as::<Connected>(cmd)?;
                log::debug!("Intercepted Connected packet for slot {}", connected.slot);

                let expected_password = login_info.get(&connected.slot);

                match expected_password {
                    Some(expected) if !expected.is_empty() => {
                        // Slot has a password, validate it
                        if password == expected {
                            log::info!(
                                "Password validated successfully for slot {}",
                                connected.slot
                            );
                            *state = ConnectionState::LoggedIn;
                        } else {
                            log::warn!("Invalid password provided for slot {}", connected.slot);
                            bail!("Invalid password");
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
            } else if cmd_type == Some("ConnectionRefused") {
                // Connection was refused by upstream, just forward it
                // Connection will close naturally after this message is sent
                log::debug!("Connection refused by upstream");
            } else {
                bail!(
                    "Expected Connected or ConnectionRefused, got {:?}",
                    cmd_type
                );
            }
        }
        ConnectionState::LoggedIn => {}
    }

    Ok(modified)
}

fn get_cmd(value: &serde_json::Value) -> Option<&str> {
    value.get("cmd").and_then(|v| v.as_str())
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

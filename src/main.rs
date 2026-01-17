use anyhow::{Result, bail};
use rocket::config::ShutdownConfig;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::{
    net::TcpListener,
    signal,
    sync::mpsc::{Receiver, channel},
};

mod api;
mod config;
mod db;
mod lobby;
mod metrics;
mod proto;
mod proxy;
mod tls;

use config::{AppState, Config, DeathlinkProbability, Signal};
use futures_util::{SinkExt, StreamExt};
use lobby::refresh_login_info;
use proxy::handle_client;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        unsafe { std::env::set_var("RUST_LOG", "apx=trace,info") };
    }

    env_logger::init();

    let config = Config::from_env()?;

    let db_pool = db::init_pool(&config.db_url).await?;

    let passwords = match refresh_login_info(&config).await {
        Ok(info) => Arc::new(RwLock::new(info)),
        Err(e) => {
            log::error!("Failed to fetch login info: {:?}", e);
            bail!("Failed to fetch login info");
        }
    };

    let deathlink_exclusions = match db::models::get_room_deathlink_exclusions(
        &db_pool,
        &config.room_id,
    )
    .await
    {
        Ok(exclusions) => {
            let set: HashSet<u32> = exclusions.into_iter().collect();
            log::info!("Loaded {} deathlink exclusions from database", set.len());
            Arc::new(RwLock::new(set))
        }
        Err(e) => {
            log::warn!(
                "Failed to load deathlink exclusions from database: {:?}, starting with empty set",
                e
            );
            Arc::new(RwLock::new(HashSet::new()))
        }
    };

    let deathlink_probability =
        match db::models::get_deathlink_settings(&db_pool, &config.room_id).await {
            Ok(Some(settings)) => {
                log::info!(
                    "Loaded deathlink probability from database: {:.2}%",
                    settings.probability * 100.0
                );
                Arc::new(DeathlinkProbability::new(settings.probability))
            }
            Ok(None) => {
                log::info!("No deathlink probability in database, using default 100%");
                Arc::new(DeathlinkProbability::default())
            }
            Err(e) => {
                log::warn!(
                    "Failed to load deathlink probability from database: {:?}, using default 100%",
                    e
                );
                Arc::new(DeathlinkProbability::default())
            }
        };

    let upstream_url = format!("ws://{}", config.ap_server);

    let datapackage = fetch_datapackage(&upstream_url).await?;
    log::info!(
        "Cached DataPackage at startup ({} bytes)",
        datapackage.len()
    );
    let datapackage_cache: Arc<str> = datapackage.into();
    let room_id = config.room_id.clone();

    let app_state = AppState {
        config,
        passwords: passwords.clone(),
        deathlink_exclusions: deathlink_exclusions.clone(),
        deathlink_probability: deathlink_probability.clone(),
        db_pool: db_pool.clone(),
    };

    let shutdown_config = ShutdownConfig {
        grace: 0,
        mercy: 0,
        ..Default::default()
    };

    let figment = rocket::Config::figment().merge(("shutdown", shutdown_config));

    let message_counter = metrics::init_metrics();
    let prometheus = rocket_prometheus::PrometheusMetrics::with_registry(
        rocket_prometheus::prometheus::Registry::new(),
    );
    prometheus
        .registry()
        .register(Box::new(message_counter))
        .expect("Failed to register message counter");

    // Load TLS config if provided (before moving app_state)
    let tls_acceptor = if let (Some(cert_path), Some(key_path)) = (
        &app_state.config.tls_cert_path,
        &app_state.config.tls_key_path,
    ) {
        let tls_config = tls::load_tls_config(cert_path, key_path)?;
        Some(tokio_rustls::TlsAcceptor::from(tls_config))
    } else {
        log::warn!("TLS not configured - only plain WebSocket will be available");
        None
    };

    tokio::spawn(async move {
        if let Err(e) = rocket::custom(figment)
            .manage(app_state)
            .attach(prometheus.clone())
            .mount("/api", api::routes())
            .mount("/metrics", api::MetricsRoute(prometheus))
            .launch()
            .await
        {
            log::error!("Rocket server error: {:?}", e);
        }
    });

    let listener = TcpListener::bind("0.0.0.0:36000").await?;

    log::info!("WebSocket proxy listening on 0.0.0.0:36000");
    if tls_acceptor.is_some() {
        log::info!("TLS enabled - supporting both WS and WSS");
    }
    log::info!("Forwarding to {}", upstream_url);
    let (signal_sender, signal_receiver) = channel::<Signal>(1024);
    tokio::spawn(signal_handler(signal_receiver, db_pool, room_id.clone()));

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (socket, addr) = match result {
                    Ok(conn) => conn,
                    Err(e) => {
                        log::error!("Failed to accept connection: {:?}", e);
                        continue;
                    }
                };
                let signal_sender = signal_sender.clone();
                let passwords = passwords.clone();
                let deathlink_exclusions = deathlink_exclusions.clone();
                let deathlink_probability = deathlink_probability.clone();
                let datapackage_cache = datapackage_cache.clone();
                let upstream_url = upstream_url.clone();
                let tls_acceptor = tls_acceptor.clone();
                let room_id = room_id.clone();

                tokio::spawn(async move {
                    log::debug!("New connection from {}", addr);

                    // Peek at first byte to detect TLS
                    let mut buf = [0u8; 1];
                    if let Err(e) = socket.peek(&mut buf).await {
                        log::error!("Failed to peek at connection from {}: {:?}", addr, e);
                        return;
                    }

                    // 0x16 is the TLS handshake record type
                    let is_tls = buf[0] == 0x16;

                    if is_tls {
                        if let Some(acceptor) = tls_acceptor {
                            log::debug!("Accepting TLS connection from {}", addr);
                            match acceptor.accept(socket).await {
                                Ok(tls_stream) => {
                                    if let Err(e) = handle_client(tls_stream, &upstream_url, signal_sender, passwords, deathlink_exclusions, deathlink_probability, datapackage_cache, room_id).await {
                                        log::error!("Error handling TLS client {}: {:?}", addr, e);
                                    }
                                }
                                Err(e) => {
                                    log::error!("TLS handshake failed for {}: {:?}", addr, e);
                                }
                            }
                        } else {
                            log::warn!("Client {} attempted TLS but TLS is not configured", addr);
                        }
                    } else {
                        log::debug!("Accepting plain connection from {}", addr);
                        if let Err(e) = handle_client(socket, &upstream_url, signal_sender, passwords, deathlink_exclusions, deathlink_probability, datapackage_cache, room_id).await {
                            log::error!("Error handling client {}: {:?}", addr, e);
                        }
                    }
                });
            }
            _ = signal::ctrl_c() => {
                log::info!("Received Ctrl+C, shutting down...");
                break;
            }
        }
    }

    Ok(())
}

async fn signal_handler(mut receiver: Receiver<Signal>, db_pool: db::DieselPool, room_id: String) {
    while let Some(signal) = receiver.recv().await {
        match signal {
            Signal::DeathLink {
                slot,
                source,
                cause,
            } => {
                let new_deathlink =
                    db::models::NewDeathLink::new(room_id.clone(), slot, source, cause);
                if let Err(e) = db::models::insert_deathlink(&db_pool, new_deathlink).await {
                    log::error!("Failed to insert deathlink into database: {:?}", e);
                }
            }
            Signal::CountdownInit { slot } => {
                let new_countdown = db::models::NewCountdown::new(room_id.clone(), slot);
                if let Err(e) = db::models::insert_countdown(&db_pool, new_countdown).await {
                    log::error!("Failed to insert countdown into database: {:?}", e);
                }
            }
        }
    }

    log::warn!("Signal channel has been closed")
}

async fn fetch_datapackage(upstream_url: &str) -> Result<String> {
    let (ws, _) = connect_async(upstream_url).await?;
    let (mut write, mut read) = ws.split();

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if let Message::Text(text) = msg {
            let commands: Vec<serde_json::Value> = serde_json::from_str(&text)?;
            for cmd in &commands {
                if cmd.get("cmd").and_then(|v| v.as_str()) == Some("RoomInfo") {
                    let request = r#"[{"cmd": "GetDataPackage"}]"#;
                    write.send(Message::Text(request.into())).await?;
                    break;
                }
            }
            break;
        }
    }

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if let Message::Text(text) = msg {
            let commands: Vec<serde_json::Value> = serde_json::from_str(&text)?;
            for cmd in commands {
                if cmd.get("cmd").and_then(|v| v.as_str()) == Some("DataPackage") {
                    return Ok(serde_json::to_string(&[cmd])?);
                }
            }
        }
    }

    bail!("Connection closed before receiving DataPackage")
}

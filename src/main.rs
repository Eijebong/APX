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

use config::{AppState, Config, Signal};
use lobby::refresh_login_info;
use proxy::handle_client;

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

    let deathlink_exclusions = Arc::new(RwLock::new(HashSet::new()));

    let upstream_url = format!("ws://{}", config.ap_server);
    let room_id = config.room_id.clone();

    let app_state = AppState {
        config,
        passwords: passwords.clone(),
        deathlink_exclusions: deathlink_exclusions.clone(),
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
                let (socket, addr) = result?;
                let signal_sender = signal_sender.clone();
                let passwords = passwords.clone();
                let deathlink_exclusions = deathlink_exclusions.clone();
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
                                    if let Err(e) = handle_client(tls_stream, &upstream_url, signal_sender, passwords, deathlink_exclusions, room_id).await {
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
                        if let Err(e) = handle_client(socket, &upstream_url, signal_sender, passwords, deathlink_exclusions, room_id).await {
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

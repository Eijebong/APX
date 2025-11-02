use anyhow::{Result, bail};
use rocket::config::ShutdownConfig;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::{
    net::TcpListener,
    signal,
    sync::mpsc::{Receiver, channel},
};

mod api;
mod config;
mod lobby;
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

    let passwords = match refresh_login_info(&config).await {
        Ok(info) => Arc::new(RwLock::new(info)),
        Err(e) => {
            log::error!("Failed to fetch login info: {:?}", e);
            bail!("Failed to fetch login info");
        }
    };

    let upstream_url = format!("ws://{}", config.ap_server);

    let app_state = AppState {
        config,
        passwords: passwords.clone(),
    };

    let shutdown_config = ShutdownConfig {
        grace: 0,
        mercy: 0,
        ..Default::default()
    };

    let figment = rocket::Config::figment().merge(("shutdown", shutdown_config));

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
            .mount("/api", api::routes())
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
    tokio::spawn(signal_handler(signal_receiver));

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (socket, addr) = result?;
                let signal_sender = signal_sender.clone();
                let passwords = passwords.clone();
                let upstream_url = upstream_url.clone();
                let tls_acceptor = tls_acceptor.clone();

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
                                    if let Err(e) = handle_client(tls_stream, &upstream_url, signal_sender, passwords).await {
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
                        if let Err(e) = handle_client(socket, &upstream_url, signal_sender, passwords).await {
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

async fn signal_handler(mut receiver: Receiver<Signal>) {
    while let Some(_signal) = receiver.recv().await {}

    log::warn!("Signal channel has been closed")
}

use anyhow::{Result, bail};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::{
    net::TcpListener,
    sync::mpsc::{Receiver, channel},
};

mod api;
mod config;
mod lobby;
mod proto;
mod proxy;

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

    tokio::spawn(async move {
        if let Err(e) = rocket::build()
            .manage(app_state)
            .mount("/api", api::routes())
            .launch()
            .await
        {
            log::error!("Rocket server error: {:?}", e);
        }
    });

    let listener = TcpListener::bind("0.0.0.0:8090").await?;

    log::info!("WebSocket proxy listening on 0.0.0.0:8090");
    log::info!("Forwarding to {}", upstream_url);
    let (signal_sender, signal_receiver) = channel::<Signal>(1024);
    tokio::spawn(signal_handler(signal_receiver));

    loop {
        let signal_sender = signal_sender.clone();
        let passwords = passwords.clone();
        let (socket, addr) = listener.accept().await?;
        let upstream_url = upstream_url.clone();

        tokio::spawn(async move {
            log::debug!("New connection from {}", addr);
            if let Err(e) = handle_client(socket, &upstream_url, signal_sender, passwords).await {
                log::error!("Error handling client {}: {:?}", addr, e);
            }
        });
    }
}

async fn signal_handler(mut receiver: Receiver<Signal>) {
    while let Some(_signal) = receiver.recv().await {}

    log::warn!("Signal channel has been closed")
}

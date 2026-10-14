mod config;
mod http;
mod pipeline;

use anyhow::Result;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "channels.toml".to_string());
    let cfg = config::Config::load(Path::new(&config_path))?;

    let output_root = PathBuf::from("output");
    let supervisor = pipeline::Supervisor::new(cfg.channels, &output_root)?;

    tokio::spawn(supervisor.clone().reap_loop());

    let app = http::router(supervisor);
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    tracing::info!("home-iptv in ascolto su http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

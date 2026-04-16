pub mod blur_api {
    #![allow(unused, non_camel_case_types, clippy::all)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/blur.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/blur.xml");
}

mod dbus;
mod wayland;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("glass=info")),
        )
        .init();

    tracing::info!("glass starting");

    let (tx, rx) = mpsc::unbounded_channel::<wayland::Event>();
    let (reply_tx, reply_rx) = mpsc::unbounded_channel::<wayland::Reply>();

    let wayland_handle = std::thread::spawn(move || {
        if let Err(e) = wayland::run(rx, reply_tx) {
            tracing::error!(error = %e, "wayland thread crashed");
        }
    });

    dbus::serve(tx, reply_rx).await?;

    let _ = wayland_handle.join();
    Ok(())
}

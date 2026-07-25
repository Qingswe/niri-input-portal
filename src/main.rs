//! xdg-desktop-portal InputCapture backend for niri.
//!
//! niri exposes `org.gnome.Mutter.ScreenCast` but not `org.gnome.Mutter.InputCapture`,
//! so xdg-desktop-portal-gnome never publishes an InputCapture impl under niri and
//! `CreateSession` fails for every client. This process fills that gap.

mod eis_server;
mod niri;
mod portal;
mod wayland;

use anyhow::{Context, Result};
use tracing::info;
use tracing_subscriber::EnvFilter;

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.niri-input";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("NIRI_INPUT_PORTAL_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Fail early with a clear message rather than at the first GetZones.
    let outputs = niri::outputs()
        .await
        .context("cannot talk to niri — is this running inside a niri session?")?;
    for o in &outputs {
        info!(
            "output {}: {}x{} at ({}, {}) scale {} {}",
            o.name, o.zone.width, o.zone.height, o.zone.x, o.zone.y, o.scale, o.transform
        );
    }

    let (wayland, wayland_events) =
        wayland::spawn().context("failed to start the Wayland barrier thread")?;

    let state = portal::State::new(std::sync::Arc::new(wayland));
    let conn = zbus::connection::Builder::session()
        .context("failed to connect to the session bus")?
        .name(BUS_NAME)
        .context("failed to claim the backend bus name")?
        .serve_at(PORTAL_PATH, portal::InputCapture::new(state.clone()))
        .context("failed to export the InputCapture interface")?
        .build()
        .await
        .context("failed to start the D-Bus service")?;

    // Signals are emitted against this connection, so the state cannot learn
    // about it until the service is up.
    state.set_connection(conn);
    tokio::spawn(portal::pump_wayland_events(state, wayland_events));

    info!("{BUS_NAME} ready at {PORTAL_PATH}");

    tokio::signal::ctrl_c()
        .await
        .context("failed to wait for shutdown signal")?;
    info!("shutting down");
    Ok(())
}

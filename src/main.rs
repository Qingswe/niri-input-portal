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
const CONTROL_PATH: &str = "/io/github/niri_input_portal/Control";
const CONTROL_IFACE: &str = "io.github.niri_input_portal.Control";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("NIRI_INPUT_PORTAL_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // These run against an already-running instance and exit; they exist so a
    // stranded pointer can be recovered over SSH without knowing D-Bus syntax.
    match std::env::args().nth(1).as_deref() {
        Some("--release") => return call_control("ForceRelease").await,
        Some("--disarm") => return call_control("Disarm").await,
        Some("--status") => return call_control("Status").await,
        Some(other) => anyhow::bail!(
            "unknown argument {other:?}; expected --release, --disarm or --status"
        ),
        None => {}
    }

    serve().await
}

/// Invoke a method on a running instance and print what it says.
async fn call_control(method: &str) -> Result<()> {
    let conn = zbus::Connection::session()
        .await
        .context("failed to connect to the session bus")?;
    let reply: String = conn
        .call_method(Some(BUS_NAME), CONTROL_PATH, Some(CONTROL_IFACE), method, &())
        .await
        .with_context(|| format!("{method} failed; is niri-input-portal running?"))?
        .body()
        .deserialize()
        .context("unexpected reply")?;
    println!("{reply}");
    Ok(())
}

async fn serve() -> Result<()> {
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

    // The keymap is captured from niri by the Wayland thread and read back when
    // an EIS keyboard device is created.
    let keymap: eis_server::SharedKeymap = std::sync::Arc::default();
    let idle_timeout = wayland::idle_timeout_from_env();

    let (wayland, wayland_events) = wayland::spawn(keymap.clone(), idle_timeout)
        .context("failed to start the Wayland barrier thread")?;

    let (state, eis_closed) = portal::State::new(std::sync::Arc::new(wayland), keymap);
    let conn = zbus::connection::Builder::session()
        .context("failed to connect to the session bus")?
        .name(BUS_NAME)
        .context("failed to claim the backend bus name")?
        .serve_at(PORTAL_PATH, portal::InputCapture::new(state.clone()))
        .context("failed to export the InputCapture interface")?
        .serve_at(CONTROL_PATH, portal::Control::new(state.clone()))
        .context("failed to export the control interface")?
        .build()
        .await
        .context("failed to start the D-Bus service")?;

    // Signals are emitted against this connection, so the state cannot learn
    // about it until the service is up.
    state.set_connection(conn);
    tokio::spawn(portal::pump_wayland_events(state.clone(), wayland_events));
    tokio::spawn(portal::handle_eis_closed(state.clone(), eis_closed));

    if idle_timeout == std::time::Duration::MAX {
        info!("{BUS_NAME} ready at {PORTAL_PATH}; idle watchdog disabled");
    } else {
        info!(
            "{BUS_NAME} ready at {PORTAL_PATH}; capture auto-releases after {}s idle",
            idle_timeout.as_secs()
        );
    }

    tokio::signal::ctrl_c()
        .await
        .context("failed to wait for shutdown signal")?;
    info!("shutting down");

    // Never exit leaving the pointer locked.
    state.release_everything();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    Ok(())
}

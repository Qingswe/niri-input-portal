//! EIS (emulated input server) side of the portal.
//!
//! `ConnectToEIS` hands the client one end of a socketpair; this module owns the
//! other end. The client connects as a *receiver* context, so we are the ones
//! creating devices and pushing events at it.
//!
//! The connection lives in its own task. Everything the portal side needs to do
//! is expressed as an [`EisCommand`] so the D-Bus handlers never touch reis
//! objects directly.

use anyhow::{Context as _, Result};
use reis::{
    eis::{self, device::DeviceType},
    enumflags2::BitFlags,
    handshake::EisHandshaker,
    request::{Connection, DeviceCapability, EisRequest, EisRequestConverter},
    PendingRequestResult,
};
use std::{
    collections::HashSet,
    os::fd::{AsFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::{io::unix::AsyncFd, sync::mpsc};
use tracing::{debug, info, trace, warn};

/// Everything the capture side can ask the EIS connection to emit.
#[derive(Debug, Clone)]
pub enum EisCommand {
    /// Begin a capture run. `sequence` must match the `activation_id` sent in
    /// the portal's `Activated` signal — that pairing is the whole point of the
    /// id, it lets the client attribute an event stream to one activation.
    StartEmulating { sequence: u32 },
    StopEmulating,
    Motion { dx: f32, dy: f32 },
    Button { button: u32, press: bool },
    Scroll { dx: f32, dy: f32 },
    ScrollDiscrete { dx: i32, dy: i32 },
    ScrollStop,
    Key { keycode: u32, press: bool },
    /// Close the frame; every batch of events needs one to be delivered.
    Frame,
}

/// Logical area the emulated devices cover, in niri's coordinate space.
///
/// A libei client reads `ei_device.region` while it builds the device for
/// `EI_EVENT_DEVICE_ADDED` and never revisits it — there is no region event —
/// so a region arriving after `ei_device.done` is silently ignored. Without one
/// Deskflow keeps its 1x1 fallback: every activation collapses onto (0, 0) and
/// only the top edge stays reachable.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub width: u32,
    pub height: u32,
}

/// Send handle for an EIS connection.
#[derive(Debug, Clone)]
pub struct EisHandle {
    tx: mpsc::UnboundedSender<EisCommand>,
}

impl EisHandle {
    pub fn send(&self, cmd: EisCommand) {
        // A closed channel means the client dropped the EIS socket; the portal
        // session will be torn down through its own path, so drop the event.
        if self.tx.send(cmd).is_err() {
            debug!("EIS command dropped: connection is gone");
        }
    }

}

/// Devices we hand the client once it binds a seat.
#[derive(Default)]
struct Devices {
    keyboard: Option<reis::request::Device>,
    pointer: Option<reis::request::Device>,
    emulating: bool,
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u32>,
}

impl Devices {
    fn apply(&mut self, cmd: &EisCommand, started: Instant) {
        match cmd {
            EisCommand::StartEmulating { sequence } => {
                if self.emulating {
                    return;
                }
                for dev in [self.pointer.as_ref(), self.keyboard.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    dev.start_emulating(*sequence);
                }
                self.emulating = true;
                return;
            }
            EisCommand::StopEmulating => {
                if !self.emulating {
                    return;
                }
                // Release any pressed buttons or keys before stopping emulation to avoid stuck inputs.
                let mut released_any = false;
                if !self.pressed_buttons.is_empty() {
                    if let Some(b) = self
                        .pointer
                        .as_ref()
                        .and_then(reis::request::Device::interface::<eis::Button>)
                    {
                        for button in self.pressed_buttons.drain() {
                            b.button(button, eis::button::ButtonState::Released);
                            released_any = true;
                        }
                    } else {
                        self.pressed_buttons.clear();
                    }
                }
                if !self.pressed_keys.is_empty() {
                    if let Some(k) = self
                        .keyboard
                        .as_ref()
                        .and_then(reis::request::Device::interface::<eis::Keyboard>)
                    {
                        for keycode in self.pressed_keys.drain() {
                            k.key(keycode, eis::keyboard::KeyState::Released);
                            released_any = true;
                        }
                    } else {
                        self.pressed_keys.clear();
                    }
                }
                if released_any {
                    let time = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    for dev in [self.pointer.as_ref(), self.keyboard.as_ref()]
                        .into_iter()
                        .flatten()
                    {
                        dev.frame(time);
                    }
                }
                for dev in [self.pointer.as_ref(), self.keyboard.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    dev.stop_emulating();
                }
                self.emulating = false;
                return;
            }
            _ => {}
        }

        if !self.emulating {
            trace!("dropping command {cmd:?} while device is not emulating");
            return;
        }

        match cmd {
            EisCommand::StartEmulating { .. } | EisCommand::StopEmulating => unreachable!(),
            EisCommand::Motion { dx, dy } => {
                if let Some(p) = self
                    .pointer
                    .as_ref()
                    .and_then(reis::request::Device::interface::<eis::Pointer>)
                {
                    p.motion_relative(*dx, *dy);
                }
            }
            EisCommand::Button { button, press } => {
                if *press {
                    self.pressed_buttons.insert(*button);
                } else {
                    self.pressed_buttons.remove(button);
                }
                if let Some(b) = self
                    .pointer
                    .as_ref()
                    .and_then(reis::request::Device::interface::<eis::Button>)
                {
                    b.button(
                        *button,
                        if *press {
                            eis::button::ButtonState::Press
                        } else {
                            eis::button::ButtonState::Released
                        },
                    );
                }
            }
            EisCommand::Scroll { dx, dy } => {
                if let Some(s) = self
                    .pointer
                    .as_ref()
                    .and_then(reis::request::Device::interface::<eis::Scroll>)
                {
                    s.scroll(*dx, *dy);
                }
            }
            EisCommand::ScrollDiscrete { dx, dy } => {
                if let Some(s) = self
                    .pointer
                    .as_ref()
                    .and_then(reis::request::Device::interface::<eis::Scroll>)
                {
                    s.scroll_discrete(*dx, *dy);
                }
            }
            EisCommand::ScrollStop => {
                if let Some(s) = self
                    .pointer
                    .as_ref()
                    .and_then(reis::request::Device::interface::<eis::Scroll>)
                {
                    s.scroll_stop(1, 1, 0);
                }
            }
            EisCommand::Key { keycode, press } => {
                if *press {
                    self.pressed_keys.insert(*keycode);
                } else {
                    self.pressed_keys.remove(keycode);
                }
                if let Some(k) = self
                    .keyboard
                    .as_ref()
                    .and_then(reis::request::Device::interface::<eis::Keyboard>)
                {
                    k.key(
                        *keycode,
                        if *press {
                            eis::keyboard::KeyState::Press
                        } else {
                            eis::keyboard::KeyState::Released
                        },
                    );
                }
            }
            EisCommand::Frame => {
                let time = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                for dev in [self.pointer.as_ref(), self.keyboard.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    dev.frame(time);
                }
            }
        }
    }
}

/// The compositor's current xkb keymap, shared with the Wayland thread.
///
/// Devices are created when the client binds a seat, which can happen before or
/// after the keymap is known, so this is read at device-creation time rather
/// than passed in.
pub type SharedKeymap = Arc<RwLock<Option<String>>>;

/// Take over one end of a socketpair and serve EIS on it.
///
/// `closed_tx` receives the session label when the connection dies for any
/// reason. A capture must never outlive the connection it feeds — that is how
/// the pointer ends up locked with nothing able to release it.
///
/// `region` is the geometry the client will size this screen with. It has to be
/// known here rather than sent later: see [`Region`].
pub fn spawn(
    socket: UnixStream,
    label: String,
    keymap: SharedKeymap,
    closed_tx: mpsc::UnboundedSender<String>,
    region: Option<Region>,
) -> Result<EisHandle> {
    let context = eis::Context::new(socket).context("failed to create EIS context")?;
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        if let Err(err) = run(context, rx, &label, &keymap, region).await {
            warn!(session = %label, "EIS connection ended: {err:#}");
        } else {
            info!(session = %label, "EIS connection closed");
        }
        let _ = closed_tx.send(label);
    });

    Ok(EisHandle { tx })
}

/// Copy the keymap into a sealed memfd for handing to the client.
fn keymap_fd(keymap: &str) -> Option<(OwnedFd, u32)> {
    use std::io::Write;

    let fd = rustix::fs::memfd_create("niri-input-keymap", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| warn!("memfd_create failed: {e}"))
        .ok()?;
    let mut file = std::fs::File::from(fd);
    // xkb keymaps passed over a file descriptor are NUL-terminated.
    file.write_all(keymap.as_bytes()).ok()?;
    file.write_all(b"\0").ok()?;
    file.flush().ok()?;

    let size = u32::try_from(keymap.len() + 1).ok()?;
    Some((file.into(), size))
}

async fn run(
    context: eis::Context,
    mut rx: mpsc::UnboundedReceiver<EisCommand>,
    label: &str,
    keymap: &SharedKeymap,
    region: Option<Region>,
) -> Result<()> {
    let started = Instant::now();
    let async_fd = AsyncFd::with_interest(context.clone(), tokio::io::Interest::READABLE)
        .context("failed to register EIS fd with the reactor")?;

    let mut handshaker = EisHandshaker::new(&context, 1);
    let mut connected: Option<(Connection, EisRequestConverter)> = None;
    let mut devices = Devices::default();
    let mut sequence: u32 = 0;

    loop {
        tokio::select! {
            // Commands are only meaningful once devices exist; before that the
            // client has not bound a seat yet and there is nothing to emit on.
            Some(cmd) = rx.recv() => {
                if connected.is_some() {
                    devices.apply(&cmd, started);
                    let _ = context.flush();
                }
            }
            guard = async_fd.readable() => {
                let mut guard = guard.context("EIS fd poll failed")?;
                match context.read() {
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return Ok(());
                    }
                    Err(err) => return Err(err).context("EIS socket read failed"),
                }
                guard.clear_ready();

                match &mut connected {
                    None => {
                        if let Some((conn, converter)) =
                            drive_handshake(&mut handshaker, &context)?
                        {
                            info!(session = %label, "EIS handshake complete, client is a {:?} context", conn.context_type());
                            connected = Some((conn, converter));
                        }
                    }
                    Some((conn, converter)) => {
                        if !drive_requests(
                            &context,
                            conn,
                            converter,
                            &mut devices,
                            &mut sequence,
                            label,
                            keymap,
                            region,
                        )? {
                            return Ok(());
                        }
                    }
                }
                let _ = context.flush();
            }
        }
    }
}

/// Feed pending requests to the handshaker until it produces a response.
fn drive_handshake(
    handshaker: &mut EisHandshaker,
    context: &eis::Context,
) -> Result<Option<(Connection, EisRequestConverter)>> {
    while let Some(result) = context.pending_request() {
        // reis keeps its own `request_result` helper crate-private, so unwrap the
        // pending result here. During the handshake there is no object to blame
        // for an InvalidObject, so it is a protocol error like any other.
        let request = match result {
            PendingRequestResult::Request(r) => r,
            PendingRequestResult::ParseError(err) => {
                anyhow::bail!("EIS handshake parse error: {err}")
            }
            PendingRequestResult::InvalidObject(id) => {
                anyhow::bail!("EIS handshake referenced invalid object {id}")
            }
        };
        if let Some(resp) = handshaker.handle_request(request)? {
            let converter = EisRequestConverter::new(context, resp, 1);
            let conn = converter.handle().clone();

            if !conn.has_interface("ei_seat") || !conn.has_interface("ei_device") {
                conn.disconnected(
                    eis::connection::DisconnectReason::Protocol,
                    Some("need ei_seat and ei_device"),
                );
                let _ = context.flush();
                anyhow::bail!("client lacks ei_seat/ei_device support");
            }

            // The client drives capability negotiation from here via ei_seat.bind,
            // which hands the seat back to us, so this handle is not needed.
            let _seat = conn.add_seat(
                Some("niri-capture"),
                DeviceCapability::Pointer
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll,
            );
            let _ = context.flush();
            return Ok(Some((conn, converter)));
        }
    }
    let _ = context.flush();
    Ok(None)
}

/// Returns `false` once the client has disconnected.
fn drive_requests(
    context: &eis::Context,
    conn: &Connection,
    converter: &mut EisRequestConverter,
    devices: &mut Devices,
    sequence: &mut u32,
    label: &str,
    keymap: &SharedKeymap,
    region: Option<Region>,
) -> Result<bool> {
    while let Some(result) = context.pending_request() {
        let request = match result {
            PendingRequestResult::Request(r) => r,
            PendingRequestResult::ParseError(err) => {
                anyhow::bail!("EIS parse error: {err}");
            }
            PendingRequestResult::InvalidObject(id) => {
                debug!(session = %label, "EIS request for unknown object {id}");
                conn.connection().invalid_object(conn.last_serial(), id);
                continue;
            }
        };
        converter.handle_request(request)?;

        while let Some(req) = converter.next_request() {
            match req {
                EisRequest::Disconnect => return Ok(false),
                EisRequest::Bind(bind) => {
                    let caps = bind.capabilities;
                    debug!(session = %label, "client bound seat with {caps:?}");

                    if devices.pointer.is_none() && caps.contains(DeviceCapability::Pointer) {
                        devices.pointer = Some(add_device(
                            "niri-capture-pointer",
                            DeviceCapability::Pointer
                                | DeviceCapability::Button
                                | DeviceCapability::Scroll,
                            &bind.seat,
                            conn,
                            sequence,
                            None,
                            region,
                        ));
                    }
                    if devices.keyboard.is_none() && caps.contains(DeviceCapability::Keyboard) {
                        let map = keymap.read().ok().and_then(|k| k.clone());
                        devices.keyboard = Some(add_device(
                            "niri-capture-keyboard",
                            DeviceCapability::Keyboard.into(),
                            &bind.seat,
                            conn,
                            sequence,
                            map.as_deref(),
                            region,
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(true)
}

fn add_device(
    name: &str,
    capabilities: BitFlags<DeviceCapability>,
    seat: &reis::request::Seat,
    conn: &Connection,
    sequence: &mut u32,
    keymap: Option<&str>,
    region: Option<Region>,
) -> reis::request::Device {
    let device = seat.add_device(Some(name), DeviceType::Virtual, capabilities, |dev| {
        // Both of these have to go out before ei_device.done, which is exactly
        // what this callback is for.
        //
        // The region is read by the client while it builds the device and never
        // looked at again, so sending it later is the same as not sending it.
        if let Some(region) = region {
            // `dev` is the high-level wrapper; the `ei_device` object underneath
            // is the one that carries the region request.
            dev.device()
                .region(0, 0, region.width, region.height, 1.0);
            debug!("reported device region {}x{}", region.width, region.height);
        }

        // Without the keymap deskflow logs "does not have a keymap, we are
        // guessing" and assumes a US layout, mistranslating every non-US keycode
        // on the remote screen.
        // Only keyboard devices carry a keymap; pointers legitimately have none.
        let Some(kb) = dev.interface::<eis::Keyboard>() else {
            return;
        };
        let Some(keymap) = keymap else {
            warn!("no keymap captured from niri yet; the client will guess a layout");
            return;
        };
        if let Some((fd, size)) = keymap_fd(keymap) {
            kb.keymap(eis::keyboard::KeymapType::Xkb, size, fd.as_fd());
            debug!("sent a {size} byte xkb keymap to the client");
        }
    });
    device.resumed();
    // A receiver context expects the device to be emulating before events flow.
    // We do not start here — capture has not been activated yet — but record the
    // context type so `StartEmulating` knows it is meaningful.
    if conn.context_type() == eis::handshake::ContextType::Receiver {
        *sequence = sequence.wrapping_add(1);
    }
    device
}

/// How long to wait for a client to finish its handshake before giving up.
#[allow(dead_code)]
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

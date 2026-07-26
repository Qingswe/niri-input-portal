//! `org.freedesktop.impl.portal.InputCapture` backend.
//!
//! xdg-desktop-portal owns the public-facing portal API and the Request objects;
//! a backend only has to answer the impl-side calls and emit the three session
//! signals. That is why this implements the `impl.portal` interface rather than
//! standing up a second `org.freedesktop.portal.Desktop`.

use anyhow::Context as _;
use crate::{
    eis_server::{self, EisHandle},
    niri,
    wayland::{WaylandCmd, WaylandEvent, WaylandHandle},
};
use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, OnceLock,
    },
};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};
use zbus::{interface, object_server::SignalEmitter, zvariant};
use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Type, Value};

/// Capability bits, matching the portal spec.
pub const CAP_KEYBOARD: u32 = 1;
pub const CAP_POINTER: u32 = 2;
#[allow(dead_code)]
pub const CAP_TOUCHSCREEN: u32 = 4;

const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_OTHER: u32 = 2;

const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

type Results = HashMap<String, OwnedValue>;

/// One zone as the portal wire format wants it: width, height, x, y.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Type, Value, OwnedValue)]
pub struct ZoneTuple {
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

impl From<niri::Zone> for ZoneTuple {
    fn from(z: niri::Zone) -> Self {
        Self {
            width: z.width,
            height: z.height,
            x: z.x,
            y: z.y,
        }
    }
}

/// A barrier the client asked us to watch.
#[derive(Debug, Clone, Copy)]
pub struct Barrier {
    pub id: u32,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Barrier {
    pub fn is_vertical(&self) -> bool {
        self.x1 == self.x2
    }

    pub fn is_horizontal(&self) -> bool {
        self.y1 == self.y2
    }
}

#[derive(Debug)]
pub struct Session {
    pub handle: OwnedObjectPath,
    pub app_id: String,
    pub capabilities: u32,
    pub zone_set: u32,
    pub zones: Vec<niri::OutputZone>,
    pub barriers: Vec<Barrier>,
    pub eis: Option<EisHandle>,
    /// False between `CreateSession2` and `Start`. The deprecated `CreateSession`
    /// produces a session that is started from the outset, which is the whole
    /// difference between the two.
    pub started: bool,
    pub enabled: bool,
    /// Non-zero while a capture run is in flight.
    pub activation_id: u32,
    /// Set by `Clipboard.RequestClipboard`, which the spec requires before the
    /// session starts. `Start` reports it back as `clipboard_enabled`, and every
    /// other clipboard method refuses a session that never asked.
    pub clipboard_requested: bool,
}

/// What the compositor says is on the clipboard right now.
///
/// Mirrored here rather than asked for on demand so that `SelectionOwnerChanged`
/// can be emitted the moment it changes, which is the only way a client learns
/// there is something new to copy across.
#[derive(Debug, Default)]
pub struct ClipboardTracker {
    pub mime_types: Vec<String>,
    /// True while the selection belongs to a portal session rather than a local
    /// application — the difference between "the remote machine copied this" and
    /// "this machine copied this".
    pub owned_by_session: bool,
    /// Reads of a session-owned selection, waiting for the client to answer with
    /// `SelectionWrite`. The fd is the compositor's own pipe: it is handed
    /// straight to the client so the content never passes through this process.
    pub transfers: HashMap<u32, std::os::fd::OwnedFd>,
    /// Which session claimed the selection, so `SelectionTransfer` goes to the
    /// one that can actually answer it.
    pub owner_session: Option<OwnedObjectPath>,
    /// Insertion order, so the oldest transfer is the one evicted.
    order: std::collections::VecDeque<u32>,
    next_serial: u32,
}

/// How many unanswered reads to hold before dropping the oldest.
///
/// Every parked transfer is an open pipe. A client that is doing its job answers
/// within a round trip, so a backlog means nobody is going to: local applications
/// poll the clipboard constantly — a clipboard manager alone produces a steady
/// trickle — and without a cap those pipes accumulate until the process runs out
/// of file descriptors. Dropping the fd closes the pipe, which the waiting
/// application reads as "no data", the honest answer when the claim is unbacked.
const MAX_PENDING_TRANSFERS: usize = 16;

impl ClipboardTracker {
    /// Park a pending read and return the serial the client answers with.
    fn begin_transfer(&mut self, fd: std::os::fd::OwnedFd) -> u32 {
        // Serial 0 is reserved so a missing value cannot look like a valid one.
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        let serial = self.next_serial;
        self.transfers.insert(serial, fd);
        self.order.push_back(serial);

        while self.order.len() > MAX_PENDING_TRANSFERS {
            if let Some(stale) = self.order.pop_front() {
                // Dropping the fd is what signals EOF to the reader.
                if self.transfers.remove(&stale).is_some() {
                    debug!(serial = stale, "dropping an unanswered clipboard transfer");
                }
            }
        }
        serial
    }

    /// Hand a parked transfer to the client that asked for it.
    pub fn take_transfer(&mut self, serial: u32) -> Option<std::os::fd::OwnedFd> {
        self.order.retain(|s| *s != serial);
        self.transfers.remove(&serial)
    }

    /// Abandon every parked transfer, closing their pipes.
    pub fn clear_transfers(&mut self) {
        self.order.clear();
        self.transfers.clear();
    }
}

#[derive(Clone)]
pub struct State {
    sessions: Arc<Mutex<HashMap<OwnedObjectPath, Session>>>,
    clipboard: Arc<Mutex<ClipboardTracker>>,
    next_zone_set: Arc<AtomicU32>,
    next_activation: Arc<AtomicU32>,
    wayland: Arc<WaylandHandle>,
    keymap: eis_server::SharedKeymap,
    /// Signalled by each EIS task when its connection dies.
    eis_closed: mpsc::UnboundedSender<String>,
    /// Filled in once the bus connection exists, which is after the interface
    /// itself has to be constructed.
    conn: Arc<OnceLock<zbus::Connection>>,
}

impl State {
    pub fn new(
        wayland: Arc<WaylandHandle>,
        keymap: eis_server::SharedKeymap,
    ) -> (Self, mpsc::UnboundedReceiver<String>) {
        let (eis_closed, closed_rx) = mpsc::unbounded_channel();
        let state = Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            clipboard: Arc::new(Mutex::new(ClipboardTracker::default())),
            next_zone_set: Arc::new(AtomicU32::new(1)),
            next_activation: Arc::new(AtomicU32::new(1)),
            wayland,
            keymap,
            eis_closed,
            conn: Arc::new(OnceLock::new()),
        };
        (state, closed_rx)
    }

    pub fn set_connection(&self, conn: zbus::Connection) {
        let _ = self.conn.set(conn);
    }

    fn connection(&self) -> Option<&zbus::Connection> {
        self.conn.get()
    }

    /// Give up the clipboard claim and abandon whatever reads were waiting on it.
    ///
    /// The two go together: once the selection is no longer ours, no `SelectionWrite`
    /// can arrive for a parked transfer, so holding those pipes open would leave the
    /// applications blocked on them waiting for a write that will never come.
    pub async fn release_clipboard_claim(&self) {
        self.wayland.send(WaylandCmd::ClipboardRelease);
        let mut clipboard = self.clipboard.lock().await;
        clipboard.owned_by_session = false;
        clipboard.owner_session = None;
        clipboard.clear_transfers();
    }

    /// Sessions that may use the clipboard interface.
    async fn clipboard_sessions(&self) -> Vec<OwnedObjectPath> {
        let sessions = self.sessions.lock().await;
        sessions
            .values()
            .filter(|s| s.clipboard_requested)
            .map(|s| s.handle.clone())
            .collect()
    }

    async fn session_has_clipboard(&self, handle: &OwnedObjectPath) -> bool {
        let sessions = self.sessions.lock().await;
        sessions.get(handle).is_some_and(|s| s.clipboard_requested)
    }

    /// Whether this session is the one currently holding the selection.
    async fn owns_clipboard(&self, handle: &OwnedObjectPath) -> bool {
        self.clipboard.lock().await.owner_session.as_ref() == Some(handle)
    }

    /// Find the enabled session that owns `barrier_id`.
    async fn session_for_barrier(&self, barrier_id: u32) -> Option<OwnedObjectPath> {
        let sessions = self.sessions.lock().await;
        sessions
            .values()
            .find(|s| s.enabled && s.barriers.iter().any(|b| b.id == barrier_id))
            .map(|s| s.handle.clone())
    }

    async fn session_handles(&self) -> Vec<OwnedObjectPath> {
        self.sessions.lock().await.keys().cloned().collect()
    }

    /// Unlock the pointer and clear the screen edges. Used on shutdown so the
    /// session is never left with a grab owned by a process that is going away.
    pub fn release_everything(&self) {
        self.wayland
            .send(WaylandCmd::EndCapture { cursor_hint: None });
        self.wayland.send(WaylandCmd::Disarm);
    }
}

pub struct InputCapture {
    state: State,
}

impl InputCapture {
    pub fn new(state: State) -> Self {
        Self { state }
    }

    /// Register a session and export its impl-side Session object.
    ///
    /// Shared by both entry points; they differ only in whether the session
    /// arrives already started and with capabilities settled.
    async fn create(
        &self,
        session_handle: &OwnedObjectPath,
        app_id: &str,
        capabilities: u32,
        started: bool,
        object_server: &zbus::ObjectServer,
    ) -> Result<(), anyhow::Error> {
        let zones = niri::outputs()
            .await
            .context("cannot enumerate niri outputs")?;
        let zone_set = self.state.next_zone_set.fetch_add(1, Ordering::Relaxed);

        let session = Session {
            handle: session_handle.clone(),
            app_id: app_id.to_owned(),
            capabilities,
            zone_set,
            zones,
            barriers: Vec::new(),
            eis: None,
            started,
            enabled: false,
            clipboard_requested: false,
            activation_id: 0,
        };

        self.state
            .sessions
            .lock()
            .await
            .insert(session_handle.clone(), session);

        // The backend owns the impl-side Session object.
        let closable = SessionObject {
            state: self.state.clone(),
            handle: session_handle.clone(),
        };
        if let Err(err) = object_server.at(session_handle, closable).await {
            self.state.sessions.lock().await.remove(session_handle);
            return Err(anyhow::anyhow!("failed to export session object: {err}"));
        }

        info!(%app_id, session = %session_handle.as_str(), capabilities, started, "session created");
        Ok(())
    }
}

/// The capability bitmask a client asked for, or zero if it asked for nothing.
fn requested_capabilities(options: &HashMap<String, OwnedValue>) -> u32 {
    options
        .get("capabilities")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0)
}

#[interface(name = "org.freedesktop.impl.portal.InputCapture")]
impl InputCapture {
    /// Bitmask of what this backend can capture. Constant, per spec — not the
    /// currently-available set.
    #[zbus(property)]
    fn supported_capabilities(&self) -> u32 {
        CAP_KEYBOARD | CAP_POINTER
    }

    // Spelled lowercase in the spec, unlike every other member.
    //
    // Version 2 is what makes clipboard sharing reachable: xdg-desktop-portal
    // only offers the Clipboard interface to an InputCapture session from 1.21.1
    // onwards, and only through the v2 handshake. A 1.20 frontend ignores the
    // v2 members and keeps using CreateSession, so this stays backwards
    // compatible.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    /// The deprecated v1 entry point. The session it makes is already started.
    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
    ) -> (u32, Results) {
        let requested = requested_capabilities(&options);
        let granted = requested & (CAP_KEYBOARD | CAP_POINTER);
        if granted == 0 {
            warn!(%app_id, requested, "rejecting session: no supported capabilities");
            return (RESPONSE_OTHER, Results::new());
        }

        if let Err(err) = self
            .create(&session_handle, &app_id, granted, true, object_server)
            .await
        {
            error!(%app_id, "CreateSession failed: {err:#}");
            return (RESPONSE_OTHER, Results::new());
        }

        let mut results = Results::new();
        insert(&mut results, "session_id", Value::from(session_handle.as_str().to_owned()));
        insert(&mut results, "capabilities", Value::from(granted));
        (RESPONSE_SUCCESS, results)
    }

    /// The v2 entry point: creates the session without starting it.
    ///
    /// Capabilities are not negotiated here — that moves to `Start`, which is
    /// what gives `RequestClipboard` somewhere to sit in between.
    #[zbus(name = "CreateSession2")]
    async fn create_session2(
        &self,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<Results> {
        // No response code on this method, so a failure has to be a D-Bus error.
        self.create(&session_handle, &app_id, 0, false, object_server)
            .await
            .map_err(|err| zbus::fdo::Error::Failed(format!("{err:#}")))?;
        Ok(Results::new())
    }

    /// Negotiate capabilities and start a session made by `CreateSession2`.
    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let requested = requested_capabilities(&options);
        let granted = requested & (CAP_KEYBOARD | CAP_POINTER);
        if granted == 0 {
            warn!(%app_id, requested, "rejecting Start: no supported capabilities");
            return (RESPONSE_OTHER, Results::new());
        }

        let clipboard_enabled = {
            let mut sessions = self.state.sessions.lock().await;
            let Some(session) = sessions.get_mut(&session_handle) else {
                warn!(%app_id, session = %session_handle, "Start for an unknown session");
                return (RESPONSE_OTHER, Results::new());
            };
            if session.started {
                warn!(%app_id, session = %session_handle, "Start called twice");
                return (RESPONSE_OTHER, Results::new());
            }
            session.capabilities = granted;
            session.started = true;
            session.clipboard_requested
        };

        info!(
            %app_id,
            session = %session_handle.as_str(),
            capabilities = granted,
            clipboard_enabled,
            "session started"
        );

        let mut results = Results::new();
        insert(&mut results, "capabilities", Value::from(granted));
        // Persistence is deliberately not offered: `restore_data` is omitted, so
        // a client asking for persist_mode gets a fresh prompt-free session each
        // time rather than a token that would restore nothing.
        insert(&mut results, "clipboard_enabled", Value::from(clipboard_enabled));
        (RESPONSE_SUCCESS, results)
    }

    async fn get_zones(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        // Re-query rather than trusting the snapshot: outputs may have changed
        // since CreateSession, and the client calls this after ZonesChanged.
        let outputs = match niri::outputs().await {
            Ok(z) => z,
            Err(err) => {
                error!(%app_id, "cannot enumerate niri outputs: {err:#}");
                return (RESPONSE_OTHER, Results::new());
            }
        };

        let mut sessions = self.state.sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_handle) else {
            warn!(%app_id, session = %session_handle.as_str(), "GetZones for unknown session");
            return (RESPONSE_OTHER, Results::new());
        };

        // A changed layout invalidates existing barriers, so the zone_set id has
        // to move with it — that is what tells the client its barriers are stale.
        if session.zones.iter().map(|o| o.zone).ne(outputs.iter().map(|o| o.zone)) {
            session.zone_set = self.state.next_zone_set.fetch_add(1, Ordering::Relaxed);
            session.barriers.clear();
        }
        session.zones = outputs;

        let tuples: Vec<ZoneTuple> = session.zones.iter().map(|o| o.zone.into()).collect();
        let zone_set = session.zone_set;
        debug!(%app_id, zone_set, "returning {} zone(s)", tuples.len());

        let mut results = Results::new();
        insert(&mut results, "zones", Value::from(tuples));
        insert(&mut results, "zone_set", Value::from(zone_set));
        (RESPONSE_SUCCESS, results)
    }

    async fn set_pointer_barriers(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
        barriers: Vec<HashMap<String, OwnedValue>>,
        zone_set: u32,
    ) -> (u32, Results) {
        let mut sessions = self.state.sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_handle) else {
            warn!(%app_id, "SetPointerBarriers for unknown session");
            return (RESPONSE_OTHER, Results::new());
        };

        let mut accepted = Vec::new();
        let mut failed: Vec<u32> = Vec::new();

        // A stale zone_set means the client is describing a layout we no longer
        // have; reject every barrier so it re-reads zones first.
        let stale = zone_set != session.zone_set;

        for spec in &barriers {
            let id = spec.get("barrier_id").and_then(|v| u32::try_from(v).ok());
            let pos = spec
                .get("position")
                .and_then(|v| <(i32, i32, i32, i32)>::try_from(v.clone()).ok());

            let (Some(id), Some((x1, y1, x2, y2))) = (id, pos) else {
                if let Some(id) = id {
                    failed.push(id);
                }
                continue;
            };

            let barrier = Barrier { id, x1, y1, x2, y2 };

            // The spec allows only axis-aligned barriers.
            if stale || id == 0 || (!barrier.is_vertical() && !barrier.is_horizontal()) {
                failed.push(id);
                continue;
            }
            accepted.push(barrier);
        }

        if stale {
            warn!(%app_id, zone_set, current = session.zone_set, "rejecting barriers for stale zone_set");
        } else {
            session.barriers = accepted;
            info!(
                %app_id,
                "accepted {} barrier(s), rejected {}",
                session.barriers.len(),
                failed.len()
            );
        }

        let mut results = Results::new();
        insert(&mut results, "failed_barriers", Value::from(failed));
        (RESPONSE_SUCCESS, results)
    }

    // zbus would derive `ConnectToEis`; the spec capitalises the acronym.
    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        let mut sessions = self.state.sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_handle) else {
            return Err(zbus::fdo::Error::InvalidArgs("unknown session".into()));
        };

        if session.eis.is_some() {
            return Err(zbus::fdo::Error::Failed(
                "EIS connection already established for this session".into(),
            ));
        }

        let (ours, theirs) = UnixStream::pair()
            .map_err(|e| zbus::fdo::Error::Failed(format!("socketpair failed: {e}")))?;

        let handle = eis_server::spawn(
            ours,
            session_handle.as_str().to_owned(),
            self.state.keymap.clone(),
            self.state.eis_closed.clone(),
        )
        .map_err(|e| zbus::fdo::Error::Failed(format!("EIS setup failed: {e:#}")))?;
        session.eis = Some(handle);

        info!(%app_id, session = %session_handle.as_str(), "handed EIS socket to client");
        Ok(zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(theirs)))
    }

    async fn enable(
        &self,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let mut sessions = self.state.sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_handle) else {
            return (RESPONSE_OTHER, Results::new());
        };
        if !session.started {
            // A v2 session has no negotiated capabilities until Start, so arming
            // barriers now would capture on behalf of a session that was never
            // granted anything.
            warn!(%app_id, "Enable called before Start");
            return (RESPONSE_OTHER, Results::new());
        }
        if session.eis.is_none() {
            warn!(%app_id, "Enable called before ConnectToEIS");
            return (RESPONSE_OTHER, Results::new());
        }
        session.enabled = true;
        info!(%app_id, barriers = session.barriers.len(), "capture enabled");
        self.state
            .wayland
            .send(WaylandCmd::Arm(session.barriers.clone()));
        (RESPONSE_SUCCESS, Results::new())
    }

    async fn disable(
        &self,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let mut sessions = self.state.sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_handle) else {
            return (RESPONSE_OTHER, Results::new());
        };
        session.enabled = false;
        session.activation_id = 0;
        info!(%app_id, "capture disabled");
        // Free the screen edges; leaving the surfaces up would keep swallowing
        // clicks on the edge pixel column for no reason.
        self.state.wayland.send(WaylandCmd::Disarm);
        (RESPONSE_SUCCESS, Results::new())
    }

    async fn release(
        &self,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, Results) {
        let cursor = options
            .get("cursor_position")
            .and_then(|v| <(f64, f64)>::try_from(v.clone()).ok());

        let mut sessions = self.state.sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_handle) else {
            return (RESPONSE_OTHER, Results::new());
        };

        if let Some(eis) = &session.eis {
            eis.send(eis_server::EisCommand::StopEmulating);
        }
        session.activation_id = 0;
        info!(%app_id, ?cursor, "capture released");

        // Unlock the pointer first, then rebuild the barriers so the pointer is
        // back under local control before an edge can trigger again.
        self.state
            .wayland
            .send(WaylandCmd::EndCapture { cursor_hint: cursor });
        if session.enabled {
            self.state
                .wayland
                .send(WaylandCmd::Arm(session.barriers.clone()));
        }
        (RESPONSE_SUCCESS, Results::new())
    }

    #[zbus(signal)]
    pub async fn activated(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn deactivated(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn disabled(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn zones_changed(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;
}

/// `org.freedesktop.impl.portal.Clipboard`.
///
/// The clipboard portal creates no session of its own: it attaches to a session
/// another portal already made, and xdg-desktop-portal only offers that to
/// InputCapture sessions from 1.21.1 onwards. Against an older frontend
/// everything here simply goes uncalled.
pub struct Clipboard {
    state: State,
}

impl Clipboard {
    pub fn new(state: State) -> Self {
        Self { state }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Clipboard")]
impl Clipboard {
    // Spelled lowercase in the spec, unlike every other member.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    /// Grant a session access to the clipboard.
    ///
    /// The spec puts this before the session starts, which is what lets `Start`
    /// answer `clipboard_enabled` truthfully.
    async fn request_clipboard(
        &self,
        session_handle: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) {
        let mut sessions = self.state.sessions.lock().await;
        match sessions.get_mut(&session_handle) {
            Some(session) => {
                session.clipboard_requested = true;
                info!(session = %session_handle, "clipboard access granted");
            }
            None => warn!(session = %session_handle, "RequestClipboard for an unknown session"),
        }
    }

    /// The session now holds clipboard content in these types.
    ///
    /// Claiming the selection discards whatever was on it, exactly as a local
    /// copy would; there is no way to put the previous content back afterwards.
    async fn set_selection(
        &self,
        session_handle: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        if !self.state.session_has_clipboard(&session_handle).await {
            return Err(zbus::fdo::Error::AccessDenied(
                "this session has no clipboard access".into(),
            ));
        }

        let mime_types: Vec<String> = options
            .get("mime_types")
            .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
            .unwrap_or_default();

        if mime_types.is_empty() {
            self.state.release_clipboard_claim().await;
            return Ok(());
        }

        {
            let mut clipboard = self.state.clipboard.lock().await;
            clipboard.owner_session = Some(session_handle.clone());
            // Reads parked against the previous owner cannot be answered from
            // the new content, so let their readers see EOF now.
            clipboard.clear_transfers();
        }
        info!(session = %session_handle, ?mime_types, "session claimed the clipboard");
        self.state
            .wayland
            .send(WaylandCmd::ClipboardClaim { mime_types });
        Ok(())
    }

    /// Hand over the pipe a pending read is waiting on.
    ///
    /// This is the compositor's own fd, passed straight through, so the content
    /// goes from the client to whoever is pasting without a copy in between.
    async fn selection_write(
        &self,
        session_handle: OwnedObjectPath,
        serial: u32,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        if !self.state.session_has_clipboard(&session_handle).await {
            return Err(zbus::fdo::Error::AccessDenied(
                "this session has no clipboard access".into(),
            ));
        }
        let fd = self
            .state
            .clipboard
            .lock()
            .await
            .take_transfer(serial)
            .ok_or_else(|| {
                // Either the client is answering twice, or the read waited long
                // enough to be evicted.
                zbus::fdo::Error::InvalidArgs(format!("no clipboard transfer with serial {serial}"))
            })?;
        Ok(zvariant::OwnedFd::from(fd))
    }

    /// The client finished writing, successfully or not.
    ///
    /// Nothing is left to do either way: the fd left this process with
    /// `SelectionWrite`, and closing it is what ends the transfer.
    async fn selection_write_done(
        &self,
        session_handle: OwnedObjectPath,
        serial: u32,
        success: bool,
    ) {
        if !success {
            warn!(session = %session_handle, serial, "the client failed to write clipboard content");
        }
    }

    /// Read the current selection, as the read end of a pipe.
    async fn selection_read(
        &self,
        session_handle: OwnedObjectPath,
        mime_type: String,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        if !self.state.session_has_clipboard(&session_handle).await {
            return Err(zbus::fdo::Error::AccessDenied(
                "this session has no clipboard access".into(),
            ));
        }

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.state.wayland.send(WaylandCmd::ClipboardRead {
            mime_type: mime_type.clone(),
            reply: reply_tx,
        });
        let fd = reply_rx
            .await
            .ok()
            .flatten()
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("nothing offers {mime_type}")))?;
        Ok(zvariant::OwnedFd::from(fd))
    }

    #[zbus(signal)]
    pub async fn selection_owner_changed(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn selection_transfer(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        mime_type: &str,
        serial: u32,
    ) -> zbus::Result<()>;
}

/// Out-of-band control, deliberately not on the spec interface.
///
/// This exists because every in-band escape can fail: the keyboard is grabbed,
/// the pointer is locked, and the client that is supposed to call `Release` may
/// be the thing that is broken. A D-Bus call works over SSH from another
/// machine, which is the only channel guaranteed to still be reachable.
pub struct Control {
    state: State,
}

impl Control {
    pub fn new(state: State) -> Self {
        Self { state }
    }
}

#[interface(name = "io.github.niri_input_portal.Control")]
impl Control {
    /// Drop any active capture and give the pointer and keyboard back.
    async fn force_release(&self) -> String {
        // Logged unconditionally: when someone reaches for this, knowing the
        // call arrived at all is the first thing worth confirming.
        info!("ForceRelease requested");
        let released = force_release_all(&self.state).await;
        if released.is_empty() {
            "no capture was active; barriers left as they were".to_owned()
        } else {
            format!("released {} capture(s)", released.len())
        }
    }

    /// Drop captures *and* tear the barrier surfaces down, so no edge can
    /// trigger again until a client re-enables.
    async fn disarm(&self) -> String {
        let released = force_release_all(&self.state).await;
        self.state.wayland.send(WaylandCmd::Disarm);
        format!(
            "released {} capture(s) and disarmed all barriers",
            released.len()
        )
    }

    /// Human-readable summary, for checking state without a GUI.
    async fn status(&self) -> String {
        let sessions = self.state.sessions.lock().await;
        if sessions.is_empty() {
            return "no sessions".to_owned();
        }
        sessions
            .values()
            .map(|s| {
                format!(
                    "{}: enabled={} barriers={} capturing={}",
                    s.handle.as_str(),
                    s.enabled,
                    s.barriers.len(),
                    s.activation_id != 0
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What the compositor currently offers on the clipboard.
    ///
    /// This exists so the data-control layer can be checked on its own, without
    /// a portal client: `wl-copy hi` then `--clip-status` should list
    /// `text/plain`.
    async fn clipboard_status(&self) -> String {
        let clipboard = self.state.clipboard.lock().await;
        if clipboard.mime_types.is_empty() {
            return "clipboard is empty (or ext-data-control is unavailable)".to_owned();
        }
        format!(
            "owner={} pending_transfers={} types:\n  {}",
            if clipboard.owned_by_session { "session" } else { "local" },
            clipboard.transfers.len(),
            clipboard.mime_types.join("\n  ")
        )
    }

    /// Read the clipboard as text, for checking the read path end to end.
    async fn clipboard_read(&self, mime_type: String) -> String {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.state.wayland.send(WaylandCmd::ClipboardRead {
            mime_type: mime_type.clone(),
            reply: reply_tx,
        });

        let Ok(Some(fd)) = reply_rx.await else {
            return format!("nothing on the clipboard for {mime_type}");
        };
        // The owner writes into the pipe from its own event loop, so this has to
        // happen off the reactor thread.
        match tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut file = std::fs::File::from(fd);
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).map(|_| buf)
        })
        .await
        {
            Ok(Ok(buf)) => String::from_utf8_lossy(&buf).into_owned(),
            Ok(Err(err)) => format!("read failed: {err}"),
            Err(err) => format!("read task failed: {err}"),
        }
    }

    /// Claim the clipboard without a session behind it.
    ///
    /// Only useful for checking the ownership half of the data-control layer: a
    /// local paste afterwards shows up as a pending transfer that nothing will
    /// ever answer, which is exactly what an unbacked claim should look like.
    async fn clipboard_claim(&self, mime_types: Vec<String>) -> String {
        if mime_types.is_empty() {
            self.state.release_clipboard_claim().await;
            return "released the clipboard claim".to_owned();
        }
        let listed = mime_types.join(", ");
        self.state
            .wayland
            .send(WaylandCmd::ClipboardClaim { mime_types });
        format!("claimed the clipboard for {listed}")
    }
}

/// End every in-flight capture, returning the sessions that were active.
async fn force_release_all(state: &State) -> Vec<OwnedObjectPath> {
    let active: Vec<OwnedObjectPath> = {
        let mut sessions = state.sessions.lock().await;
        let handles: Vec<OwnedObjectPath> = sessions
            .values()
            .filter(|s| s.activation_id != 0)
            .map(|s| s.handle.clone())
            .collect();
        for handle in &handles {
            if let Some(session) = sessions.get_mut(handle) {
                session.activation_id = 0;
                if let Some(eis) = &session.eis {
                    eis.send(eis_server::EisCommand::StopEmulating);
                }
            }
        }
        handles
    };

    // Unlock first; the client being told afterwards is fine, the point is that
    // the local pointer comes back regardless of what the client does.
    state.wayland.send(WaylandCmd::EndCapture { cursor_hint: None });

    if let Some(conn) = state.connection() {
        for handle in &active {
            let Ok(emitter) = SignalEmitter::new(conn, PORTAL_PATH) else {
                continue;
            };
            let mut options: HashMap<String, Value<'_>> = HashMap::new();
            options.insert("activation_id".into(), Value::from(0u32));
            let _ = InputCapture::deactivated(&emitter, handle.as_ref(), options).await;
        }
    }

    if !active.is_empty() {
        warn!("force-released {} capture(s)", active.len());
    }
    active
}

/// React to an EIS connection dying: a capture feeding a dead socket can never
/// be ended by its client, so it has to be ended here.
pub async fn handle_eis_closed(state: State, mut closed: mpsc::UnboundedReceiver<String>) {
    while let Some(label) = closed.recv().await {
        let was_capturing = {
            let sessions = state.sessions.lock().await;
            sessions
                .values()
                .any(|s| s.handle.as_str() == label && s.activation_id != 0)
        };
        if was_capturing {
            warn!(session = %label, "EIS connection died mid-capture, releasing the pointer");
            force_release_all(&state).await;
        }
    }
}

/// The impl-side Session object xdg-desktop-portal closes when it is done.
pub struct SessionObject {
    state: State,
    handle: OwnedObjectPath,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    async fn close(
        &self,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        info!(session = %self.handle.as_str(), "session closed by portal");
        self.state.sessions.lock().await.remove(&self.handle);
        // A selection whose owner has gone is a clipboard that hangs every paste
        // on this machine, so the claim has to go with the session.
        if self.state.owns_clipboard(&self.handle).await {
            self.state.release_clipboard_claim().await;
        }
        let _ = Self::closed(&emitter).await;
        let _ = object_server
            .remove::<SessionObject, _>(&self.handle)
            .await;
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Helper for the ergonomics of building an `a{sv}`.
fn insert(map: &mut Results, key: &str, value: Value<'_>) {
    match OwnedValue::try_from(value) {
        Ok(v) => {
            map.insert(key.to_owned(), v);
        }
        Err(err) => error!("failed to encode `{key}` for D-Bus: {err}"),
    }
}

/// Emit `Activated` for a session and start the EIS event stream.
pub async fn activate(
    state: &State,
    conn: &zbus::Connection,
    session_handle: &OwnedObjectPath,
    barrier_id: u32,
    cursor: (f64, f64),
) -> zbus::Result<()> {
    let activation_id = state.next_activation.fetch_add(1, Ordering::Relaxed);

    {
        let mut sessions = state.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_handle) else {
            return Ok(());
        };
        if !session.enabled {
            return Ok(());
        }
        if session.activation_id != 0 {
            // Already capturing; a second crossing is noise.
            return Ok(());
        }
        session.activation_id = activation_id;
        if let Some(eis) = &session.eis {
            // The EIS sequence must equal the activation_id so the client can
            // line up the event stream with this signal.
            eis.send(eis_server::EisCommand::StartEmulating {
                sequence: activation_id,
            });
            // The Wayland thread already locked the pointer; this tells it where
            // to send what it captures.
            state.wayland.send(WaylandCmd::AttachEis(eis.clone()));
        }
    }

    let emitter = SignalEmitter::new(conn, PORTAL_PATH)?;
    let mut options: HashMap<String, Value<'_>> = HashMap::new();
    options.insert("activation_id".into(), Value::from(activation_id));
    options.insert("barrier_id".into(), Value::from(barrier_id));
    options.insert("cursor_position".into(), Value::from((cursor.0, cursor.1)));

    InputCapture::activated(&emitter, session_handle.as_ref(), options).await
}

/// Tell every clipboard-enabled session what is on the selection now.
async fn announce_selection(
    state: &State,
    conn: &zbus::Connection,
    mime_types: &[String],
    is_ours: bool,
) {
    for handle in state.clipboard_sessions().await {
        let Ok(emitter) = SignalEmitter::new(conn, PORTAL_PATH) else {
            continue;
        };
        let mut options: HashMap<String, Value<'_>> = HashMap::new();
        options.insert("mime_types".into(), Value::from(mime_types.to_vec()));
        options.insert("session_is_owner".into(), Value::from(is_ours));

        if let Err(err) =
            Clipboard::selection_owner_changed(&emitter, handle.as_ref(), options).await
        {
            error!("failed to emit SelectionOwnerChanged: {err}");
        }
    }
}

/// Drive portal signals from what the Wayland thread observes.
pub async fn pump_wayland_events(state: State, mut events: mpsc::UnboundedReceiver<WaylandEvent>) {
    while let Some(event) = events.recv().await {
        let Some(conn) = state.connection().cloned() else {
            warn!("dropping a Wayland event: the bus connection is not up yet");
            continue;
        };

        match event {
            WaylandEvent::Activated {
                barrier_id,
                position,
            } => {
                let Some(handle) = state.session_for_barrier(barrier_id).await else {
                    debug!(barrier_id, "no enabled session owns this barrier");
                    continue;
                };
                if let Err(err) = activate(&state, &conn, &handle, barrier_id, position).await {
                    error!(barrier_id, "failed to emit Activated: {err}");
                }
            }
            WaylandEvent::CaptureLost => {
                // The grab is already gone; tell whichever session owned it so
                // the client stops expecting events, and put the barriers back.
                let active: Vec<(OwnedObjectPath, Vec<Barrier>)> = {
                    let sessions = state.sessions.lock().await;
                    sessions
                        .values()
                        .filter(|s| s.activation_id != 0)
                        .map(|s| (s.handle.clone(), s.barriers.clone()))
                        .collect()
                };
                for (handle, barriers) in active {
                    if let Err(err) = deactivate(&state, &conn, &handle, (0.0, 0.0)).await {
                        error!("failed to emit Deactivated: {err}");
                    }
                    state.wayland.send(WaylandCmd::Arm(barriers));
                }
            }
            WaylandEvent::ClipboardSelection {
                mime_types,
                is_ours,
            } => {
                {
                    let mut clipboard = state.clipboard.lock().await;
                    clipboard.mime_types.clone_from(&mime_types);
                    clipboard.owned_by_session = is_ours;
                }
                // Every session with clipboard access needs to know, so it can
                // offer the new content to the machine on the other end.
                announce_selection(&state, &conn, &mime_types, is_ours).await;
            }
            WaylandEvent::ClipboardSend { mime_type, fd } => {
                let (serial, owner) = {
                    let mut clipboard = state.clipboard.lock().await;
                    (clipboard.begin_transfer(fd), clipboard.owner_session.clone())
                };
                let Some(owner) = owner else {
                    debug!(mime_type, "a paste arrived for a claim with no session behind it");
                    continue;
                };
                debug!(mime_type, serial, "asking the session for clipboard content");
                let Ok(emitter) = SignalEmitter::new(&conn, PORTAL_PATH) else {
                    continue;
                };
                if let Err(err) =
                    Clipboard::selection_transfer(&emitter, owner.as_ref(), &mime_type, serial).await
                {
                    error!("failed to emit SelectionTransfer: {err}");
                }
            }
            WaylandEvent::ClipboardCancelled => {
                let mime_types = {
                    let mut clipboard = state.clipboard.lock().await;
                    clipboard.owned_by_session = false;
                    clipboard.owner_session = None;
                    // Nobody is going to answer these now.
                    clipboard.clear_transfers();
                    clipboard.mime_types.clone()
                };
                announce_selection(&state, &conn, &mime_types, false).await;
            }
            WaylandEvent::OutputsChanged => {
                // Zones are stale for every session; the spec says clients must
                // call GetZones again as soon as they see this.
                for handle in state.session_handles().await {
                    let Ok(emitter) = SignalEmitter::new(&conn, PORTAL_PATH) else {
                        continue;
                    };
                    if let Err(err) =
                        InputCapture::zones_changed(&emitter, handle.as_ref(), HashMap::new()).await
                    {
                        error!("failed to emit ZonesChanged: {err}");
                    }
                }
            }
        }
    }
}

/// Emit `Deactivated` and stop the EIS event stream.
#[allow(dead_code)]
pub async fn deactivate(
    state: &State,
    conn: &zbus::Connection,
    session_handle: &OwnedObjectPath,
    cursor: (f64, f64),
) -> zbus::Result<()> {
    let activation_id = {
        let mut sessions = state.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_handle) else {
            return Ok(());
        };
        let id = session.activation_id;
        if id == 0 {
            return Ok(());
        }
        session.activation_id = 0;
        if let Some(eis) = &session.eis {
            eis.send(eis_server::EisCommand::StopEmulating);
        }
        id
    };

    let emitter = SignalEmitter::new(conn, PORTAL_PATH)?;
    let mut options: HashMap<String, Value<'_>> = HashMap::new();
    options.insert("activation_id".into(), Value::from(activation_id));
    options.insert("cursor_position".into(), Value::from((cursor.0, cursor.1)));

    InputCapture::deactivated(&emitter, session_handle.as_ref(), options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_pipe() -> std::os::fd::OwnedFd {
        // The read end: an fd whose only job here is to be held and dropped.
        rustix::pipe::pipe().expect("pipe").0
    }

    #[test]
    fn parked_transfers_are_capped() {
        let mut tracker = ClipboardTracker::default();
        for _ in 0..MAX_PENDING_TRANSFERS * 3 {
            tracker.begin_transfer(a_pipe());
        }
        // Local applications poll the clipboard indefinitely; without the cap
        // this is where the process runs out of file descriptors.
        assert_eq!(tracker.transfers.len(), MAX_PENDING_TRANSFERS);
        assert_eq!(tracker.order.len(), MAX_PENDING_TRANSFERS);
    }

    #[test]
    fn the_oldest_transfer_is_the_one_dropped() {
        let mut tracker = ClipboardTracker::default();
        let first = tracker.begin_transfer(a_pipe());
        for _ in 0..MAX_PENDING_TRANSFERS {
            tracker.begin_transfer(a_pipe());
        }
        assert!(
            tracker.take_transfer(first).is_none(),
            "the oldest transfer should have been evicted, not a newer one"
        );
    }

    #[test]
    fn taking_a_transfer_removes_it_once() {
        let mut tracker = ClipboardTracker::default();
        let serial = tracker.begin_transfer(a_pipe());
        assert!(tracker.take_transfer(serial).is_some());
        assert!(tracker.take_transfer(serial).is_none());
        assert!(tracker.order.is_empty(), "order must not keep a taken serial");
    }

    #[test]
    fn serials_never_reuse_zero() {
        // Zero is the "no such transfer" value, so a wrap must skip it.
        let mut tracker = ClipboardTracker { next_serial: u32::MAX, ..Default::default() };
        let serial = tracker.begin_transfer(a_pipe());
        assert_ne!(serial, 0);
    }
}

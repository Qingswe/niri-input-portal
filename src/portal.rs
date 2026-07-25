//! `org.freedesktop.impl.portal.InputCapture` backend.
//!
//! xdg-desktop-portal owns the public-facing portal API and the Request objects;
//! a backend only has to answer the impl-side calls and emit the three session
//! signals. That is why this implements the `impl.portal` interface rather than
//! standing up a second `org.freedesktop.portal.Desktop`.

use crate::{
    eis_server::{self, EisHandle},
    niri,
};
use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;
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
    pub enabled: bool,
    /// Non-zero while a capture run is in flight.
    pub activation_id: u32,
}

#[derive(Clone)]
pub struct State {
    sessions: Arc<Mutex<HashMap<OwnedObjectPath, Session>>>,
    next_zone_set: Arc<AtomicU32>,
    next_activation: Arc<AtomicU32>,
}

impl State {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_zone_set: Arc::new(AtomicU32::new(1)),
            next_activation: Arc::new(AtomicU32::new(1)),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

pub struct InputCapture {
    state: State,
}

impl InputCapture {
    pub fn new(state: State) -> Self {
        Self { state }
    }
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
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] object_server: &zbus::ObjectServer,
    ) -> (u32, Results) {
        let requested: u32 = options
            .get("capabilities")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);

        // Always a subset of what was asked for, and of what we implement.
        let granted = requested & (CAP_KEYBOARD | CAP_POINTER);
        if granted == 0 {
            warn!(%app_id, requested, "rejecting session: no supported capabilities");
            return (RESPONSE_OTHER, Results::new());
        }

        let zones = match niri::outputs().await {
            Ok(z) => z,
            Err(err) => {
                error!(%app_id, "cannot enumerate niri outputs: {err:#}");
                return (RESPONSE_OTHER, Results::new());
            }
        };

        let zone_set = self.state.next_zone_set.fetch_add(1, Ordering::Relaxed);
        let session_id = session_handle.as_str().to_owned();

        let session = Session {
            handle: session_handle.clone(),
            app_id: app_id.clone(),
            capabilities: granted,
            zone_set,
            zones,
            barriers: Vec::new(),
            eis: None,
            enabled: false,
            activation_id: 0,
        };

        {
            let mut sessions = self.state.sessions.lock().await;
            sessions.insert(session_handle.clone(), session);
        }

        // The backend owns the impl-side Session object.
        let closable = SessionObject {
            state: self.state.clone(),
            handle: session_handle.clone(),
        };
        if let Err(err) = object_server.at(&session_handle, closable).await {
            error!(%app_id, "failed to export session object: {err}");
            self.state.sessions.lock().await.remove(&session_handle);
            return (RESPONSE_OTHER, Results::new());
        }

        info!(%app_id, session = %session_handle.as_str(), capabilities = granted, "session created");

        let mut results = Results::new();
        insert(&mut results, "session_id", Value::from(session_id));
        insert(&mut results, "capabilities", Value::from(granted));
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

        let handle = eis_server::spawn(ours, session_handle.as_str().to_owned())
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
        if session.eis.is_none() {
            warn!(%app_id, "Enable called before ConnectToEIS");
            return (RESPONSE_OTHER, Results::new());
        }
        session.enabled = true;
        info!(%app_id, barriers = session.barriers.len(), "capture enabled");
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
#[allow(dead_code)]
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
        session.activation_id = activation_id;
        if let Some(eis) = &session.eis {
            // The EIS sequence must equal the activation_id so the client can
            // line up the event stream with this signal.
            eis.send(eis_server::EisCommand::StartEmulating {
                sequence: activation_id,
            });
        }
    }

    let emitter = SignalEmitter::new(conn, PORTAL_PATH)?;
    let mut options: HashMap<String, Value<'_>> = HashMap::new();
    options.insert("activation_id".into(), Value::from(activation_id));
    options.insert("barrier_id".into(), Value::from(barrier_id));
    options.insert("cursor_position".into(), Value::from((cursor.0, cursor.1)));

    InputCapture::activated(&emitter, session_handle.as_ref(), options).await
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

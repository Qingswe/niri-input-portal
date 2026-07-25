//! The Wayland client that owns the barrier surfaces and the capture grab.

use super::{place, Edge, PlacedBarrier, WaylandCmd, WaylandEvent};
use crate::eis_server::{EisCommand, EisHandle, SharedKeymap};
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, Keymap, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
        relative_pointer::{RelativeMotionEvent, RelativePointerHandler, RelativePointerState},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{
        wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
    },
    Connection, QueueHandle,
};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{zwp_locked_pointer_v1, zwp_pointer_constraints_v1},
    relative_pointer::zv1::client::zwp_relative_pointer_v1,
};

/// Wayland keycodes are evdev codes offset by 8; EIS wants the evdev value.
const EVDEV_OFFSET: u32 = 8;

struct BarrierSurface {
    id: u32,
    layer: LayerSurface,
    origin: (i32, i32),
    buffer: Option<wl_buffer::WlBuffer>,
    drawn: bool,
}

/// State held while input is being redirected to the client.
struct Capture {
    barrier_id: u32,
    lock: zwp_locked_pointer_v1::ZwpLockedPointerV1,
    layer: LayerSurface,
    origin: (i32, i32),
    /// Absent until the portal hands over the session's EIS connection.
    eis: Option<EisHandle>,
    /// Events observed before the EIS handle arrived.
    pending: Vec<EisCommand>,
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    relative_pointer_state: RelativePointerState,
    pointer_constraints: PointerConstraintsState,
    pool: SlotPool,
    pointer: Option<wl_pointer::WlPointer>,
    relative_pointer: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keymap: SharedKeymap,
    modifiers: Modifiers,
    last_enter_serial: u32,
    surfaces: Vec<BarrierSurface>,
    capture: Option<Capture>,
    event_tx: mpsc::UnboundedSender<WaylandEvent>,
    exit: bool,
}

impl AppState {
    /// Current output layout as `(name, logical position, logical size)`.
    fn layout(&mut self) -> Vec<(String, (i32, i32), (i32, i32))> {
        self.output_state
            .outputs()
            .filter_map(|o| {
                let info = self.output_state.info(&o)?;
                Some((
                    info.name.clone()?,
                    info.logical_position?,
                    info.logical_size?,
                ))
            })
            .collect()
    }

    fn wl_output_named(&mut self, name: &str) -> Option<wl_output::WlOutput> {
        self.output_state.outputs().find(|o| {
            self.output_state
                .info(o)
                .and_then(|i| i.name)
                .is_some_and(|n| n == name)
        })
    }

    fn disarm(&mut self) {
        if !self.surfaces.is_empty() {
            debug!("tearing down {} barrier surface(s)", self.surfaces.len());
        }
        // Dropping LayerSurface destroys the underlying wlr surface.
        self.surfaces.clear();
    }

    fn arm(&mut self, barriers: &[crate::portal::Barrier], qh: &QueueHandle<Self>) {
        if self.capture.is_some() {
            debug!("ignoring Arm while a capture is in progress");
            return;
        }
        self.disarm();

        let layout = self.layout();
        let (placed, rejected) = place(barriers, &layout);
        if !rejected.is_empty() {
            warn!("{} barrier(s) do not lie on an output edge: {rejected:?}", rejected.len());
        }

        for p in placed {
            match self.create_surface(&p, qh) {
                Some(s) => self.surfaces.push(s),
                None => warn!(barrier = p.id, "could not create a surface for barrier"),
            }
        }
        info!("armed {} barrier surface(s)", self.surfaces.len());
    }

    fn create_surface(
        &mut self,
        p: &PlacedBarrier,
        qh: &QueueHandle<Self>,
    ) -> Option<BarrierSurface> {
        let output = self.wl_output_named(&p.output)?;
        let surface = self.compositor.create_surface(qh);

        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("niri-input-barrier"),
            Some(&output),
        );

        // Anchoring to the edge plus one perpendicular side lets `set_margin`
        // express a barrier that covers only part of that edge.
        let anchor = match p.edge {
            Edge::Right => Anchor::RIGHT | Anchor::TOP,
            Edge::Left => Anchor::LEFT | Anchor::TOP,
            Edge::Top => Anchor::TOP | Anchor::LEFT,
            Edge::Bottom => Anchor::BOTTOM | Anchor::LEFT,
        };
        layer.set_anchor(anchor);
        layer.set_size(p.size.0, p.size.1);
        // Do not reserve screen space; this surface sits on top of windows.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_margin(p.margin.0, 0, 0, p.margin.1);
        layer.commit();

        Some(BarrierSurface {
            id: p.id,
            layer,
            origin: p.origin,
            buffer: None,
            drawn: false,
        })
    }

    /// Attach a fully transparent buffer so the surface maps and can take input.
    fn draw(&mut self, index: usize, width: u32, height: u32) {
        let (w, h) = (width.max(1) as i32, height.max(1) as i32);
        let stride = w * 4;

        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
        else {
            warn!("failed to allocate a barrier buffer");
            return;
        };
        // Zeroed ARGB is transparent, so the barrier is invisible.
        canvas.fill(0);

        let s = &mut self.surfaces[index];
        let surface = s.layer.wl_surface();
        buffer.attach_to(surface).ok();
        surface.damage_buffer(0, 0, w, h);
        surface.commit();
        s.buffer = Some(buffer.wl_buffer().clone());
        s.drawn = true;
    }

    /// Take over the pointer and keyboard the moment a barrier is crossed.
    ///
    /// This runs before the portal is told anything, so the round trip through
    /// D-Bus cannot leak pointer motion onto the local screen.
    fn begin_capture(&mut self, index: usize, position: (f64, f64), qh: &QueueHandle<Self>) {
        if self.capture.is_some() {
            return;
        }
        let Some(pointer) = self.pointer.clone() else {
            warn!("barrier crossed but there is no pointer to lock");
            return;
        };

        let s = &self.surfaces[index];
        let (barrier_id, origin, layer) = (s.id, s.origin, s.layer.clone());
        let surface = layer.wl_surface().clone();

        let lock = match self.pointer_constraints.lock_pointer(
            &surface,
            &pointer,
            None,
            zwp_pointer_constraints_v1::Lifetime::Persistent,
            qh,
        ) {
            Ok(lock) => lock,
            Err(err) => {
                warn!("failed to lock the pointer, not capturing: {err}");
                return;
            }
        };

        // Exclusive keyboard focus is what lets keystrokes reach the remote
        // screen instead of whatever window happens to be focused locally.
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();

        // A locked pointer still draws a cursor; hide it so the screen looks
        // like the pointer really left.
        pointer.set_cursor(self.last_enter_serial, None, 0, 0);

        info!(barrier = barrier_id, ?position, "barrier crossed, capturing input");
        self.capture = Some(Capture {
            barrier_id,
            lock,
            layer,
            origin,
            eis: None,
            pending: Vec::new(),
        });

        let _ = self.event_tx.send(WaylandEvent::Activated {
            barrier_id,
            position,
        });
    }

    fn attach_eis(&mut self, eis: EisHandle) {
        let Some(capture) = &mut self.capture else {
            debug!("EIS handle arrived with no capture in progress");
            return;
        };
        // Flush whatever happened between the crossing and this handover.
        if !capture.pending.is_empty() {
            debug!("flushing {} buffered event(s)", capture.pending.len());
            for cmd in capture.pending.drain(..) {
                eis.send(cmd);
            }
            eis.send(EisCommand::Frame);
        }
        capture.eis = Some(eis);
    }

    fn end_capture(&mut self, cursor_hint: Option<(f64, f64)>) {
        let Some(capture) = self.capture.take() else {
            return;
        };

        if let Some((x, y)) = cursor_hint {
            // The hint is surface-local, and the surface is one pixel wide, so
            // this can only nudge the cursor along the barrier rather than back
            // into the screen. Better than nothing; a wider release surface
            // would be needed to do this properly.
            let local = (
                x - f64::from(capture.origin.0),
                y - f64::from(capture.origin.1),
            );
            capture.lock.set_cursor_position_hint(local.0, local.1);
            capture.layer.wl_surface().commit();
        }

        capture.lock.destroy();
        capture.layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        capture.layer.commit();

        if let Some(pointer) = &self.pointer {
            // Hand the cursor image back to the compositor default.
            pointer.set_cursor(self.last_enter_serial, None, 0, 0);
        }
        info!(barrier = capture.barrier_id, "capture ended, pointer released");
    }

    /// Route one event to the client, buffering if EIS is not attached yet.
    fn forward(&mut self, cmd: EisCommand) {
        let Some(capture) = &mut self.capture else {
            return;
        };
        match &capture.eis {
            Some(eis) => eis.send(cmd),
            None => capture.pending.push(cmd),
        }
    }

    fn forward_frame(&mut self) {
        self.forward(EisCommand::Frame);
    }

    /// The local escape hatch out of an exclusive keyboard grab.
    ///
    /// If the client stops calling Release the pointer stays locked and the
    /// keyboard stays grabbed, which would otherwise need a kill from another
    /// machine to undo.
    fn is_panic_key(&self, event: &KeyEvent) -> bool {
        self.modifiers.ctrl && self.modifiers.alt && event.keysym == Keysym::Escape
    }
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        let _ = self.event_tx.send(WaylandEvent::OutputsChanged);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        let _ = self.event_tx.send(WaylandEvent::OutputsChanged);
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        // A capture anchored to a vanished output cannot be recovered, and
        // leaving the grab in place would strand the pointer.
        self.end_capture(None);
        self.disarm();
        let _ = self.event_tx.send(WaylandEvent::OutputsChanged);
    }
}

impl LayerShellHandler for AppState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.capture.as_ref().is_some_and(|c| &c.layer == layer) {
            self.end_capture(None);
        }
        self.surfaces.retain(|s| &s.layer != layer);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(index) = self.surfaces.iter().position(|s| &s.layer == layer) else {
            return;
        };
        let (w, h) = configure.new_size;
        if !self.surfaces[index].drawn {
            self.draw(index, w, h);
        }
    }
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer if self.pointer.is_none() => {
                match self.seat_state.get_pointer(qh, &seat) {
                    Ok(p) => {
                        self.relative_pointer =
                            self.relative_pointer_state.get_relative_pointer(&p, qh).ok();
                        if self.relative_pointer.is_none() {
                            warn!("niri did not offer zwp_relative_pointer_v1; motion cannot be captured");
                        }
                        self.pointer = Some(p);
                    }
                    Err(err) => warn!("failed to acquire the pointer: {err}"),
                }
            }
            Capability::Keyboard if self.keyboard.is_none() => {
                // Bound up front rather than on capture, because the keymap
                // arrives on this object and the EIS device needs it earlier.
                match self.seat_state.get_keyboard(qh, &seat, None) {
                    Ok(k) => self.keyboard = Some(k),
                    Err(err) => warn!("failed to acquire the keyboard: {err}"),
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer => {
                self.end_capture(None);
                if let Some(p) = self.pointer.take() {
                    p.release();
                }
                if let Some(r) = self.relative_pointer.take() {
                    r.destroy();
                }
            }
            Capability::Keyboard => {
                if let Some(k) = self.keyboard.take() {
                    k.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for AppState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match &event.kind {
                PointerEventKind::Enter { serial } => {
                    self.last_enter_serial = *serial;
                    let Some(index) = self
                        .surfaces
                        .iter()
                        .position(|s| s.layer.wl_surface() == &event.surface)
                    else {
                        continue;
                    };
                    let s = &self.surfaces[index];
                    let position = (
                        f64::from(s.origin.0) + event.position.0,
                        f64::from(s.origin.1) + event.position.1,
                    );
                    self.begin_capture(index, position, qh);
                }
                PointerEventKind::Press { button, .. } => {
                    if self.capture.is_some() {
                        self.forward(EisCommand::Button {
                            button: *button,
                            press: true,
                        });
                        self.forward_frame();
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if self.capture.is_some() {
                        self.forward(EisCommand::Button {
                            button: *button,
                            press: false,
                        });
                        self.forward_frame();
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    if self.capture.is_none() {
                        continue;
                    }
                    if horizontal.stop || vertical.stop {
                        self.forward(EisCommand::ScrollStop);
                    }
                    // value120 is the high-resolution wheel API; discrete is the
                    // legacy fallback for compositors that do not send it.
                    let (hd, vd) = if horizontal.value120 != 0 || vertical.value120 != 0 {
                        (horizontal.value120, vertical.value120)
                    } else {
                        (horizontal.discrete * 120, vertical.discrete * 120)
                    };
                    if hd != 0 || vd != 0 {
                        self.forward(EisCommand::ScrollDiscrete { dx: hd, dy: vd });
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    if horizontal.absolute != 0.0 || vertical.absolute != 0.0 {
                        self.forward(EisCommand::Scroll {
                            dx: horizontal.absolute as f32,
                            dy: vertical.absolute as f32,
                        });
                    }
                    self.forward_frame();
                }
                PointerEventKind::Motion { .. } | PointerEventKind::Leave { .. } => {}
            }
        }
    }
}

impl RelativePointerHandler for AppState {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        _: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        if self.capture.is_none() {
            return;
        }
        // Accelerated deltas, so pointer feel on the remote screen matches the
        // local pointer settings.
        #[allow(clippy::cast_possible_truncation)]
        self.forward(EisCommand::Motion {
            dx: event.delta.0 as f32,
            dy: event.delta.1 as f32,
        });
        self.forward_frame();
    }
}

impl PointerConstraintsHandler for AppState {
    fn confined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }

    fn unconfined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wayland_protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }

    fn locked(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
        debug!("pointer lock is active");
    }

    fn unlocked(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
        // The compositor can drop the lock on its own, e.g. when the surface
        // loses focus. Capture cannot continue without it.
        if self.capture.is_some() {
            warn!("compositor released the pointer lock, ending capture");
            self.end_capture(None);
            let _ = self.event_tx.send(WaylandEvent::CaptureLost);
        }
    }
}

impl KeyboardHandler for AppState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.capture.is_none() {
            return;
        }
        if self.is_panic_key(&event) {
            warn!("Ctrl+Alt+Escape pressed, force-releasing the capture");
            self.end_capture(None);
            let _ = self.event_tx.send(WaylandEvent::CaptureLost);
            return;
        }
        self.forward(EisCommand::Key {
            keycode: event.raw_code.saturating_sub(EVDEV_OFFSET),
            press: true,
        });
        self.forward_frame();
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
        // Deliberately not forwarded. This is sctk synthesising repeats locally,
        // and the remote machine already generates its own from the held key, so
        // passing these along would double every repeat.
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.capture.is_none() {
            return;
        }
        self.forward(EisCommand::Key {
            keycode: event.raw_code.saturating_sub(EVDEV_OFFSET),
            press: false,
        });
        self.forward_frame();
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        let text = keymap.as_string();
        debug!("captured a {} byte xkb keymap from niri", text.len());
        if let Ok(mut slot) = self.keymap.write() {
            *slot = Some(text);
        }
    }
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(AppState);
// sctk 0.21 replaced the per-protocol delegate macros with one blanket impl.
smithay_client_toolkit::delegate_dispatch2!(AppState);

pub fn run(
    cmd_rx: calloop::channel::Channel<WaylandCmd>,
    event_tx: mpsc::UnboundedSender<WaylandEvent>,
    keymap: SharedKeymap,
    ready: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("failed to connect to the Wayland display")?;
    let (globals, event_queue) =
        registry_queue_init(&conn).context("failed to initialise the Wayland registry")?;
    let qh: QueueHandle<AppState> = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).context("compositor global is missing")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("niri did not advertise zwlr_layer_shell_v1")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm global is missing")?;
    let pool = SlotPool::new(4096, &shm).context("failed to create an shm pool")?;
    let pointer_constraints = PointerConstraintsState::bind(&globals, &qh);

    let mut state = AppState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        relative_pointer_state: RelativePointerState::bind(&globals, &qh),
        pointer_constraints,
        pool,
        pointer: None,
        relative_pointer: None,
        keyboard: None,
        keymap,
        modifiers: Modifiers::default(),
        last_enter_serial: 0,
        surfaces: Vec::new(),
        capture: None,
        event_tx,
        exit: false,
    };

    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().context("failed to create the event loop")?;
    let handle = event_loop.handle();

    WaylandSource::new(conn.clone(), event_queue)
        .insert(handle.clone())
        .map_err(|e| anyhow::anyhow!("failed to register the Wayland source: {e}"))?;

    let cmd_qh = qh.clone();
    handle
        .insert_source(cmd_rx, move |event, (), state: &mut AppState| {
            let calloop::channel::Event::Msg(cmd) = event else {
                return;
            };
            match cmd {
                WaylandCmd::Arm(barriers) => state.arm(&barriers, &cmd_qh),
                WaylandCmd::Disarm => state.disarm(),
                WaylandCmd::AttachEis(eis) => state.attach_eis(eis),
                WaylandCmd::EndCapture { cursor_hint } => state.end_capture(cursor_hint),
                WaylandCmd::Shutdown => state.exit = true,
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to register the command channel: {e}"))?;

    // Let the registry settle so outputs and the keymap are known before use.
    event_loop
        .dispatch(std::time::Duration::from_millis(200), &mut state)
        .context("initial Wayland dispatch failed")?;

    let layout = state.layout();
    info!("Wayland barrier thread ready, {} output(s) visible", layout.len());
    let _ = ready.send(Ok(()));

    while !state.exit {
        event_loop
            .dispatch(std::time::Duration::from_millis(200), &mut state)
            .context("Wayland dispatch failed")?;
    }

    // Never leave the session with a locked pointer or a grabbed keyboard.
    state.end_capture(None);
    Ok(())
}

use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::protocols as wayland_protocols;

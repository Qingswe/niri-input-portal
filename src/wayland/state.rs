//! The Wayland client that owns the barrier surfaces and the capture grab.

use super::clipboard::{ClipboardHandler, ClipboardState};
use super::{place, Edge, PlacedBarrier, WaylandCmd, WaylandEvent};
use crate::eis_server::{EisCommand, EisHandle, SharedKeymap};
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
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

// `KeyEvent::raw_code` is already the evdev keycode: it is the raw `wl_keyboard.key`
// wire value, which the Wayland protocol defines as the Linux evdev code, and sctk
// itself adds 8 to it wherever it needs an xkb keycode. EIS also takes evdev codes,
// so the value is passed through untouched — subtracting 8 here shifted every key
// by eight positions, turning A into U.

struct BarrierSurface {
    id: u32,
    layer: LayerSurface,
    origin: (i32, i32),
    placed: PlacedBarrier,
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
    placed: PlacedBarrier,
    /// Where the pointer sat on the barrier when capture began, surface-local.
    /// This is the honest answer to "where should the cursor come back", and it
    /// is used whenever the client's own suggestion is unusable.
    entry_along: f64,
    /// Counted per kind so a capture that produced nothing is distinguishable
    /// from one that was never wired up.
    motions: u32,
    keys: u32,
    buttons: u32,
    scrolls: u32,
    /// Last time anything at all was captured, for the idle watchdog.
    last_input: std::time::Instant,
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
    /// Absent when the compositor does not advertise `ext-data-control`, which
    /// costs the clipboard but nothing else.
    clipboard: Option<ClipboardState>,
    /// How long a capture may see no input at all before it is force-released.
    idle_timeout: std::time::Duration,
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
        // The surface is BARRIER_DEPTH deep so the cursor can be put back inside
        // the screen on release, but only its outer pixel may take input —
        // otherwise it would swallow clicks well inside the display.
        self.set_input_strip(layer.wl_surface(), Some(p.input_strip()));
        layer.commit();

        Some(BarrierSurface {
            id: p.id,
            layer,
            origin: p.origin,
            placed: p.clone(),
            buffer: None,
            drawn: false,
        })
    }

    /// Restrict a surface's input region, or pass `None` for the whole surface.
    fn set_input_strip(
        &self,
        surface: &wl_surface::WlSurface,
        strip: Option<(i32, i32, i32, i32)>,
    ) {
        match strip {
            Some((x, y, w, h)) => match Region::new(&self.compositor) {
                Ok(region) => {
                    region.add(x, y, w, h);
                    surface.set_input_region(Some(region.wl_region()));
                    // Region is destroyed on drop; the compositor keeps its own
                    // copy of the region once it has been set.
                }
                Err(err) => warn!("could not create an input region: {err}"),
            },
            None => surface.set_input_region(None),
        }
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
        let placed = s.placed.clone();
        let surface = layer.wl_surface().clone();

        // Position along the edge that the pointer crossed at, in surface-local
        // coordinates — the axis depends on whether the edge is vertical.
        let entry_along = match placed.edge {
            Edge::Left | Edge::Right => position.1 - f64::from(origin.1),
            Edge::Top | Edge::Bottom => position.0 - f64::from(origin.0),
        };

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
        // Widen the input region to the whole surface for the duration of the
        // capture; the pointer is locked anyway and this keeps focus stable.
        self.set_input_strip(&surface, None);
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
            placed,
            entry_along,
            motions: 0,
            keys: 0,
            buttons: 0,
            scrolls: 0,
            last_input: std::time::Instant::now(),
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

        // Where along the edge to come back. The client's suggestion is only
        // used when it is actually usable: Synergy 3.7 sends a constant (1, 0)
        // on every Release, which would pin the cursor to the same corner for
        // ever. Falling back to the crossing point is what users expect anyway —
        // the cursor reappears where it left.
        let along = match cursor_hint.and_then(|h| self.usable_hint(&capture, h)) {
            Some(along) => along,
            None => capture.entry_along,
        };

        let (hx, hy) = capture.placed.release_hint(along);
        capture.lock.set_cursor_position_hint(hx, hy);
        // The hint only takes effect when the lock is destroyed, and only if it
        // has been committed first.
        capture.layer.wl_surface().commit();

        capture.lock.destroy();
        capture.layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Shrink the input region back to the outer strip so the rest of the
        // surface stops swallowing clicks.
        self.set_input_strip(
            capture.layer.wl_surface(),
            Some(capture.placed.input_strip()),
        );
        capture.layer.commit();

        if let Some(pointer) = &self.pointer {
            // Hand the cursor image back to the compositor default.
            pointer.set_cursor(self.last_enter_serial, None, 0, 0);
        }
        info!(
            barrier = capture.barrier_id,
            motions = capture.motions,
            keys = capture.keys,
            buttons = capture.buttons,
            scrolls = capture.scrolls,
            "capture ended, pointer released"
        );
    }

    /// Decide whether a client's `cursor_position` is worth honouring.
    ///
    /// Returns the position along the barrier's edge if the hint lands on this
    /// barrier's surface, `None` if it is nonsense and the entry point should be
    /// used instead.
    fn usable_hint(&self, capture: &Capture, hint: (f64, f64)) -> Option<f64> {
        let (ox, oy) = (
            f64::from(capture.origin.0),
            f64::from(capture.origin.1),
        );
        let (w, h) = (
            f64::from(capture.placed.size.0),
            f64::from(capture.placed.size.1),
        );
        let (lx, ly) = (hint.0 - ox, hint.1 - oy);

        // A hint that does not even fall within the barrier surface is telling
        // us nothing about where along this edge the pointer should land.
        if lx < 0.0 || ly < 0.0 || lx > w || ly > h {
            debug!(?hint, "ignoring an out-of-range cursor hint from the client");
            return None;
        }
        Some(match capture.placed.edge {
            Edge::Left | Edge::Right => ly,
            Edge::Top | Edge::Bottom => lx,
        })
    }

    /// Route one event to the client, buffering if EIS is not attached yet.
    fn forward(&mut self, cmd: EisCommand) {
        let Some(capture) = &mut self.capture else {
            return;
        };
        // Frames are bookkeeping and carry no user intent, so they must not feed
        // the idle watchdog — otherwise it could never fire.
        let is_input = match &cmd {
            EisCommand::Motion { .. } => {
                capture.motions += 1;
                true
            }
            EisCommand::Key { .. } => {
                capture.keys += 1;
                true
            }
            EisCommand::Button { .. } => {
                capture.buttons += 1;
                true
            }
            EisCommand::Scroll { .. } | EisCommand::ScrollDiscrete { .. } => {
                capture.scrolls += 1;
                true
            }
            _ => false,
        };
        if is_input {
            capture.last_input = std::time::Instant::now();
        }
        match &capture.eis {
            Some(eis) => eis.send(cmd),
            None => capture.pending.push(cmd),
        }
    }

    fn forward_frame(&mut self) {
        self.forward(EisCommand::Frame);
    }

    /// Best-effort keyboard escape.
    ///
    /// Do not rely on this: niri handles compositor bindings before clients see
    /// them, so a combination it claims never arrives here. It is a convenience
    /// for the cases it does work in, not a safety mechanism — the watchdog and
    /// the `ForceRelease` D-Bus method are the ones that always work.
    fn is_panic_key(&self, event: &KeyEvent) -> bool {
        self.modifiers.ctrl && self.modifiers.alt && event.keysym == Keysym::Escape
    }

    /// Force-release a capture that has gone quiet.
    ///
    /// A capture that is working generates a constant stream of input, because
    /// the user is driving the remote screen with this machine's own devices.
    /// Silence means the input is going nowhere and the pointer is stranded.
    fn check_watchdog(&mut self) {
        let Some(capture) = &self.capture else {
            return;
        };
        let idle = capture.last_input.elapsed();
        if idle < self.idle_timeout {
            return;
        }
        warn!(
            barrier = capture.barrier_id,
            idle_secs = idle.as_secs(),
            "no captured input for the idle timeout, force-releasing the pointer"
        );
        self.end_capture(None);
        let _ = self.event_tx.send(WaylandEvent::CaptureLost);
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
                // The data-control device hangs off a seat too, and the
                // clipboard is watched for the whole life of the process rather
                // than only during a capture: the portal has to be able to
                // answer what is on the clipboard at any moment.
                if let Some(clipboard) = &mut self.clipboard {
                    clipboard.watch_seat(&seat, qh);
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
            keycode: event.raw_code,
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
            keycode: event.raw_code,
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

impl ClipboardHandler for AppState {
    fn clipboard(&mut self) -> Option<&mut ClipboardState> {
        self.clipboard.as_mut()
    }

    fn selection_changed(&mut self, mime_types: Vec<String>, is_ours: bool) {
        debug!(?mime_types, is_ours, "clipboard selection changed");
        let _ = self
            .event_tx
            .send(WaylandEvent::ClipboardSelection { mime_types, is_ours });
    }

    fn selection_send(&mut self, mime_type: String, fd: std::os::fd::OwnedFd) {
        // The fd travels on to whoever holds the clipboard content — the portal
        // client — so that the bytes are never copied through this process.
        let _ = self
            .event_tx
            .send(WaylandEvent::ClipboardSend { mime_type, fd });
    }

    fn selection_cancelled(&mut self) {
        debug!("another client took the clipboard from us");
        let _ = self.event_tx.send(WaylandEvent::ClipboardCancelled);
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
    idle_timeout: std::time::Duration,
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
    let clipboard = match ClipboardState::bind(&globals, &qh) {
        Ok(c) => Some(c),
        Err(err) => {
            warn!("no ext_data_control_manager_v1 ({err}); clipboard sharing is unavailable");
            None
        }
    };

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
        clipboard,
        idle_timeout,
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
    let cmd_conn = conn.clone();
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
                WaylandCmd::ClipboardRead { mime_type, reply } => {
                    let fd = state.clipboard.as_ref().and_then(|c| c.read(&mime_type));
                    // The caller is about to block reading this pipe, so the
                    // `receive` request has to be on the wire before we answer —
                    // the loop's own flush comes too late.
                    let _ = cmd_conn.flush();
                    let _ = reply.send(fd);
                }
                WaylandCmd::ClipboardClaim { mime_types } => {
                    if let Some(clipboard) = &mut state.clipboard {
                        clipboard.claim(&mime_types, &cmd_qh);
                        let _ = cmd_conn.flush();
                    }
                }
                WaylandCmd::ClipboardRelease => {
                    if let Some(clipboard) = &mut state.clipboard {
                        clipboard.release();
                        let _ = cmd_conn.flush();
                    }
                }
                WaylandCmd::ClipboardMimeTypes { reply } => {
                    let types = state
                        .clipboard
                        .as_ref()
                        .map(ClipboardState::mime_types)
                        .unwrap_or_default();
                    let _ = reply.send(types);
                }
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
        // The dispatch timeout doubles as the watchdog tick.
        state.check_watchdog();
    }

    // Never leave the session with a locked pointer or a grabbed keyboard.
    state.end_capture(None);
    Ok(())
}

use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::protocols as wayland_protocols;

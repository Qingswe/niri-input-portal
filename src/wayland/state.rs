//! The Wayland client that owns the barrier surfaces.

use super::{place, Edge, PlacedBarrier, WaylandCmd, WaylandEvent};
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::calloop::EventLoop,
    reexports::calloop_wayland_source::WaylandSource,
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
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
    protocol::{wl_buffer, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

struct BarrierSurface {
    id: u32,
    layer: LayerSurface,
    origin: (i32, i32),
    buffer: Option<wl_buffer::WlBuffer>,
    drawn: bool,
}

struct AppState {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    pool: SlotPool,
    pointer: Option<wl_pointer::WlPointer>,
    surfaces: Vec<BarrierSurface>,
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
        // An empty input region would make the surface ignore the pointer, and
        // the default region already covers the whole surface, so leave it.
        buffer.attach_to(surface).ok();
        surface.damage_buffer(0, 0, w, h);
        surface.commit();
        s.buffer = Some(buffer.wl_buffer().clone());
        s.drawn = true;
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
        // Surfaces on a vanished output are already dead; drop them so a later
        // Arm rebuilds against the new layout.
        self.disarm();
        let _ = self.event_tx.send(WaylandEvent::OutputsChanged);
    }
}

impl LayerShellHandler for AppState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
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
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(p) => self.pointer = Some(p),
                Err(err) => warn!("failed to acquire the pointer: {err}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for AppState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            // Entering a barrier surface is the crossing. Motion within it is
            // not interesting yet — phase 3 locks the pointer on activation.
            if !matches!(event.kind, PointerEventKind::Enter { .. }) {
                continue;
            }
            let Some(s) = self
                .surfaces
                .iter()
                .find(|s| s.layer.wl_surface() == &event.surface)
            else {
                continue;
            };

            let position = (
                f64::from(s.origin.0) + event.position.0,
                f64::from(s.origin.1) + event.position.1,
            );
            info!(barrier = s.id, ?position, "barrier crossed");
            let _ = self.event_tx.send(WaylandEvent::Activated {
                barrier_id: s.id,
                position,
            });
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

    let mut state = AppState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        pool,
        pointer: None,
        surfaces: Vec::new(),
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
                WaylandCmd::Shutdown => state.exit = true,
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to register the command channel: {e}"))?;

    // Let the registry settle so outputs are known before the first Arm.
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

    Ok(())
}

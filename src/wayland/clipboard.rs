//! Clipboard access through `ext-data-control`.
//!
//! The portal's Clipboard interface has to read and write the selection at
//! moments when this process holds no keyboard focus — the entire point is to
//! hand the clipboard to a machine that is not this one. `wl_data_device` only
//! works for the focused client and so cannot do that.
//! `ext_data_control_manager_v1` exists precisely for clipboard managers and
//! grants access without focus; niri advertises it, alongside the older
//! `zwlr_data_control_manager_v1` which is deliberately not used here.
//!
//! Nothing in this module knows about D-Bus. It exposes the four operations the
//! portal needs — observe the selection, read it, claim it, answer reads of what
//! was claimed — and leaves the protocol mapping to `portal.rs`.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use smithay_client_toolkit::{
    dispatch2::Dispatch2,
    reexports::client::{
        event_created_child,
        globals::{BindError, GlobalList},
        protocol::wl_seat::WlSeat,
        Connection, Dispatch, Proxy, QueueHandle,
    },
};
use tracing::{debug, warn};

use super::wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

/// What the Wayland thread reports back about the clipboard.
pub trait ClipboardHandler: Sized {
    /// `None` when the compositor has no `ext-data-control`. The dispatch code
    /// below only runs for objects created through a live [`ClipboardState`], so
    /// it treats `None` as a protocol impossibility and simply returns.
    fn clipboard(&mut self) -> Option<&mut ClipboardState>;

    /// The clipboard selection changed. `is_ours` is true while the claim made
    /// by [`ClipboardState::claim`] is still standing.
    fn selection_changed(&mut self, mime_types: Vec<String>, is_ours: bool);

    /// Somebody is pasting from the selection this process claimed. The `fd` is
    /// where the content has to be written; whoever takes it owns closing it.
    fn selection_send(&mut self, mime_type: String, fd: OwnedFd);

    /// The claim was taken over by another client.
    fn selection_cancelled(&mut self);
}

/// User data for the objects that carry no per-object state.
///
/// sctk's own `GlobalData` would say the same thing, but the orphan rule wants a
/// type from this crate on at least one side of every `Dispatch2` impl.
#[derive(Debug, Clone, Copy)]
pub struct ClipboardGlobal;

/// Mime types accumulated from one offer's `offer` events.
///
/// They arrive as a burst of separate events before the `selection` event says
/// what the offer is for, so they are collected behind a handle that the offer
/// object itself carries.
#[derive(Default, Debug, Clone)]
pub struct OfferData {
    mime_types: Arc<Mutex<Vec<String>>>,
}

impl OfferData {
    fn push(&self, mime_type: String) {
        if let Ok(mut types) = self.mime_types.lock() {
            types.push(mime_type);
        }
    }

    fn snapshot(&self) -> Vec<String> {
        self.mime_types.lock().map(|t| t.clone()).unwrap_or_default()
    }
}

/// Per-device bookkeeping.
#[derive(Default, Debug)]
pub struct DeviceData {
    /// An offer is announced before the `selection` event says whether it is the
    /// clipboard, the primary selection, or neither.
    pending: Arc<Mutex<Option<ExtDataControlOfferV1>>>,
}

pub struct ClipboardState {
    manager: ExtDataControlManagerV1,
    device: Option<ExtDataControlDeviceV1>,
    /// The clipboard offer currently on the wire, whoever owns it.
    offer: Option<ExtDataControlOfferV1>,
    /// The source published while this process holds the selection.
    source: Option<ExtDataControlSourceV1>,
}

impl ClipboardState {
    pub fn bind<D>(globals: &GlobalList, qh: &QueueHandle<D>) -> Result<Self, BindError>
    where
        D: Dispatch<ExtDataControlManagerV1, ClipboardGlobal> + 'static,
    {
        let manager = globals.bind(qh, 1..=1, ClipboardGlobal)?;
        Ok(Self { manager, device: None, offer: None, source: None })
    }

    /// Start watching a seat's clipboard. Only the first seat is used: the
    /// portal has one clipboard to offer regardless of how many seats exist.
    pub fn watch_seat<D>(&mut self, seat: &WlSeat, qh: &QueueHandle<D>)
    where
        D: Dispatch<ExtDataControlDeviceV1, DeviceData> + 'static,
    {
        if self.device.is_some() {
            return;
        }
        self.device = Some(self.manager.get_data_device(seat, qh, DeviceData::default()));
        debug!("watching the clipboard through ext-data-control");
    }

    /// Mime types on the current clipboard selection.
    pub fn mime_types(&self) -> Vec<String> {
        self.offer
            .as_ref()
            .and_then(|o| o.data::<OfferData>())
            .map(OfferData::snapshot)
            .unwrap_or_default()
    }

    /// True while this process is the selection owner.
    pub fn owns_selection(&self) -> bool {
        self.source.is_some()
    }

    /// Ask the selection owner for the content, as the read end of a pipe.
    ///
    /// Returning the pipe rather than the bytes is what keeps this cheap: the fd
    /// goes straight out over D-Bus, and the owner writes into it directly with
    /// no copy through this process. `None` means nothing is on the clipboard,
    /// or it is not offered in that type.
    pub fn read(&self, mime_type: &str) -> Option<OwnedFd> {
        let offer = self.offer.as_ref()?;
        let available = self.mime_types();
        if !available.iter().any(|m| m == mime_type) {
            debug!(mime_type, ?available, "clipboard read asked for an unoffered type");
            return None;
        }

        let (read, write) = match rustix::pipe::pipe() {
            Ok(pair) => pair,
            Err(err) => {
                warn!("could not create a clipboard pipe: {err}");
                return None;
            }
        };
        offer.receive(mime_type.to_owned(), write.as_fd());
        // `write` is dropped here, leaving the compositor's duplicate as the only
        // writer — otherwise the reader would never see EOF.
        Some(read)
    }

    /// Claim the clipboard, advertising `mime_types`.
    ///
    /// Reads of the claimed selection come back as
    /// [`ClipboardHandler::selection_send`].
    pub fn claim<D>(&mut self, mime_types: &[String], qh: &QueueHandle<D>)
    where
        D: Dispatch<ExtDataControlSourceV1, ClipboardGlobal> + 'static,
    {
        let Some(device) = &self.device else {
            warn!("cannot claim the clipboard, no data-control device");
            return;
        };
        if mime_types.is_empty() {
            self.release();
            return;
        }

        let source = self.manager.create_data_source(qh, ClipboardGlobal);
        for mime in mime_types {
            source.offer(mime.clone());
        }
        device.set_selection(Some(&source));

        if let Some(old) = self.source.replace(source) {
            old.destroy();
        }
        debug!(?mime_types, "claimed the clipboard");
    }

    /// Give up a claim made by [`Self::claim`].
    pub fn release(&mut self) {
        let Some(source) = self.source.take() else {
            return;
        };
        if let Some(device) = &self.device {
            device.set_selection(None);
        }
        source.destroy();
        debug!("released the clipboard claim");
    }

    /// Replace the tracked offer, destroying whatever it displaced.
    fn set_offer(&mut self, offer: Option<ExtDataControlOfferV1>) {
        if let Some(old) = self.offer.take() {
            old.destroy();
        }
        self.offer = offer;
    }
}

impl Drop for ClipboardState {
    fn drop(&mut self) {
        self.release();
        if let Some(device) = self.device.take() {
            device.destroy();
        }
    }
}

impl<D> Dispatch2<ExtDataControlManagerV1, D> for ClipboardGlobal {
    fn event(
        &self,
        _: &mut D,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<D>,
    ) {
        // The manager has no events.
    }
}

impl<D> Dispatch2<ExtDataControlDeviceV1, D> for DeviceData
where
    D: Dispatch<ExtDataControlOfferV1, OfferData> + ClipboardHandler + 'static,
{
    event_created_child!(D, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferData::default())
    ]);

    fn event(
        &self,
        state: &mut D,
        _: &ExtDataControlDeviceV1,
        event: <ExtDataControlDeviceV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<D>,
    ) {
        use ext_data_control_device_v1::Event;

        match event {
            Event::DataOffer { id } => {
                // A compositor should never announce two offers without a
                // selection in between, but leaking objects if it does is worse
                // than being defensive.
                if let Ok(mut pending) = self.pending.lock() {
                    if let Some(stale) = pending.replace(id) {
                        stale.destroy();
                    }
                }
            }
            Event::Selection { id } => {
                let pending = self.pending.lock().ok().and_then(|mut p| p.take());
                // Whatever was announced but is not this selection belongs to
                // nobody now.
                match (&id, pending) {
                    (Some(sel), Some(p)) if *sel != p => p.destroy(),
                    (None, Some(p)) => p.destroy(),
                    _ => {}
                }

                let mime_types = id
                    .as_ref()
                    .and_then(|o| o.data::<OfferData>())
                    .map(OfferData::snapshot)
                    .unwrap_or_default();
                let Some(clipboard) = state.clipboard() else {
                    return;
                };
                let is_ours = clipboard.owns_selection();
                clipboard.set_offer(id);
                state.selection_changed(mime_types, is_ours);
            }
            Event::PrimarySelection { id } => {
                // Middle-click paste is a local affair; the portal only deals in
                // the clipboard selection.
                if let Some(offer) = id {
                    offer.destroy();
                }
            }
            Event::Finished => {
                warn!("the compositor finished our data-control device, clipboard is now blind");
                if let Some(clipboard) = state.clipboard() {
                    clipboard.device = None;
                }
            }
            _ => {}
        }
    }
}

impl<D> Dispatch2<ExtDataControlOfferV1, D> for OfferData {
    fn event(
        &self,
        _: &mut D,
        _: &ExtDataControlOfferV1,
        event: <ExtDataControlOfferV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<D>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            self.push(mime_type);
        }
    }
}

impl<D> Dispatch2<ExtDataControlSourceV1, D> for ClipboardGlobal
where
    D: ClipboardHandler + 'static,
{
    fn event(
        &self,
        state: &mut D,
        source: &ExtDataControlSourceV1,
        event: <ExtDataControlSourceV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<D>,
    ) {
        use ext_data_control_source_v1::Event;

        let Some(clipboard) = state.clipboard() else {
            return;
        };
        let is_current = clipboard.source.as_ref() == Some(source);

        match event {
            Event::Send { mime_type, fd } => {
                // A send for a source already dropped would write into a pipe
                // nobody promised to fill; dropping `fd` lets the reader see EOF.
                if !is_current {
                    debug!(mime_type, "ignoring a send for a stale clipboard source");
                    return;
                }
                state.selection_send(mime_type, fd);
            }
            Event::Cancelled => {
                if is_current {
                    clipboard.source = None;
                    state.selection_cancelled();
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

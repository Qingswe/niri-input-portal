//! Barrier surfaces on the compositor side.
//!
//! niri has no protocol for "tell me when the pointer hits this line", so a
//! barrier is approximated with a one-pixel `wlr-layer-shell` surface pinned to
//! the matching screen edge. The pointer entering that surface *is* the barrier
//! crossing.
//!
//! Wayland event queues are not `Send`, so all of this lives on its own thread
//! and talks to the portal through calloop channels.

mod state;

use crate::portal::Barrier;
use anyhow::{Context, Result};
use std::thread;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Which screen edge a barrier sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// A barrier resolved against the current output layout.
#[derive(Debug, Clone)]
pub struct PlacedBarrier {
    pub id: u32,
    pub edge: Edge,
    /// Name of the output this edge belongs to.
    pub output: String,
    /// Surface size in logical pixels.
    pub size: (u32, u32),
    /// Distance from the anchored corner, in logical pixels.
    pub margin: (i32, i32),
    /// Global logical position of the surface's top-left corner, used to turn
    /// surface-local pointer coordinates back into portal coordinates.
    pub origin: (i32, i32),
}

/// Commands sent to the Wayland thread.
#[derive(Debug)]
pub enum WaylandCmd {
    /// Put barrier surfaces on screen. Replaces any existing set.
    Arm(Vec<Barrier>),
    /// Tear every barrier surface down, freeing the screen edges.
    Disarm,
    Shutdown,
}

/// Events raised by the Wayland thread.
#[derive(Debug)]
pub enum WaylandEvent {
    /// The pointer crossed a barrier. Position is in global logical coordinates.
    Activated { barrier_id: u32, position: (f64, f64) },
    /// The output layout changed; zones and barriers are stale.
    OutputsChanged,
}

pub struct WaylandHandle {
    cmd_tx: calloop::channel::Sender<WaylandCmd>,
}

impl WaylandHandle {
    pub fn send(&self, cmd: WaylandCmd) {
        if let Err(err) = self.cmd_tx.send(cmd) {
            warn!("Wayland thread is gone, dropping command: {err}");
        }
    }
}

impl Drop for WaylandHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WaylandCmd::Shutdown);
    }
}

/// Start the Wayland thread.
///
/// Returns a handle for sending commands plus the stream of barrier events.
pub fn spawn() -> Result<(WaylandHandle, mpsc::UnboundedReceiver<WaylandEvent>)> {
    let (cmd_tx, cmd_rx) = calloop::channel::channel::<WaylandCmd>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<WaylandEvent>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

    thread::Builder::new()
        .name("wayland-barriers".into())
        .spawn(move || match state::run(cmd_rx, event_tx, &ready_tx) {
            Ok(()) => info!("Wayland barrier thread stopped"),
            Err(err) => {
                // If startup failed the error goes to `ready_rx`; a later failure
                // has nobody left to report to, so log it here.
                let _ = ready_tx.send(Err(anyhow::anyhow!("{err:#}")));
                warn!("Wayland barrier thread failed: {err:#}");
            }
        })
        .context("failed to start the Wayland thread")?;

    ready_rx
        .recv()
        .context("Wayland thread exited before signalling readiness")??;

    Ok((WaylandHandle { cmd_tx }, event_rx))
}

/// Resolve barriers against the output layout.
///
/// A barrier that does not lie exactly on an output edge cannot be represented
/// as a layer surface, so it is dropped and reported back to the caller.
pub fn place(
    barriers: &[Barrier],
    outputs: &[(String, (i32, i32), (i32, i32))],
) -> (Vec<PlacedBarrier>, Vec<u32>) {
    let mut placed = Vec::new();
    let mut rejected = Vec::new();

    for b in barriers {
        let Some(p) = place_one(b, outputs) else {
            rejected.push(b.id);
            continue;
        };
        placed.push(p);
    }

    (placed, rejected)
}

fn place_one(
    b: &Barrier,
    outputs: &[(String, (i32, i32), (i32, i32))],
) -> Option<PlacedBarrier> {
    for (name, (ox, oy), (ow, oh)) in outputs {
        let (ox, oy, ow, oh) = (*ox, *oy, *ow, *oh);

        if b.is_vertical() {
            let (lo, hi) = (b.y1.min(b.y2), b.y1.max(b.y2));
            // Clip the barrier to this output's vertical extent.
            let (top, bottom) = (lo.max(oy), hi.min(oy + oh));
            if top >= bottom {
                continue;
            }
            let edge = if b.x1 == ox + ow {
                Edge::Right
            } else if b.x1 == ox {
                Edge::Left
            } else {
                continue;
            };
            let height = u32::try_from(bottom - top).ok()?;
            let origin_x = if edge == Edge::Right { ox + ow - 1 } else { ox };
            return Some(PlacedBarrier {
                id: b.id,
                edge,
                output: name.clone(),
                size: (1, height),
                margin: (top - oy, 0),
                origin: (origin_x, top),
            });
        }

        if b.is_horizontal() {
            let (lo, hi) = (b.x1.min(b.x2), b.x1.max(b.x2));
            let (left, right) = (lo.max(ox), hi.min(ox + ow));
            if left >= right {
                continue;
            }
            let edge = if b.y1 == oy + oh {
                Edge::Bottom
            } else if b.y1 == oy {
                Edge::Top
            } else {
                continue;
            };
            let width = u32::try_from(right - left).ok()?;
            let origin_y = if edge == Edge::Bottom { oy + oh - 1 } else { oy };
            return Some(PlacedBarrier {
                id: b.id,
                edge,
                output: name.clone(),
                size: (width, 1),
                margin: (0, left - ox),
                origin: (left, origin_y),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs() -> Vec<(String, (i32, i32), (i32, i32))> {
        // The real layout on the development machine: a 4K monitor with the
        // laptop panel below it and offset to the right.
        vec![
            ("HDMI-A-1".into(), (0, 0), (3072, 1728)),
            ("eDP-2".into(), (298, 1728), (1706, 1066)),
        ]
    }

    #[test]
    fn places_a_full_height_right_edge_barrier() {
        let b = Barrier { id: 1, x1: 3072, y1: 0, x2: 3072, y2: 1728 };
        let (placed, rejected) = place(&[b], &outputs());
        assert!(rejected.is_empty());
        assert_eq!(placed.len(), 1);
        let p = &placed[0];
        assert_eq!(p.edge, Edge::Right);
        assert_eq!(p.output, "HDMI-A-1");
        assert_eq!(p.size, (1, 1728));
        assert_eq!(p.origin, (3071, 0));
    }

    #[test]
    fn places_a_left_edge_barrier_on_the_offset_output() {
        let b = Barrier { id: 2, x1: 298, y1: 1728, x2: 298, y2: 2794 };
        let (placed, _) = place(&[b], &outputs());
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].edge, Edge::Left);
        assert_eq!(placed[0].output, "eDP-2");
        assert_eq!(placed[0].origin, (298, 1728));
    }

    #[test]
    fn clips_a_barrier_that_overhangs_the_output() {
        // Synergy may describe an edge spanning the whole desktop height even
        // though the output is shorter.
        let b = Barrier { id: 3, x1: 3072, y1: -500, x2: 3072, y2: 9000 };
        let (placed, _) = place(&[b], &outputs());
        assert_eq!(placed[0].size, (1, 1728));
        assert_eq!(placed[0].margin, (0, 0));
    }

    #[test]
    fn rejects_a_barrier_that_is_not_on_any_edge() {
        let b = Barrier { id: 4, x1: 1500, y1: 0, x2: 1500, y2: 1728 };
        let (placed, rejected) = place(&[b], &outputs());
        assert!(placed.is_empty());
        assert_eq!(rejected, vec![4]);
    }
}

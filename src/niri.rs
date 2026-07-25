//! Zone discovery via `niri msg --json outputs`.
//!
//! The portal's zones are expressed in the same logical coordinate space niri
//! reports, so scale and position map across without conversion.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Logical {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub transform: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Output {
    pub name: String,
    pub logical: Option<Logical>,
}

/// One portal zone: width, height, x-offset, y-offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

impl Zone {
    /// Whether `(x, y)` falls inside this zone.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x) + f64::from(self.width)
            && y < f64::from(self.y) + f64::from(self.height)
    }
}

/// An output that is currently enabled, paired with its zone.
#[derive(Debug, Clone)]
pub struct OutputZone {
    pub name: String,
    pub zone: Zone,
    pub scale: f64,
    pub transform: String,
}

/// Query niri for the current output layout.
///
/// Disabled outputs report `logical: null` and are skipped, which is also how
/// hotplug removal surfaces.
pub async fn outputs() -> Result<Vec<OutputZone>> {
    let out = tokio::process::Command::new("niri")
        .args(["msg", "--json", "outputs"])
        .output()
        .await
        .context("failed to run `niri msg --json outputs`")?;

    if !out.status.success() {
        anyhow::bail!(
            "`niri msg --json outputs` exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let parsed: HashMap<String, Output> =
        serde_json::from_slice(&out.stdout).context("failed to parse niri output JSON")?;

    let mut zones: Vec<OutputZone> = parsed
        .into_values()
        .filter_map(|o| {
            let l = o.logical?;
            Some(OutputZone {
                name: o.name,
                zone: Zone {
                    width: l.width,
                    height: l.height,
                    x: l.x,
                    y: l.y,
                },
                scale: l.scale,
                transform: l.transform,
            })
        })
        .collect();

    // niri returns outputs in hash order; a stable order keeps zone indices
    // meaningful across calls so barrier ids stay attached to the same screen.
    zones.sort_by(|a, b| {
        (a.zone.x, a.zone.y, a.name.as_str()).cmp(&(b.zone.x, b.zone.y, b.name.as_str()))
    });

    Ok(zones)
}

// SPDX-License-Identifier: MPL-2.0

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};

#[derive(Debug, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
pub struct Config {
    /// Last-used duration in minutes. 0 means indefinite.
    pub duration_mins: u32,
    /// Whether Vigil is currently inhibiting.
    ///
    /// `cosmic-panel` spawns one applet process per output, so a multi-monitor
    /// setup runs several independent instances. Keeping this in config rather
    /// than in `AppModel` lets every instance observe the same state through
    /// the existing `watch_config` subscription.
    pub active: bool,
    /// Unix timestamp, in seconds, at which the inhibit expires.
    ///
    /// Zero when indefinite or inactive. Stored as an absolute deadline rather
    /// than a remaining count so instances stay in agreement without writing
    /// config once per second.
    pub expiry_ts: u64,
    /// Let the display sleep normally while keeping the system awake.
    ///
    /// `cosmic-idle` treats a ScreenSaver inhibit as "no screen off *and* no
    /// suspend", so it cannot express this. When set, Vigil takes a logind
    /// `sleep` block instead, which stops suspend while leaving the configured
    /// screen-off timeout alone. Intended for leaving a machine reachable over
    /// SSH or RDP without burning the display.
    pub allow_screen_off: bool,
    /// Keep the system awake when the laptop lid is closed.
    ///
    /// Adds `handle-lid-switch` to the logind inhibit. Independent of
    /// `allow_screen_off`, since wanting the lid to be safe to close does not
    /// imply wanting the screen to sleep.
    pub awake_with_lid_closed: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duration_mins: 30,
            active: false,
            expiry_ts: 0,
            allow_screen_off: false,
            awake_with_lid_closed: false,
        }
    }
}

// SPDX-License-Identifier: MPL-2.0

//! Making `cosmic-idle`'s suspend respect Vigil's logind `sleep` block.
//!
//! `cosmic-idle` suspends by running the `Suspend` system action, which
//! defaults to a plain `systemctl suspend`. On systemd before 257, logind
//! ignores `block` inhibitors held by the same user as the caller, and
//! `systemctl` only checks them itself when attached to a terminal, so a
//! user-held `sleep` block never stops an idle suspend. Pointing the action at
//! `systemctl suspend --check-inhibitors=yes` makes `systemctl` check every
//! block inhibitor, the caller's own included. The inhibitor stays an open fd,
//! so nothing is left holding the machine up once Vigil exits, however it
//! exits.
//!
//! `cosmic-idle` also tries each idle suspend only once: a refused attempt is
//! not retried until the user has been active and gone idle again. When Vigil
//! lets go after holding the block long enough for an attempt to have been
//! refused, [`restart_suspend_countdown`] makes `cosmic-idle` start a fresh
//! suspend countdown from that moment.

use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cosmic::cosmic_config::{self, ConfigGet, ConfigSet};
use ron::value::RawValue;
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SHORTCUTS_ID: &str = "com.system76.CosmicSettings.Shortcuts";
const SYSTEM_ACTIONS_KEY: &str = "system_actions";
const SUSPEND_ACTION: &str = "Suspend";

/// The `Suspend` command Vigil installs. Also the marker that tells Vigil's
/// own override apart from one the user wrote.
pub const SUSPEND_COMMAND: &str = "systemctl suspend --check-inhibitors=yes";

const IDLE_ID: &str = "com.system76.CosmicIdle";
const SUSPEND_TIME_KEYS: [&str; 2] = ["suspend_on_ac_time", "suspend_on_battery_time"];

/// How far the suspend timeouts are moved while restarting the countdown.
/// Also the worst case left behind if Vigil dies between the two writes.
const NUDGE_MS: u32 = 1000;

/// Time between the two writes, so `cosmic-idle` sees each one separately.
/// It only rebuilds a timer whose value differs from the one it has, so two
/// writes coalesced into one event would restart nothing.
const NUDGE_GAP: Duration = Duration::from_secs(2);

/// systemd 257 made `block` inhibitors apply to their own user too.
const NATIVE_SYSTEMD: u32 = 257;

/// Where the idle suspend stands with respect to Vigil's `sleep` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendHook {
    /// logind refuses by itself on this systemd; nothing to install.
    Native,
    /// Vigil's command is configured and `cosmic-idle` has loaded it.
    Active,
    /// Configured, but `cosmic-idle` started before it was, and reads its
    /// system actions only at startup.
    PendingLogin,
    /// No user override for `Suspend`; Vigil can install its own.
    Missing,
    /// The user has their own `Suspend` command, which Vigil leaves alone.
    Custom,
    /// The shortcuts config could not be read, so it is not touched.
    Unreadable,
}

impl SuspendHook {
    /// Whether an idle suspend will actually be refused while Vigil holds its
    /// `sleep` block.
    pub fn holds(self) -> bool {
        matches!(self, Self::Native | Self::Active)
    }
}

/// The user layer of `system_actions`, in file order.
///
/// Keys are kept as raw RON rather than parsed into cosmic-settings'
/// `System` enum, the same way cosmic-settings reads them itself, so an
/// action this build does not know about survives a rewrite untouched.
#[derive(Default)]
struct SystemActions(Vec<(Box<RawValue>, String)>);

impl SystemActions {
    fn position(&self, action: &str) -> Option<usize> {
        self.0
            .iter()
            .position(|(key, _)| key.get_ron().trim() == action)
    }

    fn get(&self, action: &str) -> Option<&str> {
        self.position(action).map(|i| self.0[i].1.as_str())
    }
}

impl Serialize for SystemActions {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, command) in &self.0 {
            map.serialize_entry(key, command)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SystemActions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ActionsVisitor;

        impl<'de> Visitor<'de> for ActionsVisitor {
            type Value = SystemActions;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map of system actions to commands")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
                let mut entries = Vec::new();
                while let Some((key, command)) = access.next_entry::<Box<RawValue>, String>()? {
                    entries.push((key.trim_boxed(), command));
                }
                Ok(SystemActions(entries))
            }
        }

        deserializer.deserialize_map(ActionsVisitor)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The user's `system_actions`: empty when they have none, `Err` when there
/// is a file that could not be read or parsed.
fn read_actions(config: &cosmic_config::Config) -> Result<SystemActions, cosmic_config::Error> {
    match config.get_local::<SystemActions>(SYSTEM_ACTIONS_KEY) {
        Ok(actions) => Ok(actions),
        Err(cosmic_config::Error::NotFound) => Ok(SystemActions::default()),
        Err(e) => Err(e),
    }
}

/// Major version of the running systemd, from the manager itself.
fn systemd_major() -> Option<u32> {
    let conn = zbus::blocking::Connection::system().ok()?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .ok()?;
    let version: String = proxy.get_property("Version").ok()?;
    version
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// When this user's `cosmic-idle` started, in Unix seconds.
fn cosmic_idle_started() -> Option<u64> {
    // USER_HZ, the unit of /proc/<pid>/stat's start time. It is part of the
    // userspace ABI and fixed at 100 on every architecture COSMIC ships on.
    const USER_HZ: u64 = 100;

    let boot_secs = BufReader::new(File::open("/proc/stat").ok()?)
        .lines()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix("btime ")?.trim().parse::<u64>().ok())?;

    let uid = fs::metadata("/proc/self").ok().map(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid()
    })?;

    fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        let comm = fs::read_to_string(path.join("comm")).ok()?;
        if comm.trim_end() != "cosmic-idle" {
            return None;
        }
        {
            use std::os::unix::fs::MetadataExt;
            if entry.metadata().ok()?.uid() != uid {
                return None;
            }
        }
        // Field 22 is the start time. The command name in field 2 can hold
        // spaces, so count from the closing parenthesis.
        let stat = fs::read_to_string(path.join("stat")).ok()?;
        let after_comm = &stat[stat.rfind(')')? + 1..];
        let ticks: u64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
        Some(boot_secs + ticks / USER_HZ)
    })
}

/// Work out where the idle suspend stands.
///
/// `installed_at` is when Vigil wrote its override (Unix seconds), zero if it
/// has not; it decides whether `cosmic-idle` has had the chance to load it.
pub fn status(installed_at: u64) -> SuspendHook {
    if systemd_major().is_some_and(|v| v >= NATIVE_SYSTEMD) {
        return SuspendHook::Native;
    }

    let Ok(config) = cosmic_config::Config::new(SHORTCUTS_ID, 1) else {
        return SuspendHook::Unreadable;
    };
    let Ok(actions) = read_actions(&config) else {
        return SuspendHook::Unreadable;
    };

    match actions.get(SUSPEND_ACTION) {
        None => SuspendHook::Missing,
        Some(command) if command != SUSPEND_COMMAND => SuspendHook::Custom,
        Some(_) => match cosmic_idle_started() {
            Some(started) if installed_at != 0 && started < installed_at => {
                SuspendHook::PendingLogin
            }
            _ => SuspendHook::Active,
        },
    }
}

/// Add Vigil's `Suspend` command to the user's system actions, keeping
/// everything else in them. Only acts when there is no user `Suspend` entry.
///
/// Returns when it was written, for [`status`] and [`remove`].
pub fn install() -> Result<u64, String> {
    let config = cosmic_config::Config::new(SHORTCUTS_ID, 1).map_err(|e| e.to_string())?;
    install_in(&config)
}

fn install_in(config: &cosmic_config::Config) -> Result<u64, String> {
    let mut actions = read_actions(config).map_err(|e| e.to_string())?;
    if actions.get(SUSPEND_ACTION).is_some() {
        return Err("a Suspend command is already configured".into());
    }

    let key = RawValue::from_boxed_ron(SUSPEND_ACTION.into()).map_err(|e| e.to_string())?;
    actions.0.push((key, SUSPEND_COMMAND.to_owned()));
    config
        .set(SYSTEM_ACTIONS_KEY, &actions)
        .map_err(|e| e.to_string())?;
    Ok(now_secs())
}

/// Take Vigil's `Suspend` command back out, if Vigil put it there and it has
/// not been changed since. Returns whether anything was removed.
pub fn remove(installed_at: u64) -> Result<bool, String> {
    if installed_at == 0 {
        return Ok(false);
    }
    let config = cosmic_config::Config::new(SHORTCUTS_ID, 1).map_err(|e| e.to_string())?;
    remove_in(&config)
}

fn remove_in(config: &cosmic_config::Config) -> Result<bool, String> {
    let mut actions = read_actions(config).map_err(|e| e.to_string())?;
    let Some(i) = actions.position(SUSPEND_ACTION) else {
        return Ok(false);
    };
    if actions.0[i].1 != SUSPEND_COMMAND {
        return Ok(false);
    }

    actions.0.remove(i);
    config
        .set(SYSTEM_ACTIONS_KEY, &actions)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Serialises countdown restarts across panel instances. Every instance
/// reaches an expiry at the same moment; overlapping restarts could read
/// each other's temporary value as the original and leave it behind. The
/// kernel drops the lock with the fd, so a killed instance cannot wedge it.
fn nudge_lock() -> Option<File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    let file = File::create(dir.join("cosmic-ext-applet-vigil-nudge.lock")).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Whether an idle suspend can have been refused while the block was held.
///
/// Turning Vigil on is itself activity, so the user cannot have been idle for
/// longer than `held`. Below the shortest suspend timeout no attempt was made,
/// and the countdown already running should stand: restarting it would only
/// postpone the suspend.
fn needs_restart(held: Duration, timeouts_ms: &[u32]) -> bool {
    timeouts_ms
        .iter()
        .min()
        .is_some_and(|&ms| held >= Duration::from_millis(u64::from(ms)))
}

/// Make `cosmic-idle` start its suspend countdown again from now, if the
/// block was `held` long enough for a suspend attempt to have been refused.
///
/// It rebuilds a suspend timer only when the timeout changes, so the timeouts
/// are moved by [`NUDGE_MS`] and put back. The screen-off timer is untouched,
/// so a blanked screen stays blank. Runs on its own thread; returns at once.
pub fn restart_suspend_countdown(held: Duration) {
    std::thread::spawn(move || {
        let Some(_lock) = nudge_lock() else {
            return;
        };
        let Ok(idle) = cosmic_config::Config::new(IDLE_ID, 1) else {
            return;
        };

        // A disabled suspend (`None`) has no countdown to restart.
        let timeouts: Vec<(&str, u32)> = SUSPEND_TIME_KEYS
            .iter()
            .filter_map(|&key| match idle.get::<Option<u32>>(key) {
                Ok(Some(ms)) => Some((key, ms)),
                _ => None,
            })
            .collect();
        let timeouts_ms: Vec<u32> = timeouts.iter().map(|&(_, ms)| ms).collect();
        if !needs_restart(held, &timeouts_ms) {
            return;
        }

        let write = |offset: u32| {
            let tx = idle.transaction();
            for &(key, ms) in &timeouts {
                if let Err(e) = tx.set(key, Some(ms.saturating_add(offset))) {
                    eprintln!("failed to set {IDLE_ID}/{key}: {e}");
                }
            }
            if let Err(e) = tx.commit() {
                eprintln!("failed to write {IDLE_ID}: {e}");
            }
        };

        write(NUDGE_MS);
        std::thread::sleep(NUDGE_GAP);
        write(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shortcuts config in a fresh directory of its own, never the user's,
    /// removed again when the test ends.
    struct Scratch {
        config: cosmic_config::Config,
        file: PathBuf,
        dir: PathBuf,
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn scratch_config(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("vigil-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let config = cosmic_config::Config::with_custom_path(SHORTCUTS_ID, 1, dir.clone()).unwrap();
        let file = dir.join(format!("cosmic/{SHORTCUTS_ID}/v1/{SYSTEM_ACTIONS_KEY}"));
        Scratch { config, file, dir }
    }

    #[test]
    fn install_keeps_other_actions_and_remove_takes_only_its_own() {
        let scratch = scratch_config("roundtrip");
        let (config, file) = (&scratch.config, &scratch.file);
        fs::write(
            file,
            "{\n    Terminal: \"ghostty\",\n    FutureAction: \"x\",\n}",
        )
        .unwrap();

        install_in(config).unwrap();
        let actions = read_actions(config).unwrap();
        assert_eq!(actions.get("Terminal"), Some("ghostty"));
        assert_eq!(actions.get("FutureAction"), Some("x"));
        assert_eq!(actions.get(SUSPEND_ACTION), Some(SUSPEND_COMMAND));
        assert!(
            install_in(config).is_err(),
            "never overwrites an existing Suspend"
        );

        assert!(remove_in(config).unwrap());
        let actions = read_actions(config).unwrap();
        assert_eq!(actions.get(SUSPEND_ACTION), None);
        assert_eq!(actions.get("Terminal"), Some("ghostty"));
        assert_eq!(actions.get("FutureAction"), Some("x"));
        assert!(!remove_in(config).unwrap(), "nothing left to remove");
    }

    #[test]
    fn install_creates_the_file_when_there_is_none() {
        let scratch = scratch_config("fresh");
        let config = &scratch.config;
        install_in(config).unwrap();
        assert_eq!(
            read_actions(config).unwrap().get(SUSPEND_ACTION),
            Some(SUSPEND_COMMAND)
        );
    }

    #[test]
    fn a_users_own_suspend_command_is_never_removed() {
        let scratch = scratch_config("custom");
        let (config, file) = (&scratch.config, &scratch.file);
        fs::write(file, "{ Suspend: \"my-suspend\" }").unwrap();
        assert!(install_in(config).is_err());
        assert!(!remove_in(config).unwrap());
        assert_eq!(
            read_actions(config).unwrap().get(SUSPEND_ACTION),
            Some("my-suspend")
        );
    }

    #[test]
    fn an_unparseable_file_is_left_alone() {
        let scratch = scratch_config("broken");
        let (config, file) = (&scratch.config, &scratch.file);
        fs::write(file, "{ Terminal: ").unwrap();
        assert!(install_in(config).is_err());
        assert_eq!(fs::read_to_string(file).unwrap(), "{ Terminal: ");
    }

    #[test]
    fn restarts_only_once_an_attempt_can_have_been_refused() {
        let thirty_min = 30 * 60 * 1000;
        let min = |m: u64| Duration::from_secs(m * 60);
        assert!(!needs_restart(min(15), &[thirty_min]));
        assert!(needs_restart(min(30), &[thirty_min]));
        assert!(needs_restart(min(120), &[thirty_min]));
        // The shorter timeout decides, whichever power source is in use.
        assert!(needs_restart(min(20), &[thirty_min, 15 * 60 * 1000]));
        // Suspend disabled on both: nothing to restart.
        assert!(!needs_restart(min(120), &[]));
    }

    #[test]
    fn unknown_actions_survive_a_round_trip() {
        let input = r#"{ Terminal: "ghostty", FutureAction: "x", Suspend: "systemctl suspend" }"#;
        let actions: SystemActions = ron::from_str(input).unwrap();
        assert_eq!(actions.0.len(), 3);
        assert_eq!(actions.get("FutureAction"), Some("x"));
        assert_eq!(actions.get(SUSPEND_ACTION), Some("systemctl suspend"));

        let output = ron::to_string(&actions).unwrap();
        let reparsed: SystemActions = ron::from_str(&output).unwrap();
        assert_eq!(reparsed.get("Terminal"), Some("ghostty"));
        assert_eq!(reparsed.get("FutureAction"), Some("x"));
    }

    #[test]
    fn an_added_suspend_key_is_a_bare_identifier() {
        let mut actions: SystemActions = ron::from_str(r#"{ Terminal: "ghostty" }"#).unwrap();
        let key = RawValue::from_boxed_ron(SUSPEND_ACTION.into()).unwrap();
        actions.0.push((key, SUSPEND_COMMAND.to_owned()));

        let output = ron::to_string(&actions).unwrap();
        assert!(
            output.contains(r#"Suspend:"systemctl suspend --check-inhibitors=yes""#),
            "{output}"
        );
    }
}

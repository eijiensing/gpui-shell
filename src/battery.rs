//! Watches the system battery through Linux's power-supply sysfs interface.
//!
//! Battery capacity changes slowly, so a low-frequency poll is enough and avoids
//! keeping an inotify watch (or depending on UPower) alive just for the island.
//! Machines without a system battery - desktops, or a laptop whose battery is
//! detached - are reported as [`Battery::Absent`] instead of an error.
//!
//! Peripheral batteries (wireless mice, keyboards, ...) also appear under
//! `/sys/class/power_supply` as `type=Battery`. They are filtered out via their
//! `scope`, which is `System` for the machine's own battery and `Device` for
//! accessories.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// How often the sysfs files are re-read. Capacity moves in whole percent, so
/// there is nothing to gain from polling quickly.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Where the kernel exposes power supplies.
const POWER_SUPPLY: &str = "/sys/class/power_supply";

/// Battery percentage at or below which the battery counts as low.
pub const LOW_PERCENT: u8 = 20;

/// The machine's battery state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Battery {
    /// No system battery was found, as on a desktop.
    Absent,
    /// A system battery is present.
    Present {
        /// Charge level in whole percent (`0..=100`).
        percent: u8,
        /// Whether the battery reports itself as actively charging.
        charging: bool,
    },
}

impl Battery {
    /// Charge level, or `None` when this machine has no battery.
    pub fn percent(self) -> Option<u8> {
        match self {
            Battery::Absent => None,
            Battery::Present { percent, .. } => Some(percent),
        }
    }

    /// Whether the battery is actively charging. Always false without a
    /// battery.
    ///
    /// The island only needs the charge level for its low-battery glyph, so
    /// this is currently exposed for other consumers.
    #[allow(dead_code)]
    pub fn charging(self) -> bool {
        matches!(self, Battery::Present { charging: true, .. })
    }

    /// Whether the charge is at or below [`LOW_PERCENT`].
    ///
    /// A desktop is never low.
    pub fn is_low(self) -> bool {
        self.percent().is_some_and(|percent| percent <= LOW_PERCENT)
    }
}

/// Subscribers that want the latest state, fanned out from the watcher thread.
type Subscribers = Arc<Mutex<Vec<async_channel::Sender<Battery>>>>;

/// The lazily-started watcher is shared by every subscriber (normally just the
/// server).
static SUBSCRIBERS: OnceLock<Subscribers> = OnceLock::new();

/// The state most recently sent to subscribers.
static LAST_STATE: OnceLock<Mutex<Option<Battery>>> = OnceLock::new();

/// Subscribes to battery changes.
///
/// The watcher is started on the first call and lives for the lifetime of the
/// process. A new subscriber immediately receives the last known state, so it
/// does not have to wait for the next poll.
pub fn subscribe() -> async_channel::Receiver<Battery> {
    let (sender, receiver) = async_channel::unbounded();

    let subscribers = SUBSCRIBERS.get_or_init(|| {
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let watcher_subscribers = subscribers.clone();
        thread::Builder::new()
            .name("battery".to_string())
            .spawn(move || watch(&watcher_subscribers))
            .ok();
        subscribers
    });

    subscribers.lock().unwrap().push(sender.clone());

    if let Some(state) = *LAST_STATE.get_or_init(|| Mutex::new(None)).lock().unwrap() {
        let _ = sender.try_send(state);
    }

    receiver
}

/// Reads the current battery state directly. Cheap enough to call on demand.
pub fn current() -> Battery {
    read_battery(Path::new(POWER_SUPPLY))
}

/// Polls the battery until the process exits.
fn watch(subscribers: &Subscribers) {
    let mut last_sent: Option<Battery> = None;

    loop {
        let state = current();
        if last_sent != Some(state) {
            last_sent = Some(state);
            *LAST_STATE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(state);
            publish(subscribers, state);
        }

        thread::sleep(POLL_INTERVAL);
    }
}

/// Fans `state` out to every live subscriber.
fn publish(subscribers: &Subscribers, state: Battery) {
    let mut subscribers = subscribers.lock().unwrap();
    // Windows can be re-created, so drop subscribers whose receiver is gone.
    subscribers.retain(|sender| !sender.is_closed());
    for sender in subscribers.iter() {
        // Only fails if the receiver is dropped between the prune and here.
        let _ = sender.try_send(state);
    }
}

/// Finds the first system battery under `root` and reads its state.
///
/// The directory is a parameter so tests can point at a temporary tree.
fn read_battery(root: &Path) -> Battery {
    let Ok(entries) = fs::read_dir(root) else {
        return Battery::Absent;
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    // Prefer `BAT0` over `BAT1` and keep the result stable between polls.
    paths.sort();

    for path in paths {
        if read_field(&path, "type").as_deref() != Some("Battery") {
            continue;
        }

        // `scope` was added later; absent means a system battery.
        if let Some(scope) = read_field(&path, "scope")
            && scope != "System"
        {
            continue;
        }

        // Some batteries stay listed while removed; `present` says whether the
        // hardware is actually there.
        if read_field(&path, "present").as_deref() == Some("0") {
            continue;
        }

        let Some(percent) =
            read_field(&path, "capacity").and_then(|value| value.parse::<u16>().ok())
        else {
            continue;
        };

        let charging = read_field(&path, "status").as_deref() == Some("Charging");

        return Battery::Present {
            percent: percent.min(100) as u8,
            charging,
        };
    }

    Battery::Absent
}

/// Reads a sysfs field, trimming the trailing newline.
fn read_field(directory: &Path, name: &str) -> Option<String> {
    fs::read_to_string(directory.join(name))
        .ok()
        .map(|value| value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    /// Temporary power-supply directory removed when the test finishes.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("gpui-shell-battery-{}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn battery(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(directory: &Path, name: &str, value: &str) {
        fs::write(directory.join(name), value).unwrap();
    }

    #[test]
    fn a_machine_without_power_supplies_has_no_battery() {
        let fixture = Fixture::new();
        assert_eq!(read_battery(&fixture.0), Battery::Absent);
    }

    #[test]
    fn reads_a_system_battery() {
        let fixture = Fixture::new();
        let battery = fixture.battery("BAT0");
        write(&battery, "type", "Battery\n");
        write(&battery, "scope", "System\n");
        write(&battery, "capacity", "73\n");
        write(&battery, "status", "Discharging\n");

        assert_eq!(
            read_battery(&fixture.0),
            Battery::Present {
                percent: 73,
                charging: false,
            }
        );
    }

    #[test]
    fn reports_charging() {
        let fixture = Fixture::new();
        let battery = fixture.battery("BAT0");
        write(&battery, "type", "Battery\n");
        write(&battery, "capacity", "41\n");
        write(&battery, "status", "Charging\n");

        let state = read_battery(&fixture.0);
        assert_eq!(state.percent(), Some(41));
        assert!(state.charging());
    }

    #[test]
    fn ignores_peripheral_batteries_on_desktops() {
        let fixture = Fixture::new();
        let mouse = fixture.battery("hidpp_battery_0");
        write(&mouse, "type", "Battery\n");
        write(&mouse, "scope", "Device\n");
        write(&mouse, "capacity", "88\n");
        write(&mouse, "status", "Discharging\n");

        assert_eq!(read_battery(&fixture.0), Battery::Absent);
    }

    #[test]
    fn ignores_a_removed_battery() {
        let fixture = Fixture::new();
        let battery = fixture.battery("BAT0");
        write(&battery, "type", "Battery\n");
        write(&battery, "present", "0\n");
        write(&battery, "capacity", "50\n");

        assert_eq!(read_battery(&fixture.0), Battery::Absent);
    }

    #[test]
    fn low_threshold_is_inclusive_and_ignores_desktops() {
        assert!(
            Battery::Present {
                percent: LOW_PERCENT,
                charging: false
            }
            .is_low()
        );
        assert!(
            !Battery::Present {
                percent: LOW_PERCENT + 1,
                charging: false
            }
            .is_low()
        );
        assert!(!Battery::Absent.is_low());
    }
}

//! Detects when the focused niri window is fullscreen.
//!
//! niri renders a fullscreen window above the top layer-shell layer once its
//! resize animation settles, which covers the bar. The IPC types do not expose
//! a fullscreen flag, so this module infers it from the event stream: a window
//! is fullscreen when its IPC `tile_size` covers its whole logical output. The
//! event stream reports the target tile size as soon as the window commits its
//! fullscreen state, so the bar can start moving out of the way before niri
//! covers it.
//!
//! Like [`crate::battery`], the watcher runs on its own thread and fans changes
//! out to subscribers. Connecting does not require niri: if `NIRI_SOCKET` is
//! unset or the compositor stops responding, the watcher keeps retrying and the
//! bar simply stays visible.

use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// How long to wait before reopening a dropped event stream.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Tolerance when comparing a fullscreen tile against the output size.
const SIZE_EPSILON: f64 = 0.5;

/// Requests are single-line JSON; `Outputs` is a unit variant.
const OUTPUTS_REQUEST: &[u8] = b"\"Outputs\"\n";
const EVENT_STREAM_REQUEST: &[u8] = b"\"EventStream\"\n";

type Subscribers = Arc<Mutex<Vec<async_channel::Sender<Option<String>>>>>;

/// The lazily-started watcher is shared by every subscriber (normally just the
/// server).
static SUBSCRIBERS: OnceLock<Subscribers> = OnceLock::new();

/// The state most recently sent to subscribers. The outer `None` means no
/// event has been published yet.
static LAST_STATE: OnceLock<Mutex<Option<Option<String>>>> = OnceLock::new();

/// Subscribes to the name of the output whose focused window is fullscreen.
///
/// `None` means no focused window is fullscreen. The watcher is started on the
/// first call and lives for the lifetime of the process. A new subscriber
/// immediately receives the last known state, so it does not have to wait for
/// the next event.
pub fn subscribe() -> async_channel::Receiver<Option<String>> {
    let (sender, receiver) = async_channel::unbounded();

    let subscribers = SUBSCRIBERS.get_or_init(|| {
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let watcher_subscribers = subscribers.clone();
        thread::Builder::new()
            .name("niri-fullscreen".to_string())
            .spawn(move || watch(&watcher_subscribers))
            .ok();
        subscribers
    });

    subscribers.lock().unwrap().push(sender.clone());

    if let Some(fullscreen) = LAST_STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone()
    {
        let _ = sender.try_send(fullscreen);
    }

    receiver
}

/// Keeps an event stream open, reconnecting after niri restarts or the socket
/// disconnects.
fn watch(subscribers: &Subscribers) {
    let mut reported = false;

    loop {
        match stream(subscribers) {
            Ok(()) => reported = false,
            Err(err) => {
                // A missing `NIRI_SOCKET` or a stopped niri would otherwise log
                // on every retry.
                if !reported {
                    eprintln!("gpui-shell: niri event stream: {err}");
                    reported = true;
                }
            }
        }

        thread::sleep(RECONNECT_DELAY);
    }
}

/// Opens one event stream and publishes fullscreen changes until it ends.
fn stream(subscribers: &Subscribers) -> io::Result<()> {
    let path = env::var_os("NIRI_SOCKET")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "NIRI_SOCKET is not set"))?;
    let stream = UnixStream::connect(path)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    // The event stream replaces the request/response protocol, so read the
    // output sizes first. The stream itself does not include them and we need
    // them to recognize a fullscreen tile.
    writer.write_all(OUTPUTS_REQUEST)?;
    writer.flush()?;
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "niri closed before replying to Outputs",
        ));
    }
    let outputs = parse_outputs(&line)?;

    writer.write_all(EVENT_STREAM_REQUEST)?;
    writer.flush()?;
    line.clear();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "niri closed before starting the event stream",
        ));
    }

    let mut tracker = Tracker {
        outputs,
        ..Tracker::default()
    };
    let mut last_published: Option<Option<String>> = None;

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            // Normal disconnection; the watcher reconnects.
            return Ok(());
        }

        // Ignore events we cannot parse or do not care about rather than
        // dropping the stream: niri adds variants over time.
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        tracker.apply(&event);

        let fullscreen = tracker.focused_fullscreen_output().map(str::to_owned);
        if last_published.as_ref() != Some(&fullscreen) {
            last_published = Some(fullscreen.clone());
            publish(subscribers, fullscreen);
        }
    }
}

/// Parses the `Outputs` reply into output name -> logical size.
fn parse_outputs(line: &str) -> io::Result<HashMap<String, OutputSize>> {
    let reply: Value = serde_json::from_str(line).map_err(invalid_data)?;
    let outputs = reply
        .get("Ok")
        .and_then(|ok| ok.get("Outputs"))
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_data("unexpected Outputs reply"))?;

    let mut sizes = HashMap::new();
    for (name, output) in outputs {
        let Some(logical) = output.get("logical").filter(|logical| !logical.is_null()) else {
            continue;
        };
        let width = logical.get("width").and_then(Value::as_f64);
        let height = logical.get("height").and_then(Value::as_f64);
        if let (Some(width), Some(height)) = (width, height) {
            sizes.insert(name.clone(), OutputSize { width, height });
        }
    }

    Ok(sizes)
}

fn invalid_data(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

/// Fans `fullscreen` out to every live subscriber.
fn publish(subscribers: &Subscribers, fullscreen: Option<String>) {
    *LAST_STATE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(fullscreen.clone());

    let mut subscribers = subscribers.lock().unwrap();
    // Windows can be re-created, so drop subscribers whose receiver is gone.
    subscribers.retain(|sender| !sender.is_closed());
    for sender in subscribers.iter() {
        // Only fails if the receiver is dropped between the prune and here.
        let _ = sender.try_send(fullscreen.clone());
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OutputSize {
    width: f64,
    height: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct WindowState {
    /// IPC `tile_size`, which includes borders and the fullscreen backdrop.
    tile_size: (f64, f64),
    /// Fullscreen windows are always in the scrolling layout; requiring this
    /// keeps a floating window that happens to fill the output from matching.
    in_scrolling_layout: bool,
}

#[derive(Clone, Debug, Default)]
struct WorkspaceState {
    output: Option<String>,
    focused: bool,
    active_window_id: Option<u64>,
}

/// The slice of niri's event-stream state needed to recognize a focused
/// fullscreen window.
#[derive(Default)]
struct Tracker {
    outputs: HashMap<String, OutputSize>,
    windows: HashMap<u64, WindowState>,
    workspaces: HashMap<u64, WorkspaceState>,
}

impl Tracker {
    /// Applies one event, leaving unknown events alone so newer niri versions
    /// stay compatible.
    fn apply(&mut self, event: &Value) {
        let Some((name, data)) = event.as_object().and_then(|event| event.iter().next()) else {
            return;
        };

        match name.as_str() {
            "WindowsChanged" => {
                self.windows.clear();
                if let Some(windows) = data.get("windows").and_then(Value::as_array) {
                    for window in windows {
                        self.insert_window(window);
                    }
                }
            }
            "WindowOpenedOrChanged" => {
                if let Some(window) = data.get("window") {
                    self.insert_window(window);
                }
            }
            "WindowClosed" => {
                if let Some(id) = data.get("id").and_then(Value::as_u64) {
                    self.windows.remove(&id);
                }
            }
            "WindowLayoutsChanged" => {
                let Some(changes) = data.get("changes").and_then(Value::as_array) else {
                    return;
                };
                for change in changes {
                    let Some(change) = change.as_array() else {
                        continue;
                    };
                    let id = change.first().and_then(Value::as_u64);
                    let layout = change.get(1);
                    if let (Some(id), Some(layout)) = (id, layout) {
                        apply_layout(self.windows.entry(id).or_default(), layout);
                    }
                }
            }
            "WorkspacesChanged" => {
                self.workspaces.clear();
                if let Some(workspaces) = data.get("workspaces").and_then(Value::as_array) {
                    for workspace in workspaces {
                        self.insert_workspace(workspace);
                    }
                }
            }
            "WorkspaceActiveWindowChanged" => {
                let Some(id) = data.get("workspace_id").and_then(Value::as_u64) else {
                    return;
                };
                let active_window_id = data.get("active_window_id").and_then(Value::as_u64);
                self.workspaces.entry(id).or_default().active_window_id = active_window_id;
            }
            "WorkspaceActivated" => {
                let Some(id) = data.get("id").and_then(Value::as_u64) else {
                    return;
                };
                let focused = data
                    .get("focused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                // Only one workspace is focused at a time. The event arrives
                // before the other workspaces report the change.
                if focused {
                    for workspace in self.workspaces.values_mut() {
                        workspace.focused = false;
                    }
                }
                self.workspaces.entry(id).or_default().focused = focused;
            }
            _ => {}
        }
    }

    fn insert_window(&mut self, window: &Value) {
        let Some(id) = window.get("id").and_then(Value::as_u64) else {
            return;
        };

        let mut state = WindowState::default();
        if let Some(layout) = window.get("layout") {
            apply_layout(&mut state, layout);
        }
        self.windows.insert(id, state);
    }

    fn insert_workspace(&mut self, workspace: &Value) {
        let Some(id) = workspace.get("id").and_then(Value::as_u64) else {
            return;
        };

        let state = WorkspaceState {
            output: workspace
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_owned),
            focused: workspace
                .get("is_focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            active_window_id: workspace.get("active_window_id").and_then(Value::as_u64),
        };
        self.workspaces.insert(id, state);
    }

    /// Returns the output whose active window is fullscreen, if any.
    ///
    /// Using the workspace's active window rather than `Window::is_focused`
    /// keeps the bar hidden while the overlay takes keyboard focus, which
    /// clears toplevel focus in niri.
    fn focused_fullscreen_output(&self) -> Option<&str> {
        self.workspaces
            .values()
            .filter(|workspace| workspace.focused)
            .find_map(|workspace| {
                let window_id = workspace.active_window_id?;
                let window = self.windows.get(&window_id)?;
                if !window.in_scrolling_layout {
                    return None;
                }
                let output_name = workspace.output.as_deref()?;
                let output = self.outputs.get(output_name)?;

                // A fullscreen tile is the output size or larger; ordinary
                // tiles are clamped to the working area, which is smaller.
                let covers_output = window.tile_size.0 + SIZE_EPSILON >= output.width
                    && window.tile_size.1 + SIZE_EPSILON >= output.height;

                covers_output.then_some(output_name)
            })
    }
}

fn apply_layout(state: &mut WindowState, layout: &Value) {
    if let Some(size) = layout.get("tile_size").and_then(Value::as_array) {
        let width = size.first().and_then(Value::as_f64);
        let height = size.get(1).and_then(Value::as_f64);
        if let (Some(width), Some(height)) = (width, height) {
            state.tile_size = (width, height);
        }
    }

    state.in_scrolling_layout = layout
        .get("pos_in_scrolling_layout")
        .is_some_and(|position| !position.is_null());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tracker_with_output() -> Tracker {
        let mut tracker = Tracker::default();
        tracker.outputs.insert(
            "eDP-1".to_string(),
            OutputSize {
                width: 1920.0,
                height: 1080.0,
            },
        );
        tracker
    }

    fn workspace(id: u64, active_window_id: Option<u64>) -> Value {
        json!({
            "id": id,
            "output": "eDP-1",
            "is_focused": true,
            "is_active": true,
            "active_window_id": active_window_id,
        })
    }

    fn window(id: u64, tile: (f64, f64), scrolling: bool) -> Value {
        json!({
            "id": id,
            "layout": {
                "tile_size": [tile.0, tile.1],
                "window_size": [tile.0 as i64, tile.1 as i64],
                "pos_in_scrolling_layout": scrolling.then_some([1, 1]),
                "tile_pos_in_workspace_view": [0.0, 0.0],
                "window_offset_in_tile": [0.0, 0.0],
            },
        })
    }

    #[test]
    fn detects_a_fullscreen_scrolling_window() {
        let mut tracker = tracker_with_output();
        tracker.apply(&json!({ "WorkspacesChanged": { "workspaces": [workspace(1, Some(10))] } }));
        tracker.apply(
            &json!({ "WindowsChanged": { "windows": [window(10, (1920.0, 1080.0), true)] } }),
        );

        assert_eq!(tracker.focused_fullscreen_output(), Some("eDP-1"));
    }

    #[test]
    fn a_normal_tiled_window_does_not_match() {
        let mut tracker = tracker_with_output();
        tracker.apply(&json!({ "WorkspacesChanged": { "workspaces": [workspace(1, Some(10))] } }));
        tracker.apply(
            &json!({ "WindowsChanged": { "windows": [window(10, (1900.0, 1040.0), true)] } }),
        );

        assert_eq!(tracker.focused_fullscreen_output(), None);
    }

    #[test]
    fn a_floating_window_the_size_of_the_output_does_not_match() {
        let mut tracker = tracker_with_output();
        tracker.apply(&json!({ "WorkspacesChanged": { "workspaces": [workspace(1, Some(10))] } }));
        tracker.apply(
            &json!({ "WindowsChanged": { "windows": [window(10, (1920.0, 1080.0), false)] } }),
        );

        assert_eq!(tracker.focused_fullscreen_output(), None);
    }

    #[test]
    fn layout_updates_switch_fullscreen_on_and_off() {
        let mut tracker = tracker_with_output();
        tracker.apply(&json!({ "WorkspacesChanged": { "workspaces": [workspace(1, Some(10))] } }));
        tracker.apply(
            &json!({ "WindowsChanged": { "windows": [window(10, (1000.0, 800.0), true)] } }),
        );
        assert_eq!(tracker.focused_fullscreen_output(), None);

        tracker.apply(&json!({
            "WindowLayoutsChanged": {
                "changes": [[10, {
                    "tile_size": [1920.0, 1080.0],
                    "window_size": [1920, 1080],
                    "pos_in_scrolling_layout": [1, 1],
                    "tile_pos_in_workspace_view": [0.0, 0.0],
                    "window_offset_in_tile": [0.0, 0.0],
                }]]
            }
        }));
        assert_eq!(tracker.focused_fullscreen_output(), Some("eDP-1"));

        tracker.apply(&json!({
            "WindowLayoutsChanged": {
                "changes": [[10, {
                    "tile_size": [1000.0, 800.0],
                    "window_size": [1000, 800],
                    "pos_in_scrolling_layout": [1, 1],
                    "tile_pos_in_workspace_view": [0.0, 0.0],
                    "window_offset_in_tile": [0.0, 0.0],
                }]]
            }
        }));
        assert_eq!(tracker.focused_fullscreen_output(), None);
    }

    #[test]
    fn only_the_focused_workspace_decides() {
        let mut tracker = tracker_with_output();
        tracker.apply(&json!({
            "WorkspacesChanged": {
                "workspaces": [
                    json!({
                        "id": 1,
                        "output": "eDP-1",
                        "is_focused": false,
                        "is_active": true,
                        "active_window_id": 10,
                    }),
                    workspace(2, None),
                ]
            }
        }));
        tracker.apply(
            &json!({ "WindowsChanged": { "windows": [window(10, (1920.0, 1080.0), true)] } }),
        );

        assert_eq!(tracker.focused_fullscreen_output(), None);

        tracker.apply(&json!({ "WorkspaceActivated": { "id": 1, "focused": true } }));
        assert_eq!(tracker.focused_fullscreen_output(), Some("eDP-1"));
    }

    #[test]
    fn reports_the_fullscreen_windows_own_output() {
        let mut tracker = tracker_with_output();
        tracker.outputs.insert(
            "DP-2".to_string(),
            OutputSize {
                width: 2560.0,
                height: 1440.0,
            },
        );
        tracker.apply(&json!({
            "WorkspacesChanged": {
                "workspaces": [json!({
                    "id": 7,
                    "output": "DP-2",
                    "is_focused": true,
                    "is_active": true,
                    "active_window_id": 11,
                })]
            }
        }));
        tracker.apply(
            &json!({ "WindowsChanged": { "windows": [window(11, (2560.0, 1440.0), true)] } }),
        );

        assert_eq!(tracker.focused_fullscreen_output(), Some("DP-2"));
    }

    #[test]
    fn unknown_events_are_ignored() {
        let mut tracker = tracker_with_output();
        tracker.apply(&json!({ "SomeFutureEvent": { "value": true } }));
        tracker.apply(&json!({ "WindowsChanged": { "windows": [] } }));
        assert_eq!(tracker.focused_fullscreen_output(), None);
    }
}

//! Freedesktop notification server and notification center storage.
//!
//! This is the D-Bus side of the notification screen. On startup it takes the
//! `org.freedesktop.Notifications` name on the session bus and implements the
//! [Desktop Notifications Specification] on
//! `/org/freedesktop/Notifications`. Applications send [`Notify`] requests
//! here; the GPUI side only reads an owned [`Snapshot`] and asks for
//! notifications to be dismissed, so no D-Bus type ever crosses into the render
//! path.
//!
//! Notifications are kept until the user dismisses them, or until the sender's
//! positive `expire_timeout` elapses. The `-1` timeout asks the server to
//! decide; because this server is a notification center rather than a popup,
//! it chooses to keep those notifications.
//!
//! [`Notify`]: https://specifications.freedesktop.org/notification/latest/protocol.html
//! [Desktop Notifications Specification]: https://specifications.freedesktop.org/notification/latest/

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use chrono::{DateTime, Local};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;
use zbus::{connection, fdo, interface};

/// Well-known name, object path, and interface from the specification.
const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// Reasons carried by `NotificationClosed`, as defined by the specification.
const CLOSED_EXPIRED: u32 = 1;
const CLOSED_DISMISSED: u32 = 2;
const CLOSED_BY_CALL: u32 = 3;

/// How urgent a notification is, from the standard `urgency` hint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    fn from_hint(value: Option<u8>) -> Self {
        match value {
            Some(0) => Self::Low,
            Some(2) => Self::Critical,
            _ => Self::Normal,
        }
    }
}

/// One notification, flattened into owned data for the UI.
#[derive(Clone, Debug)]
pub struct Notification {
    /// Server-assigned ID, unique for the lifetime of the process.
    pub id: u32,
    /// The sending application's display name.
    pub app_name: String,
    /// Icon name or image path, captured for a future icon renderer.
    #[allow(dead_code)]
    pub app_icon: Option<String>,
    /// One-line overview.
    pub summary: String,
    /// Optional multi-line body, rendered as sent (the server does not claim
    /// the `body-markup` capability).
    pub body: String,
    pub urgency: Urgency,
    /// When the notification last arrived or was replaced.
    pub received: DateTime<Local>,
}

/// The current notification list, cloned for the UI.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Newest first.
    pub notifications: Vec<Notification>,
    pub error: Option<String>,
}

/// Something changed in the notification store.
///
/// [`Update::Arrived`] is also the hook for a future on-arrival toast: a
/// listener can read [`snapshot`] and present the newest entry.
#[derive(Clone, Copy, Debug)]
pub enum Update {
    /// A notification was added or replaced.
    Arrived,
    /// A notification was dismissed, expired, or closed by its sender.
    Closed,
}

/// A notification plus the token that identifies its current expiry timer.
struct Stored {
    notification: Notification,
    /// Bumped whenever this ID is replaced, so a stale timer cannot close the
    /// newer notification.
    token: u64,
}

#[derive(Default)]
struct State {
    notifications: Vec<Stored>,
    next_id: u32,
    next_token: u64,
    /// Update subscribers. `try_send` removes entries whose receiver is gone.
    subscribers: Vec<(u64, Sender<Update>)>,
    next_subscriber: u64,
    error: Option<String>,
}

struct Service {
    shared: Arc<Mutex<State>>,
    commands: Sender<Command>,
}

static SERVICE: OnceLock<Service> = OnceLock::new();

enum Command {
    Dismiss { id: u32 },
    DismissAll,
    Expire { id: u32, token: u64 },
}

/// A live feed of notification changes. Dropping it unsubscribes.
pub struct Subscription {
    id: u64,
    receiver: Receiver<Update>,
    shared: Option<Arc<Mutex<State>>>,
}

impl Subscription {
    /// The channel of changes seen since the subscription was created.
    pub fn receiver(&self) -> &Receiver<Update> {
        &self.receiver
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let Some(shared) = &self.shared else {
            return;
        };
        if let Ok(mut state) = shared.lock() {
            state.subscribers.retain(|(id, _)| *id != self.id);
        }
    }
}

/// Scheduled notification expiry: deadline, notification ID, and the token of
/// the notification that requested it.
type Expiry = (Instant, u32, u64);

/// Starts the D-Bus notification server. Idempotent.
pub fn start() {
    SERVICE.get_or_init(|| {
        let shared = Arc::new(Mutex::new(State::default()));
        let (commands, command_rx) = async_channel::unbounded();
        let (expiry, expiry_rx) = mpsc::channel::<Expiry>();

        let expiry_commands = commands.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("notifications-expiry".to_string())
            .spawn(move || run_expiry(expiry_rx, expiry_commands))
        {
            eprintln!("gpui-shell: could not start the notification expiry thread: {err}");
        }

        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("notifications".to_string())
            .spawn(move || {
                if let Err(err) = zbus::block_on(run(thread_shared.clone(), command_rx, expiry)) {
                    thread_shared.lock().unwrap().error =
                        Some(format!("notification server: {err}"));
                }
            });

        if let Err(err) = spawned {
            shared.lock().unwrap().error = Some(format!(
                "could not start the notification server thread: {err}"
            ));
        }

        Service { shared, commands }
    });
}

/// Closes notifications whose explicit expiry timeout has elapsed.
///
/// A single thread owns all deadlines, so replacing a notification often does
/// not leave a sleeping thread behind for every request.
fn run_expiry(schedules: mpsc::Receiver<Expiry>, commands: Sender<Command>) {
    let mut pending: BinaryHeap<Reverse<Expiry>> = BinaryHeap::new();

    loop {
        let now = Instant::now();
        while let Some(&Reverse((at, id, token))) = pending.peek() {
            if at > now {
                break;
            }
            pending.pop();
            let _ = commands.try_send(Command::Expire { id, token });
        }

        let wait = pending
            .peek()
            .map(|&Reverse((at, _, _))| at.saturating_duration_since(Instant::now()));

        match wait {
            Some(wait) => match schedules.recv_timeout(wait) {
                Ok(entry) => pending.push(Reverse(entry)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match schedules.recv() {
                Ok(entry) => pending.push(Reverse(entry)),
                Err(_) => break,
            },
        }
    }
}

/// Subscribes to notification changes. Dropping the result unsubscribes.
pub fn subscribe() -> Subscription {
    start();

    let Some(service) = SERVICE.get() else {
        // `start` always installs the service, but avoid panicking if that
        // ever changes: an already-closed channel produces no updates.
        let (_, receiver) = async_channel::unbounded();
        return Subscription {
            id: 0,
            receiver,
            shared: None,
        };
    };

    let shared = service.shared.clone();
    let (sender, receiver) = async_channel::unbounded();
    let id = {
        let mut state = shared.lock().unwrap();
        let id = state.next_subscriber;
        state.next_subscriber = state.next_subscriber.wrapping_add(1);
        state.subscribers.push((id, sender));
        id
    };

    Subscription {
        id,
        receiver,
        shared: Some(shared),
    }
}

/// Returns the current notification list.
pub fn snapshot() -> Snapshot {
    let Some(service) = SERVICE.get() else {
        return Snapshot {
            notifications: Vec::new(),
            error: Some("notification server has not started".to_string()),
        };
    };

    let state = service.shared.lock().unwrap();
    Snapshot {
        notifications: state
            .notifications
            .iter()
            .map(|stored| stored.notification.clone())
            .collect(),
        error: state.error.clone(),
    }
}

/// Dismisses one notification, as if the user had closed it.
pub fn dismiss(id: u32) {
    send(Command::Dismiss { id });
}

/// Dismisses every notification.
pub fn dismiss_all() {
    send(Command::DismissAll);
}

fn send(command: Command) {
    let Some(service) = SERVICE.get() else {
        return;
    };
    // The channel is unbounded, so this only fails if the worker has stopped.
    let _ = service.commands.try_send(command);
}

/// The background worker: owns the connection and serves the D-Bus interface.
async fn run(
    shared: Arc<Mutex<State>>,
    commands: Receiver<Command>,
    expiry: mpsc::Sender<Expiry>,
) -> zbus::Result<()> {
    let connection = connection::Builder::session()?.build().await?;

    connection
        .object_server()
        .at(
            NOTIFICATIONS_PATH,
            Notifications {
                shared: shared.clone(),
                expiry,
            },
        )
        .await?;

    // The name is normally free. If another server owns it, applications will
    // keep talking to that one, so report it instead of pretending otherwise.
    match connection.request_name(NOTIFICATIONS_NAME).await {
        Ok(()) => {}
        Err(zbus::Error::NameTaken) => {
            shared.lock().unwrap().error = Some(
                "another notification server already owns org.freedesktop.Notifications"
                    .to_string(),
            );
        }
        Err(err) => return Err(err),
    }

    let emitter = SignalEmitter::new(&connection, NOTIFICATIONS_PATH)?;

    while let Ok(command) = commands.recv().await {
        match command {
            Command::Dismiss { id } => {
                let removed = {
                    let mut state = shared.lock().unwrap();
                    close_locked(&mut state, id, None)
                };
                if removed.is_some() {
                    let _ = emitter.notification_closed(id, CLOSED_DISMISSED).await;
                }
            }
            Command::DismissAll => {
                let ids = {
                    let mut state = shared.lock().unwrap();
                    let ids: Vec<u32> = state
                        .notifications
                        .iter()
                        .map(|stored| stored.notification.id)
                        .collect();
                    if !ids.is_empty() {
                        state.notifications.clear();
                        broadcast(&mut state, Update::Closed);
                    }
                    ids
                };

                for id in ids {
                    let _ = emitter.notification_closed(id, CLOSED_DISMISSED).await;
                }
            }
            Command::Expire { id, token } => {
                let removed = {
                    let mut state = shared.lock().unwrap();
                    close_locked(&mut state, id, Some(token))
                };
                if removed.is_some() {
                    let _ = emitter.notification_closed(id, CLOSED_EXPIRED).await;
                }
            }
        }
    }

    Ok(())
}

/// The `org.freedesktop.Notifications` object.
struct Notifications {
    shared: Arc<Mutex<State>>,
    /// Used to schedule expiry timers for positive `expire_timeout` values.
    expiry: mpsc::Sender<Expiry>,
}

#[interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    fn get_capabilities(&self) -> Vec<String> {
        // Body markup is not rendered and actions are not offered in the
        // panel, so neither capability is advertised.
        vec!["body".to_string(), "persistence".to_string()]
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> fdo::Result<u32> {
        let urgency = Urgency::from_hint(
            hints
                .get("urgency")
                .and_then(|value| value.downcast_ref::<u8>().ok()),
        );
        let app_name = if app_name.trim().is_empty() {
            hint_string(&hints, "desktop-entry").unwrap_or_else(|| "Application".to_string())
        } else {
            app_name
        };
        let app_icon = (!app_icon.trim().is_empty())
            .then_some(app_icon)
            .or_else(|| {
                hint_string(&hints, "image-path").or_else(|| hint_string(&hints, "image_path"))
            });

        let (id, token) = {
            let mut state = self.shared.lock().unwrap();
            let stored = store(
                &mut state,
                replaces_id,
                Notification {
                    id: 0,
                    app_name,
                    app_icon,
                    summary,
                    body,
                    urgency,
                    received: Local::now(),
                },
            );
            broadcast(&mut state, Update::Arrived);
            stored
        };

        // The specification treats -1 as "server decides" and 0 as "never
        // expire". A notification center keeps both until dismissed; only an
        // explicit positive timeout closes the notification automatically.
        if expire_timeout > 0 {
            let deadline = Instant::now() + Duration::from_millis(expire_timeout as u64);
            let _ = self.expiry.send((deadline, id, token));
        }

        Ok(id)
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let removed = {
            let mut state = self.shared.lock().unwrap();
            close_locked(&mut state, id, None)
        };

        if removed.is_none() {
            return Err(fdo::Error::InvalidArgs("no such notification".to_string()));
        }

        emitter
            .notification_closed(id, CLOSED_BY_CALL)
            .await
            .map_err(|err| fdo::Error::Failed(err.to_string()))?;

        Ok(())
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "gpui-shell".to_string(),
            "gpui-shell".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            "1.3".to_string(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}

/// Stores a new notification, replacing `replaces_id` in place when it exists.
///
/// Returns the notification's ID and the token of the stored entry.
fn store(state: &mut State, replaces_id: u32, mut notification: Notification) -> (u32, u64) {
    if replaces_id != 0 {
        let token = next_token(state);
        if let Some(stored) = state
            .notifications
            .iter_mut()
            .find(|stored| stored.notification.id == replaces_id)
        {
            notification.id = replaces_id;
            *stored = Stored {
                notification,
                token,
            };
            return (replaces_id, token);
        }

        notification.id = replaces_id;
        // Keep the surrogate ID unique even when a client supplies one.
        if replaces_id >= state.next_id {
            state.next_id = replaces_id.wrapping_add(1).max(1);
        }
        state.notifications.insert(
            0,
            Stored {
                notification,
                token,
            },
        );
        return (replaces_id, token);
    }

    let id = allocate_id(state);
    notification.id = id;
    let token = next_token(state);
    state.notifications.insert(
        0,
        Stored {
            notification,
            token,
        },
    );
    (id, token)
}

/// Removes `id` (and, when given, verifies its expiry token) and broadcasts the
/// removal. Returns the removed notification when it existed.
fn close_locked(state: &mut State, id: u32, token: Option<u64>) -> Option<Notification> {
    let index = state.notifications.iter().position(|stored| {
        stored.notification.id == id && token.is_none_or(|token| stored.token == token)
    })?;
    let removed = state.notifications.remove(index).notification;
    broadcast(state, Update::Closed);
    Some(removed)
}

/// Returns the next notification ID, skipping zero.
fn allocate_id(state: &mut State) -> u32 {
    if state.next_id == 0 {
        state.next_id = 1;
    }
    let id = state.next_id;
    state.next_id = state.next_id.wrapping_add(1).max(1);
    id
}

fn next_token(state: &mut State) -> u64 {
    let token = state.next_token;
    state.next_token = state.next_token.wrapping_add(1);
    token
}

/// Sends an update to every subscriber, dropping receivers that are gone.
fn broadcast(state: &mut State, update: Update) {
    state
        .subscribers
        .retain(|(_, sender)| sender.try_send(update).is_ok());
}

fn hint_string(hints: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    hints
        .get(key)
        .and_then(|value| value.downcast_ref::<&str>().ok())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use futures_lite::StreamExt;
    use zbus::zvariant::OwnedValue;

    use super::*;

    #[test]
    #[ignore = "needs a session bus"]
    fn server_serves_the_protocol() {
        start();
        std::thread::sleep(Duration::from_millis(300));

        zbus::block_on(async {
            let connection = zbus::Connection::session().await.unwrap();
            let proxy = zbus::Proxy::new(
                &connection,
                NOTIFICATIONS_NAME,
                NOTIFICATIONS_PATH,
                NOTIFICATIONS_NAME,
            )
            .await
            .unwrap();

            let capabilities: Vec<String> = proxy.call("GetCapabilities", &()).await.unwrap();
            assert!(capabilities.iter().any(|cap| cap == "body"));

            let _: (String, String, String, String) =
                proxy.call("GetServerInformation", &()).await.unwrap();

            let id: u32 = notify(&proxy, 0, "First", 0).await;
            assert!(id > 0);
            let stored = snapshot()
                .notifications
                .into_iter()
                .find(|notification| notification.id == id)
                .expect("the notification should be stored");
            assert_eq!(stored.summary, "First");
            assert_eq!(stored.urgency, Urgency::Critical);

            // Replacing an ID updates it instead of adding a duplicate.
            let replaced: u32 = notify(&proxy, id, "Replacement", 0).await;
            assert_eq!(replaced, id);
            let matches = snapshot()
                .notifications
                .into_iter()
                .filter(|notification| notification.id == id)
                .count();
            assert_eq!(matches, 1);

            // Closing emits NotificationClosed and removes the notification.
            let mut closed = proxy.receive_signal("NotificationClosed").await.unwrap();
            let _: () = proxy.call("CloseNotification", &(id,)).await.unwrap();
            let message = closed.next().await.unwrap();
            let (closed_id, reason): (u32, u32) = message.body().deserialize().unwrap();
            assert_eq!(closed_id, id);
            assert_eq!(reason, CLOSED_BY_CALL);
            assert!(
                snapshot()
                    .notifications
                    .iter()
                    .all(|notification| notification.id != id)
            );

            // A positive timeout closes the notification automatically.
            let expiring: u32 = notify(&proxy, 0, "Expires", 60).await;
            let message = closed.next().await.unwrap();
            let (expired_id, reason): (u32, u32) = message.body().deserialize().unwrap();
            assert_eq!(expired_id, expiring);
            assert_eq!(reason, CLOSED_EXPIRED);
            assert!(
                snapshot()
                    .notifications
                    .iter()
                    .all(|notification| notification.id != expiring)
            );
        });
    }

    async fn notify(
        proxy: &zbus::Proxy<'_>,
        replaces_id: u32,
        summary: &str,
        expire_timeout: i32,
    ) -> u32 {
        let mut hints = HashMap::new();
        hints.insert("urgency".to_string(), OwnedValue::from(2u8));

        proxy
            .call(
                "Notify",
                &(
                    "gpui-shell-tests",
                    replaces_id,
                    "",
                    summary,
                    "Test body",
                    Vec::<String>::new(),
                    hints,
                    expire_timeout,
                ),
            )
            .await
            .unwrap()
    }

    #[test]
    fn ids_start_at_one_and_never_are_zero() {
        let mut state = State::default();
        assert_eq!(allocate_id(&mut state), 1);
        assert_eq!(allocate_id(&mut state), 2);

        state.next_id = u32::MAX;
        assert_eq!(allocate_id(&mut state), u32::MAX);
        assert_eq!(allocate_id(&mut state), 1);
    }

    #[test]
    fn urgency_hint_maps_to_the_standard_levels() {
        assert_eq!(Urgency::from_hint(Some(0)), Urgency::Low);
        assert_eq!(Urgency::from_hint(Some(1)), Urgency::Normal);
        assert_eq!(Urgency::from_hint(Some(2)), Urgency::Critical);
        assert_eq!(Urgency::from_hint(Some(9)), Urgency::Normal);
        assert_eq!(Urgency::from_hint(None), Urgency::Normal);
    }

    #[test]
    fn replacing_keeps_the_id_and_position() {
        let mut state = State::default();
        let (first, _) = store(&mut state, 0, notification("first"));
        let (second, _) = store(&mut state, 0, notification("second"));
        assert_eq!(state.notifications[0].notification.id, second);

        let (replacement, _) = store(&mut state, first, notification("replacement"));
        assert_eq!(replacement, first);
        assert_eq!(state.notifications.len(), 2);
        assert_eq!(state.notifications[1].notification.summary, "replacement");
    }

    #[test]
    fn an_unknown_replacement_id_creates_a_notification_with_that_id() {
        let mut state = State::default();
        let (id, _) = store(&mut state, 42, notification("late"));
        assert_eq!(id, 42);
        assert_eq!(state.notifications[0].notification.id, 42);
        // Later allocations must not collide with the supplied ID.
        assert_eq!(allocate_id(&mut state), 43);
    }

    #[test]
    fn only_the_current_token_expires_a_notification() {
        let mut state = State::default();
        let (id, _) = store(&mut state, 0, notification("keep"));
        let stale = state.notifications[0].token;

        store(&mut state, id, notification("replacement"));

        assert!(close_locked(&mut state, id, Some(stale)).is_none());
        assert_eq!(state.notifications.len(), 1);

        let current = state.notifications[0].token;
        assert!(close_locked(&mut state, id, Some(current)).is_some());
        assert!(state.notifications.is_empty());
    }

    #[test]
    fn hint_strings_are_read_without_borrowing_lifetimes() {
        let mut hints = HashMap::new();
        hints.insert(
            "image-path".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::from("/tmp/icon.png")).unwrap(),
        );

        assert_eq!(
            hint_string(&hints, "image-path").as_deref(),
            Some("/tmp/icon.png")
        );
        assert_eq!(hint_string(&hints, "missing"), None);
    }

    fn notification(summary: &str) -> Notification {
        Notification {
            id: 0,
            app_name: "Test".to_string(),
            app_icon: None,
            summary: summary.to_string(),
            body: String::new(),
            urgency: Urgency::Normal,
            received: Local::now(),
        }
    }
}

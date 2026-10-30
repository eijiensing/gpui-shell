//! StatusNotifierItem host and watcher over the session bus.
//!
//! This is the D-Bus side of the system-tray screen. It implements the
//! [StatusNotifierWatcher] when no other process owns the name (so applications
//! can register their tray items even without a panel), and otherwise registers
//! as a [StatusNotifierHost] with the existing watcher.
//!
//! The D-Bus connection and all proxies live on a dedicated thread running a
//! zbus executor. The GPUI side only reads an owned [`Snapshot`] and sends
//! small commands, so no D-Bus type ever crosses into the render path.
//!
//! [StatusNotifierWatcher]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/StatusNotifierWatcher
//! [StatusNotifierHost]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/StatusNotifierHost

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_channel::{Receiver, Sender};
use futures_lite::StreamExt;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy, connection, fdo, interface};

/// Well-known name and path used by the watcher implementations in the wild.
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const FREEDESKTOP_WATCHER_IFACE: &str = "org.freedesktop.StatusNotifierWatcher";

/// Interface names used by item implementations. KDE's is the de-facto
/// standard; the freedesktop name comes from the specification text.
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const FREEDESKTOP_ITEM_IFACE: &str = "org.freedesktop.StatusNotifierItem";

const MENU_IFACE: &str = "com.canonical.dbusmenu";

/// The value returned by the `GetLayout` method of `com.canonical.dbusmenu`.
///
/// The wire type is `(ia{sv}av)`: an item id, its properties, and its children
/// as variants of the same structure.
type LayoutNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// The attention state advertised by an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemStatus {
    Passive,
    Active,
    NeedsAttention,
}

/// A registered StatusNotifierItem, flattened into owned data.
#[derive(Clone, Debug)]
pub struct Item {
    /// Stable identity used by the UI and for deduplication.
    pub key: String,
    /// Service name reported to/read from the watcher.
    pub registered: String,
    /// Bus name the item's object lives on.
    pub service: String,
    /// Object path of the item.
    pub path: String,
    /// Interface the item actually implements.
    pub interface: String,
    /// The item's `Id` property.
    pub id: String,
    /// The item's `Title`, falling back to `Id` or the service name.
    pub title: String,
    pub category: String,
    pub status: ItemStatus,
    pub icon_name: Option<String>,
    pub tooltip: Option<String>,
    /// Object path of the item's `com.canonical.dbusmenu` object, if any.
    pub menu_path: Option<String>,
    pub item_is_menu: bool,
}

/// One actionable entry from an item's DBusMenu.
#[derive(Clone, Debug)]
pub struct MenuEntry {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    /// Nesting depth, used only for indentation.
    pub depth: usize,
}

/// The current tray contents, cloned for the UI.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub items: Vec<Item>,
    pub error: Option<String>,
}

#[derive(Default)]
struct State {
    items: Vec<Item>,
    hosts: Vec<String>,
    error: Option<String>,
    is_watcher: bool,
}

struct Service {
    shared: Arc<Mutex<State>>,
    commands: Sender<Command>,
}

static SERVICE: OnceLock<Service> = OnceLock::new();

enum Command {
    Activate {
        service: String,
        path: String,
        interface: String,
        reply: Sender<Result<(), String>>,
    },
    SecondaryActivate {
        service: String,
        path: String,
        interface: String,
        reply: Sender<Result<(), String>>,
    },
    OpenMenu {
        service: String,
        menu_path: String,
        reply: Sender<Result<Vec<MenuEntry>, String>>,
    },
    InvokeMenu {
        service: String,
        menu_path: String,
        id: i32,
        reply: Sender<Result<(), String>>,
    },
    Quit {
        service: String,
        menu_path: Option<String>,
        reply: Sender<Result<String, String>>,
    },
}

/// Starts the watcher/host thread. Idempotent.
pub fn start() {
    SERVICE.get_or_init(|| {
        let shared = Arc::new(Mutex::new(State::default()));
        let (commands, command_rx) = async_channel::unbounded();
        let thread_shared = shared.clone();

        let spawned = std::thread::Builder::new()
            .name("status-notifier".to_string())
            .spawn(move || {
                if let Err(err) = zbus::block_on(run(thread_shared.clone(), command_rx)) {
                    thread_shared.lock().unwrap().error = Some(format!("status notifier: {err}"));
                }
            });

        if let Err(err) = spawned {
            shared.lock().unwrap().error =
                Some(format!("could not start the status notifier thread: {err}"));
        }

        Service { shared, commands }
    });
}

/// Returns the current tray contents.
pub fn snapshot() -> Snapshot {
    let Some(service) = SERVICE.get() else {
        return Snapshot {
            items: Vec::new(),
            error: Some("status notifier has not started".to_string()),
        };
    };

    let state = service.shared.lock().unwrap();
    Snapshot {
        items: state.items.clone(),
        error: state.error.clone(),
    }
}

/// Asks an item to activate (left click).
pub fn activate(service: &str, path: &str, interface: &str) -> Receiver<Result<(), String>> {
    let (reply, receiver) = async_channel::bounded(1);
    send(Command::Activate {
        service: service.to_string(),
        path: path.to_string(),
        interface: interface.to_string(),
        reply,
    });
    receiver
}

/// Asks an item to perform its secondary action (middle click).
pub fn secondary_activate(
    service: &str,
    path: &str,
    interface: &str,
) -> Receiver<Result<(), String>> {
    let (reply, receiver) = async_channel::bounded(1);
    send(Command::SecondaryActivate {
        service: service.to_string(),
        path: path.to_string(),
        interface: interface.to_string(),
        reply,
    });
    receiver
}

/// Loads an item's DBusMenu.
pub fn open_menu(service: &str, menu_path: &str) -> Receiver<Result<Vec<MenuEntry>, String>> {
    let (reply, receiver) = async_channel::bounded(1);
    send(Command::OpenMenu {
        service: service.to_string(),
        menu_path: menu_path.to_string(),
        reply,
    });
    receiver
}

/// Clicks one DBusMenu entry.
pub fn invoke_menu(service: &str, menu_path: &str, id: i32) -> Receiver<Result<(), String>> {
    let (reply, receiver) = async_channel::bounded(1);
    send(Command::InvokeMenu {
        service: service.to_string(),
        menu_path: menu_path.to_string(),
        id,
        reply,
    });
    receiver
}

/// Asks an item to quit.
///
/// The application's own quit action is preferred: clicking the menu entry
/// lets it save state and prompt the user. Items without a menu, or whose menu
/// has no recognizable quit entry, fall back to SIGTERM on the process that
/// owns the item's bus name.
pub fn quit(service: &str, menu_path: Option<&str>) -> Receiver<Result<String, String>> {
    let (reply, receiver) = async_channel::bounded(1);
    send(Command::Quit {
        service: service.to_string(),
        menu_path: menu_path.map(str::to_string),
        reply,
    });
    receiver
}

fn send(command: Command) {
    let Some(service) = SERVICE.get() else {
        return;
    };
    // The channel is unbounded, so this only fails if the worker has stopped.
    let _ = service.commands.try_send(command);
}

/// The background worker: owns the connection and handles UI requests.
async fn run(shared: Arc<Mutex<State>>, commands: Receiver<Command>) -> zbus::Result<()> {
    let connection = connect(&shared).await?;

    while let Ok(command) = commands.recv().await {
        match command {
            Command::Activate {
                service,
                path,
                interface,
                reply,
            } => {
                let result = call_item(&connection, &service, &path, &interface, "Activate").await;
                let _ = reply.send(result).await;
            }
            Command::SecondaryActivate {
                service,
                path,
                interface,
                reply,
            } => {
                let result = call_item(
                    &connection,
                    &service,
                    &path,
                    &interface,
                    "SecondaryActivate",
                )
                .await;
                let _ = reply.send(result).await;
            }
            Command::OpenMenu {
                service,
                menu_path,
                reply,
            } => {
                let result = load_menu(&connection, &service, &menu_path).await;
                let _ = reply.send(result).await;
            }
            Command::InvokeMenu {
                service,
                menu_path,
                id,
                reply,
            } => {
                let result = click_menu(&connection, &service, &menu_path, id).await;
                let _ = reply.send(result).await;
            }
            Command::Quit {
                service,
                menu_path,
                reply,
            } => {
                let result = quit_item(&connection, &service, menu_path.as_deref()).await;
                let _ = reply.send(result).await;
            }
        }
    }

    Ok(())
}

/// Connects to the session bus as a watcher if possible, otherwise as a host.
async fn connect(shared: &Arc<Mutex<State>>) -> zbus::Result<Connection> {
    let host_name = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    let connection = connection::Builder::session()?.build().await?;

    // Serve the watcher object first so it is ready the moment the well-known
    // name is claimed. In host mode the object is simply unreachable.
    connection
        .object_server()
        .at(
            WATCHER_PATH,
            Watcher {
                shared: shared.clone(),
            },
        )
        .await?;

    let is_watcher = match connection.request_name(WATCHER_NAME).await {
        Ok(()) => true,
        Err(zbus::Error::NameTaken) => false,
        Err(err) => return Err(err),
    };

    let _ = connection.request_name(host_name.as_str()).await;

    if is_watcher {
        {
            let mut state = shared.lock().unwrap();
            state.is_watcher = true;
            if !state.hosts.contains(&host_name) {
                state.hosts.push(host_name.clone());
            }
        }

        // Announce the host the same way an external host would.
        let emitter = SignalEmitter::new(&connection, WATCHER_PATH)?;
        emitter.status_notifier_host_registered().await?;
    } else {
        shared.lock().unwrap().is_watcher = false;

        let proxy = watcher_proxy(&connection).await?;

        // Subscribe before reading the current list so nothing registered
        // between the two calls is missed. Duplicate additions are deduped.
        spawn_watcher_listener(connection.clone(), shared.clone(), proxy.clone());

        let _: () = proxy
            .call("RegisterStatusNotifierHost", &(host_name.as_str(),))
            .await?;

        let registered: Vec<String> = proxy
            .get_property("RegisteredStatusNotifierItems")
            .await
            .unwrap_or_default();
        for service in registered {
            add_host_item(&connection, shared, &service).await;
        }
    }

    Ok(connection)
}

/// Finds the existing watcher, trying the KDE and freedesktop interface names.
async fn watcher_proxy(connection: &Connection) -> zbus::Result<Proxy<'static>> {
    let mut last_error = None;

    for interface in [WATCHER_IFACE, FREEDESKTOP_WATCHER_IFACE] {
        let Ok(proxy) = Proxy::new_owned(
            connection.clone(),
            WATCHER_NAME.to_string(),
            WATCHER_PATH.to_string(),
            interface.to_string(),
        )
        .await
        else {
            continue;
        };

        match proxy
            .get_property::<Vec<String>>("RegisteredStatusNotifierItems")
            .await
        {
            Ok(_) => return Ok(proxy),
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        zbus::Error::Failure("no StatusNotifierWatcher on the session bus".to_string())
    }))
}

fn spawn_watcher_listener(
    connection: Connection,
    shared: Arc<Mutex<State>>,
    proxy: Proxy<'static>,
) {
    let registered_proxy = proxy.clone();
    let registered_connection = connection.clone();
    let registered_shared = shared.clone();
    connection
        .executor()
        .spawn(
            async move {
                let Ok(mut stream) = registered_proxy
                    .receive_signal("StatusNotifierItemRegistered")
                    .await
                else {
                    return;
                };

                while let Some(message) = stream.next().await {
                    let Ok(service) = message.body().deserialize::<String>() else {
                        continue;
                    };
                    add_host_item(&registered_connection, &registered_shared, &service).await;
                }
            },
            "sni-watcher-registered",
        )
        .detach();

    connection
        .executor()
        .spawn(
            async move {
                let Ok(mut stream) = proxy.receive_signal("StatusNotifierItemUnregistered").await
                else {
                    return;
                };

                while let Some(message) = stream.next().await {
                    let Ok(service) = message.body().deserialize::<String>() else {
                        continue;
                    };
                    let mut state = shared.lock().unwrap();
                    state.items.retain(|item| item.registered != service);
                }
            },
            "sni-watcher-unregistered",
        )
        .detach();
}

async fn add_host_item(connection: &Connection, shared: &Arc<Mutex<State>>, service: &str) {
    // The watcher property contains bus names, but tolerate the older
    // "service/path" form.
    let Some((key, registered, destination, path)) = split_registration(service) else {
        return;
    };

    if let Err(err) = add_item(connection, shared, key, registered, destination, path).await {
        eprintln!("gpui-shell: cannot read status item {service}: {err}");
    }
}

/// The watcher object, served only when we own the watcher name.
struct Watcher {
    shared: Arc<Mutex<State>>,
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let sender = header
            .sender()
            .map(|sender| sender.to_string())
            .unwrap_or_default();

        let Some((key, registered, destination, path)) = resolve_registration(&service, &sender)
        else {
            return Err(fdo::Error::InvalidArgs(
                "invalid StatusNotifierItem service".to_string(),
            ));
        };

        let item = add_item(connection, &self.shared, key, registered, destination, path)
            .await
            .map_err(fdo::Error::Failed)?;

        emitter
            .status_notifier_item_registered(&item.registered)
            .await
            .map_err(|err| fdo::Error::Failed(err.to_string()))?;

        Ok(())
    }

    async fn register_status_notifier_host(
        &self,
        service: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        {
            let mut state = self.shared.lock().unwrap();
            if !state.hosts.contains(&service) {
                state.hosts.push(service);
            }
        }

        emitter
            .status_notifier_host_registered()
            .await
            .map_err(|err| fdo::Error::Failed(err.to_string()))?;

        Ok(())
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        let state = self.shared.lock().unwrap();
        let mut registered = Vec::new();
        for item in &state.items {
            if !registered.contains(&item.registered) {
                registered.push(item.registered.clone());
            }
        }
        registered
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        !self.shared.lock().unwrap().hosts.is_empty()
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Resolves the argument of `RegisterStatusNotifierItem`.
///
/// The argument is normally a bus name, but the older form is an object path on
/// the caller's connection, in which case the caller's unique name is the
/// service to talk to.
fn resolve_registration(service: &str, sender: &str) -> Option<(String, String, String, String)> {
    let service = service.trim();

    if service.is_empty() {
        if sender.is_empty() {
            return None;
        }
        let path = "/StatusNotifierItem".to_string();
        return Some((
            format!("{sender}{path}"),
            sender.to_string(),
            sender.to_string(),
            path,
        ));
    }

    if service.starts_with('/') {
        if sender.is_empty() {
            return None;
        }
        let path = service.to_string();
        return Some((
            format!("{sender}{path}"),
            sender.to_string(),
            sender.to_string(),
            path,
        ));
    }

    split_registration(service)
}

/// Splits `bus-name` or `bus-name/object/path` into its parts.
fn split_registration(service: &str) -> Option<(String, String, String, String)> {
    if service.is_empty() || service.starts_with('/') {
        return None;
    }

    let (service, path) = match service.split_once('/') {
        Some((service, path)) => (service, format!("/{path}")),
        None => (service, "/StatusNotifierItem".to_string()),
    };

    if service.is_empty() {
        return None;
    }

    Some((
        format!("{service}{path}"),
        service.to_string(),
        service.to_string(),
        path,
    ))
}

/// Reads an item and inserts it into the shared snapshot.
async fn add_item(
    connection: &Connection,
    shared: &Arc<Mutex<State>>,
    key: String,
    registered: String,
    service: String,
    path: String,
) -> Result<Item, String> {
    let mut errors = Vec::new();

    for interface in [ITEM_IFACE, FREEDESKTOP_ITEM_IFACE] {
        let proxy = match Proxy::new_owned(
            connection.clone(),
            service.clone(),
            path.clone(),
            interface.to_string(),
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(err) => {
                errors.push(format!("{interface}: {err}"));
                continue;
            }
        };

        let item = match read_item(
            &proxy,
            key.clone(),
            registered.clone(),
            service.clone(),
            path.clone(),
            interface,
        )
        .await
        {
            Ok(item) => item,
            Err(err) => {
                errors.push(format!("{interface}: {err}"));
                continue;
            }
        };

        {
            let mut state = shared.lock().unwrap();
            state.items.retain(|existing| existing.key != item.key);
            state.items.push(item.clone());
            sort_items(&mut state.items);
        }

        spawn_item_listeners(connection.clone(), shared.clone(), proxy, item.clone());

        return Ok(item);
    }

    Err(errors.join("; "))
}

async fn read_item(
    proxy: &Proxy<'_>,
    key: String,
    registered: String,
    service: String,
    path: String,
    interface: &str,
) -> Result<Item, String> {
    let id = proxy.get_property::<String>("Id").await.unwrap_or_default();
    let title = proxy
        .get_property::<String>("Title")
        .await
        .unwrap_or_default();
    let category = proxy
        .get_property::<String>("Category")
        .await
        .unwrap_or_default();
    let status = proxy
        .get_property::<String>("Status")
        .await
        .ok()
        .map(|status| match status.as_str() {
            "Passive" => ItemStatus::Passive,
            "NeedsAttention" => ItemStatus::NeedsAttention,
            _ => ItemStatus::Active,
        })
        .unwrap_or(ItemStatus::Active);
    let icon_name = proxy
        .get_property::<String>("IconName")
        .await
        .ok()
        .filter(|name| !name.is_empty());
    let menu_path = proxy
        .get_property::<OwnedObjectPath>("Menu")
        .await
        .ok()
        .map(|path| path.to_string())
        .filter(|path| path != "/NO_DBUSMENU" && path != "/");
    let item_is_menu = proxy
        .get_property::<bool>("ItemIsMenu")
        .await
        .unwrap_or(false);
    let tooltip = proxy
        .get_property::<(String, Vec<(i32, i32, Vec<u8>)>, String, String)>("ToolTip")
        .await
        .ok()
        .and_then(tooltip_text);

    if id.trim().is_empty() && title.trim().is_empty() && menu_path.is_none() {
        return Err("item exports no Id, Title, or Menu".to_string());
    }

    let title = if !title.trim().is_empty() {
        title.trim().to_string()
    } else if !id.trim().is_empty() {
        id.trim().to_string()
    } else {
        service.clone()
    };

    Ok(Item {
        key,
        registered,
        service,
        path,
        interface: interface.to_string(),
        id,
        title,
        category,
        status,
        icon_name,
        tooltip,
        menu_path,
        item_is_menu,
    })
}

fn tooltip_text(
    (_, _, title, description): (String, Vec<(i32, i32, Vec<u8>)>, String, String),
) -> Option<String> {
    let text = if description.trim().is_empty() {
        title
    } else {
        description
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn sort_items(items: &mut [Item]) {
    items.sort_by_cached_key(|item| (item.title.to_lowercase(), item.id.clone(), item.key.clone()));
}

fn spawn_item_listeners(
    connection: Connection,
    shared: Arc<Mutex<State>>,
    proxy: Proxy<'static>,
    item: Item,
) {
    // Property-change signals: reload the item so every notification kind
    // (title, icon, status, tooltip) is handled uniformly.
    let signal_connection = connection.clone();
    let signal_shared = shared.clone();
    let signal_item = item.clone();
    connection
        .executor()
        .spawn(
            async move {
                let Ok(mut signals) = proxy.receive_all_signals().await else {
                    return;
                };

                while let Some(message) = signals.next().await {
                    let changed = matches!(
                        message.header().member().map(|member| member.as_str()),
                        Some(
                            "NewTitle"
                                | "NewIcon"
                                | "NewStatus"
                                | "NewToolTip"
                                | "NewAttentionIcon"
                                | "NewOverlayIcon"
                        )
                    );
                    if !changed {
                        continue;
                    }

                    let Ok(updated) = read_item(
                        &proxy,
                        signal_item.key.clone(),
                        signal_item.registered.clone(),
                        signal_item.service.clone(),
                        signal_item.path.clone(),
                        &signal_item.interface,
                    )
                    .await
                    else {
                        continue;
                    };

                    let mut state = signal_shared.lock().unwrap();
                    if let Some(existing) = state
                        .items
                        .iter_mut()
                        .find(|existing| existing.key == updated.key)
                    {
                        *existing = updated;
                        sort_items(&mut state.items);
                    }
                }
            },
            "sni-item-signals",
        )
        .detach();

    // Remove the item when its bus name loses its owner.
    let owner_shared = shared;
    let owner_item = item;
    connection
        .executor()
        .spawn(
            async move {
                let Ok(dbus) = Proxy::new_owned(
                    signal_connection.clone(),
                    "org.freedesktop.DBus".to_string(),
                    "/org/freedesktop/DBus".to_string(),
                    "org.freedesktop.DBus".to_string(),
                )
                .await
                else {
                    return;
                };
                let Ok(mut changes) = dbus
                    .receive_signal_with_args(
                        "NameOwnerChanged",
                        &[(0, owner_item.service.as_str())],
                    )
                    .await
                else {
                    return;
                };

                while let Some(message) = changes.next().await {
                    let Ok((_, _, new_owner)) =
                        message.body().deserialize::<(String, String, String)>()
                    else {
                        continue;
                    };
                    if !new_owner.is_empty() {
                        continue;
                    }

                    let (removed, is_watcher) = {
                        let mut state = owner_shared.lock().unwrap();
                        let removed = state
                            .items
                            .iter()
                            .position(|existing| existing.key == owner_item.key)
                            .map(|index| state.items.remove(index));
                        (removed, state.is_watcher)
                    };

                    if let (Some(removed), true) = (removed, is_watcher)
                        && let Ok(emitter) = SignalEmitter::new(&signal_connection, WATCHER_PATH)
                    {
                        let _ = emitter
                            .status_notifier_item_unregistered(&removed.registered)
                            .await;
                    }

                    break;
                }
            },
            "sni-item-owner",
        )
        .detach();
}

/// Calls `Activate` or `SecondaryActivate` on an item.
async fn call_item(
    connection: &Connection,
    service: &str,
    path: &str,
    interface: &str,
    method: &str,
) -> Result<(), String> {
    let proxy = Proxy::new_owned(
        connection.clone(),
        service.to_string(),
        path.to_string(),
        interface.to_string(),
    )
    .await
    .map_err(|err| err.to_string())?;

    let _: () = proxy
        .call(method, &(0i32, 0i32))
        .await
        .map_err(|err| err.to_string())?;

    Ok(())
}

/// Reads an item's menu and flattens visible, actionable entries.
async fn load_menu(
    connection: &Connection,
    service: &str,
    menu_path: &str,
) -> Result<Vec<MenuEntry>, String> {
    let proxy = Proxy::new_owned(
        connection.clone(),
        service.to_string(),
        menu_path.to_string(),
        MENU_IFACE.to_string(),
    )
    .await
    .map_err(|err| err.to_string())?;

    let _: bool = proxy
        .call("AboutToShow", &(0i32,))
        .await
        .map_err(|err| err.to_string())?;

    let properties: Vec<String> = ["label", "enabled", "visible", "type", "children-display"]
        .into_iter()
        .map(str::to_string)
        .collect();

    let reply = proxy
        .call_method("GetLayout", &(0i32, -1i32, &properties))
        .await
        .map_err(|err| err.to_string())?;
    let (_, root): (u32, LayoutNode) = reply.body().deserialize().map_err(|err| err.to_string())?;

    let mut entries = Vec::new();
    flatten_menu(root, 0, &mut entries);
    Ok(entries)
}

fn flatten_menu(node: LayoutNode, depth: usize, entries: &mut Vec<MenuEntry>) {
    let (id, properties, children) = node;

    if !bool_property(&properties, "visible").unwrap_or(true) {
        return;
    }

    let kind = string_property(&properties, "type").unwrap_or_default();
    if kind == "separator" {
        return;
    }

    let label = string_property(&properties, "label")
        .map(|label| clean_label(&label))
        .unwrap_or_default();
    let is_root = id == 0;
    let submenu = string_property(&properties, "children-display").as_deref() == Some("submenu");

    // Submenu headers and the invisible root are not actionable; their children
    // are still shown, one level deeper.
    if !is_root && !submenu && !label.is_empty() {
        entries.push(MenuEntry {
            id,
            label,
            enabled: bool_property(&properties, "enabled").unwrap_or(true),
            depth,
        });
    }

    let child_depth = if is_root { depth } else { depth + 1 };
    for child in children {
        if let Ok(child) = LayoutNode::try_from(child) {
            flatten_menu(child, child_depth, entries);
        }
    }
}

/// Quits an item through its own menu when possible, otherwise by signalling
/// the process behind its bus name.
async fn quit_item(
    connection: &Connection,
    service: &str,
    menu_path: Option<&str>,
) -> Result<String, String> {
    let mut menu_error = None;

    if let Some(menu_path) = menu_path {
        match load_menu(connection, service, menu_path).await {
            Ok(entries) => match quit_entry(&entries) {
                Some(entry) => {
                    let (id, label) = (entry.id, entry.label.clone());
                    match click_menu(connection, service, menu_path, id).await {
                        Ok(()) => return Ok(format!("Requested quit via “{label}”")),
                        Err(err) => {
                            menu_error = Some(format!("couldn't select “{label}”: {err}"));
                        }
                    }
                }
                None => menu_error = Some("no recognizable Quit/Exit entry".to_string()),
            },
            Err(err) => menu_error = Some(err),
        }
    }

    let pid = connection_pid(connection, service).await.map_err(|err| {
        menu_error
            .map(|menu_error| format!("no quit action in the menu ({menu_error}); {err}"))
            .unwrap_or(err)
    })?;

    crate::process::stop_pid(pid).map_err(|err| format!("couldn't stop pid {pid}: {err}"))?;

    Ok(format!("Sent SIGTERM to pid {pid}"))
}

/// The PID of the process that owns a bus name.
///
/// The D-Bus daemon resolves well-known names to their current owner, so this
/// is exact and avoids matching on window titles or command lines. Under a
/// sandbox (Flatpak/Snap) this may name the proxy rather than the application,
/// which is why the menu action is attempted first.
async fn connection_pid(connection: &Connection, service: &str) -> Result<u32, String> {
    let reply = connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(service,),
        )
        .await
        .map_err(|err| format!("couldn't find the process behind {service}: {err}"))?;

    let pid: u32 = reply
        .body()
        .deserialize()
        .map_err(|err| format!("couldn't read the process id behind {service}: {err}"))?;

    if pid <= 1 || pid == std::process::id() {
        return Err(format!("refusing to signal pid {pid}"));
    }

    Ok(pid)
}

/// Labels used for "quit" in the languages applications commonly ship.
///
/// DBusMenu entries have no semantic action id, so the label is the only
/// signal available. Keep this list to unambiguous quit actions: words like
/// "stop" or "close" routinely mean something else in a tray menu.
const QUIT_WORDS: &[&str] = &[
    "quit",
    "exit",
    "beenden",
    "quitter",
    "salir",
    "sair",
    "esci",
    "afsluiten",
    "avsluta",
    "avslutt",
    "afslut",
    "lopeta",
    "zakończ",
    "выйти",
    "выход",
    "завершить",
    "終了",
    "退出",
    "종료",
    "çıkış",
    "ukončit",
    "έξοδος",
    "יציאה",
    "خروج",
];

/// Finds the menu entry that quits the application.
fn quit_entry(entries: &[MenuEntry]) -> Option<&MenuEntry> {
    entries
        .iter()
        .filter_map(|entry| quit_score(entry).map(|score| (score, entry)))
        .min_by_key(|(score, entry)| (std::cmp::Reverse(*score), entry.depth))
        .map(|(_, entry)| entry)
}

/// Ranks a menu entry as a quit action, preferring the most explicit label.
fn quit_score(entry: &MenuEntry) -> Option<u8> {
    if !entry.enabled {
        return None;
    }

    // Qt marks mnemonics with `&`; GTK's `_` is already stripped by
    // `flatten_menu`. Some labels carry an accelerator after a tab.
    let label = entry
        .label
        .split('\t')
        .next()
        .unwrap_or_default()
        .replace('&', "")
        .to_lowercase();
    let label = label
        .trim()
        .trim_end_matches(|ch| ch == '.' || ch == '…')
        .trim();

    // "Exit fullscreen" is a common non-quit action in media players.
    if label.contains("fullscreen")
        || label.contains("full screen")
        || label.contains("full-screen")
    {
        return None;
    }

    QUIT_WORDS
        .iter()
        .map(|word| {
            if label == *word {
                100
            } else if label.starts_with(word)
                && label[word.len()..]
                    .starts_with(|ch: char| ch.is_whitespace() || ch == '-' || ch == ':')
            {
                90
            } else if label.ends_with(word) {
                80
            } else if label.contains(word) {
                70
            } else {
                0
            }
        })
        .max()
        .filter(|score| *score > 0)
}

fn string_property(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    properties
        .get(name)
        .and_then(|value| value.downcast_ref::<&str>().ok())
        .map(str::to_string)
}

fn bool_property(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<bool> {
    properties
        .get(name)
        .and_then(|value| value.downcast_ref::<bool>().ok())
}

/// Removes DBusMenu mnemonic underscores (`_Quit` -> `Quit`), preserving `__`
/// as a literal underscore.
fn clean_label(label: &str) -> String {
    let mut cleaned = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '_' {
            if chars.peek() == Some(&'_') {
                cleaned.push('_');
                chars.next();
            }
        } else {
            cleaned.push(ch);
        }
    }

    cleaned.trim().to_string()
}

/// Clicks a menu entry.
async fn click_menu(
    connection: &Connection,
    service: &str,
    menu_path: &str,
    id: i32,
) -> Result<(), String> {
    let proxy = Proxy::new_owned(
        connection.clone(),
        service.to_string(),
        menu_path.to_string(),
        MENU_IFACE.to_string(),
    )
    .await
    .map_err(|err| err.to_string())?;

    let _: () = proxy
        .call("Event", &(id, "clicked", Value::I32(0), 0u32))
        .await
        .map_err(|err| err.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use zbus::zvariant::{OwnedObjectPath, OwnedValue, StructureBuilder, Value};

    use super::{
        ItemStatus, LayoutNode, MenuEntry, clean_label, quit_entry, resolve_registration,
        split_registration,
    };

    #[test]
    fn resolves_bus_name_registrations() {
        let (key, registered, service, path) =
            resolve_registration("org.kde.StatusNotifierItem-42-1", ":1.9").unwrap();

        assert_eq!(key, "org.kde.StatusNotifierItem-42-1/StatusNotifierItem");
        assert_eq!(registered, "org.kde.StatusNotifierItem-42-1");
        assert_eq!(service, "org.kde.StatusNotifierItem-42-1");
        assert_eq!(path, "/StatusNotifierItem");
    }

    #[test]
    fn resolves_path_registrations_against_the_sender() {
        let (key, registered, service, path) =
            resolve_registration("/StatusNotifierItem", ":1.9").unwrap();

        assert_eq!(key, ":1.9/StatusNotifierItem");
        assert_eq!(registered, ":1.9");
        assert_eq!(service, ":1.9");
        assert_eq!(path, "/StatusNotifierItem");
    }

    #[test]
    fn resolves_empty_registrations_against_the_sender() {
        let (_, registered, service, path) = resolve_registration("", ":1.9").unwrap();

        assert_eq!(registered, ":1.9");
        assert_eq!(service, ":1.9");
        assert_eq!(path, "/StatusNotifierItem");
    }

    #[test]
    fn splits_service_and_path() {
        let (key, registered, service, path) = split_registration(":1.9/org/example/Tray").unwrap();

        assert_eq!(key, ":1.9/org/example/Tray");
        assert_eq!(registered, ":1.9");
        assert_eq!(service, ":1.9");
        assert_eq!(path, "/org/example/Tray");
    }

    #[test]
    fn cleans_menu_mnemonics() {
        assert_eq!(clean_label("_Quit"), "Quit");
        assert_eq!(clean_label("_File"), "File");
        assert_eq!(clean_label("Undo__Redo"), "Undo_Redo");
    }

    #[test]
    fn finds_a_quit_entry_by_label() {
        let entries = vec![
            entry(1, "Show", true, 0),
            entry(2, "Quit Discord", true, 0),
            entry(3, "Settings", true, 0),
        ];
        assert_eq!(quit_entry(&entries).map(|entry| entry.id), Some(2));
    }

    #[test]
    fn prefers_an_exact_quit_label() {
        let entries = vec![entry(1, "Exit game", true, 0), entry(2, "Exit", true, 1)];
        // The exact label outranks the prefix even though it is deeper.
        assert_eq!(quit_entry(&entries).map(|entry| entry.id), Some(2));
    }

    #[test]
    fn ignores_disabled_and_unrelated_actions() {
        let entries = vec![
            entry(1, "Quit", false, 0),
            entry(2, "Stop playback", true, 0),
            entry(3, "Close window", true, 0),
        ];
        assert!(quit_entry(&entries).is_none());
    }

    #[test]
    fn recognizes_localized_labels_and_qt_mnemonics() {
        assert_eq!(
            quit_entry(&[entry(1, "Beenden", true, 0)]).map(|entry| entry.id),
            Some(1)
        );
        assert_eq!(
            quit_entry(&[entry(2, "&Quitter", true, 0)]).map(|entry| entry.id),
            Some(2)
        );
    }

    #[test]
    fn does_not_mistake_fullscreen_actions_for_quitting() {
        let entries = vec![
            entry(1, "Exit Fullscreen", true, 0),
            entry(2, "&Quit", true, 0),
        ];
        assert_eq!(quit_entry(&entries).map(|entry| entry.id), Some(2));
    }

    #[test]
    fn prefers_a_shallow_entry_when_labels_tie() {
        let entries = vec![entry(1, "Quit", true, 1), entry(2, "Quit", true, 0)];
        assert_eq!(quit_entry(&entries).map(|entry| entry.id), Some(2));
    }

    fn entry(id: i32, label: &str, enabled: bool, depth: usize) -> MenuEntry {
        MenuEntry {
            id,
            label: label.to_string(),
            enabled,
            depth,
        }
    }

    struct TestItem {
        activated: Arc<Mutex<usize>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl TestItem {
        #[zbus(property)]
        fn id(&self) -> String {
            "test-item".to_string()
        }

        #[zbus(property)]
        fn title(&self) -> String {
            "Test Item".to_string()
        }

        #[zbus(property)]
        fn status(&self) -> String {
            "Active".to_string()
        }

        #[zbus(property)]
        fn category(&self) -> String {
            "ApplicationStatus".to_string()
        }

        #[zbus(property)]
        fn icon_name(&self) -> String {
            "test-icon".to_string()
        }

        #[zbus(property)]
        fn item_is_menu(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from("/MenuBar").unwrap()
        }

        fn activate(&self, _x: i32, _y: i32) {
            *self.activated.lock().unwrap() += 1;
        }

        fn secondary_activate(&self, _x: i32, _y: i32) {}

        fn context_menu(&self, _x: i32, _y: i32) {}

        fn scroll(&self, _delta: i32, _orientation: String) {}
    }

    struct TestMenu {
        clicked: Arc<Mutex<Vec<i32>>>,
    }

    #[zbus::interface(name = "com.canonical.dbusmenu")]
    impl TestMenu {
        #[zbus(property)]
        fn version(&self) -> u32 {
            3
        }

        #[zbus(property)]
        fn status(&self) -> String {
            "normal".to_string()
        }

        #[zbus(property)]
        fn text_direction(&self) -> String {
            "ltr".to_string()
        }

        fn about_to_show(&self, _id: i32) -> bool {
            true
        }

        fn get_layout(&self, _parent: i32, _depth: i32, _names: Vec<String>) -> (u32, LayoutNode) {
            let properties = HashMap::from([
                (
                    "label".to_string(),
                    OwnedValue::try_from(Value::from("Quit")).unwrap(),
                ),
                (
                    "visible".to_string(),
                    OwnedValue::try_from(Value::from(true)).unwrap(),
                ),
                (
                    "enabled".to_string(),
                    OwnedValue::try_from(Value::from(true)).unwrap(),
                ),
            ]);
            let child = OwnedValue::try_from(Value::Structure(
                StructureBuilder::new()
                    .add_field(2i32)
                    .append_field(Value::from(properties))
                    .append_field(Value::from(Vec::<OwnedValue>::new()))
                    .build()
                    .unwrap(),
            ))
            .unwrap();

            (3, (0, HashMap::new(), vec![child]))
        }

        fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
            if event_id == "clicked" {
                self.clicked.lock().unwrap().push(id);
            }
        }
    }

    #[test]
    #[ignore = "needs a session bus"]
    fn registers_an_item_and_clicks_its_menu() {
        super::start();
        std::thread::sleep(Duration::from_millis(300));

        let activated = Arc::new(Mutex::new(0));
        let clicked = Arc::new(Mutex::new(Vec::new()));
        let service_name = format!("org.kde.StatusNotifierItem-test-{}", std::process::id());
        let item = TestItem {
            activated: activated.clone(),
        };
        let menu = TestMenu {
            clicked: clicked.clone(),
        };

        let connection = zbus::block_on(async move {
            let connection = zbus::connection::Builder::session()
                .unwrap()
                .name(service_name.as_str())
                .unwrap()
                .serve_at("/StatusNotifierItem", item)
                .unwrap()
                .serve_at("/MenuBar", menu)
                .unwrap()
                .build()
                .await
                .unwrap();

            let watcher = zbus::Proxy::new(
                &connection,
                "org.kde.StatusNotifierWatcher",
                "/StatusNotifierWatcher",
                "org.kde.StatusNotifierWatcher",
            )
            .await
            .unwrap();
            let _: () = watcher
                .call("RegisterStatusNotifierItem", &(service_name.as_str(),))
                .await
                .unwrap();

            let registered: Vec<String> = watcher
                .get_property("RegisteredStatusNotifierItems")
                .await
                .unwrap();
            assert!(registered.contains(&service_name));

            connection
        });

        let mut item = None;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(found) = super::snapshot()
                .items
                .into_iter()
                .find(|item| item.title == "Test Item")
            {
                item = Some(found);
                break;
            }
        }
        let item = item.expect("the item should be registered");
        assert_eq!(item.status, ItemStatus::Active);
        assert_eq!(item.menu_path.as_deref(), Some("/MenuBar"));

        zbus::block_on(async {
            super::activate(&item.service, &item.path, &item.interface)
                .recv()
                .await
                .unwrap()
                .unwrap();
        });
        assert_eq!(*activated.lock().unwrap(), 1);

        let entries = zbus::block_on(async {
            super::open_menu(&item.service, "/MenuBar")
                .recv()
                .await
                .unwrap()
                .unwrap()
        });
        assert!(entries.iter().any(|entry| entry.label == "Quit"));

        zbus::block_on(async {
            super::invoke_menu(&item.service, "/MenuBar", 2)
                .recv()
                .await
                .unwrap()
                .unwrap();
        });
        assert_eq!(*clicked.lock().unwrap(), vec![2]);

        // Dropping the item's connection should unregister it automatically.
        drop(connection);
        let mut removed = false;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if !super::snapshot()
                .items
                .iter()
                .any(|item| item.title == "Test Item")
            {
                removed = true;
                break;
            }
        }
        assert!(removed, "the item should disappear with its service");
    }

    #[test]
    #[ignore = "needs a session bus"]
    fn watcher_serves_the_protocol() {
        super::start();
        std::thread::sleep(std::time::Duration::from_millis(500));

        zbus::block_on(async {
            let connection = zbus::Connection::session().await.unwrap();
            let proxy = zbus::Proxy::new(
                &connection,
                "org.kde.StatusNotifierWatcher",
                "/StatusNotifierWatcher",
                "org.kde.StatusNotifierWatcher",
            )
            .await
            .unwrap();

            let host: bool = proxy
                .get_property("IsStatusNotifierHostRegistered")
                .await
                .unwrap();
            assert!(host);
        });
    }
}

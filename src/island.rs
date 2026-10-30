use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use gpui::{
    AnyElement, App, Context, DisplayId, FocusHandle, Hsla, KeyDownEvent, Render, ScrollHandle,
    Window, div, prelude::*, px,
};
use uuid::Uuid;

use crate::animation::Animation;
use crate::appearance::{Appearance, IslandMode, NORMAL_TEXT_SIZE, Settings, TEXT_FONT_FAMILY};
use crate::audio::{self, Volume};
use crate::glyphs::{self, Glyph, Severity};
use crate::launcher::Catalog;
use crate::notifications::{self, Notification, Urgency};
use crate::process::Processes;
use crate::status_notifier::{self, Item, MenuEntry};

/// How long the volume stays on screen after the last change before the island
/// shows the clock again.
const VOLUME_HOLD: Duration = Duration::from_millis(2000);

/// Fraction of the travel between the normal and expanded sizes at which the
/// launcher content is considered hidden (below) or shown (above). Deriving
/// visibility from the animated size keeps the content switch tied to the
/// resize animation rather than to a separate timer.
const REVEAL_EDGE: f32 = 0.35;

/// Normal-mode content: the clock, or a transient volume indicator.
enum NormalIndicator {
    Clock,
    Volume(Volume),
}

/// Launcher-specific interaction state, owned per window.
struct Launcher {
    /// Focus target for the search input.
    focus: FocusHandle,
    /// What the user has typed.
    query: String,
    /// Whether Ctrl/Cmd+A has selected the whole query. The field has no
    /// movable caret, so selection is all-or-nothing.
    select_all: bool,
    /// Catalog indices matching `query`, in display order.
    results: Vec<usize>,
    /// Index into `results` of the highlighted row.
    selected: usize,
    /// Keeps the selected row in view.
    scroll: ScrollHandle,
    /// Message shown when launching fails.
    error: Option<String>,
}

impl Launcher {
    fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            query: String::new(),
            select_all: false,
            results: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            error: None,
        }
    }
}

/// How long after a stop request the tray rescans processes, so a process that
/// exits promptly disappears without waiting for the periodic refresh.
const TRAY_REFRESH_DELAY: Duration = Duration::from_millis(350);

/// Which list the tray is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TrayTab {
    /// StatusNotifierItems registered on the session bus.
    Items,
    /// Fallback process list for applications that don't expose a tray item.
    Processes,
}

/// System-tray-specific interaction state, owned per window.
struct Tray {
    /// Focus target shared by the item, menu, and process search fields.
    focus: FocusHandle,
    /// Which list is showing when no menu is open.
    tab: TrayTab,
    /// One-line feedback shown under the current list.
    message: Option<TrayMessage>,
    /// Error reported by the D-Bus side, if any.
    service_error: Option<String>,

    /// Registered StatusNotifierItems.
    items: Vec<Item>,
    item_query: String,
    item_select_all: bool,
    item_results: Vec<usize>,
    item_selected: usize,
    item_scroll: ScrollHandle,

    /// Drill-down into an item's DBusMenu.
    menu: Option<MenuView>,

    /// Fallback process list.
    processes: Processes,
    process_query: String,
    process_select_all: bool,
    process_results: Vec<usize>,
    process_selected: usize,
    process_scroll: ScrollHandle,
    /// PID that has been armed by one Enter press and will be stopped by the
    /// next one.
    process_armed: Option<u32>,
}

struct TrayMessage {
    text: String,
    error: bool,
}

/// An item's DBusMenu opened in place.
struct MenuView {
    /// Key of the item that owns this menu.
    item_key: String,
    title: String,
    entries: Vec<MenuEntry>,
    loading: bool,
    error: Option<String>,
    query: String,
    select_all: bool,
    results: Vec<usize>,
    selected: usize,
    scroll: ScrollHandle,
    message: Option<TrayMessage>,
}

impl Tray {
    fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            tab: TrayTab::Items,
            message: None,
            service_error: None,
            items: Vec::new(),
            item_query: String::new(),
            item_select_all: false,
            item_results: Vec::new(),
            item_selected: 0,
            item_scroll: ScrollHandle::new(),
            menu: None,
            processes: Processes::default(),
            process_query: String::new(),
            process_select_all: false,
            process_results: Vec::new(),
            process_selected: 0,
            process_scroll: ScrollHandle::new(),
            process_armed: None,
        }
    }

    fn selected_item(&self) -> Option<&Item> {
        self.item_results
            .get(self.item_selected)
            .and_then(|&index| self.items.get(index))
    }

    fn selected_process(&self) -> Option<&crate::process::Process> {
        self.process_results
            .get(self.process_selected)
            .and_then(|&index| self.processes.entry(index))
    }

    /// Replaces the status-item list while keeping the highlighted item stable.
    fn refresh_items(&mut self, snapshot: status_notifier::Snapshot) {
        let selected_key = self.selected_item().map(|item| item.key.clone());
        let selected_row = self.item_selected;

        self.items = snapshot.items;
        self.service_error = snapshot.error;
        self.item_results = self.filter_items(&self.item_query);
        self.item_selected = selected_key
            .and_then(|key| {
                self.item_results
                    .iter()
                    .position(|&index| self.items.get(index).is_some_and(|item| item.key == key))
            })
            .unwrap_or(selected_row);
        if self.item_selected >= self.item_results.len() {
            self.item_selected = self.item_results.len().saturating_sub(1);
        }

        // The item backing an open menu may have exited.
        if let Some(menu) = &self.menu
            && !self.items.iter().any(|item| item.key == menu.item_key)
        {
            self.menu = None;
            self.message = Some(TrayMessage {
                text: "The status item disappeared".to_string(),
                error: true,
            });
        }
    }

    fn filter_items(&self, query: &str) -> Vec<usize> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.items.len()).collect();
        }

        let mut prefix = Vec::new();
        let mut substring = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let title = item.title.to_lowercase();
            let id = item.id.to_lowercase();

            if title.starts_with(&query) || id.starts_with(&query) {
                prefix.push(index);
            } else if title.contains(&query)
                || id.contains(&query)
                || item.category.to_lowercase().contains(&query)
                || item
                    .tooltip
                    .as_deref()
                    .is_some_and(|tooltip| tooltip.to_lowercase().contains(&query))
            {
                substring.push(index);
            }
        }

        prefix.extend(substring);
        prefix
    }

    fn refilter_items(&mut self) {
        self.item_results = self.filter_items(&self.item_query);
        self.item_selected = 0;
        self.item_scroll = ScrollHandle::new();
    }

    /// Replaces the process list while keeping the highlighted process stable.
    fn refresh_processes(&mut self, processes: Processes) {
        let selected_pid = self.selected_process().map(|process| process.pid);
        let selected_row = self.process_selected;
        let armed = self.process_armed;

        self.processes = processes;
        self.process_results = self.processes.search(&self.process_query);
        // Follow the process if it is still there; otherwise keep the row where
        // it was, so stopping an item doesn't jump the selection to the top.
        self.process_selected = selected_pid
            .and_then(|pid| {
                self.process_results.iter().position(|&index| {
                    self.processes
                        .entry(index)
                        .is_some_and(|process| process.pid == pid)
                })
            })
            .unwrap_or(selected_row);
        if self.process_selected >= self.process_results.len() {
            self.process_selected = self.process_results.len().saturating_sub(1);
        }

        // A process that has exited is no longer armed.
        if armed.is_some_and(|pid| !self.processes.contains(pid)) {
            self.process_armed = None;
        }
    }

    fn refilter_processes(&mut self) {
        self.process_results = self.processes.search(&self.process_query);
        self.process_selected = 0;
        self.process_scroll = ScrollHandle::new();
        self.process_armed = None;
    }
}

impl MenuView {
    fn new(item_key: String, title: String) -> Self {
        Self {
            item_key,
            title,
            entries: Vec::new(),
            loading: true,
            error: None,
            query: String::new(),
            select_all: false,
            results: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            message: None,
        }
    }

    fn selected_entry(&self) -> Option<&MenuEntry> {
        self.results
            .get(self.selected)
            .and_then(|&index| self.entries.get(index))
    }

    fn refilter(&mut self) {
        let query = self.query.trim().to_lowercase();
        self.results = if query.is_empty() {
            (0..self.entries.len()).collect()
        } else {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.label.to_lowercase().contains(&query))
                .map(|(index, _)| index)
                .collect()
        };
        self.selected = 0;
        self.scroll = ScrollHandle::new();
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.results.len();
        if len == 0 {
            return;
        }

        let current = self.selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.selected = next;
        self.scroll.scroll_to_item(next);
    }
}

/// Notification-center interaction state, owned per overlay window.
struct NotificationPanel {
    /// Focus target for the filter input and key handling.
    focus: FocusHandle,
    /// Notifications currently stored by the D-Bus server, newest first.
    list: Vec<Notification>,
    /// What the user has typed to filter the list.
    query: String,
    /// Whether Ctrl/Cmd+A has selected the whole query.
    select_all: bool,
    /// Indices into `list` matching `query`, in display order.
    results: Vec<usize>,
    /// Index into `results` of the highlighted notification.
    selected: usize,
    /// Keeps the selected notification in view.
    scroll: ScrollHandle,
    /// Error reported by the D-Bus side, if any.
    error: Option<String>,
    /// Change feed from the notification server, present on the overlay only.
    subscription: Option<notifications::Subscription>,
}

impl NotificationPanel {
    fn new(focus: FocusHandle, subscribe: bool) -> Self {
        Self {
            focus,
            list: Vec::new(),
            query: String::new(),
            select_all: false,
            results: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            error: None,
            subscription: subscribe.then(notifications::subscribe),
        }
    }

    fn selected_notification(&self) -> Option<&Notification> {
        self.results
            .get(self.selected)
            .and_then(|&index| self.list.get(index))
    }

    fn filter(&self, query: &str) -> Vec<usize> {
        filter_notifications(&self.list, query)
    }

    /// Replaces the list while keeping the highlighted notification stable.
    fn apply(&mut self, snapshot: notifications::Snapshot) {
        let selected_id = self
            .selected_notification()
            .map(|notification| notification.id);
        let selected_row = self.selected;

        self.list = snapshot.notifications;
        self.error = snapshot.error;
        self.results = self.filter(&self.query);
        self.selected = selected_id
            .and_then(|id| {
                self.results.iter().position(|&index| {
                    self.list
                        .get(index)
                        .is_some_and(|notification| notification.id == id)
                })
            })
            .unwrap_or(selected_row);
        if self.selected >= self.results.len() {
            self.selected = self.results.len().saturating_sub(1);
        }
    }

    fn refilter(&mut self) {
        self.results = self.filter(&self.query);
        self.selected = 0;
        self.scroll = ScrollHandle::new();
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.results.len();
        if len == 0 {
            return;
        }

        let current = self.selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.selected = next;
        self.scroll.scroll_to_item(next);
    }

    /// Removes a notification locally so dismissal feels immediate. The D-Bus
    /// side also broadcasts the change and the next snapshot confirms it.
    fn remove(&mut self, id: u32) {
        self.list.retain(|notification| notification.id != id);
        let selected_row = self.selected;
        self.results = self.filter(&self.query);
        self.selected = selected_row.min(self.results.len().saturating_sub(1));
    }

    fn clear(&mut self) {
        self.list.clear();
        self.results.clear();
        self.selected = 0;
        self.scroll = ScrollHandle::new();
    }
}

/// Indices of notifications matching `query`, case-insensitively.
fn filter_notifications(list: &[Notification], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..list.len()).collect();
    }

    list.iter()
        .enumerate()
        .filter(|(_, notification)| {
            notification.app_name.to_lowercase().contains(&query)
                || notification.summary.to_lowercase().contains(&query)
                || notification.body.to_lowercase().contains(&query)
        })
        .map(|(index, _)| index)
        .collect()
}

/// A panel reachable from the overview hub.
///
/// The declaration order is the order the rows are shown in, so the hub reads
/// top to bottom like the launcher list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverviewCommand {
    Launcher,
    Tray,
    Notifications,
}

impl OverviewCommand {
    const ALL: [OverviewCommand; 3] = [
        OverviewCommand::Launcher,
        OverviewCommand::Tray,
        OverviewCommand::Notifications,
    ];

    /// Caption shown next to the icon.
    fn label(self) -> &'static str {
        match self {
            OverviewCommand::Launcher => "Launcher",
            OverviewCommand::Tray => "Tray",
            OverviewCommand::Notifications => "Notifications",
        }
    }

    /// Nerd Font codepoint drawn at the start of the row.
    fn icon(self) -> &'static str {
        match self {
            OverviewCommand::Launcher => "\u{f009}",
            OverviewCommand::Tray => "\u{f0ca}",
            OverviewCommand::Notifications => "\u{f0f3}",
        }
    }

    /// Shortcut letter shown on the right of the row and handled as the hotkey.
    fn initial(self) -> char {
        match self {
            OverviewCommand::Launcher => 'l',
            OverviewCommand::Tray => 't',
            OverviewCommand::Notifications => 'n',
        }
    }

    /// Mode this command opens.
    fn mode(self) -> IslandMode {
        match self {
            OverviewCommand::Launcher => IslandMode::Launcher,
            OverviewCommand::Tray => IslandMode::Tray,
            OverviewCommand::Notifications => IslandMode::Notifications,
        }
    }

    fn from_initial(ch: char) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| command.initial().eq_ignore_ascii_case(&ch))
    }
}

/// Overview-hub interaction state, owned per overlay window.
struct Overview {
    /// Focus target for key handling; the hub has no text field.
    focus: FocusHandle,
    /// Index into [`OverviewCommand::ALL`] of the highlighted row.
    selected: usize,
}

impl Overview {
    fn new(focus: FocusHandle) -> Self {
        Self { focus, selected: 0 }
    }

    fn selected_command(&self) -> OverviewCommand {
        OverviewCommand::ALL[self.selected.min(OverviewCommand::ALL.len() - 1)]
    }

    fn move_selection(&mut self, delta: isize) {
        let last = OverviewCommand::ALL.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }
}

pub struct Island {
    /// What this surface is for.
    role: IslandRole,
    /// Display this surface was opened for, if any.
    ///
    /// Kept separately from [`Window::display`] because the bar is created
    /// before its surface has entered an output; the platform window's display
    /// can still be unset on the first frames.
    display_id: Option<DisplayId>,
    /// Current wall-clock time, refreshed once a second.
    time: String,
    /// What normal mode shows right now.
    indicator: NormalIndicator,
    /// Bumped on every volume change so a superseded revert timer is ignored.
    generation: u64,
    launcher: Launcher,
    tray: Tray,
    notifications: NotificationPanel,
    overview: Overview,
}

/// Which kind of surface an [`Island`] drives.
///
/// [`IslandRole::Bar`] surfaces exist for every display and only ever show the
/// compact clock. [`IslandRole::Overlay`] is a transient surface created on the
/// compositor's focused output while an overlay panel is open, so it can
/// expand and take keyboard focus without disturbing the bars.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IslandRole {
    Bar,
    Overlay,
}

impl Island {
    pub fn new(role: IslandRole, display_id: Option<DisplayId>, cx: &mut Context<Self>) -> Self {
        let island = Self {
            role,
            display_id,
            time: Local::now().format("%H:%M").to_string(),
            indicator: NormalIndicator::Clock,
            generation: 0,
            launcher: Launcher::new(cx.focus_handle()),
            tray: Tray::new(cx.focus_handle()),
            notifications: NotificationPanel::new(cx.focus_handle(), role == IslandRole::Overlay),
            overview: Overview::new(cx.focus_handle()),
        };

        island.start_timer(cx);
        island.watch_volume(cx);
        if role == IslandRole::Overlay {
            island.start_tray_refresh(cx);
            island.start_notification_updates(cx);
        }

        island
    }

    pub fn role(&self) -> IslandRole {
        self.role
    }

    /// Resets the overview hub to its first row. Called on the transient
    /// overlay surface when the overview is requested.
    pub fn prepare_overview(&mut self, cx: &mut Context<Self>) {
        self.overview.selected = 0;
        cx.notify();
    }

    /// Resets the launcher for a fresh search. Called on the transient launcher
    /// surface when the launcher mode is requested.
    pub fn prepare_launcher(&mut self, cx: &mut Context<Self>) {
        self.launcher.query.clear();
        self.launcher.select_all = false;
        self.launcher.selected = 0;
        self.launcher.scroll = ScrollHandle::new();
        self.launcher.error = None;
        self.launcher.results = cx.global::<Catalog>().search("");
        cx.notify();
    }

    /// Resets the tray for a fresh look at the session bus. Called on the
    /// transient overlay surface when the tray mode is requested.
    pub fn prepare_tray(&mut self, cx: &mut Context<Self>) {
        status_notifier::start();

        self.tray.tab = TrayTab::Items;
        self.tray.message = None;
        self.tray.menu = None;
        self.tray.service_error = None;

        self.tray.item_query.clear();
        self.tray.item_select_all = false;
        self.tray.item_selected = 0;
        self.tray.item_scroll = ScrollHandle::new();

        self.tray.process_query.clear();
        self.tray.process_select_all = false;
        self.tray.process_selected = 0;
        self.tray.process_scroll = ScrollHandle::new();
        self.tray.process_armed = None;
        self.tray.processes = Processes::scan();
        self.tray.process_results = self.tray.processes.search("");

        let snapshot = status_notifier::snapshot();
        self.tray.refresh_items(snapshot);
        cx.notify();
    }

    /// Resets the notification center for a fresh look at the stored
    /// notifications. Called on the transient overlay surface.
    pub fn prepare_notifications(&mut self, cx: &mut Context<Self>) {
        notifications::start();

        self.notifications.query.clear();
        self.notifications.select_all = false;
        self.notifications.selected = 0;
        self.notifications.scroll = ScrollHandle::new();
        self.notifications.apply(notifications::snapshot());
        cx.notify();
    }

    /// Keeps the notification panel live while the overlay surface exists.
    ///
    /// Compact-island toasts are driven globally in `server`, so this listener
    /// only refreshes the open panel.
    fn start_notification_updates(&self, cx: &mut Context<Self>) {
        let Some(subscription) = &self.notifications.subscription else {
            return;
        };
        let updates = subscription.receiver().clone();

        cx.spawn(async move |island, cx| {
            while updates.recv().await.is_ok() {
                let active = island.read_with(cx, |_island, cx| {
                    cx.global::<Settings>().mode == IslandMode::Notifications
                });
                let Ok(active) = active else {
                    break;
                };
                if !active {
                    continue;
                }

                let snapshot = notifications::snapshot();
                if island
                    .update(cx, |island, cx| {
                        island.notifications.apply(snapshot);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Asks the server to leave overlay mode.
    ///
    /// Deferred because leaving an overlay mode re-creates the layer surface (to
    /// hand keyboard interactivity back), and that must not run while this
    /// window is borrowed.
    fn request_close(&mut self, cx: &mut Context<Self>) {
        self.launcher.error = None;
        self.tray.process_armed = None;
        self.tray.message = None;
        self.tray.menu = None;
        cx.defer(crate::server::close_overlay);
    }

    /// Recomputes the filtered results for the current query and selects the
    /// first match.
    fn refilter(&mut self, cx: &mut Context<Self>) {
        self.launcher.results = cx.global::<Catalog>().search(&self.launcher.query);
        self.launcher.selected = 0;
        self.launcher.scroll = ScrollHandle::new();
        cx.notify();
    }

    /// Moves the highlighted row by `delta`, clamped to the result list.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.launcher.results.len();
        if len == 0 {
            return;
        }

        let current = self.launcher.selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.launcher.selected = next;
        self.launcher.scroll.scroll_to_item(next);
        cx.notify();
    }

    /// Launches the highlighted application and closes the launcher.
    fn launch_selected(&mut self, cx: &mut Context<Self>) {
        let Some(&index) = self.launcher.results.get(self.launcher.selected) else {
            return;
        };

        // Resolve the entry and spawn it while only holding an immutable borrow
        // of the app context, so the launcher can then be closed.
        let outcome = {
            let catalog = cx.global::<Catalog>();
            match catalog.entry(index) {
                Some(entry) => (entry.name.clone(), entry.launch()),
                None => return,
            }
        };

        match outcome.1 {
            Ok(()) => self.request_close(cx),
            Err(err) => {
                eprintln!("gpui-shell: failed to launch {}: {err}", outcome.0);
                self.launcher.error = Some(format!("Couldn't launch {}", outcome.0));
                cx.notify();
            }
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Keys arriving after an overlay has been closed are ignored.
        match cx.global::<Settings>().mode {
            IslandMode::Overview => self.on_overview_key_down(event, cx),
            IslandMode::Launcher => self.on_launcher_key_down(event, cx),
            IslandMode::Tray => self.on_tray_key_down(event, cx),
            IslandMode::Notifications => self.on_notifications_key_down(event, cx),
            IslandMode::Normal | IslandMode::Calendar => {}
        }
    }

    fn on_overview_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control =
            keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;

        match keystroke.key.as_str() {
            "escape" => return self.request_close(cx),
            "enter" => {
                let command = self.overview.selected_command();
                return self.open_overview_command(command, cx);
            }
            "left" | "up" => {
                self.overview.move_selection(-1);
                cx.notify();
                return;
            }
            "right" | "down" => {
                self.overview.move_selection(1);
                cx.notify();
                return;
            }
            // Leave Tab to GPUI's focus navigation.
            "tab" => return,
            _ => {}
        }

        // Plain typing only; ignore shortcuts and control characters.
        if control {
            return;
        }
        if let Some(text) = keystroke.key_char.as_deref()
            && let Some(command) = text.chars().find_map(OverviewCommand::from_initial)
        {
            self.open_overview_command(command, cx);
        }
    }

    /// Opens a command from the hub, reusing the overlay surface for that panel.
    /// Deferred because switching modes re-prepares the overlay content and
    /// must not run while this window is borrowed.
    fn open_overview_command(&mut self, command: OverviewCommand, cx: &mut Context<Self>) {
        cx.defer(move |cx| crate::server::open_panel(cx, command.mode()));
    }

    fn on_launcher_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control =
            keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;

        // Select the whole query so the next keystroke replaces it. `secondary`
        // is Ctrl on Linux and Cmd on macOS.
        if keystroke.modifiers.secondary() && keystroke.key == "a" {
            self.launcher.select_all = true;
            cx.notify();
            return;
        }

        match keystroke.key.as_str() {
            "escape" => return self.request_close(cx),
            "enter" => return self.launch_selected(cx),
            "up" => return self.move_selection(-1, cx),
            "down" => return self.move_selection(1, cx),
            "backspace" => {
                if self.launcher.select_all {
                    self.launcher.select_all = false;
                    self.launcher.query.clear();
                    self.refilter(cx);
                } else if self.launcher.query.pop().is_some() {
                    self.refilter(cx);
                }
                return;
            }
            // Leave Tab to GPUI's focus navigation.
            "tab" => return,
            _ => {}
        }

        // Plain typing only; ignore shortcuts and control characters.
        if control {
            return;
        }
        if let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text.chars().any(char::is_control)
        {
            if self.launcher.select_all {
                self.launcher.select_all = false;
                self.launcher.query.clear();
            }
            self.launcher.query.push_str(text);
            self.refilter(cx);
        }
    }

    fn on_notifications_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control =
            keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;

        if keystroke.modifiers.secondary() && keystroke.key == "a" {
            self.notifications.select_all = true;
            cx.notify();
            return;
        }

        // Ctrl/Cmd+D clears the whole center, matching the footer hint.
        if keystroke.modifiers.secondary() && keystroke.key == "d" {
            return self.dismiss_all_notifications(cx);
        }

        match keystroke.key.as_str() {
            "escape" => return self.request_close(cx),
            "enter" | "delete" => return self.dismiss_selected_notification(cx),
            "up" => return self.move_notification_selection(-1, cx),
            "down" => return self.move_notification_selection(1, cx),
            "backspace" => {
                if self.notifications.select_all {
                    self.notifications.select_all = false;
                    self.notifications.query.clear();
                    self.refilter_notifications(cx);
                } else if self.notifications.query.pop().is_some() {
                    self.refilter_notifications(cx);
                }
                return;
            }
            // Leave Tab to GPUI's focus navigation.
            "tab" => return,
            _ => {}
        }

        // Plain typing only; ignore shortcuts and control characters.
        if control {
            return;
        }
        if let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text.chars().any(char::is_control)
        {
            if self.notifications.select_all {
                self.notifications.select_all = false;
                self.notifications.query.clear();
            }
            self.notifications.query.push_str(text);
            self.refilter_notifications(cx);
        }
    }

    fn refilter_notifications(&mut self, cx: &mut Context<Self>) {
        self.notifications.refilter();
        cx.notify();
    }

    fn move_notification_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.notifications.move_selection(delta);
        cx.notify();
    }

    /// Dismisses the highlighted notification and removes it immediately.
    fn dismiss_selected_notification(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .notifications
            .selected_notification()
            .map(|notification| notification.id)
        else {
            return;
        };

        notifications::dismiss(id);
        self.notifications.remove(id);
        cx.notify();
    }

    /// Dismisses every notification in one go.
    fn dismiss_all_notifications(&mut self, cx: &mut Context<Self>) {
        if self.notifications.list.is_empty() {
            return;
        }

        notifications::dismiss_all();
        self.notifications.clear();
        cx.notify();
    }

    fn on_tray_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.tray.menu.is_some() {
            self.on_tray_menu_key_down(event, cx);
            return;
        }

        match self.tray.tab {
            TrayTab::Items => self.on_tray_item_key_down(event, cx),
            TrayTab::Processes => self.on_tray_process_key_down(event, cx),
        }
    }

    fn on_tray_item_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control =
            keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;

        if keystroke.modifiers.secondary() && keystroke.key == "a" {
            self.tray.item_select_all = true;
            cx.notify();
            return;
        }

        match keystroke.key.as_str() {
            "escape" => return self.request_close(cx),
            "tab" => {
                self.tray.tab = TrayTab::Processes;
                self.tray.message = None;
                cx.notify();
                return;
            }
            "enter" => {
                if keystroke.modifiers.shift {
                    return self.secondary_activate_selected(cx);
                }
                return self.activate_selected(cx);
            }
            // Quit the selected application without opening its menu.
            "delete" => return self.quit_selected(cx),
            // Right opens the menu even for items that don't set ItemIsMenu.
            "right" => return self.open_selected_menu(cx),
            "up" => return self.move_item_selection(-1, cx),
            "down" => return self.move_item_selection(1, cx),
            "backspace" => {
                if self.tray.item_select_all {
                    self.tray.item_select_all = false;
                    self.tray.item_query.clear();
                    self.refilter_tray_items(cx);
                } else if self.tray.item_query.pop().is_some() {
                    self.refilter_tray_items(cx);
                }
                return;
            }
            _ => {}
        }

        if control {
            return;
        }
        if let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text.chars().any(char::is_control)
        {
            if self.tray.item_select_all {
                self.tray.item_select_all = false;
                self.tray.item_query.clear();
            }
            self.tray.item_query.push_str(text);
            self.refilter_tray_items(cx);
        }
    }

    fn on_tray_process_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control =
            keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;

        if keystroke.modifiers.secondary() && keystroke.key == "a" {
            self.tray.process_select_all = true;
            cx.notify();
            return;
        }

        match keystroke.key.as_str() {
            "escape" => {
                // Escape cancels an armed stop before it closes the tray.
                if self.tray.process_armed.take().is_some() {
                    self.tray.message = None;
                    cx.notify();
                    return;
                }
                return self.request_close(cx);
            }
            "tab" => {
                self.tray.tab = TrayTab::Items;
                self.tray.message = None;
                self.tray.process_armed = None;
                cx.notify();
                return;
            }
            "enter" => return self.stop_selected(keystroke.modifiers.shift, cx),
            // Delete is explicit enough to skip the arming press.
            "delete" => return self.stop_selected_now(keystroke.modifiers.shift, cx),
            "up" => return self.move_process_selection(-1, cx),
            "down" => return self.move_process_selection(1, cx),
            "backspace" => {
                if self.tray.process_select_all {
                    self.tray.process_select_all = false;
                    self.tray.process_query.clear();
                    self.refilter_tray_processes(cx);
                } else if self.tray.process_query.pop().is_some() {
                    self.refilter_tray_processes(cx);
                }
                return;
            }
            _ => {}
        }

        if control {
            return;
        }
        if let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text.chars().any(char::is_control)
        {
            if self.tray.process_select_all {
                self.tray.process_select_all = false;
                self.tray.process_query.clear();
            }
            self.tray.process_query.push_str(text);
            self.refilter_tray_processes(cx);
        }
    }

    fn on_tray_menu_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control =
            keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;

        if keystroke.modifiers.secondary() && keystroke.key == "a" {
            if let Some(menu) = self.tray.menu.as_mut() {
                menu.select_all = true;
            }
            cx.notify();
            return;
        }

        match keystroke.key.as_str() {
            "escape" | "left" => {
                self.tray.menu = None;
                self.tray.message = None;
                cx.notify();
                return;
            }
            "enter" => return self.invoke_selected_menu(cx),
            "up" | "down" => {
                let delta = if keystroke.key == "up" { -1 } else { 1 };
                if let Some(menu) = self.tray.menu.as_mut() {
                    menu.move_selection(delta);
                }
                cx.notify();
                return;
            }
            "backspace" => {
                if let Some(menu) = self.tray.menu.as_mut() {
                    if menu.select_all {
                        menu.select_all = false;
                        menu.query.clear();
                    } else if menu.query.pop().is_none() {
                        return;
                    }
                    menu.refilter();
                }
                cx.notify();
                return;
            }
            // Leave Tab to GPUI's focus navigation.
            "tab" => return,
            _ => {}
        }

        if control {
            return;
        }
        if let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text.chars().any(char::is_control)
            && let Some(menu) = self.tray.menu.as_mut()
        {
            if menu.select_all {
                menu.select_all = false;
                menu.query.clear();
            }
            menu.query.push_str(text);
            menu.refilter();
            cx.notify();
        }
    }

    fn refilter_tray_items(&mut self, cx: &mut Context<Self>) {
        self.tray.refilter_items();
        self.tray.message = None;
        cx.notify();
    }

    fn move_item_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.tray.item_results.len();
        if len == 0 {
            return;
        }

        let current = self.tray.item_selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.tray.item_selected = next;
        self.tray.item_scroll.scroll_to_item(next);
        cx.notify();
    }

    fn refilter_tray_processes(&mut self, cx: &mut Context<Self>) {
        self.tray.refilter_processes();
        self.tray.message = None;
        cx.notify();
    }

    fn move_process_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.tray.process_results.len();
        if len == 0 {
            return;
        }

        let current = self.tray.process_selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.tray.process_selected = next;
        self.tray.process_armed = None;
        self.tray.process_scroll.scroll_to_item(next);
        cx.notify();
    }

    /// Activates the selected item, or opens its menu when it asks to be a menu.
    fn activate_selected(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.tray.selected_item() else {
            return;
        };
        let menu_is_primary = item.item_is_menu && item.menu_path.is_some();

        if menu_is_primary {
            self.open_selected_menu(cx);
        } else {
            self.send_item_command(false, cx);
        }
    }

    fn secondary_activate_selected(&mut self, cx: &mut Context<Self>) {
        self.send_item_command(true, cx);
    }

    /// Asks the selected application to quit.
    ///
    /// The item's own menu action is used when it has one, so the application
    /// can save state and prompt; otherwise the process that owns the item's
    /// bus name receives SIGTERM. The item removes itself from the tray once
    /// its bus name loses its owner, so there is nothing to clean up here.
    fn quit_selected(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.tray.selected_item() else {
            return;
        };
        let service = item.service.clone();
        let menu_path = item.menu_path.clone();

        self.tray.message = Some(TrayMessage {
            text: "Quitting…".to_string(),
            error: false,
        });
        cx.notify();

        let reply = status_notifier::quit(&service, menu_path.as_deref());
        cx.spawn(async move |island, cx| {
            let result = reply.recv().await;
            island
                .update(cx, |island, cx| {
                    island.tray.message = Some(match result {
                        Ok(Ok(text)) => TrayMessage { text, error: false },
                        Ok(Err(err)) => TrayMessage {
                            text: format!("Couldn't quit: {err}"),
                            error: true,
                        },
                        Err(_) => TrayMessage {
                            text: "quit request was cancelled".to_string(),
                            error: true,
                        },
                    });
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Sends Activate or SecondaryActivate to the selected item.
    fn send_item_command(&mut self, secondary: bool, cx: &mut Context<Self>) {
        let Some(item) = self.tray.selected_item() else {
            return;
        };
        let (service, path, interface) = (
            item.service.clone(),
            item.path.clone(),
            item.interface.clone(),
        );

        let reply = if secondary {
            status_notifier::secondary_activate(&service, &path, &interface)
        } else {
            status_notifier::activate(&service, &path, &interface)
        };
        let action = if secondary {
            "secondary-activate"
        } else {
            "activate"
        };

        self.watch_item_command(reply, action, cx);
    }

    /// Loads the selected item's DBusMenu into a drill-down view.
    fn open_selected_menu(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.tray.selected_item() else {
            return;
        };
        let Some(menu_path) = item.menu_path.clone() else {
            return;
        };
        let (service, key, title) = (item.service.clone(), item.key.clone(), item.title.clone());

        self.tray.menu = Some(MenuView::new(key, title));
        self.tray.message = None;
        cx.notify();

        let reply = status_notifier::open_menu(&service, &menu_path);
        cx.spawn(async move |island, cx| {
            let result = reply.recv().await;
            island
                .update(cx, |island, cx| {
                    let Some(menu) = &mut island.tray.menu else {
                        return;
                    };
                    menu.loading = false;
                    match result {
                        Ok(Ok(entries)) => {
                            menu.entries = entries;
                            menu.refilter();
                        }
                        Ok(Err(err)) => menu.error = Some(err),
                        Err(_) => {
                            menu.error = Some("menu request was cancelled".to_string());
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Clicks the highlighted DBusMenu entry.
    fn invoke_selected_menu(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.tray.menu.as_ref() else {
            return;
        };
        let Some(entry) = menu.selected_entry() else {
            return;
        };
        if !entry.enabled {
            let text = format!("{} is disabled", entry.label);
            if let Some(menu) = &mut self.tray.menu {
                menu.message = Some(TrayMessage { text, error: false });
            }
            cx.notify();
            return;
        }

        let (id, label) = (entry.id, entry.label.clone());
        let Some(item) = self
            .tray
            .items
            .iter()
            .find(|item| item.key == menu.item_key)
        else {
            return;
        };
        let Some(menu_path) = item.menu_path.clone() else {
            return;
        };
        let service = item.service.clone();

        let reply = status_notifier::invoke_menu(&service, &menu_path, id);
        cx.spawn(async move |island, cx| {
            let result = reply.recv().await;
            island
                .update(cx, |island, cx| {
                    let Some(menu) = &mut island.tray.menu else {
                        return;
                    };
                    menu.message = Some(match result {
                        Ok(Ok(())) => TrayMessage {
                            text: format!("Selected {label}"),
                            error: false,
                        },
                        Ok(Err(err)) => TrayMessage {
                            text: format!("Couldn't select {label}: {err}"),
                            error: true,
                        },
                        Err(_) => TrayMessage {
                            text: "menu request was cancelled".to_string(),
                            error: true,
                        },
                    });
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Reports a failed item command in the tray footer.
    fn watch_item_command<T: Send + 'static>(
        &self,
        reply: async_channel::Receiver<Result<T, String>>,
        action: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |island, cx| {
            let Ok(result) = reply.recv().await else {
                return;
            };
            if let Err(err) = result {
                island
                    .update(cx, |island, cx| {
                        island.tray.message = Some(TrayMessage {
                            text: format!("Couldn't {action} item: {err}"),
                            error: true,
                        });
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Stops the highlighted process.
    ///
    /// The first press only arms the row; a second press sends the signal. This
    /// makes a stray Enter harmless while keeping the common case to two keys.
    /// Shift forces the kill (SIGKILL) instead of the polite stop (SIGTERM).
    fn stop_selected(&mut self, force: bool, cx: &mut Context<Self>) {
        self.stop_selected_with(force, true, cx);
    }

    /// Stops the highlighted process without arming it first, for Delete.
    fn stop_selected_now(&mut self, force: bool, cx: &mut Context<Self>) {
        self.stop_selected_with(force, false, cx);
    }

    fn stop_selected_with(&mut self, force: bool, confirm: bool, cx: &mut Context<Self>) {
        let Some(process) = self.tray.selected_process() else {
            return;
        };
        let (pid, start_time, name) = (process.pid, process.start_time, process.name.clone());

        if confirm && self.tray.process_armed != Some(pid) {
            self.tray.process_armed = Some(pid);
            self.tray.message = None;
            cx.notify();
            return;
        }

        let outcome = if force {
            crate::process::force_stop(pid, start_time)
        } else {
            crate::process::stop(pid, start_time)
        };

        self.tray.process_armed = None;
        match outcome {
            Ok(()) => {
                let signal = if force { "SIGKILL" } else { "SIGTERM" };
                eprintln!("gpui-shell: sent {signal} to {name} ({pid})");
                self.tray.message = Some(TrayMessage {
                    text: format!("Sent {signal} to {name}"),
                    error: false,
                });
            }
            Err(err) => {
                eprintln!("gpui-shell: failed to stop {name} ({pid}): {err}");
                self.tray.message = Some(TrayMessage {
                    text: format!("Couldn't stop {name}: {err}"),
                    error: true,
                });
            }
        }
        cx.notify();
        self.refresh_tray_soon(cx);
    }

    /// Rescans shortly after a signal so a prompt exit disappears from the list
    /// without waiting for the periodic refresh.
    fn refresh_tray_soon(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |island, cx| {
            cx.background_executor().timer(TRAY_REFRESH_DELAY).await;
            let processes = cx
                .background_executor()
                .spawn(async { Processes::scan() })
                .await;

            island
                .update(cx, |island, cx| {
                    if cx.global::<Settings>().mode == IslandMode::Tray {
                        island.tray.refresh_processes(processes);
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Keeps status items and the process fallback fresh while the overlay
    /// surface exists.
    fn start_tray_refresh(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |island, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;

                // The window is reused for the launcher, so only refresh while
                // the tray is actually showing. A failed read means it was
                // removed.
                let Ok(tray_active) = island.read_with(cx, |_island, cx| {
                    cx.global::<Settings>().mode == IslandMode::Tray
                }) else {
                    break;
                };
                if !tray_active {
                    continue;
                }

                let Ok(tab) = island.read_with(cx, |island, _cx| island.tray.tab) else {
                    break;
                };
                let processes = if tab == TrayTab::Processes {
                    Some(
                        cx.background_executor()
                            .spawn(async { Processes::scan() })
                            .await,
                    )
                } else {
                    None
                };
                let snapshot = status_notifier::snapshot();

                if island
                    .update(cx, |island, cx| {
                        island.tray.refresh_items(snapshot);
                        if let Some(processes) = processes {
                            island.tray.refresh_processes(processes);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Shows the default sink's volume, reverting to the clock shortly after
    /// the last change.
    fn watch_volume(&self, cx: &mut Context<Self>) {
        let volume_rx = audio::subscribe();

        cx.spawn(async move |island, cx| {
            while let Ok(volume) = volume_rx.recv().await {
                let Ok(generation) = island.update(cx, |island, cx| {
                    island.indicator = NormalIndicator::Volume(volume);
                    island.generation = island.generation.wrapping_add(1);
                    cx.notify();
                    island.generation
                }) else {
                    break;
                };

                // Each change schedules its own revert; the generation check
                // makes sure only the newest one hides the volume.
                let scheduled = island.update(cx, |_island, cx| {
                    cx.spawn(async move |island, cx| {
                        cx.background_executor().timer(VOLUME_HOLD).await;
                        island
                            .update(cx, |island, cx| {
                                if island.generation == generation {
                                    island.indicator = NormalIndicator::Clock;
                                    cx.notify();
                                }
                            })
                            .ok();
                    })
                    .detach();
                });
                if scheduled.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_timer(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |island, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;

                // A window may be re-created (e.g. when keyboard interactivity
                // changes); stop ticking once this island's entity is gone.
                let updated = island.update(cx, |island, cx| {
                    island.time = Local::now().format("%H:%M").to_string();
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// The compact clock/volume view, with the status dot to its right.
    ///
    /// The clock is centered in whatever room the dot leaves, so the island
    /// grows to the right when a severity is active. The dot is only part of
    /// the clock/volume state; a notification summary replaces both, so it is
    /// hidden while a toast is on screen.
    fn render_normal(&self, cx: &Context<Self>) -> AnyElement {
        let (severity, text_color, toast) = {
            let settings = cx.global::<Settings>();
            (
                settings.severity,
                settings.current.text_color,
                settings.toast.as_ref().map(|toast| toast.summary.clone()),
            )
        };
        // Audio is transient feedback the user is actively producing, so it
        // wins over a toast just like it wins over the clock.
        let showing_toast = toast.is_some();
        let text = match &self.indicator {
            NormalIndicator::Clock => toast.unwrap_or_else(|| self.time.clone()),
            NormalIndicator::Volume(Volume::Muted) => "muted".to_string(),
            NormalIndicator::Volume(Volume::Percent(percent)) => format!("{percent}%"),
        };

        let center = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(text),
            );

        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h_full()
            .child(center);

        if !showing_toast && let Some(icon) = severity.icon() {
            row = row.child(status_dot(icon, text_color));
        }

        row.into_any_element()
    }

    /// Search field shared by the launcher and tray overlays.
    #[allow(clippy::too_many_arguments)]
    fn render_search_field(
        &self,
        id: &'static str,
        focus: &FocusHandle,
        query_text: &str,
        select_all: bool,
        placeholder: &str,
        text: Hsla,
        muted: Hsla,
        border: Hsla,
        selection: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_query = !query_text.is_empty();

        let query = div()
            .text_sm()
            .whitespace_nowrap()
            .text_color(if has_query { text } else { muted })
            .min_w_0()
            .overflow_hidden()
            .when(has_query && select_all, |query| {
                query.bg(selection).rounded_xs()
            })
            .child(if has_query {
                query_text.to_string()
            } else {
                placeholder.to_string()
            });

        let caret = div().flex_shrink_0().w(px(1.5)).h(px(16.0)).bg(text);

        // With a placeholder, put the caret before the text so it sits at the
        // start of the field instead of being pushed to the end of the hint.
        let field = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .min_w_0()
            .overflow_hidden();
        let field = if has_query {
            field.child(query).child(caret.ml(px(1.0)))
        } else {
            field.child(caret).child(query)
        };

        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(36.0))
            .px(px(10.0))
            .flex_shrink_0()
            .rounded_md()
            .border_1()
            .border_color(border)
            .track_focus(focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(field)
            .into_any_element()
    }

    /// The overview hub: every panel as a row in a vertical list.
    fn render_overview(
        &self,
        progress: f32,
        island: Appearance,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = tray_colors(progress, island);

        let rows = OverviewCommand::ALL
            .into_iter()
            .enumerate()
            .map(|(index, command)| {
                let selected = index == self.overview.selected;
                let label_color = if selected { colors.text } else { colors.muted };

                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .px(px(10.0))
                    .py(px(8.0))
                    .flex_shrink_0()
                    .rounded_md()
                    .when(selected, |row| row.bg(colors.selection))
                    .child(
                        // Icon and name form the left group; the hotkey badge is
                        // pushed to the far right by `justify_between`.
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_family(glyphs::FONT_FAMILY)
                                    .text_size(px(18.0))
                                    .line_height(px(20.0))
                                    .text_color(colors.text)
                                    .child(command.icon()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(label_color)
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .min_w_0()
                                    .child(command.label()),
                            ),
                    )
                    .child(
                        // Hotkey hint doubles as the shortcut, so the row needs
                        // no search field to be discoverable.
                        div()
                            .flex_shrink_0()
                            .px(px(6.0))
                            .rounded_xs()
                            .bg(colors.border)
                            .text_xs()
                            .text_color(label_color)
                            .child(command.initial().to_ascii_uppercase().to_string()),
                    )
                    .into_any_element()
            });

        let hint = "↑/↓ select · Enter open · or press L / T / N · esc close";

        let active: Vec<Glyph> = cx.global::<Settings>().glyphs.active().collect();

        div()
            .flex()
            .flex_col()
            .gap_2()
            .size_full()
            .p(px(10.0))
            .track_focus(&self.overview.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(div().text_xs().text_color(colors.muted).child("Overview"))
            .child(overview_status(active, colors))
            .child(
                div()
                    .id("overview-list")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .flex_1()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(tray_footer(None, hint, colors))
            .into_any_element()
    }

    /// The expanded search view.
    fn render_launcher(
        &self,
        progress: f32,
        island: Appearance,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Fade the whole view in and out with the resize so the content appears
        // in step with the growing island.
        let alpha = progress.clamp(0.0, 1.0);
        let text = island.text_color.opacity(alpha);
        let muted = island.text_color.opacity(alpha * 0.5);
        let border = island.text_color.opacity(alpha * 0.25);
        let selection = island.text_color.opacity(alpha * 0.15);

        let has_query = !self.launcher.query.is_empty();

        let input = self.render_search_field(
            "launcher-input",
            &self.launcher.focus,
            &self.launcher.query,
            self.launcher.select_all,
            "Search applications…",
            text,
            muted,
            border,
            selection,
            cx,
        );

        let catalog = cx.global::<Catalog>();

        let body: AnyElement =
            if let Some(error) = self.launcher.error.clone() {
                status_message(error, muted)
            } else if self.launcher.results.is_empty() {
                let message = if has_query {
                    "No matching applications".to_string()
                } else {
                    "No applications found".to_string()
                };
                status_message(message, muted)
            } else {
                let rows =
                    self.launcher
                        .results
                        .iter()
                        .enumerate()
                        .map(|(row, &index)| {
                            let Some(entry) = catalog.entry(index) else {
                                return div().into_any_element();
                            };
                            let selected = row == self.launcher.selected;

                            div()
                                .flex()
                                .flex_col()
                                .w_full()
                                .px(px(10.0))
                                .py(px(6.0))
                                .flex_shrink_0()
                                .rounded_md()
                                .when(selected, |row| row.bg(selection))
                                .child(div().text_sm().text_color(text).child(entry.name.clone()))
                                .children(entry.comment.clone().map(|comment| {
                                    div().text_xs().text_color(muted).child(comment)
                                }))
                                .into_any_element()
                        });

                div()
                    .id("launcher-results")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.launcher.scroll)
                    .children(rows)
                    .into_any_element()
            };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .size_full()
            .p(px(10.0))
            .child(input)
            .child(body)
            .into_any_element()
    }

    /// The notification center: one card per stored notification.
    fn render_notifications(
        &self,
        progress: f32,
        island: Appearance,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = tray_colors(progress, island);

        let input = self.render_search_field(
            "notifications-input",
            &self.notifications.focus,
            &self.notifications.query,
            self.notifications.select_all,
            "Filter notifications…",
            colors.text,
            colors.muted,
            colors.border,
            colors.selection,
            cx,
        );

        let body: AnyElement = if self.notifications.list.is_empty() {
            if let Some(error) = &self.notifications.error {
                status_message(format!("Notifications unavailable: {error}"), colors.danger)
            } else {
                status_message("No unread notifications".to_string(), colors.muted)
            }
        } else if self.notifications.results.is_empty() {
            status_message("No matching notifications".to_string(), colors.muted)
        } else {
            let now = Local::now();
            let rows = self
                .notifications
                .results
                .iter()
                .enumerate()
                .map(|(row, &index)| {
                    let Some(notification) = self.notifications.list.get(index) else {
                        return div().into_any_element();
                    };
                    let selected = row == self.notifications.selected;
                    let urgency_color = match notification.urgency {
                        Urgency::Low => colors.muted,
                        Urgency::Normal => colors.text,
                        Urgency::Critical => colors.danger,
                    };

                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap_2()
                        .w_full()
                        .px(px(10.0))
                        .py(px(8.0))
                        .flex_shrink_0()
                        .rounded_md()
                        .when(selected, |row| row.bg(colors.selection))
                        .when(notification.urgency == Urgency::Critical, |row| {
                            row.border_1().border_color(colors.danger)
                        })
                        .child(
                            div()
                                .flex_shrink_0()
                                .mt(px(4.0))
                                .w(px(6.0))
                                .h(px(6.0))
                                .rounded_full()
                                .bg(urgency_color),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .w_full()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(colors.muted)
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .flex_1()
                                                .min_w_0()
                                                .child(notification.app_name.clone()),
                                        )
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .text_xs()
                                                .text_color(colors.muted)
                                                .child(relative_time(now, notification.received)),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(colors.text)
                                        .child(notification.summary.clone()),
                                )
                                .children((!notification.body.is_empty()).then(|| {
                                    div()
                                        .text_xs()
                                        .text_color(colors.muted)
                                        .child(notification.body.clone())
                                })),
                        )
                        .into_any_element()
                });

            div()
                .id("notifications")
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.notifications.scroll)
                .children(rows)
                .into_any_element()
        };

        let hint = "Type to filter · Enter dismiss · ctrl+d dismiss all · esc close";
        let footer = tray_footer(None, hint, colors);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .size_full()
            .p(px(10.0))
            .child(input)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    /// The expanded tray view.
    fn render_tray(&self, progress: f32, island: Appearance, cx: &mut Context<Self>) -> AnyElement {
        if let Some(menu) = &self.tray.menu {
            return self.render_tray_menu(progress, island, menu, cx);
        }

        match self.tray.tab {
            TrayTab::Items => self.render_tray_items(progress, island, cx),
            TrayTab::Processes => self.render_tray_processes(progress, island, cx),
        }
    }

    /// Registered StatusNotifierItems.
    fn render_tray_items(
        &self,
        progress: f32,
        island: Appearance,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = tray_colors(progress, island);
        let input = self.render_search_field(
            "tray-items-input",
            &self.tray.focus,
            &self.tray.item_query,
            self.tray.item_select_all,
            "Search status items…",
            colors.text,
            colors.muted,
            colors.border,
            colors.selection,
            cx,
        );

        let body: AnyElement = if self.tray.items.is_empty() {
            if let Some(error) = &self.tray.service_error {
                status_message(format!("Tray unavailable: {error}"), colors.danger)
            } else {
                status_message(
                    "No status notifier items registered".to_string(),
                    colors.muted,
                )
            }
        } else if self.tray.item_results.is_empty() {
            status_message("No matching status items".to_string(), colors.muted)
        } else {
            let rows = self
                .tray
                .item_results
                .iter()
                .enumerate()
                .map(|(row, &index)| {
                    let Some(item) = self.tray.items.get(index) else {
                        return div().into_any_element();
                    };
                    let selected = row == self.tray.item_selected;
                    let status_color = match item.status {
                        status_notifier::ItemStatus::Passive => colors.muted,
                        status_notifier::ItemStatus::Active => colors.text,
                        status_notifier::ItemStatus::NeedsAttention => colors.danger,
                    };
                    let has_menu = item.menu_path.is_some();
                    let subtitle = item.tooltip.clone().or_else(|| {
                        let mut parts = Vec::new();
                        if !item.category.is_empty() {
                            parts.push(item.category.clone());
                        }
                        if let Some(icon) = &item.icon_name {
                            parts.push(icon.clone());
                        }
                        (!parts.is_empty()).then(|| parts.join(" · "))
                    });

                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .px(px(10.0))
                        .py(px(6.0))
                        .flex_shrink_0()
                        .rounded_md()
                        .when(selected, |row| row.bg(colors.selection))
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(6.0))
                                .h(px(6.0))
                                .rounded_full()
                                .bg(status_color),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .w_full()
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(status_color)
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .flex_1()
                                                .min_w_0()
                                                .child(item.title.clone()),
                                        )
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .text_xs()
                                                .text_color(colors.muted)
                                                .child(item.id.clone()),
                                        ),
                                )
                                .children(subtitle.map(|text| {
                                    div()
                                        .w_full()
                                        .text_xs()
                                        .text_color(colors.muted)
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(text)
                                })),
                        )
                        .when(has_menu, |row| {
                            row.child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .text_color(colors.muted)
                                    .child("›"),
                            )
                        })
                        .into_any_element()
                });

            div()
                .id("tray-items")
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.tray.item_scroll)
                .children(rows)
                .into_any_element()
        };

        let hint = match self.tray.selected_item() {
            Some(item) if item.item_is_menu && item.menu_path.is_some() => {
                "Enter menu · Del quit · → opens menu · shift+enter secondary · tab processes · esc close"
            }
            Some(item) if item.menu_path.is_some() => {
                "Enter activate · Del quit · → menu · shift+enter secondary · tab processes · esc close"
            }
            Some(_) => {
                "Enter activate · Del quit · shift+enter secondary · tab processes · esc close"
            }
            None => "tab processes · esc close",
        };
        let footer = tray_footer(self.tray.message.as_ref(), hint, colors);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .size_full()
            .p(px(10.0))
            .child(input)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    /// Fallback process list used when an application has no tray item.
    fn render_tray_processes(
        &self,
        progress: f32,
        island: Appearance,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = tray_colors(progress, island);
        let input = self.render_search_field(
            "tray-processes-input",
            &self.tray.focus,
            &self.tray.process_query,
            self.tray.process_select_all,
            "Search processes…",
            colors.text,
            colors.muted,
            colors.border,
            colors.selection,
            cx,
        );

        let body: AnyElement = if self.tray.process_results.is_empty() {
            let message = if self.tray.process_query.is_empty() {
                "No running processes".to_string()
            } else {
                "No matching processes".to_string()
            };
            status_message(message, colors.muted)
        } else {
            let rows = self
                .tray
                .process_results
                .iter()
                .enumerate()
                .map(|(row, &index)| {
                    let Some(process) = self.tray.processes.entry(index) else {
                        return div().into_any_element();
                    };
                    let selected = row == self.tray.process_selected;
                    let armed = self.tray.process_armed == Some(process.pid);
                    let name_color = if armed { colors.danger } else { colors.text };

                    div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .px(px(10.0))
                        .py(px(6.0))
                        .flex_shrink_0()
                        .rounded_md()
                        .when(selected, |row| row.bg(colors.selection))
                        .when(armed, |row| row.border_1().border_color(colors.danger))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .w_full()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(name_color)
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .flex_1()
                                        .min_w_0()
                                        .child(process.name.clone()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(colors.muted)
                                        .child(process.pid.to_string()),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.muted)
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(process.command.clone()),
                        )
                        .into_any_element()
                });

            div()
                .id("tray-processes")
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.tray.process_scroll)
                .children(rows)
                .into_any_element()
        };

        let hint = if self.tray.process_armed.is_some() {
            "Press enter again to stop · shift+enter to force · esc cancels"
        } else {
            "Enter arms stop · shift+enter force · Del stops now · tab items · esc close"
        };
        let footer = tray_footer(self.tray.message.as_ref(), hint, colors);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .size_full()
            .p(px(10.0))
            .child(input)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    /// An item's DBusMenu, rendered as a drill-down list.
    fn render_tray_menu(
        &self,
        progress: f32,
        island: Appearance,
        menu: &MenuView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = tray_colors(progress, island);
        let placeholder = format!("Search {} menu…", menu.title);
        let input = self.render_search_field(
            "tray-menu-input",
            &self.tray.focus,
            &menu.query,
            menu.select_all,
            &placeholder,
            colors.text,
            colors.muted,
            colors.border,
            colors.selection,
            cx,
        );

        let body: AnyElement = if menu.loading {
            status_message("Loading menu…".to_string(), colors.muted)
        } else if let Some(error) = &menu.error {
            status_message(error.clone(), colors.danger)
        } else if menu.results.is_empty() {
            let message = if menu.query.is_empty() {
                "This menu has no actions"
            } else {
                "No matching menu entries"
            };
            status_message(message.to_string(), colors.muted)
        } else {
            let rows = menu.results.iter().enumerate().map(|(row, &index)| {
                let Some(entry) = menu.entries.get(index) else {
                    return div().into_any_element();
                };
                let selected = row == menu.selected;

                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .w_full()
                    .pl(px(10.0 + entry.depth as f32 * 12.0))
                    .pr(px(10.0))
                    .py(px(6.0))
                    .flex_shrink_0()
                    .rounded_md()
                    .when(selected, |row| row.bg(colors.selection))
                    .child(
                        div()
                            .text_sm()
                            .text_color(if entry.enabled {
                                colors.text
                            } else {
                                colors.muted
                            })
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .flex_1()
                            .min_w_0()
                            .child(entry.label.clone()),
                    )
                    .into_any_element()
            });

            div()
                .id("tray-menu")
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&menu.scroll)
                .children(rows)
                .into_any_element()
        };

        let hint = if menu.loading {
            "Loading menu…"
        } else {
            "Enter selects · ← back · esc back"
        };
        let footer = tray_footer(menu.message.as_ref(), hint, colors);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .size_full()
            .p(px(10.0))
            .child(input)
            .child(body)
            .child(footer)
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
struct TrayColors {
    text: Hsla,
    muted: Hsla,
    border: Hsla,
    selection: Hsla,
    danger: Hsla,
    warning: Hsla,
}

fn tray_colors(progress: f32, island: Appearance) -> TrayColors {
    let alpha = progress.clamp(0.0, 1.0);
    TrayColors {
        text: island.text_color.opacity(alpha),
        muted: island.text_color.opacity(alpha * 0.5),
        border: island.text_color.opacity(alpha * 0.25),
        selection: island.text_color.opacity(alpha * 0.15),
        danger: gpui::red().opacity(alpha),
        warning: gpui::yellow().opacity(alpha),
    }
}

/// The detailed conditions shown on the overview, or a calm note when none
/// are active.
///
/// This is the place to see *what* is wrong, while the compact dot only says
/// how urgent it is.
fn overview_status(active: Vec<Glyph>, colors: TrayColors) -> AnyElement {
    if active.is_empty() {
        return div()
            .flex_shrink_0()
            .text_xs()
            .text_color(colors.muted)
            .child("All clear")
            .into_any_element();
    }

    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_x(px(12.0))
        .gap_y(px(4.0))
        .flex_shrink_0()
        .children(active.into_iter().map(|glyph| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .font_family(glyphs::FONT_FAMILY)
                        .text_size(px(14.0))
                        .line_height(px(14.0))
                        .text_color(glyph_tint(glyph, colors))
                        .child(glyph.icon()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted)
                        .whitespace_nowrap()
                        .child(glyph.label()),
                )
                .into_any_element()
        }))
        .into_any_element()
}

/// Colours the detailed glyphs so urgent conditions stand out from informative
/// ones at a glance.
fn glyph_tint(glyph: Glyph, colors: TrayColors) -> Hsla {
    match glyph {
        Glyph::BatteryLow => colors.danger,
        Glyph::WifiDown => colors.warning,
        Glyph::BluetoothOff | Glyph::Unread | Glyph::Dnd => colors.text,
    }
}

fn tray_footer(message: Option<&TrayMessage>, hint: &str, colors: TrayColors) -> AnyElement {
    let (text, error) = match message {
        Some(message) => (message.text.clone(), message.error),
        None => (hint.to_string(), false),
    };

    div()
        .flex_shrink_0()
        .w_full()
        .text_xs()
        .text_color(if error { colors.danger } else { colors.muted })
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text)
        .into_any_element()
}

fn status_message(message: String, color: Hsla) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_1()
        .w_full()
        .text_sm()
        .text_color(color)
        .child(message)
        .into_any_element()
}

/// Compact age label shown on a notification card.
fn relative_time(now: DateTime<Local>, received: DateTime<Local>) -> String {
    let seconds = now.signed_duration_since(received).num_seconds().max(0);
    match seconds {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// The single severity dot drawn to the right of the clock.
///
/// The slot is narrow and fully occupied by the icon so the clock keeps its
/// visual weight; `severity.slot_width()` reserves exactly this much room. The
/// dot uses the Nerd Font family so the private-use codepoint resolves even
/// when the body face is a plain monospace build.
fn status_dot(icon: &'static str, color: Hsla) -> AnyElement {
    div()
        .w(px(Severity::SLOT_WIDTH))
        .h_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .font_family(glyphs::FONT_FAMILY)
        .text_size(px(10.0))
        .line_height(px(10.0))
        .text_color(color)
        .child(icon)
        .into_any_element()
}

/// How far the island has travelled from the normal size towards the expanded
/// overlay size, in `0..=1`.
fn overlay_progress(settings: &Settings) -> f32 {
    let normal = f32::from(settings.normal_size_with_status().height);
    let expanded = f32::from(settings.overlay_size.height);
    let current = f32::from(settings.current.size.height);

    if expanded <= normal {
        return 0.0;
    }

    ((current - normal) / (expanded - normal)).clamp(0.0, 1.0)
}

/// Whether this surface belongs to the output niri reports as fullscreen.
///
/// niri names outputs by their connector name, which the Wayland backend also
/// uses as the basis of [`gpui::PlatformDisplay::uuid`].
fn display_matches_output(cx: &App, display_id: Option<DisplayId>, output: &str) -> bool {
    let Some(display_id) = display_id else {
        return false;
    };

    let expected = Uuid::new_v5(&Uuid::NAMESPACE_DNS, output.as_bytes());
    cx.displays().into_iter().any(|display| {
        display.id() == display_id && display.uuid().is_ok_and(|uuid| uuid == expected)
    })
}

impl Render for Island {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Advance the shared animation before building the tree so this frame
        // draws the new position. While it is in flight, request the next frame
        // from the compositor; this paces the animation on frame callbacks
        // instead of a fixed timer, keeping it in step with presentation.
        let target = cx.global::<Settings>().target;
        let (animated, reveal, settled) = cx.global_mut::<Animation>().tick(target, Instant::now());
        cx.global_mut::<Settings>().current = animated;

        if settled {
            // Surface bookkeeping must not mutate windows while this one is
            // being drawn, so hand it to the next effect cycle.
            cx.defer(crate::server::finish_animation);
        } else if cx.global::<Animation>().running() {
            window.request_animation_frame();
        }

        let (
            window_size,
            island,
            mode,
            overlay_mode,
            normal_size,
            progress,
            overlay_engaged,
            fullscreen_output,
        ) = {
            let settings = cx.global::<Settings>();
            (
                settings.window_size,
                settings.current,
                settings.mode,
                settings.overlay_mode,
                settings.normal_size_with_status(),
                overlay_progress(settings),
                settings.overlay_engaged,
                settings.fullscreen_output.clone(),
            )
        };

        // `true` from the moment an overlay starts opening until the island has
        // contracted back past the reveal edge.
        let overlay_active = mode != IslandMode::Normal || progress > REVEAL_EDGE;

        // Only the transient overlay surface is interactive, and only while a
        // mode with a search field is open.
        if self.role == IslandRole::Overlay {
            let focus = match mode {
                IslandMode::Overview => Some(self.overview.focus.clone()),
                IslandMode::Launcher => Some(self.launcher.focus.clone()),
                IslandMode::Tray => Some(self.tray.focus.clone()),
                IslandMode::Notifications => Some(self.notifications.focus.clone()),
                IslandMode::Normal | IslandMode::Calendar => None,
            };
            if let Some(focus) = focus
                && !focus.is_focused(window)
            {
                window.defer(cx, move |window, cx| focus.focus(window, cx));
            }
        }

        // Bar surfaces stay compact and keep showing the clock while another
        // display hosts an overlay. The overlay surface follows the animated
        // size and swaps back to the clock at the end of the closing animation.
        let (surface_size, box_size, content): (_, _, AnyElement) = match self.role {
            IslandRole::Bar => {
                // Bars never follow an overlay resize. They stay compact until
                // the closing animation has fully settled and the surface is
                // handed back, otherwise they would briefly jump to the
                // expanded surface size as the overlay shrinks past the reveal
                // edge.
                let (surface, box_size) = if overlay_engaged {
                    (normal_size, normal_size)
                } else {
                    (window_size, island.size)
                };
                // Fullscreen slides the bar up against the top edge by
                // collapsing its animated height. Only the bar on the output
                // hosting the fullscreen window is affected; other displays
                // keep their clock. The surface keeps its normal size so it
                // can be reused when the overlay toggles.
                let bar_reveal = if fullscreen_output
                    .as_deref()
                    .is_some_and(|output| display_matches_output(cx, self.display_id, output))
                {
                    reveal
                } else {
                    1.0
                };
                let box_size = gpui::Size::new(box_size.width, box_size.height * bar_reveal);
                (surface, box_size, self.render_normal(cx))
            }
            IslandRole::Overlay => {
                // While closing, `mode` is already `Normal`; keep drawing the
                // overlay that is animating away until it contracts.
                let content_mode = if mode == IslandMode::Normal {
                    overlay_mode
                } else {
                    mode
                };
                let content = if overlay_active {
                    match content_mode {
                        IslandMode::Overview => self.render_overview(progress, island, cx),
                        IslandMode::Launcher => self.render_launcher(progress, island, cx),
                        IslandMode::Tray => self.render_tray(progress, island, cx),
                        IslandMode::Notifications => {
                            self.render_notifications(progress, island, cx)
                        }
                        IslandMode::Normal | IslandMode::Calendar => self.render_normal(cx),
                    }
                } else {
                    self.render_normal(cx)
                };
                (window_size, island.size, content)
            }
        };

        // The surface is transparent around the island, so we fill it with a
        // transparent layout and draw the (animated) island inside it, centered
        // horizontally and pinned to the top.
        div()
            .w(surface_size.width)
            .h(surface_size.height)
            .flex()
            .justify_center()
            .items_start()
            .child(
                div()
                    .w(box_size.width)
                    .h(box_size.height)
                    .bg(island.background)
                    .text_color(island.text_color)
                    .text_size(px(NORMAL_TEXT_SIZE))
                    .rounded_b_lg()
                    .font_family(TEXT_FONT_FAMILY)
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(content),
            )
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    #[test]
    fn overview_commands_are_reachable_by_their_initial() {
        assert_eq!(
            OverviewCommand::from_initial('l'),
            Some(OverviewCommand::Launcher)
        );
        assert_eq!(
            OverviewCommand::from_initial('L'),
            Some(OverviewCommand::Launcher)
        );
        assert_eq!(
            OverviewCommand::from_initial('t'),
            Some(OverviewCommand::Tray)
        );
        assert_eq!(
            OverviewCommand::from_initial('n'),
            Some(OverviewCommand::Notifications)
        );
        assert_eq!(OverviewCommand::from_initial('x'), None);
    }

    #[test]
    fn filter_matches_app_summary_and_body() {
        let list = vec![
            notification("Mail", "You have mail", ""),
            notification("Chat", "Hello", "from Alice"),
            notification("System", "Battery low", ""),
        ];

        assert_eq!(filter_notifications(&list, ""), vec![0, 1, 2]);
        assert_eq!(filter_notifications(&list, "MAIL"), vec![0]);
        assert_eq!(filter_notifications(&list, "alice"), vec![1]);
        assert_eq!(filter_notifications(&list, "battery"), vec![2]);
        assert!(filter_notifications(&list, "missing").is_empty());
    }

    #[test]
    fn relative_time_uses_compact_units() {
        let now = Local.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap();

        assert_eq!(relative_time(now, now), "now");
        assert_eq!(relative_time(now, now - Duration::seconds(30)), "now");
        assert_eq!(relative_time(now, now - Duration::seconds(60)), "1m");
        assert_eq!(relative_time(now, now - Duration::seconds(3600)), "1h");
        assert_eq!(relative_time(now, now - Duration::seconds(86400)), "1d");
        // Future timestamps clamp instead of producing negative output.
        assert_eq!(relative_time(now, now + Duration::seconds(60)), "now");
    }

    fn notification(app_name: &str, summary: &str, body: &str) -> Notification {
        Notification {
            id: 0,
            app_name: app_name.to_string(),
            app_icon: None,
            summary: summary.to_string(),
            body: body.to_string(),
            urgency: Urgency::Normal,
            received: Local::now(),
        }
    }
}

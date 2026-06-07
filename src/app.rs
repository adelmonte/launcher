use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};

use crate::desktop::{self, AppEntry};
use crate::icon::create_icon;
use crate::lockfile;

const CSS: &str = r#"
    window.app-launcher {
        background-color: alpha(@theme_bg_color, 0.8);
    }
    window.app-launcher box,
    window.app-launcher scrolledwindow,
    window.app-launcher viewport,
    window.app-launcher list {
        background-color: transparent;
    }
    window.app-launcher label {
        color: white;
        text-shadow:
            0px 1px 2px rgba(0, 0, 0, 0.9),
            0px 1px 4px rgba(0, 0, 0, 0.6);
    }
    window.app-launcher scrollbar {
        opacity: 0;
    }
    .menu-separator {
        background: rgba(255, 255, 255, 0.2);
        min-height: 1px;
        margin: 2px 0;
    }
    list row:selected {
        background-color: alpha(@theme_selected_bg_color, 0.6);
    }
    list row:hover {
        background-color: alpha(@theme_selected_bg_color, 0.0);
    }
    list row:selected:hover {
        background-color: alpha(@theme_selected_bg_color, 0.7);
    }
"#;

#[derive(Clone, Copy, PartialEq)]
enum ActiveView {
    One,
    Two,
}

#[derive(Clone)]
enum View {
    Favorites,
    Categories,
    Category(String),
}

#[derive(Clone)]
enum RowMeta {
    App { idx: usize, draggable: bool },
    Category { name: String },
}

#[derive(Clone, Copy)]
enum NavAction {
    ShowCategories,
    GoBack,
}

const META_KEY: &str = "launcher-meta";

fn set_row_meta(row: &gtk::ListBoxRow, meta: RowMeta) {
    unsafe {
        row.set_data(META_KEY, meta);
    }
}

fn row_meta(row: &gtk::ListBoxRow) -> Option<RowMeta> {
    unsafe { row.data::<RowMeta>(META_KEY).map(|p| p.as_ref().clone()) }
}

fn favorites_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".config")
        .join("launcher-favorites.json")
}

fn load_favorites() -> Vec<String> {
    let path = favorites_path();
    if path.exists() {
        if let Ok(s) = fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Vec<String>>(&s) {
                return v;
            }
        }
        return Vec::new();
    }
    Vec::new()
}

fn category_icon(name: &str) -> &'static str {
    match name {
        "Multimedia" => "applications-multimedia",
        "Development" => "applications-development",
        "Education" => "applications-science",
        "Games" => "applications-games",
        "Graphics" => "applications-graphics",
        "Internet" => "applications-internet",
        "Office" => "applications-office",
        "Science" => "applications-science",
        "Settings" => "preferences-system",
        "System Tools" => "applications-system",
        "Accessories" => "applications-accessories",
        "Other" => "applications-other",
        _ => "folder",
    }
}

fn configure_detached(cmd: &mut Command) {
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

pub struct App {
    win: gtk::Window,
    content_stack: gtk::Stack,
    listbox_1: gtk::ListBox,
    listbox_2: gtk::ListBox,
    nav_button_box: gtk::Box,
    search_entry: gtk::Entry,

    all_apps: RefCell<Vec<AppEntry>>,
    by_id: RefCell<HashMap<String, usize>>,
    categories: RefCell<BTreeMap<String, Vec<usize>>>,
    favorites: RefCell<Vec<String>>,
    view_stack: RefCell<Vec<View>>,

    current: Cell<ActiveView>,
    dragging: Cell<bool>,
    drag_row: RefCell<Option<gtk::ListBoxRow>>,
    last_hovered_row: RefCell<Option<gtk::ListBoxRow>>,
    is_animating: Cell<bool>,
    visible: Cell<bool>,
    focus_out_timeout: RefCell<Option<glib::SourceId>>,
    reload_pending: Cell<bool>,
    monitors: RefCell<Vec<gio::FileMonitor>>,

    row_activated_1: RefCell<Option<glib::SignalHandlerId>>,
    row_activated_2: RefCell<Option<glib::SignalHandlerId>>,
    search_changed_id: RefCell<Option<glib::SignalHandlerId>>,

    icon_cache: RefCell<HashMap<String, gdk::cairo::Surface>>,
}

impl App {
    pub fn new(start_hidden: bool) -> Rc<App> {
        let win = gtk::Window::new(gtk::WindowType::Toplevel);
        win.set_title("Applications");
        win.set_role("launcher");
        win.set_default_size(250, 400);
        win.set_decorated(false);
        win.set_type_hint(gdk::WindowTypeHint::Dialog);
        win.set_skip_taskbar_hint(true);
        win.set_skip_pager_hint(true);
        if let Some(screen) = gtk::prelude::WidgetExt::screen(&win) {
            if let Some(visual) = screen.rgba_visual() {
                win.set_visual(Some(&visual));
            }
        }
        win.style_context().add_class("app-launcher");

        // --- build UI tree ---
        let main_vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        win.add(&main_vbox);

        let content_stack = gtk::Stack::new();
        content_stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
        content_stack.set_transition_duration(250);

        let scrolled_1 = gtk::ScrolledWindow::builder().build();
        scrolled_1.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let listbox_1 = gtk::ListBox::new();
        listbox_1.set_selection_mode(gtk::SelectionMode::Single);
        scrolled_1.add(&listbox_1);

        let scrolled_2 = gtk::ScrolledWindow::builder().build();
        scrolled_2.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let listbox_2 = gtk::ListBox::new();
        listbox_2.set_selection_mode(gtk::SelectionMode::Single);
        scrolled_2.add(&listbox_2);

        content_stack.add_named(&scrolled_1, "view1");
        content_stack.add_named(&scrolled_2, "view2");
        main_vbox.pack_start(&content_stack, true, true, 0);

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.style_context().add_class("menu-separator");
        main_vbox.pack_start(&separator, false, false, 0);

        let nav_button_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        main_vbox.pack_start(&nav_button_box, false, false, 0);

        let separator2 = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator2.style_context().add_class("menu-separator");
        main_vbox.pack_start(&separator2, false, false, 0);

        let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        search_box.set_margin_start(5);
        search_box.set_margin_end(5);
        search_box.set_margin_top(5);
        search_box.set_margin_bottom(5);

        let search_entry = gtk::Entry::new();
        search_entry.set_placeholder_text(Some("🔍 Search"));
        search_box.pack_start(&search_entry, true, true, 0);
        main_vbox.pack_start(&search_box, false, false, 0);

        let app = Rc::new(App {
            win,
            content_stack,
            listbox_1,
            listbox_2,
            nav_button_box,
            search_entry,
            all_apps: RefCell::new(Vec::new()),
            by_id: RefCell::new(HashMap::new()),
            categories: RefCell::new(BTreeMap::new()),
            favorites: RefCell::new(load_favorites()),
            view_stack: RefCell::new(Vec::new()),
            current: Cell::new(ActiveView::One),
            dragging: Cell::new(false),
            drag_row: RefCell::new(None),
            last_hovered_row: RefCell::new(None),
            is_animating: Cell::new(false),
            visible: Cell::new(false),
            focus_out_timeout: RefCell::new(None),
            reload_pending: Cell::new(false),
            monitors: RefCell::new(Vec::new()),
            row_activated_1: RefCell::new(None),
            row_activated_2: RefCell::new(None),
            search_changed_id: RefCell::new(None),
            icon_cache: RefCell::new(HashMap::new()),
        });

        app.apply_css();
        app.connect_signals();

        *app.all_apps.borrow_mut() = desktop::load_applications();
        app.rebuild_by_id();
        let cats = desktop::organize_by_category(&app.all_apps.borrow());
        *app.categories.borrow_mut() = cats;

        app.show_favorites_view(false, "back");
        app.setup_app_monitors();
        app.connect_window_signals();

        if !start_hidden {
            app.win.show_all();
            app.visible.set(true);
            app.current_listbox().grab_focus();
            app.signal_waybar();
        }

        app
    }

    fn current_listbox(&self) -> gtk::ListBox {
        match self.current.get() {
            ActiveView::One => self.listbox_1.clone(),
            ActiveView::Two => self.listbox_2.clone(),
        }
    }

    fn apply_css(&self) {
        let provider = gtk::CssProvider::new();
        let _ = provider.load_from_data(CSS.as_bytes());
        if let Some(screen) = gdk::Screen::default() {
            gtk::StyleContext::add_provider_for_screen(
                &screen,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    }

    fn connect_signals(self: &Rc<Self>) {
        for listbox in [&self.listbox_1, &self.listbox_2] {
            listbox.add_events(
                gdk::EventMask::POINTER_MOTION_MASK | gdk::EventMask::LEAVE_NOTIFY_MASK,
            );
        }

        {
            let app = self.clone();
            let id = self
                .listbox_1
                .connect_row_activated(move |_lb, row| app.on_row_activated(row));
            *self.row_activated_1.borrow_mut() = Some(id);
        }
        {
            let app = self.clone();
            let id = self
                .listbox_2
                .connect_row_activated(move |_lb, row| app.on_row_activated(row));
            *self.row_activated_2.borrow_mut() = Some(id);
        }
        {
            let app = self.clone();
            self.listbox_1
                .connect_motion_notify_event(move |_lb, ev| app.on_listbox_motion(ev));
        }
        {
            let app = self.clone();
            self.listbox_2
                .connect_motion_notify_event(move |_lb, ev| app.on_listbox_motion(ev));
        }
        {
            let app = self.clone();
            self.listbox_1
                .connect_leave_notify_event(move |_lb, _ev| app.on_listbox_leave());
        }
        {
            let app = self.clone();
            self.listbox_2
                .connect_leave_notify_event(move |_lb, _ev| app.on_listbox_leave());
        }

        {
            let app = self.clone();
            let id = self.search_entry.connect_changed(move |_| app.on_search_changed());
            *self.search_changed_id.borrow_mut() = Some(id);
        }
        {
            let app = self.clone();
            self.search_entry
                .connect_activate(move |_| app.on_search_activate());
        }
        {
            let app = self.clone();
            self.search_entry
                .connect_key_press_event(move |_, ev| app.on_search_entry_key_press(ev));
        }
    }

    fn connect_window_signals(self: &Rc<Self>) {
        {
            let app = self.clone();
            self.win.connect_focus_out_event(move |_, _| app.on_focus_out());
        }
        {
            let app = self.clone();
            self.win.connect_focus_in_event(move |_, _| app.on_focus_in());
        }
        {
            let app = self.clone();
            self.win.connect_key_press_event(move |_, ev| app.on_key_press(ev));
        }
        {
            let app = self.clone();
            self.win.connect_delete_event(move |_, _| {
                app.hide_launcher();
                glib::Propagation::Stop
            });
        }
    }

    pub fn toggle_visibility(self: &Rc<Self>) {
        if self.visible.get() {
            self.hide_launcher();
        } else {
            self.show_launcher();
        }
    }

    fn show_launcher(self: &Rc<Self>) {
        *self.favorites.borrow_mut() = load_favorites();

        // Reset to favorites view
        let lb = self.current_listbox();
        for child in lb.children() {
            lb.remove(&child);
        }
        self.view_stack.borrow_mut().clear();

        if let Some(id) = self.search_changed_id.borrow().as_ref() {
            self.search_entry.block_signal(id);
        }
        self.search_entry.set_text("");
        if let Some(id) = self.search_changed_id.borrow().as_ref() {
            self.search_entry.unblock_signal(id);
        }

        self.show_favorites_view(false, "back");

        // Block row activation until stale Wayland key events pass.
        if let Some(id) = self.row_activated_1.borrow().as_ref() {
            self.listbox_1.block_signal(id);
        }
        if let Some(id) = self.row_activated_2.borrow().as_ref() {
            self.listbox_2.block_signal(id);
        }

        self.win.show_all();
        self.win.present();
        self.visible.set(true);

        let app = self.clone();
        glib::idle_add_local_once(move || app.select_first_row());
        self.signal_waybar();

        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(150), move || {
            app.unblock_row_activation();
        });
    }

    fn unblock_row_activation(&self) {
        if let Some(id) = self.row_activated_1.borrow().as_ref() {
            self.listbox_1.unblock_signal(id);
        }
        if let Some(id) = self.row_activated_2.borrow().as_ref() {
            self.listbox_2.unblock_signal(id);
        }
    }

    fn hide_launcher(self: &Rc<Self>) {
        if let Some(id) = self.focus_out_timeout.borrow_mut().take() {
            id.remove();
        }
        self.win.hide();
        self.visible.set(false);
        self.signal_waybar();
    }

    fn signal_waybar(&self) {
        lockfile::signal_waybar(self.visible.get());
    }

    // --- navigation / views ---

    fn select_first_row(&self) {
        let lb = self.current_listbox();
        if let Some(row) = lb.row_at_index(0) {
            lb.select_row(Some(&row));
            row.grab_focus();
        }
    }

    fn show_favorites_view(self: &Rc<Self>, animate: bool, direction: &str) {
        *self.view_stack.borrow_mut() = vec![View::Favorites];

        if animate {
            let app = self.clone();
            self.animate_transition(direction, move |lb| app.populate_favorites(lb));
        } else {
            let lb = self.current_listbox();
            self.populate_favorites(&lb);
            lb.show_all();
            let app = self.clone();
            glib::idle_add_local_once(move || app.select_first_row());
        }
    }

    fn populate_favorites(self: &Rc<Self>, lb: &gtk::ListBox) {
        let favs = self.favorites.borrow().clone();
        for desktop_id in favs {
            let idx = self.by_id.borrow().get(&desktop_id).copied();
            if let Some(idx) = idx {
                let row = self.create_app_row(idx, true, true);
                lb.add(&row);
            }
        }
        self.rebuild_nav_button("forward", "All Applications", "folder", NavAction::ShowCategories);
    }

    fn show_categories_view(self: &Rc<Self>, direction: &str) {
        self.view_stack.borrow_mut().push(View::Categories);
        let app = self.clone();
        self.animate_transition(direction, move |lb| app.populate_categories(lb));
    }

    fn populate_categories(self: &Rc<Self>, lb: &gtk::ListBox) {
        let all_row = self.create_category_row("All Applications", "applications-other");
        lb.add(&all_row);

        let cats: Vec<String> = self.categories.borrow().keys().cloned().collect();
        for cat in cats {
            let nonempty = self
                .categories
                .borrow()
                .get(&cat)
                .map_or(false, |v| !v.is_empty());
            if nonempty {
                let row = self.create_category_row(&cat, category_icon(&cat));
                lb.add(&row);
            }
        }
        self.rebuild_nav_button("back", "Back", "go-previous", NavAction::GoBack);
    }

    fn show_category_apps(self: &Rc<Self>, name: String, direction: &str, animate: bool) {
        self.view_stack
            .borrow_mut()
            .push(View::Category(name.clone()));

        if animate {
            let app = self.clone();
            let name = name.clone();
            self.animate_transition(direction, move |lb| app.populate_category(&name, lb));
        } else {
            let lb = self.current_listbox();
            for child in lb.children() {
                lb.remove(&child);
            }
            self.populate_category(&name, &lb);
            lb.show_all();
            let app = self.clone();
            glib::idle_add_local_once(move || app.select_first_row());
        }
    }

    fn apps_for_category(&self, name: &str) -> Vec<usize> {
        if name == "All Applications" {
            (0..self.all_apps.borrow().len()).collect()
        } else {
            self.categories
                .borrow()
                .get(name)
                .cloned()
                .unwrap_or_default()
        }
    }

    fn populate_category(self: &Rc<Self>, name: &str, lb: &gtk::ListBox) {
        let mut idxs = self.apps_for_category(name);
        {
            let apps = self.all_apps.borrow();
            idxs.sort_by(|&a, &b| apps[a].name_lower.cmp(&apps[b].name_lower));
        }
        let favs = self.favorites.borrow().clone();
        for idx in idxs {
            let is_fav = {
                let apps = self.all_apps.borrow();
                favs.contains(&apps[idx].desktop_id)
            };
            let row = self.create_app_row(idx, is_fav, false);
            lb.add(&row);
        }
        self.rebuild_nav_button("back", "Back", "go-previous", NavAction::GoBack);
    }

    fn show_search_results(self: &Rc<Self>, query: &str) {
        let lb = self.current_listbox();
        for child in lb.children() {
            lb.remove(&child);
        }

        let results = self.search_apps(query);
        let favs = self.favorites.borrow().clone();
        for idx in results.into_iter().take(20) {
            let is_fav = {
                let apps = self.all_apps.borrow();
                favs.contains(&apps[idx].desktop_id)
            };
            let row = self.create_app_row(idx, is_fav, false);
            lb.add(&row);
        }
        lb.show_all();
        let app = self.clone();
        glib::idle_add_local_once(move || app.select_first_row());
    }

    fn go_back(self: &Rc<Self>) {
        let prev = {
            let mut vs = self.view_stack.borrow_mut();
            if vs.len() <= 1 {
                return;
            }
            vs.pop();
            let p = vs.last().cloned();
            if p.is_some() {
                vs.pop();
            }
            p
        };
        match prev {
            Some(View::Favorites) => self.show_favorites_view(true, "back"),
            Some(View::Categories) => self.show_categories_view("back"),
            Some(View::Category(name)) => self.show_category_apps(name, "back", true),
            None => {}
        }
    }

    fn restore_current_view(self: &Rc<Self>) {
        let current = self.view_stack.borrow().last().cloned();
        let current = match current {
            Some(v) => v,
            None => {
                self.show_favorites_view(false, "back");
                return;
            }
        };

        let lb = self.current_listbox();
        for child in lb.children() {
            lb.remove(&child);
        }

        match current {
            View::Favorites => self.populate_favorites(&lb),
            View::Categories => self.populate_categories(&lb),
            View::Category(name) => self.populate_category(&name, &lb),
        }

        lb.show_all();
        let app = self.clone();
        glib::idle_add_local_once(move || app.select_first_row());
    }

    fn animate_transition<F: FnOnce(&gtk::ListBox)>(
        self: &Rc<Self>,
        direction: &str,
        populate: F,
    ) {
        if self.is_animating.get() {
            return;
        }
        self.is_animating.set(true);

        let (next_name, next_listbox, next_active) = match self.current.get() {
            ActiveView::One => ("view2", self.listbox_2.clone(), ActiveView::Two),
            ActiveView::Two => ("view1", self.listbox_1.clone(), ActiveView::One),
        };

        for child in next_listbox.children() {
            next_listbox.remove(&child);
        }
        populate(&next_listbox);
        next_listbox.show_all();

        self.content_stack.set_transition_type(if direction == "forward" {
            gtk::StackTransitionType::SlideLeft
        } else {
            gtk::StackTransitionType::SlideRight
        });
        self.content_stack.set_visible_child_name(next_name);
        self.current.set(next_active);

        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(260), move || {
            app.is_animating.set(false);
            let app2 = app.clone();
            glib::idle_add_local_once(move || app2.select_first_row());
        });
    }

    fn rebuild_nav_button(
        self: &Rc<Self>,
        button_type: &str,
        label_text: &str,
        icon_name: &str,
        action: NavAction,
    ) {
        for child in self.nav_button_box.children() {
            self.nav_button_box.remove(&child);
        }

        let event_box = gtk::EventBox::new();
        event_box.add_events(
            gdk::EventMask::ENTER_NOTIFY_MASK
                | gdk::EventMask::LEAVE_NOTIFY_MASK
                | gdk::EventMask::BUTTON_PRESS_MASK,
        );

        let hand_cursor = gdk::Display::default()
            .and_then(|d| gdk::Cursor::from_name(&d, "pointer"));
        {
            let hand = hand_cursor.clone();
            event_box.connect_enter_notify_event(move |w, _| {
                if let Some(win) = w.window() {
                    win.set_cursor(hand.as_ref());
                }
                glib::Propagation::Proceed
            });
        }
        event_box.connect_leave_notify_event(move |w, _| {
            if let Some(win) = w.window() {
                win.set_cursor(None);
            }
            glib::Propagation::Proceed
        });

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_start(5);
        hbox.set_margin_end(5);
        hbox.set_margin_top(5);
        hbox.set_size_request(-1, 30);

        let icon = gtk::Image::from_icon_name(Some(icon_name), gtk::IconSize::LargeToolbar);
        hbox.pack_start(&icon, false, false, 0);

        let label = gtk::Label::new(Some(label_text));
        label.set_xalign(0.0);
        hbox.pack_start(&label, true, true, 0);

        if button_type == "forward" {
            let arrow = gtk::Image::from_icon_name(Some("pan-end-symbolic"), gtk::IconSize::Button);
            hbox.pack_start(&arrow, false, false, 0);
        }

        event_box.add(&hbox);
        {
            let app = self.clone();
            event_box.connect_button_press_event(move |_, _| {
                app.nav_action(action);
                glib::Propagation::Proceed
            });
        }

        self.nav_button_box.pack_start(&event_box, true, true, 0);
        self.nav_button_box.show_all();
    }

    fn nav_action(self: &Rc<Self>, action: NavAction) {
        match action {
            NavAction::ShowCategories => self.show_categories_view("forward"),
            NavAction::GoBack => self.go_back(),
        }
    }

    // --- row construction ---

    fn create_category_row(self: &Rc<Self>, name: &str, icon_name: &str) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        set_row_meta(&row, RowMeta::Category { name: name.to_string() });

        let event_box = gtk::EventBox::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_start(5);
        hbox.set_margin_end(5);
        hbox.set_margin_top(5);
        hbox.set_margin_bottom(5);
        hbox.set_size_request(-1, 28);

        let icon = gtk::Image::from_icon_name(Some(icon_name), gtk::IconSize::LargeToolbar);
        hbox.pack_start(&icon, false, false, 0);

        let label = gtk::Label::new(Some(name));
        label.set_xalign(0.0);
        hbox.pack_start(&label, true, true, 0);

        let arrow = gtk::Image::from_icon_name(Some("pan-end-symbolic"), gtk::IconSize::Button);
        hbox.pack_start(&arrow, false, false, 0);

        event_box.add(&hbox);
        row.add(&event_box);

        event_box.add_events(
            gdk::EventMask::ENTER_NOTIFY_MASK | gdk::EventMask::LEAVE_NOTIFY_MASK,
        );
        {
            let app = self.clone();
            let r = row.clone();
            event_box.connect_enter_notify_event(move |_, _| {
                app.on_row_enter(&r);
                glib::Propagation::Proceed
            });
        }
        {
            let app = self.clone();
            let r = row.clone();
            event_box.connect_leave_notify_event(move |_, _| {
                app.on_row_leave(&r);
                glib::Propagation::Proceed
            });
        }

        row
    }

    fn create_app_row(self: &Rc<Self>, idx: usize, is_favorite: bool, draggable: bool) -> gtk::ListBoxRow {
        let name = self.all_apps.borrow()[idx].name.clone();
        let scale = self.win.scale_factor();

        let row = gtk::ListBoxRow::new();
        set_row_meta(&row, RowMeta::App { idx, draggable });

        let event_box = gtk::EventBox::new();
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        hbox.set_margin_start(5);
        hbox.set_margin_end(5);
        hbox.set_margin_top(3);
        hbox.set_margin_bottom(3);

        let icon = {
            let apps = self.all_apps.borrow();
            create_icon(&apps[idx], scale, &self.icon_cache)
        };
        hbox.pack_start(&icon, false, false, 0);

        let label = gtk::Label::new(Some(&name));
        label.set_xalign(0.0);
        label.set_ellipsize(pango::EllipsizeMode::End);
        hbox.pack_start(&label, true, true, 0);

        let fav_button = gtk::Button::new();
        let fav_icon_name = if is_favorite { "starred" } else { "non-starred" };
        fav_button.set_image(Some(&gtk::Image::from_icon_name(
            Some(fav_icon_name),
            gtk::IconSize::Button,
        )));
        fav_button.set_relief(gtk::ReliefStyle::None);
        {
            let app = self.clone();
            fav_button.connect_clicked(move |_| app.on_favorite_clicked(idx));
        }
        hbox.pack_start(&fav_button, false, false, 0);

        event_box.add(&hbox);
        row.add(&event_box);

        if draggable {
            event_box.add_events(
                gdk::EventMask::BUTTON_PRESS_MASK
                    | gdk::EventMask::BUTTON_RELEASE_MASK
                    | gdk::EventMask::POINTER_MOTION_MASK
                    | gdk::EventMask::ENTER_NOTIFY_MASK
                    | gdk::EventMask::LEAVE_NOTIFY_MASK,
            );
            {
                let app = self.clone();
                let r = row.clone();
                event_box.connect_button_press_event(move |_, ev| app.on_button_press(ev, &r));
            }
            {
                let app = self.clone();
                event_box.connect_button_release_event(move |_, _| app.on_button_release());
            }
            {
                let app = self.clone();
                event_box.connect_motion_notify_event(move |w, ev| app.on_motion_notify(ev, w));
            }
            {
                let app = self.clone();
                let r = row.clone();
                event_box.connect_enter_notify_event(move |_, _| {
                    app.on_row_enter(&r);
                    glib::Propagation::Proceed
                });
            }
            {
                let app = self.clone();
                let r = row.clone();
                event_box.connect_leave_notify_event(move |_, _| {
                    app.on_row_leave(&r);
                    glib::Propagation::Proceed
                });
            }
        } else {
            event_box.add_events(
                gdk::EventMask::BUTTON_PRESS_MASK
                    | gdk::EventMask::ENTER_NOTIFY_MASK
                    | gdk::EventMask::LEAVE_NOTIFY_MASK,
            );
            {
                let app = self.clone();
                let r = row.clone();
                event_box.connect_button_press_event(move |_, ev| app.on_button_press(ev, &r));
            }
            {
                let app = self.clone();
                let r = row.clone();
                event_box.connect_enter_notify_event(move |_, _| {
                    app.on_row_enter(&r);
                    glib::Propagation::Proceed
                });
            }
            {
                let app = self.clone();
                let r = row.clone();
                event_box.connect_leave_notify_event(move |_, _| {
                    app.on_row_leave(&r);
                    glib::Propagation::Proceed
                });
            }
        }

        row
    }

    // --- event handlers ---

    fn on_row_activated(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        match row_meta(row) {
            Some(RowMeta::Category { name }) => {
                self.show_category_apps(name, "forward", true);
            }
            Some(RowMeta::App { idx, .. }) => {
                if !self.dragging.get() {
                    self.launch_app(idx);
                }
            }
            None => {}
        }
    }

    fn on_row_enter(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        if !self.dragging.get() {
            *self.last_hovered_row.borrow_mut() = Some(row.clone());
            self.current_listbox().select_row(Some(row));
        }
    }

    fn on_row_leave(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        if !self.dragging.get() {
            let mut lh = self.last_hovered_row.borrow_mut();
            if lh.as_ref() == Some(row) {
                *lh = None;
            }
        }
    }

    fn on_listbox_motion(self: &Rc<Self>, ev: &gdk::EventMotion) -> glib::Propagation {
        if self.dragging.get() && self.drag_row.borrow().is_some() {
            let (_x, y) = ev.position();
            self.drag_motion_to(y as i32);
        }
        glib::Propagation::Proceed
    }

    fn drag_motion_to(self: &Rc<Self>, y: i32) {
        if !self.dragging.get() {
            return;
        }
        let drag_row = match self.drag_row.borrow().clone() {
            Some(r) => r,
            None => return,
        };
        let lb = self.current_listbox();
        if let Some(target) = lb.row_at_y(y) {
            if target != drag_row {
                let drag_index = drag_row.index();
                let target_index = target.index();
                if drag_index != target_index {
                    lb.remove(&drag_row);
                    lb.insert(&drag_row, target_index);
                    lb.show_all();
                    lb.select_row(Some(&drag_row));
                }
            }
        }
    }

    fn on_listbox_leave(self: &Rc<Self>) -> glib::Propagation {
        let _ = self.last_hovered_row.borrow_mut().take();
        glib::Propagation::Proceed
    }

    fn on_button_press(
        self: &Rc<Self>,
        ev: &gdk::EventButton,
        row: &gtk::ListBoxRow,
    ) -> glib::Propagation {
        let meta = row_meta(row);
        if ev.button() == 3 {
            if let Some(RowMeta::App { idx, .. }) = meta {
                self.show_context_menu(idx, ev);
            }
            return glib::Propagation::Stop;
        }
        if ev.button() == 1 {
            if let Some(RowMeta::App { draggable: true, .. }) = meta {
                self.dragging.set(true);
                *self.drag_row.borrow_mut() = Some(row.clone());
                self.current_listbox().select_row(Some(row));
            }
        }
        glib::Propagation::Proceed
    }

    fn on_motion_notify(
        self: &Rc<Self>,
        ev: &gdk::EventMotion,
        widget: &gtk::EventBox,
    ) -> glib::Propagation {
        if self.dragging.get() {
            let (x, y) = ev.position();
            if let Some((_dx, dy)) =
                widget.translate_coordinates(&self.current_listbox(), x as i32, y as i32)
            {
                self.drag_motion_to(dy);
            }
        }
        glib::Propagation::Stop
    }

    fn on_button_release(self: &Rc<Self>) -> glib::Propagation {
        if self.dragging.get() {
            self.dragging.set(false);

            let lb = self.current_listbox();
            let mut new_order = Vec::new();
            for child in lb.children() {
                if let Some(row) = child.downcast_ref::<gtk::ListBoxRow>() {
                    if let Some(RowMeta::App { idx, .. }) = row_meta(row) {
                        new_order.push(self.all_apps.borrow()[idx].desktop_id.clone());
                    }
                }
            }
            *self.favorites.borrow_mut() = new_order;
            self.save_favorites();
            *self.drag_row.borrow_mut() = None;
        }
        glib::Propagation::Proceed
    }

    fn show_context_menu(self: &Rc<Self>, idx: usize, ev: &gdk::EventButton) {
        let (name, desktop_path) = {
            let apps = self.all_apps.borrow();
            (apps[idx].name.clone(), apps[idx].desktop_path.clone())
        };

        let menu = gtk::Menu::new();

        let launch_item = gtk::MenuItem::with_label(&format!("Launch {}", name));
        {
            let app = self.clone();
            launch_item.connect_activate(move |_| app.launch_app(idx));
        }
        menu.append(&launch_item);

        menu.append(&gtk::SeparatorMenuItem::new());

        let open_location_item = gtk::MenuItem::with_label("Open .desktop file location");
        {
            let path = desktop_path.clone();
            open_location_item.connect_activate(move |_| open_file_location(&path));
        }
        menu.append(&open_location_item);

        menu.show_all();
        menu.popup_at_pointer(Some(&**ev));
    }

    fn on_favorite_clicked(self: &Rc<Self>, idx: usize) {
        let desktop_id = self.all_apps.borrow()[idx].desktop_id.clone();

        {
            let mut favs = self.favorites.borrow_mut();
            if let Some(pos) = favs.iter().position(|d| *d == desktop_id) {
                favs.remove(pos);
            } else {
                favs.push(desktop_id.clone());
            }
        }
        self.save_favorites();

        let top = self.view_stack.borrow().last().cloned();
        match top {
            Some(View::Favorites) => {
                self.view_stack.borrow_mut().pop();
                self.show_favorites_view(false, "back");
            }
            Some(View::Category(name)) => {
                self.view_stack.borrow_mut().pop();
                self.show_category_apps(name, "forward", false);
            }
            _ => {}
        }
    }

    fn on_search_changed(self: &Rc<Self>) {
        let query = self.search_entry.text().to_string();
        if !query.is_empty() {
            self.show_search_results(&query);
        } else {
            self.restore_current_view();
        }
    }

    fn on_search_activate(self: &Rc<Self>) {
        let query = self.search_entry.text().to_string();
        let query = query.trim().to_string();
        if query.is_empty() {
            return;
        }

        let lb = self.current_listbox();

        if let Some(row) = lb.selected_row() {
            if let Some(RowMeta::App { idx, .. }) = row_meta(&row) {
                self.launch_app(idx);
                return;
            }
        }

        let children = lb.children();
        if children.len() == 1 {
            if let Some(row) = children[0].downcast_ref::<gtk::ListBoxRow>() {
                if let Some(RowMeta::App { idx, .. }) = row_meta(row) {
                    self.launch_app(idx);
                    return;
                }
            }
        }

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = Command::new(shell);
        cmd.arg("-c").arg(&query);
        configure_detached(&mut cmd);
        if cmd.spawn().is_ok() {
            self.hide_launcher();
        }
    }

    fn search_apps(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        let apps = self.all_apps.borrow();

        let mut exact = Vec::new();
        let mut title = Vec::new();
        let mut other = Vec::new();

        for (i, app) in apps.iter().enumerate() {
            if app.name_lower == query {
                exact.push(i);
                continue;
            }
            if app.name_lower.contains(&query) {
                title.push(i);
                continue;
            }
            if app.search_text.contains(&query) {
                other.push(i);
            }
        }

        exact.extend(title);
        exact.extend(other);
        exact
    }

    fn launch_app(self: &Rc<Self>, idx: usize) {
        let (app_info, desktop_id, name) = {
            let apps = self.all_apps.borrow();
            (
                apps[idx].app_info.clone(),
                apps[idx].desktop_id.clone(),
                apps[idx].name.clone(),
            )
        };

        if app_info
            .launch(&[], None::<&gio::AppLaunchContext>)
            .is_ok()
        {
            self.hide_launcher();
            return;
        }

        {
            let mut cmd = Command::new("gtk-launch");
            cmd.arg(&desktop_id);
            configure_detached(&mut cmd);
            if cmd.spawn().is_ok() {
                self.hide_launcher();
                return;
            }
        }

        {
            let mut cmd = Command::new("dbus-launch");
            cmd.arg("gtk-launch").arg(&desktop_id);
            configure_detached(&mut cmd);
            if cmd.spawn().is_ok() {
                self.hide_launcher();
                return;
            }
        }

        println!("Failed to launch: {}", name);
    }

    fn save_favorites(&self) {
        let path = favorites_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let favs = self.favorites.borrow();
        if let Ok(json) = serde_json::to_string_pretty(&*favs) {
            let _ = fs::write(&path, json);
        }
    }

    fn rebuild_by_id(&self) {
        let mut map = HashMap::new();
        {
            let apps = self.all_apps.borrow();
            for (i, a) in apps.iter().enumerate() {
                map.entry(a.desktop_id.clone()).or_insert(i);
            }
        }
        *self.by_id.borrow_mut() = map;
    }

    // --- file monitoring / reload ---

    fn setup_app_monitors(self: &Rc<Self>) {
        let mut monitors = Vec::new();
        for path in desktop::watch_dirs() {
            if !path.exists() {
                continue;
            }
            let gfile = gio::File::for_path(&path);
            if let Ok(monitor) =
                gfile.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
            {
                let app = self.clone();
                monitor.connect_changed(move |_m, file, _other, _event| {
                    app.on_desktop_dir_changed(file);
                });
                monitors.push(monitor);
            }
        }
        *self.monitors.borrow_mut() = monitors;
    }

    fn on_desktop_dir_changed(self: &Rc<Self>, file: &gio::File) {
        let is_desktop = file
            .basename()
            .as_ref()
            .and_then(|p| p.to_str())
            .map_or(false, |s| s.ends_with(".desktop"));
        if !is_desktop {
            return;
        }
        if self.reload_pending.get() {
            return;
        }
        self.reload_pending.set(true);
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            app.reload_applications();
        });
    }

    fn reload_applications(self: &Rc<Self>) {
        self.reload_pending.set(false);
        *self.all_apps.borrow_mut() = desktop::load_applications();
        self.rebuild_by_id();
        let cats = desktop::organize_by_category(&self.all_apps.borrow());
        *self.categories.borrow_mut() = cats;
        if self.visible.get() {
            self.refresh_current_view();
        }
    }

    fn refresh_current_view(self: &Rc<Self>) {
        let empty = self.view_stack.borrow().is_empty();
        if empty {
            self.show_favorites_view(false, "back");
            return;
        }
        // Category apps are always re-derived from the category name at
        // populate time, so no stored snapshot needs refreshing here.
        self.restore_current_view();
    }

    // --- focus / keyboard ---

    fn on_focus_out(self: &Rc<Self>) -> glib::Propagation {
        if let Some(id) = self.focus_out_timeout.borrow_mut().take() {
            id.remove();
        }
        let app = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(500), move || {
            app.hide_launcher();
            glib::ControlFlow::Break
        });
        *self.focus_out_timeout.borrow_mut() = Some(id);
        glib::Propagation::Proceed
    }

    fn on_focus_in(self: &Rc<Self>) -> glib::Propagation {
        if let Some(id) = self.focus_out_timeout.borrow_mut().take() {
            id.remove();
        }
        if self.search_entry.text().is_empty() {
            self.current_listbox().grab_focus();
            self.search_entry.queue_draw();
        }
        glib::Propagation::Proceed
    }

    fn on_search_entry_key_press(self: &Rc<Self>, ev: &gdk::EventKey) -> glib::Propagation {
        if ev.keyval() == gdk::keys::constants::Down {
            let lb = self.current_listbox();
            if let Some(row) = lb.row_at_index(0) {
                lb.select_row(Some(&row));
                row.grab_focus();
            }
        }
        glib::Propagation::Proceed
    }

    fn on_key_press(self: &Rc<Self>, ev: &gdk::EventKey) -> glib::Propagation {
        use gdk::keys::constants as key;
        let kv = ev.keyval();

        if kv == key::Escape {
            let depth = self.view_stack.borrow().len();
            if depth > 1 {
                self.go_back();
            } else {
                self.hide_launcher();
            }
            return glib::Propagation::Stop;
        }

        if kv == key::Up || kv == key::Down {
            let lb = self.current_listbox();
            if !lb.has_focus() {
                lb.grab_focus();
            }
            return glib::Propagation::Proceed;
        }

        if kv == key::Right && !self.search_entry.has_focus() {
            let selected = self.current_listbox().selected_row();
            let mut navigated = false;
            if let Some(row) = &selected {
                if let Some(RowMeta::Category { name }) = row_meta(row) {
                    self.show_category_apps(name, "forward", true);
                    navigated = true;
                }
            }
            if !navigated {
                let top_is_favorites =
                    matches!(self.view_stack.borrow().last(), Some(View::Favorites));
                if top_is_favorites {
                    self.show_categories_view("forward");
                }
            }
            return glib::Propagation::Stop;
        }

        if kv == key::Left && !self.search_entry.has_focus() {
            let depth = self.view_stack.borrow().len();
            if depth > 1 {
                self.go_back();
                return glib::Propagation::Stop;
            }
        }

        if kv == key::Return || kv == key::KP_Enter {
            return glib::Propagation::Proceed;
        }

        if !self.search_entry.has_focus() {
            self.search_entry.grab_focus();
            self.search_entry.set_position(-1);
        }

        glib::Propagation::Proceed
    }
}

fn open_file_location(desktop_path: &str) {
    if let Some(dir) = std::path::Path::new(desktop_path).parent() {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(dir);
        configure_detached(&mut cmd);
        let _ = cmd.spawn();
    }
}

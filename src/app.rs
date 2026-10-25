use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
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
    /* Suppress the theme's own hover tint; selection is the only highlight. */
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

/// The listbox a row actually belongs to. During the stack transition both
/// listboxes are on screen, so `current_listbox()` is not necessarily it.
fn listbox_of(row: &gtk::ListBoxRow) -> Option<gtk::ListBox> {
    row.parent().and_then(|p| p.downcast::<gtk::ListBox>().ok())
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
        "Multimedia" => "applications-multimedia-symbolic",
        "Development" => "applications-development-symbolic",
        "Education" => "applications-education-symbolic",
        "Games" => "applications-games-symbolic",
        "Graphics" => "applications-graphics-symbolic",
        "Internet" => "applications-internet-symbolic",
        "Office" => "applications-office-symbolic",
        "Science" => "applications-science-symbolic",
        "Settings" => "preferences-system-symbolic",
        "System Tools" => "applications-system-symbolic",
        "Accessories" => "applications-utilities-symbolic",
        "Other" => "applications-other-symbolic",
        _ => "folder-symbolic",
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
    scrolled_1: gtk::ScrolledWindow,
    scrolled_2: gtk::ScrolledWindow,
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
    drag_moved: Cell<bool>,
    keyboard_mode: Cell<bool>,
    last_pointer_root: Cell<(f64, f64)>,
    activate_blocked: Cell<bool>,
    armed_key: Cell<Option<gdk::keys::Key>>,
    menu_open: Cell<bool>,
    is_animating: Cell<bool>,
    visible: Cell<bool>,
    focus_out_timeout: RefCell<Option<glib::SourceId>>,
    reload_pending: Cell<bool>,
    monitors: RefCell<Vec<gio::FileMonitor>>,

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
        // Focus stays in the search entry; selection is driven explicitly so the
        // painted highlight and the row Enter acts on can never disagree.
        listbox_1.set_can_focus(false);
        scrolled_1.add(&listbox_1);

        let scrolled_2 = gtk::ScrolledWindow::builder().build();
        scrolled_2.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        let listbox_2 = gtk::ListBox::new();
        listbox_2.set_selection_mode(gtk::SelectionMode::Single);
        listbox_2.set_can_focus(false);
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
        search_entry.set_placeholder_text(Some("🔎 Search"));
        search_box.pack_start(&search_entry, true, true, 0);
        main_vbox.pack_start(&search_box, false, false, 0);

        let app = Rc::new(App {
            win,
            content_stack,
            scrolled_1,
            scrolled_2,
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
            drag_moved: Cell::new(false),
            keyboard_mode: Cell::new(true),
            last_pointer_root: Cell::new((f64::NAN, f64::NAN)),
            activate_blocked: Cell::new(false),
            armed_key: Cell::new(None),
            menu_open: Cell::new(false),
            is_animating: Cell::new(false),
            visible: Cell::new(false),
            focus_out_timeout: RefCell::new(None),
            reload_pending: Cell::new(false),
            monitors: RefCell::new(Vec::new()),
            search_changed_id: RefCell::new(None),
            icon_cache: RefCell::new(HashMap::new()),
        });

        app.apply_css();
        app.connect_signals();

        *app.all_apps.borrow_mut() = desktop::load_applications();
        app.rebuild_by_id();
        let cats = desktop::organize_by_category(&app.all_apps.borrow());
        *app.categories.borrow_mut() = cats;

        app.show_favorites_view();
        app.setup_app_monitors();
        app.connect_window_signals();

        if !start_hidden {
            app.win.show_all();
            app.visible.set(true);
            app.blur_entry();
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

    fn scrolled_for(&self, lb: &gtk::ListBox) -> gtk::ScrolledWindow {
        if lb == &self.listbox_1 {
            self.scrolled_1.clone()
        } else {
            self.scrolled_2.clone()
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
            listbox.add_events(gdk::EventMask::POINTER_MOTION_MASK);
        }

        for listbox in [&self.listbox_1, &self.listbox_2] {
            let app = self.clone();
            listbox.connect_row_activated(move |_lb, row| app.on_row_activated(row));
        }
        for listbox in [&self.listbox_1, &self.listbox_2] {
            let app = self.clone();
            listbox.connect_motion_notify_event(move |lb, ev| app.on_listbox_motion(lb, ev));
        }

        {
            let app = self.clone();
            let id = self.search_entry.connect_changed(move |_| app.on_search_changed());
            *self.search_changed_id.borrow_mut() = Some(id);
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
            self.win.connect_key_release_event(move |_, ev| app.on_key_release(ev));
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
        self.view_stack.borrow_mut().clear();
        self.set_search_text_silently("");
        self.keyboard_mode.set(true);
        self.show_favorites_view();

        // Block activation until stale Wayland key events pass.
        self.activate_blocked.set(true);
        self.armed_key.set(None);

        self.win.show_all();
        self.win.present();
        self.visible.set(true);
        self.blur_entry();
        self.signal_waybar();

        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(150), move || {
            app.activate_blocked.set(false);
        });
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

    fn set_search_text_silently(&self, text: &str) {
        let id = self.search_changed_id.borrow();
        if let Some(id) = id.as_ref() {
            self.search_entry.block_signal(id);
        }
        self.search_entry.set_text(text);
        if let Some(id) = id.as_ref() {
            self.search_entry.unblock_signal(id);
        }
    }

    // --- selection ---

    fn scroll_to_row(&self, lb: &gtk::ListBox, row: &gtk::ListBoxRow) {
        let adj = self.scrolled_for(lb).vadjustment();
        if adj.page_size() <= 0.0 {
            return;
        }
        let a = row.allocation();
        adj.clamp_page(a.y() as f64, (a.y() + a.height()) as f64);
    }

    fn scroll_to_top(&self, lb: &gtk::ListBox) {
        let adj = self.scrolled_for(lb).vadjustment();
        adj.set_value(adj.lower());
    }

    /// Select a row and scroll the minimum distance to bring it into view.
    fn select_and_reveal(&self, lb: &gtk::ListBox, row: &gtk::ListBoxRow) {
        lb.select_row(Some(row));
        self.scroll_to_row(lb, row);
    }

    fn select_index(&self, lb: &gtk::ListBox, index: i32) {
        let count = lb.children().len() as i32;
        if count == 0 {
            return;
        }
        if let Some(row) = lb.row_at_index(index.clamp(0, count - 1)) {
            self.select_and_reveal(lb, &row);
        }
    }

    fn select_first(&self, lb: &gtk::ListBox) {
        if let Some(row) = lb.row_at_index(0) {
            lb.select_row(Some(&row));
        }
        self.scroll_to_top(lb);
    }

    fn move_selection(&self, delta: i32) {
        let lb = self.current_listbox();
        let count = lb.children().len() as i32;
        if count == 0 {
            return;
        }
        let next = match lb.selected_row() {
            Some(row) => (row.index() + delta).clamp(0, count - 1),
            None if delta > 0 => 0,
            None => count - 1,
        };
        self.select_index(&lb, next);
        self.blur_entry_if_empty();
    }

    /// Move the highlight under the mouse, without scrolling.
    fn hover_select(&self, row: &gtk::ListBoxRow) {
        if let Some(lb) = listbox_of(row) {
            if lb.selected_row().as_ref() != Some(row) {
                lb.select_row(Some(row));
                self.blur_entry_if_empty();
            }
        }
    }

    /// Records a pointer position, returning `false` if the pointer has not
    /// actually moved. Rebuilding rows under a stationary pointer makes GDK
    /// synthesize crossing events; those must not steal the highlight.
    fn note_pointer(&self, root: (f64, f64)) -> bool {
        if self.last_pointer_root.get() == root {
            return false;
        }
        self.last_pointer_root.set(root);
        self.keyboard_mode.set(false);
        true
    }

    fn focus_entry(&self) {
        if !self.search_entry.has_focus() {
            self.search_entry.grab_focus_without_selecting();
            self.search_entry.set_position(-1);
        }
    }

    /// Park focus outside the entry so it renders its placeholder. GTK3 hides
    /// placeholder text as soon as an entry is focused, and the first keypress
    /// hands focus back via `on_key_press`, so nothing is lost by waiting.
    fn blur_entry(&self) {
        self.win.set_focus(None::<&gtk::Widget>);
    }

    /// Moving the selection by key or pointer with nothing typed means the user
    /// is browsing rather than searching, so hand the placeholder back — this
    /// is what restores it after a query is backspaced away.
    fn blur_entry_if_empty(&self) {
        if self.search_entry.text().is_empty() {
            self.blur_entry();
        }
    }

    // --- navigation / views ---

    fn render_view(self: &Rc<Self>, view: &View, lb: &gtk::ListBox) {
        match view {
            View::Favorites => self.populate_favorites(lb),
            View::Categories => self.populate_categories(lb),
            View::Category(name) => self.populate_category(name, lb),
        }
    }

    fn repopulate(&self, lb: &gtk::ListBox, populate: impl FnOnce(&gtk::ListBox)) {
        for child in lb.children() {
            lb.remove(&child);
        }
        populate(lb);
        lb.show_all();
        self.select_first(lb);
    }

    fn show_favorites_view(self: &Rc<Self>) {
        *self.view_stack.borrow_mut() = vec![View::Favorites];
        let lb = self.current_listbox();
        self.repopulate(&lb, |lb| self.populate_favorites(lb));
    }

    fn push_view(self: &Rc<Self>, view: View) {
        if self.is_animating.get() {
            return;
        }
        self.view_stack.borrow_mut().push(view.clone());
        let app = self.clone();
        self.animate_transition("forward", move |lb| app.render_view(&view, lb));
    }

    fn go_back(self: &Rc<Self>) {
        if self.is_animating.get() {
            return;
        }
        let prev = {
            let mut vs = self.view_stack.borrow_mut();
            if vs.len() <= 1 {
                return;
            }
            vs.pop();
            match vs.last() {
                Some(v) => v.clone(),
                None => return,
            }
        };
        let app = self.clone();
        self.animate_transition("back", move |lb| app.render_view(&prev, lb));
    }

    /// Rebuild the view currently on top of the stack, in place.
    fn restore_current_view(self: &Rc<Self>) {
        let current = match self.view_stack.borrow().last().cloned() {
            Some(v) => v,
            None => {
                self.show_favorites_view();
                return;
            }
        };
        let lb = self.current_listbox();
        self.repopulate(&lb, |lb| self.render_view(&current, lb));
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

    fn favorite_ids(&self) -> HashSet<String> {
        self.favorites.borrow().iter().cloned().collect()
    }

    fn populate_category(self: &Rc<Self>, name: &str, lb: &gtk::ListBox) {
        let mut idxs = self.apps_for_category(name);
        {
            let apps = self.all_apps.borrow();
            idxs.sort_by(|&a, &b| apps[a].name_lower.cmp(&apps[b].name_lower));
        }
        let favs = self.favorite_ids();
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
        let results = self.search_apps(query);
        let favs = self.favorite_ids();
        self.repopulate(&lb, |lb| {
            for idx in results.into_iter().take(20) {
                let is_fav = {
                    let apps = self.all_apps.borrow();
                    favs.contains(&apps[idx].desktop_id)
                };
                let row = self.create_app_row(idx, is_fav, false);
                lb.add(&row);
            }
        });
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

        self.repopulate(&next_listbox, populate);

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
            NavAction::ShowCategories => self.push_view(View::Categories),
            NavAction::GoBack => self.go_back(),
        }
    }

    // --- row construction ---

    fn create_category_row(self: &Rc<Self>, name: &str, icon_name: &str) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_can_focus(false);
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
            gdk::EventMask::ENTER_NOTIFY_MASK | gdk::EventMask::POINTER_MOTION_MASK,
        );
        self.connect_hover(&event_box, &row);

        row
    }

    fn create_app_row(self: &Rc<Self>, idx: usize, is_favorite: bool, draggable: bool) -> gtk::ListBoxRow {
        let name = self.all_apps.borrow()[idx].name.clone();
        let scale = self.win.scale_factor();

        let row = gtk::ListBoxRow::new();
        row.set_can_focus(false);
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
        fav_button.set_can_focus(false);
        {
            let app = self.clone();
            fav_button.connect_clicked(move |_| app.on_favorite_clicked(idx));
        }
        hbox.pack_start(&fav_button, false, false, 0);

        event_box.add(&hbox);
        row.add(&event_box);

        event_box.add_events(
            gdk::EventMask::BUTTON_PRESS_MASK
                | gdk::EventMask::ENTER_NOTIFY_MASK
                | gdk::EventMask::POINTER_MOTION_MASK,
        );
        {
            let app = self.clone();
            let r = row.clone();
            event_box.connect_button_press_event(move |_, ev| app.on_button_press(ev, &r));
        }
        if draggable {
            event_box.add_events(gdk::EventMask::BUTTON_RELEASE_MASK);
            let app = self.clone();
            event_box.connect_button_release_event(move |_, _| app.on_button_release());
        }
        self.connect_hover(&event_box, &row);

        row
    }

    /// Hover highlighting: crossings catch entering a new row, motion catches a
    /// pointer that was already resting inside one.
    fn connect_hover(self: &Rc<Self>, event_box: &gtk::EventBox, row: &gtk::ListBoxRow) {
        {
            let app = self.clone();
            let r = row.clone();
            event_box.connect_enter_notify_event(move |_, ev| {
                app.on_row_enter(&r, ev);
                glib::Propagation::Proceed
            });
        }
        {
            let app = self.clone();
            let r = row.clone();
            event_box.connect_motion_notify_event(move |w, ev| app.on_row_motion(&r, w, ev));
        }
    }

    // --- event handlers ---

    fn on_row_activated(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        if self.activate_blocked.get() || self.dragging.get() {
            return;
        }
        match row_meta(row) {
            Some(RowMeta::Category { name }) => self.push_view(View::Category(name)),
            Some(RowMeta::App { idx, .. }) => self.launch_app(idx),
            None => {}
        }
    }

    fn on_row_enter(self: &Rc<Self>, row: &gtk::ListBoxRow, ev: &gdk::EventCrossing) {
        if self.dragging.get() || self.keyboard_mode.get() {
            return;
        }
        if !self.note_pointer(ev.root()) {
            return;
        }
        self.hover_select(row);
    }

    fn on_row_motion(
        self: &Rc<Self>,
        row: &gtk::ListBoxRow,
        widget: &gtk::EventBox,
        ev: &gdk::EventMotion,
    ) -> glib::Propagation {
        let moved = self.note_pointer(ev.root());
        if self.dragging.get() {
            if let Some(lb) = listbox_of(row) {
                let (x, y) = ev.position();
                if let Some((_dx, dy)) = widget.translate_coordinates(&lb, x as i32, y as i32) {
                    self.drag_motion_to(dy);
                }
            }
            return glib::Propagation::Stop;
        }
        if moved {
            self.hover_select(row);
        }
        glib::Propagation::Proceed
    }

    fn on_listbox_motion(
        self: &Rc<Self>,
        lb: &gtk::ListBox,
        ev: &gdk::EventMotion,
    ) -> glib::Propagation {
        let moved = self.note_pointer(ev.root());
        if self.dragging.get() {
            if self.drag_row.borrow().is_some() {
                let (_x, y) = ev.position();
                self.drag_motion_to(y as i32);
            }
        } else if moved {
            // Gaps between rows still belong to the listbox window.
            if let Some(row) = lb.row_at_y(ev.position().1 as i32) {
                self.hover_select(&row);
            }
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
        let lb = match listbox_of(&drag_row) {
            Some(lb) => lb,
            None => return,
        };
        if let Some(target) = lb.row_at_y(y) {
            if target != drag_row {
                let drag_index = drag_row.index();
                let target_index = target.index();
                if drag_index != target_index {
                    lb.remove(&drag_row);
                    lb.insert(&drag_row, target_index);
                    lb.show_all();
                    lb.select_row(Some(&drag_row));
                    self.drag_moved.set(true);
                }
            }
        }
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
            self.keyboard_mode.set(false);
            self.hover_select(row);
            if let Some(RowMeta::App { draggable: true, .. }) = meta {
                self.dragging.set(true);
                self.drag_moved.set(false);
                *self.drag_row.borrow_mut() = Some(row.clone());
            }
        }
        glib::Propagation::Proceed
    }

    fn on_button_release(self: &Rc<Self>) -> glib::Propagation {
        if !self.dragging.get() {
            return glib::Propagation::Proceed;
        }
        self.dragging.set(false);

        let reordered = self.drag_moved.get();
        self.drag_moved.set(false);
        let lb = self.drag_row.borrow().as_ref().and_then(listbox_of);
        *self.drag_row.borrow_mut() = None;

        if !reordered {
            // A plain click, not a drag — let the listbox activate the row.
            return glib::Propagation::Proceed;
        }

        if let Some(lb) = lb {
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
        }
        // Swallow the release so the reorder does not also launch the app.
        glib::Propagation::Stop
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

        // The menu is its own toplevel, so the launcher gets a focus-out while
        // it is up; suppress the auto-hide until it closes.
        self.menu_open.set(true);
        {
            let weak = Rc::downgrade(self);
            menu.connect_hide(move |_| {
                if let Some(app) = weak.upgrade() {
                    app.menu_open.set(false);
                    app.sync_entry_focus();
                }
            });
        }

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
        if !matches!(top, Some(View::Favorites) | Some(View::Category(_))) {
            return;
        }

        // Keep the user where they were instead of snapping back to the top.
        let previous = self.current_listbox().selected_row().map(|r| r.index());
        self.restore_current_view();
        if let Some(index) = previous {
            let app = self.clone();
            glib::idle_add_local_once(move || {
                let lb = app.current_listbox();
                app.select_index(&lb, index);
            });
        }
    }

    fn on_search_changed(self: &Rc<Self>) {
        let text = self.search_entry.text();
        let query = text.trim();
        if query.is_empty() {
            self.restore_current_view();
        } else {
            self.show_search_results(query);
        }
    }

    fn activate_selection(self: &Rc<Self>) {
        if self.activate_blocked.get() {
            return;
        }

        if let Some(row) = self.current_listbox().selected_row() {
            match row_meta(&row) {
                Some(RowMeta::App { idx, .. }) => {
                    self.launch_app(idx);
                    return;
                }
                Some(RowMeta::Category { name }) => {
                    self.push_view(View::Category(name));
                    return;
                }
                None => {}
            }
        }

        let text = self.search_entry.text();
        let query = text.trim();
        if query.is_empty() {
            return;
        }

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = Command::new(shell);
        cmd.arg("-c").arg(query);
        configure_detached(&mut cmd);
        if cmd.spawn().is_ok() {
            self.hide_launcher();
        }
    }

    fn search_apps(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        let apps = self.all_apps.borrow();

        let mut exact = Vec::new();
        let mut prefix = Vec::new();
        let mut infix = Vec::new();
        let mut other = Vec::new();

        for (i, app) in apps.iter().enumerate() {
            if app.name_lower == query {
                exact.push(i);
            } else if app.name_lower.starts_with(&query) {
                prefix.push(i);
            } else if app.name_lower.contains(&query) {
                infix.push(i);
            } else if app.search_text.contains(&query) {
                other.push(i);
            }
        }

        exact.extend(prefix);
        exact.extend(infix);
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

        eprintln!("Failed to launch: {}", name);
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
            // Category apps are always re-derived from the category name at
            // populate time, so no stored snapshot needs refreshing here.
            self.restore_current_view();
        }
    }

    // --- focus / keyboard ---

    fn on_focus_out(self: &Rc<Self>) -> glib::Propagation {
        if let Some(id) = self.focus_out_timeout.borrow_mut().take() {
            id.remove();
        }
        if self.menu_open.get() {
            return glib::Propagation::Proceed;
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
        self.sync_entry_focus();
        glib::Propagation::Proceed
    }

    /// Mid-search the caret belongs in the entry; with an empty query the
    /// placeholder should be showing instead.
    fn sync_entry_focus(&self) {
        if self.search_entry.text().is_empty() {
            self.blur_entry();
        } else {
            self.focus_entry();
        }
    }

    /// Escape and Enter act on release: hiding on the press unmaps the window
    /// before the release arrives, which leaves GTK's client-side key repeat
    /// running and replaying the key into the next show.
    fn on_key_release(self: &Rc<Self>, ev: &gdk::EventKey) -> glib::Propagation {
        use gdk::keys::constants as key;
        let kv = ev.keyval();

        if self.armed_key.get() != Some(kv) {
            return glib::Propagation::Proceed;
        }
        self.armed_key.set(None);

        if kv == key::Escape {
            self.hide_launcher();
        } else {
            self.activate_selection();
        }
        glib::Propagation::Stop
    }

    fn on_key_press(self: &Rc<Self>, ev: &gdk::EventKey) -> glib::Propagation {
        use gdk::keys::constants as key;
        let kv = ev.keyval();

        self.keyboard_mode.set(true);

        if kv == key::Escape {
            let depth = self.view_stack.borrow().len();
            if depth > 1 {
                self.go_back();
            } else {
                self.armed_key.set(Some(kv));
            }
            return glib::Propagation::Stop;
        }

        if kv == key::Up {
            self.move_selection(-1);
            return glib::Propagation::Stop;
        }
        if kv == key::Down {
            self.move_selection(1);
            return glib::Propagation::Stop;
        }
        if kv == key::Page_Up || kv == key::KP_Page_Up {
            self.move_selection(-10);
            return glib::Propagation::Stop;
        }
        if kv == key::Page_Down || kv == key::KP_Page_Down {
            self.move_selection(10);
            return glib::Propagation::Stop;
        }

        if kv == key::Return || kv == key::KP_Enter {
            self.armed_key.set(Some(kv));
            return glib::Propagation::Stop;
        }

        // With focus pinned to the entry, an empty query is what distinguishes
        // "browsing the list" from "editing a search".
        let browsing = self.search_entry.text().trim().is_empty();

        if browsing && (kv == key::Home || kv == key::End) {
            let lb = self.current_listbox();
            let last = lb.children().len() as i32 - 1;
            self.select_index(&lb, if kv == key::Home { 0 } else { last });
            self.blur_entry_if_empty();
            return glib::Propagation::Stop;
        }

        if browsing && kv == key::Right {
            let selected = self.current_listbox().selected_row();
            let mut navigated = false;
            if let Some(row) = &selected {
                if let Some(RowMeta::Category { name }) = row_meta(row) {
                    self.push_view(View::Category(name));
                    navigated = true;
                }
            }
            if !navigated {
                let top_is_favorites =
                    matches!(self.view_stack.borrow().last(), Some(View::Favorites));
                if top_is_favorites {
                    self.push_view(View::Categories);
                }
            }
            return glib::Propagation::Stop;
        }

        if browsing && kv == key::Left {
            let depth = self.view_stack.borrow().len();
            if depth > 1 {
                self.go_back();
                return glib::Propagation::Stop;
            }
        }

        // Safety net: anything else is text, and the entry should own it.
        self.focus_entry();
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

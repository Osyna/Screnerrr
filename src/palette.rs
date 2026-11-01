use gtk::{gdk, glib, prelude::*};
use std::{cell::RefCell, rc::Rc};

pub struct Command {
    pub name: String,
    pub shortcut: String,
    pub description: String,
    pub widget: gtk::Button,
}

pub struct Palette {
    window: gtk::Window,
    entry: gtk::SearchEntry,
    list: gtk::ListBox,
    empty: gtk::Label,
    commands: Vec<Command>,
    search_keys: Vec<String>,
    visible: RefCell<Vec<usize>>,
}

fn matches(haystack: &str, query: &str) -> bool {
    query.split_whitespace().all(|word| haystack.contains(word))
}

impl Palette {
    pub fn new(parent: &gtk::ApplicationWindow, commands: Vec<Command>) -> Rc<Self> {
        let window = gtk::Window::builder()
            .title("Command palette")
            .transient_for(parent)
            .modal(true)
            .default_width(560)
            .default_height(460)
            .build();
        window.add_css_class("sharpshot");
        window.set_titlebar(Some(&gtk::HeaderBar::new()));
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Search tools and actions…")
            .build();
        entry.update_property(&[gtk::accessible::Property::Label("Search commands")]);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        let empty = gtk::Label::new(Some("No matching commands. Try a tool name or shortcut."));
        empty.set_wrap(true);
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&list)
            .build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
        for side in [
            gtk::PositionType::Top,
            gtk::PositionType::Bottom,
            gtk::PositionType::Left,
            gtk::PositionType::Right,
        ] {
            match side {
                gtk::PositionType::Top => root.set_margin_top(12),
                gtk::PositionType::Bottom => root.set_margin_bottom(12),
                gtk::PositionType::Left => root.set_margin_start(12),
                _ => root.set_margin_end(12),
            }
        }
        root.append(&entry);
        root.append(&empty);
        root.append(&scroll);
        window.set_child(Some(&root));
        let search_keys = commands
            .iter()
            .map(|c| format!("{} {} {}", c.name, c.description, c.shortcut).to_lowercase())
            .collect();
        let this = Rc::new(Self {
            window,
            entry,
            list,
            empty,
            commands,
            search_keys,
            visible: RefCell::new(Vec::new()),
        });
        this.entry.connect_search_changed(glib::clone!(
            #[weak]
            this,
            move |_| this.filter()
        ));
        this.entry.connect_activate(glib::clone!(
            #[weak]
            this,
            move |_| this.activate()
        ));
        this.list.connect_row_activated(glib::clone!(
            #[weak]
            this,
            move |_, _| this.activate()
        ));
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(glib::clone!(
            #[weak]
            this,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    this.window.set_visible(false);
                    return glib::Propagation::Stop;
                }
                if key == gdk::Key::Down || key == gdk::Key::Up {
                    let direction = if key == gdk::Key::Down { 1 } else { -1 };
                    let len = this.visible.borrow().len() as i32;
                    let mut i = this.list.selected_row().map_or(-1, |r| r.index());
                    for _ in 0..len {
                        i = (i + direction).rem_euclid(len);
                        if let Some(row) = this.list.row_at_index(i) {
                            if row.is_sensitive() {
                                this.list.select_row(Some(&row));
                                break;
                            }
                        }
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        ));
        this.window.add_controller(keys);
        this.window.connect_close_request(|window| {
            window.set_visible(false);
            glib::Propagation::Stop
        });
        this
    }
    pub fn open(&self) {
        self.entry.set_text("");
        self.filter();
        self.window.present();
        self.entry.grab_focus();
    }
    fn filter(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let query = self.entry.text().to_lowercase();
        let mut visible = self.visible.borrow_mut();
        visible.clear();
        for (index, command) in self
            .commands
            .iter()
            .enumerate()
            .filter(|(index, _)| matches(&self.search_keys[*index], &query))
        {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            line.set_margin_top(8);
            line.set_margin_bottom(8);
            line.set_margin_start(10);
            line.set_margin_end(10);
            let labels = gtk::Box::new(gtk::Orientation::Vertical, 3);
            labels.set_hexpand(true);
            let name = gtk::Label::new(Some(&command.name));
            name.set_xalign(0.);
            labels.append(&name);
            let detail = gtk::Label::new(Some(&command.description));
            detail.set_xalign(0.);
            detail.set_wrap(true);
            detail.add_css_class("field-label");
            labels.append(&detail);
            line.append(&labels);
            let shortcut = gtk::Label::new(Some(&command.shortcut));
            shortcut.add_css_class("field-label");
            line.append(&shortcut);
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&line));
            row.set_sensitive(command.widget.is_sensitive());
            self.list.append(&row);
            visible.push(index);
        }
        self.empty.set_visible(visible.is_empty());
        for i in 0..visible.len() {
            if let Some(row) = self.list.row_at_index(i as i32) {
                if row.is_sensitive() {
                    self.list.select_row(Some(&row));
                    break;
                }
            }
        }
    }
    fn activate(&self) {
        let Some(row) = self.list.selected_row() else {
            return;
        };
        let Some(&index) = self.visible.borrow().get(row.index() as usize) else {
            return;
        };
        let command = &self.commands[index];
        if !command.widget.is_sensitive() {
            return;
        }
        self.window.set_visible(false);
        command.widget.emit_clicked();
    }
}

#[cfg(test)]
mod tests {
    use super::matches;
    #[test]
    fn search_combines_words_in_any_order_and_filters_missing_terms() {
        assert!(matches(
            "export svg private vector ctrl alt s",
            "private svg"
        ));
        assert!(matches("export svg private vector ctrl alt s", "ctrl svg"));
        assert!(!matches("export svg private vector ctrl alt s", "svg pdf"));
        assert!(matches("export svg private vector ctrl alt s", "  "));
    }
}

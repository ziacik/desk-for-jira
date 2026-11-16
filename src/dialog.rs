//! Shared Escape-to-close handling for secondary GTK windows.

use gtk::prelude::*;

pub(crate) fn close_on_escape(window: &gtk::Window) {
    let keys = gtk::EventControllerKey::new();
    // Handle Escape even when a search field or another child has focus.
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_window = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            if let Some(window) = weak_window.upgrade() {
                window.close();
            }
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
}

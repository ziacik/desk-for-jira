use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::{glib, prelude::*};

use crate::preferences::{self, UiLayout};

const MIN_ZOOM: i32 = 70;
const MAX_ZOOM: i32 = 160;
const ZOOM_STEP: i32 = 10;

// GTK has no browser-style per-window zoom. Scale the custom CSS dimensions
// alongside the inherited text size, without changing the display's DPI.
fn scale_css_dimensions(source: &str, percent: i32) -> String {
    if percent == 100 {
        return source.to_owned();
    }

    let bytes = source.as_bytes();
    let mut result = String::with_capacity(source.len());
    let mut copy_from = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_digit()
            && (i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'.'))
        {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if bytes.get(i) == Some(&b'p') && bytes.get(i + 1) == Some(&b'x') {
                if let Ok(value) = source[start..i].parse::<f64>() {
                    result.push_str(&source[copy_from..start]);
                    result.push_str(&format!("{:.2}px", value * f64::from(percent) / 100.0));
                    i += 2;
                    copy_from = i;
                }
            }
        } else {
            i += 1;
        }
    }

    result.push_str(&source[copy_from..]);
    result
}

fn css_for_zoom(percent: i32) -> String {
    let mut css = scale_css_dimensions(include_str!("board.css"), percent);
    // Relative text sizes in board.css follow the inherited window font.
    css.push_str(&format!("\nwindow {{ font-size: {percent}%; }}\n"));
    css
}

fn set_zoom(
    target: i32,
    current: &Cell<i32>,
    css: &gtk::CssProvider,
    layout: &RefCell<UiLayout>,
) {
    let target = target.clamp(MIN_ZOOM, MAX_ZOOM);
    if target == current.get() {
        return;
    }

    css.load_from_string(&css_for_zoom(target));
    current.set(target);
    layout.borrow_mut().zoom_percent = target;
    if let Err(error) = preferences::save_layout(&layout.borrow()) {
        eprintln!("Could not save UI zoom: {error}");
    }
}

pub fn install(window: &adw::ApplicationWindow, layout: Rc<RefCell<UiLayout>>) {
    let current = Rc::new(Cell::new(layout.borrow().zoom_percent.clamp(MIN_ZOOM, MAX_ZOOM)));
    let css = gtk::CssProvider::new();
    css.load_from_string(&css_for_zoom(current.get()));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let current = current.clone();
        let css = css.clone();
        let layout = layout.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                || modifiers.intersects(
                    gtk::gdk::ModifierType::ALT_MASK | gtk::gdk::ModifierType::SUPER_MASK,
                )
            {
                return glib::Propagation::Proceed;
            }
            let target = match key {
                gtk::gdk::Key::plus | gtk::gdk::Key::equal | gtk::gdk::Key::KP_Add => {
                    current.get() + ZOOM_STEP
                }
                gtk::gdk::Key::minus | gtk::gdk::Key::KP_Subtract => {
                    current.get() - ZOOM_STEP
                }
                gtk::gdk::Key::_0 | gtk::gdk::Key::KP_0 => 100,
                _ => return glib::Propagation::Proceed,
            };
            set_zoom(target, &current, &css, &layout);
            glib::Propagation::Stop
        });
    }
    window.add_controller(keys);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let current = current.clone();
        let css = css.clone();
        let layout = layout.clone();
        let remainder = Cell::new(0.0_f64);
        scroll.connect_scroll(move |controller, _, dy| {
            if !controller
                .current_event_state()
                .contains(gtk::gdk::ModifierType::CONTROL_MASK)
            {
                remainder.set(0.0);
                return glib::Propagation::Proceed;
            }
            // Discrete wheels report whole steps; smooth touchpads can report
            // fractions. Accumulate fractions and consume Ctrl+scroll either way.
            let accumulated = (remainder.get() + dy).clamp(-3.0, 3.0);
            let steps = accumulated.trunc() as i32;
            remainder.set(accumulated - f64::from(steps));
            if steps != 0 {
                set_zoom(current.get() - steps * ZOOM_STEP, &current, &css, &layout);
            }
            glib::Propagation::Stop
        });
    }
    window.add_controller(scroll);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_dimensions_scale_without_touching_relative_font_sizes() {
        let css = "padding: 10px 2px; border: 0.4px; font-size: 1.2em;";
        assert_eq!(scale_css_dimensions(css, 100), css);
        assert_eq!(
            scale_css_dimensions(css, 125),
            "padding: 12.50px 2.50px; border: 0.50px; font-size: 1.2em;"
        );
    }

    #[test]
    fn css_zoom_changes_inherited_text_size() {
        assert!(css_for_zoom(110).ends_with("window { font-size: 110%; }\n"));
    }
}

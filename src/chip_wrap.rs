//! Natural-width, wrapping layout for ticket label chips.
//!
//! GtkFlowBox is a grid: a short chip inherits the width of a longer chip in
//! the same column. Here each child keeps its measured natural width, and only
//! the *row* wraps when the allocated width is exhausted.
use gtk::{glib, prelude::*, subclass::prelude::*};

const GAP: i32 = 5;

fn chip_positions(sizes: &[(i32, i32)], available_width: i32) -> (Vec<(i32, i32, i32, i32)>, i32) {
    let limit = available_width.max(1);
    let mut positions = Vec::with_capacity(sizes.len());
    let mut x = 0;
    let mut y = 0;
    let mut line_height = 0;
    for &(natural_width, height) in sizes {
        let width = natural_width.min(limit).max(1);
        if x > 0 && x + GAP + width > limit {
            x = 0;
            y += line_height + GAP;
            line_height = 0;
        }
        let left = if x == 0 { 0 } else { x + GAP };
        positions.push((left, y, width, height));
        x = left + width;
        line_height = line_height.max(height);
    }
    (positions, if sizes.is_empty() { 0 } else { y + line_height })
}

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct ChipWrapLayout;

    #[glib::object_subclass]
    impl ObjectSubclass for ChipWrapLayout {
        const NAME: &'static str = "DeskForJiraChipWrapLayout";
        type Type = super::ChipWrapLayout;
        type ParentType = gtk::LayoutManager;
    }

    impl ObjectImpl for ChipWrapLayout {}

    impl LayoutManagerImpl for ChipWrapLayout {
        fn request_mode(&self, _widget: &gtk::Widget) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(
            &self,
            widget: &gtk::Widget,
            orientation: gtk::Orientation,
            for_size: i32,
        ) -> (i32, i32, i32, i32) {
            let children = super::visible_children(widget);
            let widths: Vec<_> = children.iter()
                .map(|child| child.measure(gtk::Orientation::Horizontal, -1))
                .collect();
            if orientation == gtk::Orientation::Horizontal {
                let minimum = widths.iter().map(|(min, _, _, _)| *min).max().unwrap_or(0);
                let natural = widths.iter().map(|(_, nat, _, _)| nat).sum::<i32>()
                    + GAP * widths.len().saturating_sub(1) as i32;
                return (minimum, natural, -1, -1);
            }

            let natural_width = widths.iter().map(|(_, nat, _, _)| nat).sum::<i32>()
                + GAP * widths.len().saturating_sub(1) as i32;
            let available = if for_size < 0 { natural_width } else { for_size };
            let sizes: Vec<_> = children.iter().zip(widths.iter())
                .map(|(child, (_, nat, _, _))| {
                    let width = (*nat).min(available.max(1));
                    let (_, height, _, _) = child.measure(gtk::Orientation::Vertical, width);
                    (width, height)
                })
                .collect();
            let (_, height) = super::chip_positions(&sizes, available);
            (height, height, -1, -1)
        }

        fn allocate(&self, widget: &gtk::Widget, width: i32, _height: i32, _baseline: i32) {
            let children = super::visible_children(widget);
            let sizes: Vec<_> = children.iter().map(|child| {
                let (_, natural_width, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
                let w = natural_width.min(width.max(1));
                let (_, natural_height, _, _) = child.measure(gtk::Orientation::Vertical, w);
                (w, natural_height)
            }).collect();
            let (positions, _) = super::chip_positions(&sizes, width);
            for (child, (x, y, w, h)) in children.iter().zip(positions) {
                child.size_allocate(&gtk::Allocation::new(x, y, w, h), -1);
            }
        }
    }

    #[derive(Debug, Default)]
    pub struct ChipWrap;

    #[glib::object_subclass]
    impl ObjectSubclass for ChipWrap {
        const NAME: &'static str = "DeskForJiraChipWrap";
        type Type = super::ChipWrap;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<super::ChipWrapLayout>();
        }
    }

    impl ObjectImpl for ChipWrap {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }
    impl WidgetImpl for ChipWrap {}
}

fn visible_children(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut children = Vec::new();
    let mut child = widget.first_child();
    while let Some(current) = child {
        child = current.next_sibling();
        if current.should_layout() {
            children.push(current);
        }
    }
    children
}

glib::wrapper! {
    pub struct ChipWrapLayout(ObjectSubclass<imp::ChipWrapLayout>)
        @extends gtk::LayoutManager;
}

glib::wrapper! {
    pub struct ChipWrap(ObjectSubclass<imp::ChipWrap>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ChipWrap {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ChipWrap {
    pub fn append(&self, child: &impl IsA<gtk::Widget>) {
        child.set_parent(self);
    }

    pub fn clear(&self) {
        while let Some(child) = self.first_child() {
            child.unparent();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_keep_their_own_width_and_wrap_without_grid_columns() {
        let (items, height) = chip_positions(
            &[(44, 24), (46, 24), (51, 24), (58, 24), (166, 24)], 240,
        );
        assert_eq!(items, vec![
            (0, 0, 44, 24),
            (49, 0, 46, 24),
            (100, 0, 51, 24),
            (156, 0, 58, 24),
            (0, 29, 166, 24),
        ]);
        assert_eq!(height, 53);
    }

    #[test]
    fn oversized_label_is_constrained_to_available_width() {
        let (items, height) = chip_positions(&[(200, 22), (30, 22)], 100);
        assert_eq!(items, vec![(0, 0, 100, 22), (0, 27, 30, 22)]);
        assert_eq!(height, 49);
    }
}

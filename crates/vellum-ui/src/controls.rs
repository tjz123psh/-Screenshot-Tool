//! Reusable building blocks for the settings panel.
//!
//! The panel is assembled from a handful of repeated shapes — a labelled row
//! with its control on the right, a card that groups rows, an inset text field,
//! a compact stepper — and keeping them here means the widget structure and the
//! CSS class it depends on stay in one place instead of drifting apart across
//! three pages. theme.rs owns every value; this module only owns the shape.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Entry, Image, Label, Orientation, Overlay, PasswordEntry,
    Separator, SpinButton, Stack, StackTransitionType, ToggleButton,
};

/// Status-dot colours. theme.rs owns the actual values; the panel picks one of
/// these so a dot and the text beside it cannot disagree.
pub const DOT_READY: &str = "vellum-dot-ready";
pub const DOT_MISSING: &str = "vellum-dot-missing";
pub const DOT_INFO: &str = "vellum-dot-info";

/// The label stack shared by both row shapes.
fn row_text(title: &str, subtitle: Option<&str>) -> GtkBox {
    let text = GtkBox::new(Orientation::Vertical, 3);
    text.set_hexpand(true);
    text.set_valign(Align::Center);

    let title = Label::builder().label(title).xalign(0.0).build();
    title.add_css_class("vellum-row-title");
    text.append(&title);

    if let Some(subtitle) = subtitle {
        let note = Label::builder()
            .label(subtitle)
            .xalign(0.0)
            .wrap(true)
            .build();
        note.add_css_class("vellum-row-sub");
        text.append(&note);
    }
    text
}

/// Name and (optional) one-line description on the left, control on the right.
///
/// This is the shape every modern settings surface uses, and it is what keeps a
/// long form scannable: the eye reads a column of names, not a column of boxes.
pub fn action_row(title: &str, subtitle: Option<&str>, control: &impl IsA<gtk4::Widget>) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 16);
    row.add_css_class("vellum-row");
    row.append(&row_text(title, subtitle));
    control.set_valign(Align::Center);
    row.append(control);
    row
}

/// Name and description above, control on its own line.
///
/// Wide controls — a URL field, a model picker, a segmented choice — look
/// broken squeezed into the right third of a row, so they get the full column
/// width with the label stacked over them.
pub fn action_row_stacked(
    title: &str,
    subtitle: Option<&str>,
    control: &impl IsA<gtk4::Widget>,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Vertical, 7);
    row.add_css_class("vellum-row");
    row.add_css_class("vellum-row-stacked");
    row.append(&row_text(title, subtitle));
    control.set_valign(Align::Center);
    row.append(control);
    row
}

/// A titled card that groups related rows.
///
/// Returns (card, body, head): append rows to body, trailing actions (the
/// 获取模型 button) to head.
pub fn card(title: &str, subtitle: Option<&str>) -> (GtkBox, GtkBox, GtkBox) {
    let card = GtkBox::new(Orientation::Vertical, 0);
    card.add_css_class("vellum-section-card");

    let body = GtkBox::new(Orientation::Vertical, 10);
    body.set_margin_top(14);
    body.set_margin_bottom(15);
    body.set_margin_start(16);
    body.set_margin_end(16);

    let head = GtkBox::new(Orientation::Horizontal, 10);
    let titles = GtkBox::new(Orientation::Vertical, 3);
    titles.set_hexpand(true);
    let title_label = Label::builder().label(title).xalign(0.0).build();
    title_label.add_css_class("vellum-section-title");
    titles.append(&title_label);
    if let Some(subtitle) = subtitle {
        let note = Label::builder()
            .label(subtitle)
            .xalign(0.0)
            .wrap(true)
            .build();
        note.add_css_class("vellum-section-hint");
        titles.append(&note);
    }
    head.append(&titles);
    body.append(&head);
    card.append(&body);
    (card, body, head)
}

/// A group of rows inside a card.
///
/// push_row inserts the hairline separators, so the caller never has to remember
/// where they go — or to leave one off the last row, which is the classic way
/// these lists end up looking hand-made.
pub fn row_group() -> GtkBox {
    let group = GtkBox::new(Orientation::Vertical, 0);
    group.add_css_class("vellum-rows");
    group
}

/// Appends one row to a row_group, separating it from the previous one.
pub fn push_row(group: &GtkBox, row: &impl IsA<gtk4::Widget>) {
    if group.first_child().is_some() {
        let separator = Separator::new(Orientation::Horizontal);
        separator.add_css_class("vellum-row-separator");
        group.append(&separator);
    }
    group.append(row);
}

/// The API key field: a masked entry and a plain one sharing the same text,
/// with the reveal toggle drawn inside the field.
///
/// A bare PasswordEntry only offers a press-and-hold peek icon, which a keyboard
/// user cannot reach and which hides the value again on release; this keeps the
/// explicit, sticky reveal while dropping the detached 显示 button that used to
/// sit next to the field.
pub struct SecretEntry {
    pub root: Overlay,
    masked: PasswordEntry,
    plain: Entry,
}

impl SecretEntry {
    pub fn new(placeholder: &str) -> Self {
        let masked = PasswordEntry::builder()
            .placeholder_text(placeholder)
            .show_peek_icon(false)
            .hexpand(true)
            .build();
        masked.add_css_class("vellum-inset");
        masked.add_css_class("vellum-secret-entry");

        let plain = Entry::builder()
            .placeholder_text(placeholder)
            .hexpand(true)
            .build();
        plain.add_css_class("vellum-inset");
        plain.add_css_class("vellum-secret-entry");

        let stack = Stack::builder()
            .transition_type(StackTransitionType::None)
            .hexpand(true)
            .build();
        stack.add_named(&masked, Some("masked"));
        stack.add_named(&plain, Some("plain"));
        stack.set_visible_child_name("masked");

        // Keep both entries on the same text. Comparing before assigning stops
        // the two changed handlers from ping-ponging.
        let other = plain.clone();
        masked.connect_changed(move |entry| {
            if other.text() != entry.text() {
                other.set_text(&entry.text());
            }
        });
        let other = masked.clone();
        plain.connect_changed(move |entry| {
            if other.text() != entry.text() {
                other.set_text(&entry.text());
            }
        });

        let toggle = ToggleButton::builder()
            .halign(Align::End)
            .valign(Align::Center)
            .tooltip_text("显示密钥")
            .build();
        toggle.set_icon_name("view-reveal-symbolic");
        toggle.set_margin_end(5);
        toggle.add_css_class("vellum-input-action");

        let target = stack.clone();
        toggle.connect_toggled(move |button| {
            let showing = button.is_active();
            target.set_visible_child_name(if showing { "plain" } else { "masked" });
            button.set_icon_name(if showing {
                "view-conceal-symbolic"
            } else {
                "view-reveal-symbolic"
            });
            button.set_tooltip_text(Some(if showing {
                "隐藏密钥"
            } else {
                "显示密钥"
            }));
        });

        let root = Overlay::builder().child(&stack).build();
        root.add_overlay(&toggle);

        Self {
            root,
            masked,
            plain,
        }
    }

    pub fn text(&self) -> String {
        self.masked.text().to_string()
    }

    pub fn set_text(&self, text: &str) {
        self.masked.set_text(text);
        if self.plain.text() != text {
            self.plain.set_text(text);
        }
    }

    /// Fires for edits in either the masked or the revealed entry.
    pub fn connect_changed(&self, callback: Rc<dyn Fn()>) {
        let masked = Rc::clone(&callback);
        self.masked.connect_changed(move |_| masked());
        self.plain.connect_changed(move |_| callback());
    }
}

/// A compact number field: the inset comes from the container, the spin button
/// itself is reduced to plain text, and the unit is spelled out at the end
/// rather than left to the label above.
///
/// The caller creates the spin button so its range, digits and value keep
/// working exactly as before; this only changes the chrome around it.
pub fn stepper(spin: &SpinButton, unit: &str) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 0);
    row.add_css_class("vellum-stepper");

    spin.set_valign(Align::Center);
    row.append(spin);

    let unit = Label::builder().label(unit).build();
    unit.add_css_class("vellum-unit");
    unit.set_valign(Align::Center);
    unit.set_margin_start(2);
    unit.set_margin_end(8);
    row.append(&unit);

    row
}

/// A secondary action: translucent fill, hairline border, gentle hover.
pub fn secondary_button(label: &str, icon: Option<&str>) -> Button {
    let button = Button::builder().tooltip_text(label).build();
    match icon {
        Some(icon) => {
            let content = GtkBox::new(Orientation::Horizontal, 7);
            let image = Image::from_icon_name(icon);
            image.set_pixel_size(14);
            content.append(&image);
            content.append(&Label::new(Some(label)));
            button.set_child(Some(&content));
        }
        None => button.set_label(label),
    }
    button.add_css_class("vellum-secondary");
    button
}

/// The one filled action of the window.
pub fn primary_button(label: &str) -> Button {
    let button = Button::builder().label(label).build();
    button.add_css_class("vellum-primary");
    button
}

/// A small glowing dot, used beside a state line.
pub fn status_dot(class: &str) -> GtkBox {
    let dot = GtkBox::new(Orientation::Horizontal, 0);
    dot.add_css_class("vellum-dot");
    dot.add_css_class(class);
    dot.set_valign(Align::Center);
    dot
}

/// A low-saturation pill with its own dot: the window's state read-out.
///
/// The dot's colour follows the pill's class through the stylesheet, so text and
/// dot can never contradict each other.
pub struct StatusPill {
    pub root: GtkBox,
    pub label: Label,
}

impl StatusPill {
    pub fn new(text: &str, class: &str) -> Self {
        let root = GtkBox::new(Orientation::Horizontal, 6);
        root.add_css_class("vellum-pill");
        root.add_css_class(class);
        root.set_valign(Align::Center);
        root.append(&status_dot(DOT_INFO));

        let label = Label::new(Some(text));
        label.add_css_class("vellum-pill-text");
        root.append(&label);

        Self { root, label }
    }

    /// Rewrites the text and swaps the state class in one step, so the pill is
    /// never briefly green with grey text (or the other way round).
    pub fn set_state(&self, text: &str, class: &str) {
        self.label.set_label(text);
        for known in ["vellum-success", "vellum-error"] {
            self.root.remove_css_class(known);
        }
        self.root.add_css_class(class);
    }
}

/// A rounded segmented control. The caller keeps the concrete widgets (to call
/// set_group and read is_active); this only provides the shell and marks each
/// child as a segment.
pub fn segmented(children: &[&impl IsA<gtk4::Widget>]) -> GtkBox {
    let segmented = GtkBox::new(Orientation::Horizontal, 2);
    segmented.add_css_class("vellum-segmented");
    for child in children {
        child.add_css_class("vellum-segment");
        segmented.append(*child);
    }
    segmented
}

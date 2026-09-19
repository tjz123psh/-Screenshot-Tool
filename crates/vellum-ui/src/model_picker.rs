//! A model field that accepts typing *and* a list fetched from the endpoint.
//!
//! Hand-typing a model id is the failure mode this avoids: every OpenAI-compatible
//! endpoint names its models differently (a Gemini endpoint 404s `gpt-4o-mini`),
//! and the only authority on the names is the endpoint's own `/models`.
//!
//! The list is a popover rather than an inline dropdown because the value has to
//! stay typeable: an endpoint can serve models it never advertises, and a user who
//! already knows the id should not have to fetch first.
//!
//! Three choices here are load-bearing and easy to "simplify" into bugs:
//!
//! * A row's label is a *display* name; the id it commits is the endpoint's own
//!   string, so shortening a name never changes what reaches the entry.
//! * The list never asks for more width than the entry has, so the popover cannot
//!   stretch the panel it hangs from; long ids ellipsize inside the row instead.
//! * The highlight is this module's own cursor, mirrored onto the row's label as
//!   `StateFlags::SELECTED`. GTK keeps `:selected` on the `ListBoxRow`, where the
//!   theme's `label.vellum-popover-row` rules cannot see it, so the list is left
//!   unselectable and hover and keyboard selection stay the same pill.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use gtk4::gdk::Key;
use gtk4::glib::{Propagation, WeakRef};
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Entry, EventControllerKey, Image, Label, ListBox, ListBoxRow, MenuButton,
    Orientation, PolicyType, Popover, PropagationPhase, ScrolledWindow, SearchEntry, SelectionMode,
    StateFlags, Viewport,
};
use pango::EllipsizeMode;

/// The list scrolls past this instead of covering the panel it belongs to.
const LIST_MAX_HEIGHT: i32 = 320;

/// An id longer than this is unreadable in a row anyway, and the row's tooltip
/// keeps the whole string. Counted in characters — see [`truncate_chars`].
const MAX_DISPLAY_CHARS: usize = 64;

/// The dropdown chevron is drawn at this size, one step under GTK's 16px default,
/// so the button reads as part of the field rather than a toolbar icon.
const ARROW_PIXELS: i32 = 14;

/// What the button says before a fetch ever ran. It is replaced by the model
/// count once one has (see [`ModelPicker::set_models`]).
const BUTTON_TOOLTIP: &str = "选择模型";

/// Rows are measured at this width at most. Pango ellipsizes whatever width a row
/// is actually given, so this only stops a long list of long ids from asking for
/// a popover wider than the field it drops down from.
const ROW_MEASURE_CHARS: i32 = 40;

/// Case-insensitive substring filter used by the popover's search field.
///
/// Matching anywhere is deliberate: endpoint ids are qualified
/// (`models/gemini-2.5-flash`), so users search for the part they remember
/// rather than the prefix they would have to know.
pub fn filter_models(models: &[String], query: &str) -> Vec<String> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return models.to_vec();
    }
    models
        .iter()
        .filter(|model| model.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// Drop blank ids and repeats, keeping the endpoint's own order.
///
/// `/models` answers do repeat ids — a proxy that merges several upstreams is the
/// usual source — and a picker that lists the same name twice reads as broken.
/// Repeats are compared case-insensitively (no endpoint ships two ids that differ
/// only in case) and the first spelling wins.
fn dedupe_models(models: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for model in models {
        let name = model.trim();
        if name.is_empty() || !seen.insert(name.to_lowercase()) {
            continue;
        }
        unique.push(name.to_string());
    }
    unique
}

/// The name a row shows for `model`.
///
/// The path an endpoint puts in front of every id (`models/…` on Gemini) is noise
/// in a list where every row carries it. Stripping it is display only: the entry
/// still receives the exact string the endpoint expects.
fn display_name(model: &str) -> String {
    truncate_chars(
        model.strip_prefix("models/").unwrap_or(model),
        MAX_DISPLAY_CHARS,
    )
}

/// Clip `text` to at most `max` characters, marking the cut with an ellipsis.
///
/// Counted in characters, not bytes: model ids do contain non-ASCII names, and a
/// byte-index slice would panic on them.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    // The ellipsis is part of the budget, so the result never exceeds `max`.
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// Where Up/Down leaves the cursor.
///
/// `None` means nothing was highlighted yet; the first press enters the list from
/// the end it is heading towards, which is what makes a single Down land on the
/// first row instead of skipping it.
fn next_cursor(current: Option<usize>, delta: i32, count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let last = (count - 1) as i32;
    match current {
        Some(index) => Some((index as i32 + delta).clamp(0, last) as usize),
        None if delta > 0 => Some(0),
        None => Some(last as usize),
    }
}

/// What the popover shows and where its keyboard cursor is.
///
/// Kept in one place so the search field, the rows and the cursor cannot drift
/// apart: every refill resets all three together.
#[derive(Default)]
struct ListState {
    /// Every id the endpoint reported, deduped, in its own order.
    models: RefCell<Vec<String>>,
    /// The ids the rows currently stand for, in row order.
    displayed: RefCell<Vec<String>>,
    /// Highlighted row, as an index into `displayed`.
    cursor: Cell<Option<usize>>,
    /// The label carrying `StateFlags::SELECTED`, so a move repaints two rows
    /// instead of walking a list that can hold hundreds.
    highlighted: RefCell<Option<Label>>,
}

impl ListState {
    /// Adopt a freshly fetched list. Deduping happens here rather than at the call
    /// site so every entry point gets the same treatment.
    fn set_models(&self, models: &[String]) {
        *self.models.borrow_mut() = dedupe_models(models);
    }

    fn count(&self) -> usize {
        self.models.borrow().len()
    }

    /// Rebuild the rows for `query` and reset the cursor: the row it pointed at
    /// may be gone.
    fn refill(&self, list: &ListBox, viewport: &Viewport, query: &str) {
        // Drop the highlight before the rows it lives on are removed.
        self.clear_highlight();
        let filtered = filter_models(&self.models.borrow(), query);
        let unfetched = self.models.borrow().is_empty();
        list.remove_all();
        if filtered.is_empty() {
            append_hint(list, unfetched);
        } else {
            for model in &filtered {
                append_row(list, model);
            }
        }
        *self.displayed.borrow_mut() = filtered;
        // Land on the first match: that is where Down and Enter expect to start,
        // and the highlighted row says what Enter would commit.
        let first = (!self.displayed.borrow().is_empty()).then_some(0);
        self.highlight(list, viewport, first);
    }

    /// Move the highlight to `index` (or drop it) and keep the row in view.
    fn highlight(&self, list: &ListBox, viewport: &Viewport, index: Option<usize>) {
        self.clear_highlight();
        let Some(index) = index else {
            return;
        };
        let Some(row) = list.row_at_index(index as i32) else {
            return;
        };
        if let Some(label) = row_label(&row) {
            // Mirrored from the ListBoxRow, which is where GTK would keep it.
            label.set_state_flags(StateFlags::SELECTED, false);
            *self.highlighted.borrow_mut() = Some(label);
        }
        self.cursor.set(Some(index));
        // The search field holds the focus, so ListBox will not scroll the cursor
        // row into view on its own.
        viewport.scroll_to(&row, None);
    }

    /// Forget the highlight. Only the label that had it is touched.
    fn clear_highlight(&self) {
        if let Some(label) = self.highlighted.borrow_mut().take() {
            label.unset_state_flags(StateFlags::SELECTED);
        }
        self.cursor.set(None);
    }

    /// Move the cursor `delta` rows and highlight where it lands.
    fn step(&self, list: &ListBox, viewport: &Viewport, delta: i32) {
        let next = next_cursor(self.cursor.get(), delta, self.displayed.borrow().len());
        // Pressing on at either end must not re-scroll the row that is already
        // highlighted.
        if next != self.cursor.get() {
            self.highlight(list, viewport, next);
        }
    }

    /// The id under the cursor: what Enter, Space or a click commits.
    fn selected(&self) -> Option<String> {
        self.model_at(i32::try_from(self.cursor.get()?).ok()?)
    }

    /// The id a row stands for, by row index.
    fn model_at(&self, index: i32) -> Option<String> {
        let index = usize::try_from(index).ok()?;
        self.displayed.borrow().get(index).cloned()
    }

    /// Highlight the row the entry already holds, so reopening the list shows where
    /// the current value sits. An id the endpoint did not advertise falls back to
    /// the first row, which is where a fresh cursor would start anyway.
    fn highlight_current(&self, list: &ListBox, viewport: &Viewport, current: &str) {
        let current = current.trim();
        let index = {
            let displayed = self.displayed.borrow();
            displayed
                .iter()
                .position(|model| model.eq_ignore_ascii_case(current))
                .or_else(|| (!displayed.is_empty()).then_some(0))
        };
        self.highlight(list, viewport, index);
    }
}

/// An editable model entry plus a dropdown of the models the endpoint reported.
pub struct ModelPicker {
    pub root: GtkBox,
    pub entry: Entry,
    button: MenuButton,
    list: ListBox,
    search: SearchEntry,
    viewport: Viewport,
    state: Rc<ListState>,
}

impl ModelPicker {
    pub fn new(label: &str, hint: Option<&str>, placeholder: &str) -> Self {
        // Title, field and hint are one stacked row: the same small gap the
        // card's other rows use, so the picker lines up with its neighbours.
        let root = GtkBox::new(Orientation::Vertical, 6);
        let title = Label::builder().label(label).xalign(0.0).build();
        title.add_css_class("vellum-row-title");
        root.append(&title);

        let row = GtkBox::new(Orientation::Horizontal, 8);
        let entry = Entry::builder()
            .placeholder_text(placeholder)
            .hexpand(true)
            .build();
        // Two classes on purpose: the shared inset paints the recessed slot the
        // rest of the form uses, and the picker class keeps the field's own
        // metrics beside its button.
        entry.add_css_class("vellum-inset");
        entry.add_css_class("vellum-picker-entry");
        row.append(&entry);

        let popover = Popover::builder().has_arrow(false).build();
        popover.add_css_class("vellum-popover");
        let content = GtkBox::new(Orientation::Vertical, 8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(8);
        content.set_margin_end(8);
        let search = SearchEntry::builder().placeholder_text("搜索模型").build();
        search.add_css_class("vellum-inset");
        content.append(&search);

        let list = ListBox::new();
        list.add_css_class("vellum-popover-list");
        // The highlight is the cursor below, not GTK's selection: a selected
        // ListBoxRow keeps `:selected` on the row, out of reach of the theme's
        // `label.vellum-popover-row` rules, and would paint a second highlight in
        // whatever colours the user's theme picked.
        list.set_selection_mode(SelectionMode::None);
        // An explicit viewport: the one GtkScrolledWindow creates for a
        // non-scrollable child is not reachable from here, and the cursor needs
        // `scroll_to` to stay visible.
        let viewport = Viewport::builder().child(&list).build();
        let scroller = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .vscrollbar_policy(PolicyType::Automatic)
            .propagate_natural_height(true)
            .max_content_height(LIST_MAX_HEIGHT)
            .child(&viewport)
            .build();
        content.append(&scroller);
        popover.set_child(Some(&content));

        // A single chevron, sized here instead of left to the icon theme, and no
        // always-show-arrow: the button is an affordance for the field, not a
        // second piece of chrome standing next to it.
        let button = MenuButton::builder()
            .popover(&popover)
            .always_show_arrow(false)
            .tooltip_text(BUTTON_TOOLTIP)
            .build();
        button.set_child(Some(
            &Image::builder()
                .icon_name("pan-down-symbolic")
                .pixel_size(ARROW_PIXELS)
                .build(),
        ));
        button.add_css_class("vellum-picker-button");
        row.append(&button);
        root.append(&row);

        if let Some(hint) = hint {
            let note = Label::builder().label(hint).xalign(0.0).wrap(true).build();
            note.add_css_class("vellum-caption");
            root.append(&note);
        }

        let state = Rc::new(ListState::default());

        // Everything the list needs when it becomes visible hangs off the popover's
        // `map` rather than the button's `activate`: a mouse click goes straight to
        // the popover's own toggle and never emits `activate`, so a refill wired to
        // that signal left the first opening with an empty list.
        let entry_for_show = entry.clone();
        let search_for_show = search.clone();
        let list_for_show = list.clone();
        let viewport_for_show = viewport.clone();
        let state_for_show = Rc::clone(&state);
        popover.connect_map(move |popover| {
            // The popover is as wide as the field it belongs to, and the entry has
            // no width until the panel is laid out: measured here, which is also
            // early enough that the first opening is drawn at the right size
            // instead of resizing a frame later.
            let width = entry_for_show.width();
            if width > 0 {
                popover.set_size_request(width, -1);
            }
            // A query left over from last time would hide the models just fetched,
            // so every open starts from the whole list. Clearing a non-empty field
            // refills through `search-changed`; an already empty one has to refill
            // by hand because no signal would fire.
            if search_for_show.text().is_empty() {
                state_for_show.refill(&list_for_show, &viewport_for_show, "");
            } else {
                search_for_show.set_text("");
            }
            // Typing should work without a second click, and a focus left on a row
            // from last time must not swallow the first keystroke.
            search_for_show.grab_focus();
            state_for_show.highlight_current(
                &list_for_show,
                &viewport_for_show,
                &entry_for_show.text(),
            );
        });

        let list_for_search = list.clone();
        let viewport_for_search = viewport.clone();
        let state_for_search = Rc::clone(&state);
        search.connect_search_changed(move |search| {
            state_for_search.refill(
                &list_for_search,
                &viewport_for_search,
                search.text().as_str(),
            );
        });

        let entry_for_row = entry.clone();
        let button_for_row = button.downgrade();
        let state_for_row = Rc::clone(&state);
        list.connect_row_activated(move |_, row| {
            let Some(model) = state_for_row.model_at(row.index()) else {
                return;
            };
            // Weak on purpose: this handler lives on a widget inside the popover
            // and the button owns the popover, so a strong reference here would
            // keep the whole subtree alive after the panel is gone.
            if let Some(button) = button_for_row.upgrade() {
                commit(&entry_for_row, &button, &model);
            }
        });

        let entry_for_keys = entry.clone();
        let search_for_keys = search.clone();
        let list_for_keys = list.clone();
        let viewport_for_keys = viewport.clone();
        let button_for_keys = button.downgrade();
        let state_for_keys = Rc::clone(&state);
        let keys = EventControllerKey::new();
        // Capture: the search field holds the focus and would otherwise swallow
        // Return before the list ever sees it.
        keys.set_propagation_phase(PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| {
            match key {
                Key::Escape => {
                    if let Some(button) = button_for_keys.upgrade() {
                        button.popdown();
                    }
                }
                Key::Up | Key::KP_Up => state_for_keys.step(&list_for_keys, &viewport_for_keys, -1),
                Key::Down | Key::KP_Down => {
                    state_for_keys.step(&list_for_keys, &viewport_for_keys, 1);
                }
                Key::Return | Key::KP_Enter => {
                    commit_cursor(&state_for_keys, &entry_for_keys, &button_for_keys);
                }
                // A space is a character while the search field is focused; it
                // commits only once the list itself has the keyboard.
                Key::space if !search_for_keys.has_focus() => {
                    commit_cursor(&state_for_keys, &entry_for_keys, &button_for_keys);
                }
                _ => return Propagation::Proceed,
            }
            Propagation::Stop
        });
        content.add_controller(keys);

        Self {
            root,
            entry,
            button,
            list,
            search,
            viewport,
            state,
        }
    }

    pub fn text(&self) -> String {
        self.entry.text().to_string()
    }

    pub fn set_text(&self, text: &str) {
        self.entry.set_text(text);
    }

    pub fn connect_changed(&self, callback: Rc<dyn Fn()>) {
        self.entry.connect_changed(move |_| callback());
    }

    /// Open the list without a click.
    ///
    /// For callers that drive the picker themselves — the panel's screenshot
    /// helper does — since a popover is otherwise only reachable with a pointer.
    pub fn popup(&self) {
        self.button.popup();
    }

    /// Replace the selectable models. An empty list means "nothing fetched yet".
    pub fn set_models(&self, models: &[String]) {
        self.state.set_models(models);
        // Whatever the old query was filtering no longer describes this list, and
        // the fetch usually lands while the popover is closed.
        if self.search.text().is_empty() {
            self.state.refill(&self.list, &self.viewport, "");
        } else {
            // Clearing the field refills through `search-changed`.
            self.search.set_text("");
        }
        // The button is the only affordance that says whether a fetch ever ran, so
        // an empty list must not claim the endpoint answered with zero models.
        let tooltip = match self.state.count() {
            0 => BUTTON_TOOLTIP.to_string(),
            count => format!("选择模型（接口返回 {count} 个）"),
        };
        self.button.set_tooltip_text(Some(&tooltip));
    }
}

/// Put `model` in the entry and dismiss the list — the single action a click on a
/// row and every "choose this" key share.
fn commit(entry: &Entry, button: &MenuButton, model: &str) {
    entry.set_text(model);
    button.popdown();
}

/// Commit the row under the keyboard cursor, if there is one.
fn commit_cursor(state: &ListState, entry: &Entry, button: &WeakRef<MenuButton>) {
    if let (Some(model), Some(button)) = (state.selected(), button.upgrade()) {
        commit(entry, &button, &model);
    }
}

/// The label a row was built with (see [`append_row`]).
fn row_label(row: &ListBoxRow) -> Option<Label> {
    row.child().and_then(|child| child.downcast::<Label>().ok())
}

/// One selectable model row.
fn append_row(list: &ListBox, model: &str) {
    let label = Label::builder()
        .label(display_name(model))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(EllipsizeMode::Middle)
        .max_width_chars(ROW_MEASURE_CHARS)
        .build();
    label.add_css_class("vellum-popover-row");
    // 7px above and below a 13px line is the row's 30px rhythm; the theme owns the
    // rest (radius, hover fill). The 8px sides match the row's 8px radius so the
    // fill's corners are not clipped by the label box.
    label.set_margin_top(7);
    label.set_margin_bottom(7);
    label.set_margin_start(8);
    label.set_margin_end(8);
    // The row shows a shortened name, so the id it commits has to survive the
    // shortening somewhere the user can read it.
    label.set_tooltip_text(Some(model));
    list.append(&ListBoxRow::builder().child(&label).build());
}

/// The one row shown when there is nothing to pick, saying why.
fn append_hint(list: &ListBox, unfetched: bool) {
    let label = Label::builder()
        .label(if unfetched {
            "先点「获取模型」"
        } else {
            "没有匹配的模型"
        })
        .xalign(0.0)
        .build();
    label.add_css_class("vellum-caption");
    label.set_margin_top(10);
    label.set_margin_bottom(10);
    label.set_margin_start(12);
    label.set_margin_end(12);
    // A hint is not a choice: it must not take the click, the cursor or the focus
    // that a model row does.
    let row = ListBoxRow::builder()
        .child(&label)
        .activatable(false)
        .selectable(false)
        .can_focus(false)
        .build();
    list.append(&row);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<String> {
        vec![
            "models/gemini-2.5-flash".into(),
            "models/gemini-flash-latest".into(),
            "gpt-4o-mini".into(),
        ]
    }

    #[test]
    fn an_empty_query_lists_everything() {
        assert_eq!(filter_models(&models(), "").len(), 3);
        assert_eq!(filter_models(&models(), "   ").len(), 3);
    }

    #[test]
    fn the_query_matches_case_insensitively_anywhere() {
        let found = filter_models(&models(), "FLASH");
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|model| model.contains("flash")));
    }

    #[test]
    fn the_query_is_trimmed_before_matching() {
        assert_eq!(filter_models(&models(), "  gpt-4o  ").len(), 1);
    }

    #[test]
    fn an_unknown_query_yields_nothing() {
        assert!(filter_models(&models(), "llama").is_empty());
    }

    #[test]
    fn an_empty_list_yields_nothing_for_any_query() {
        assert!(filter_models(&[], "").is_empty());
        assert!(filter_models(&[], "flash").is_empty());
    }

    #[test]
    fn repeats_collapse_to_the_first_spelling() {
        let fetched = vec!["gpt-4o".into(), "GPT-4O".into(), "gpt-4o".into()];
        assert_eq!(dedupe_models(&fetched), vec!["gpt-4o"]);
    }

    #[test]
    fn blank_ids_are_dropped_and_the_rest_keeps_its_order() {
        let fetched = vec!["".into(), "  ".into(), "b".into(), "a".into()];
        assert_eq!(dedupe_models(&fetched), vec!["b", "a"]);
    }

    #[test]
    fn an_empty_fetch_dedupes_to_nothing() {
        assert!(dedupe_models(&[]).is_empty());
    }

    #[test]
    fn the_display_name_drops_the_endpoints_path_prefix() {
        assert_eq!(display_name("models/gemini-2.5-flash"), "gemini-2.5-flash");
        assert_eq!(display_name("gpt-4o-mini"), "gpt-4o-mini");
        // Only the prefix the endpoint repeats on every row goes.
        assert_eq!(display_name("openai/gpt-4o-mini"), "openai/gpt-4o-mini");
    }

    #[test]
    fn a_name_inside_the_budget_is_untouched() {
        assert_eq!(truncate_chars("gpt-4o-mini", 64), "gpt-4o-mini");
        assert_eq!(truncate_chars("abcd", 4), "abcd");
    }

    #[test]
    fn clipping_marks_the_cut_and_keeps_the_budget() {
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(
            truncate_chars(&"x".repeat(100), MAX_DISPLAY_CHARS)
                .chars()
                .count(),
            MAX_DISPLAY_CHARS
        );
    }

    #[test]
    fn clipping_counts_characters_not_bytes() {
        // A byte-index slice would panic on this input.
        let clipped = truncate_chars("模型模型模型", 3);
        assert_eq!(clipped, "模型…");
        assert_eq!(clipped.chars().count(), 3);
    }

    #[test]
    fn the_cursor_enters_from_the_end_it_heads_to() {
        assert_eq!(next_cursor(None, 1, 3), Some(0));
        assert_eq!(next_cursor(None, -1, 3), Some(2));
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        assert_eq!(next_cursor(Some(0), -1, 3), Some(0));
        assert_eq!(next_cursor(Some(2), 1, 3), Some(2));
        assert_eq!(next_cursor(Some(1), 1, 3), Some(2));
    }

    #[test]
    fn an_empty_list_has_no_cursor_to_move() {
        assert_eq!(next_cursor(None, 1, 0), None);
        assert_eq!(next_cursor(Some(0), 1, 0), None);
    }
}

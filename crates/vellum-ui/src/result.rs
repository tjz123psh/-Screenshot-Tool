//! Result window for extracted and translated text.
//!
//! Three decisions here are load-bearing:
//!
//! * The window opens *before* OCR runs, showing a placeholder. Recognition on
//!   a busy background can take a second or two, and a screenshot tool that
//!   shows nothing during that time reads as broken.
//! * `NON_UNIQUE` is mandatory. Under the default single-instance behaviour a
//!   second `text-file` process only forwards `activate` to the first one,
//!   which re-presents its *old* text and then exits, taking its `--cleanup`
//!   temp file with it. The new screenshot would never be recognised.
//! * The text view is editable. OCR gets a character wrong often enough that
//!   fixing it in place beats re-running the capture.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gtk4::gdk::Key;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, EventControllerKey, Image, Label,
    Orientation, ScrolledWindow, Separator, Spinner, TextView, WrapMode, gio, glib,
};
use pango::EllipsizeMode;
use vellum_core::Rgb8;
use vellum_core::config::Config;

use crate::{niri, theme};

const APP_ID: &str = "ai.vellum.result";
const WIDTH: i32 = 560;
const HEIGHT: i32 = 420;
/// Niri needs the window mapped before it can be floated.
const FLOAT_DELAY: Duration = Duration::from_millis(60);

/// Which pipeline produced the text on screen.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ocr,
    Translate,
}

impl Mode {
    fn badge(self) -> &'static str {
        match self {
            Mode::Ocr => "OCR",
            Mode::Translate => "译",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Mode::Ocr => "提取的文字",
            Mode::Translate => "翻译结果",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Mode::Ocr => "可直接编辑，再复制或翻译",
            Mode::Translate => "可直接编辑或复制到剪贴板",
        }
    }
}

thread_local! {
    /// Live windows, so a worker thread can address one without holding an
    /// `Rc`. `glib::idle_add_once` demands `Send`, and GTK widgets are not; the
    /// worker carries a plain `u64` and the main loop resolves it here. Weak
    /// references mean a window the user already closed simply disappears.
    static WINDOWS: RefCell<Vec<(u64, Weak<ResultWindow>)>> = const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// What a worker thread can ask the main loop to do.
///
/// Every variant is plain data so the message itself is `Send`.
enum Update {
    /// Replace the body text and the footer state in one step.
    Result {
        text: String,
        status: String,
        busy: bool,
        usable: bool,
    },
    /// Footer-only feedback; the body text is left alone.
    Flash {
        message: String,
        busy: bool,
        error: bool,
    },
    /// A translation finished: it gets its own window.
    Translation(String),
}

/// Hands `update` to the main loop for the window with `id`.
fn post(id: u64, update: Update) {
    glib::idle_add_once(move || {
        let target = WINDOWS.with(|slot| {
            slot.borrow()
                .iter()
                .find(|(known, _)| *known == id)
                .and_then(|(_, weak)| weak.upgrade())
        });
        if let Some(window) = target {
            window.apply(update);
        }
    });
}

struct ResultWindow {
    id: u64,
    app: Application,
    window: ApplicationWindow,
    text: TextView,
    status: Label,
    spinner: Spinner,
    copy_button: Button,
    translate_button: Option<Button>,
    /// Set from `close-request`. Late updates from a worker must not resurrect
    /// a window the user dismissed.
    closed: Cell<bool>,
    /// Guards against a second translation while one is in flight.
    translating: Cell<bool>,
}

impl ResultWindow {
    fn new(
        app: &Application,
        mode: Mode,
        text: &str,
        status: &str,
        busy: bool,
        usable: bool,
    ) -> Rc<Self> {
        theme::install_default();

        let window = ApplicationWindow::builder()
            .application(app)
            .default_width(WIDTH)
            .default_height(HEIGHT)
            .title(mode.title())
            .build();
        window.add_css_class("vellum-window");

        let root = GtkBox::new(Orientation::Vertical, 0);

        // The header is ours rather than a compositor title bar: the target
        // session runs without server-side decorations, so a plain window
        // would show no title at all.
        let header = GtkBox::new(Orientation::Horizontal, 12);
        header.set_margin_top(16);
        header.set_margin_bottom(14);
        header.set_margin_start(18);
        header.set_margin_end(14);

        let badge = Label::new(Some(mode.badge()));
        badge.add_css_class("vellum-status-chip");
        badge.set_valign(Align::Center);
        header.append(&badge);

        let titles = GtkBox::new(Orientation::Vertical, 2);
        titles.set_hexpand(true);
        let title = Label::builder().label(mode.title()).xalign(0.0).build();
        title.add_css_class("vellum-title");
        let subtitle = Label::builder().label(mode.subtitle()).xalign(0.0).build();
        subtitle.add_css_class("vellum-dim");
        titles.append(&title);
        titles.append(&subtitle);
        header.append(&titles);

        let close = Button::builder()
            .child(&Image::from_icon_name("window-close-symbolic"))
            .tooltip_text("关闭")
            .valign(Align::Center)
            .build();
        close.add_css_class("vellum-quiet");
        close.add_css_class("vellum-icon-button");
        header.append(&close);
        root.append(&header);

        let divider = Separator::new(Orientation::Horizontal);
        divider.add_css_class("vellum-divider");
        root.append(&divider);

        let shell = GtkBox::new(Orientation::Vertical, 0);
        shell.add_css_class("vellum-text-shell");
        shell.set_margin_top(14);
        shell.set_margin_bottom(12);
        shell.set_margin_start(18);
        shell.set_margin_end(18);
        shell.set_vexpand(true);

        let view = TextView::builder()
            .wrap_mode(WrapMode::WordChar)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(4)
            .margin_end(4)
            .build();
        view.add_css_class("vellum-textview");
        view.buffer().set_text(text);

        let scroller = ScrolledWindow::builder().vexpand(true).child(&view).build();
        shell.append(&scroller);
        root.append(&shell);

        // The footer keeps a fixed slot for transient feedback so the buttons
        // do not jump sideways when a message appears.
        let footer = GtkBox::new(Orientation::Horizontal, 12);
        footer.set_margin_bottom(16);
        footer.set_margin_start(18);
        footer.set_margin_end(18);

        let status_row = GtkBox::new(Orientation::Horizontal, 8);
        status_row.set_hexpand(true);
        status_row.set_valign(Align::Center);
        let spinner = Spinner::new();
        let status_label = Label::builder().label(status).xalign(0.0).build();
        status_label.add_css_class("vellum-dim");
        status_label.set_ellipsize(EllipsizeMode::End);
        status_row.append(&spinner);
        status_row.append(&status_label);
        footer.append(&status_row);

        let buttons = GtkBox::new(Orientation::Horizontal, 8);
        buttons.set_halign(Align::End);
        let copy_button = action_button("复制", "edit-copy-symbolic");
        let translate_button = match mode {
            Mode::Ocr => {
                let button = action_button("翻译", "preferences-desktop-locale-symbolic");
                button.add_css_class("suggested-action");
                Some(button)
            }
            Mode::Translate => {
                copy_button.add_css_class("suggested-action");
                None
            }
        };
        buttons.append(&copy_button);
        if let Some(button) = &translate_button {
            buttons.append(button);
        }
        footer.append(&buttons);
        root.append(&footer);

        window.set_child(Some(&root));

        let result = Rc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            app: app.clone(),
            window,
            text: view,
            status: status_label,
            spinner,
            copy_button,
            translate_button,
            closed: Cell::new(false),
            translating: Cell::new(false),
        });

        WINDOWS.with(|slot| slot.borrow_mut().push((result.id, Rc::downgrade(&result))));
        result.set_busy(busy);
        result.set_content_ready(usable);
        result.connect(&close);
        result
    }

    fn connect(self: &Rc<Self>, close: &Button) {
        let window = self.window.clone();
        close.connect_clicked(move |_| window.close());

        let this = Rc::downgrade(self);
        self.copy_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.copy();
            }
        });

        if let Some(button) = &self.translate_button {
            let this = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.translate();
                }
            });
        }

        let keys = EventControllerKey::new();
        let window = self.window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == Key::Escape {
                window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);

        let this = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(this) = this.upgrade() {
                this.closed.set(true);
            }
            glib::Propagation::Proceed
        });

        // Floating rather than tiled: a text result is something you read
        // beside the window you captured, not a new column in the scroll.
        self.window.connect_map(|_| {
            glib::timeout_add_local_once(FLOAT_DELAY, || {
                niri::move_focused_to_floating();
            });
        });
    }

    fn present(self: &Rc<Self>) {
        self.window.present();
    }

    fn apply(self: &Rc<Self>, update: Update) {
        if self.closed.get() {
            return;
        }
        match update {
            Update::Result {
                text,
                status,
                busy,
                usable,
            } => {
                self.text.buffer().set_text(&text);
                self.status.set_label(&status);
                self.status.remove_css_class("vellum-error");
                self.set_busy(busy);
                self.set_content_ready(usable);
            }
            Update::Flash {
                message,
                busy,
                error,
            } => self.flash(&message, busy, error),
            Update::Translation(text) => {
                self.translating.set(false);
                if let Some(button) = &self.translate_button {
                    button.set_sensitive(true);
                }
                self.flash("", false, false);
                let window = Self::new(&self.app, Mode::Translate, &text, "", false, true);
                window.present();
            }
        }
    }

    fn set_busy(&self, busy: bool) {
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
    }

    /// Enables editing and the action buttons only once real text is present.
    fn set_content_ready(&self, ready: bool) {
        self.text.set_editable(ready);
        self.text.set_cursor_visible(ready);
        self.copy_button.set_sensitive(ready);
        if let Some(button) = &self.translate_button {
            button.set_sensitive(ready && !self.translating.get());
        }
    }

    fn flash(&self, message: &str, busy: bool, error: bool) {
        self.status.set_label(message);
        if error {
            self.status.add_css_class("vellum-error");
        } else {
            self.status.remove_css_class("vellum-error");
        }
        self.set_busy(busy);
    }

    fn current_text(&self) -> String {
        let buffer = self.text.buffer();
        let (start, end) = buffer.bounds();
        buffer.text(&start, &end, false).to_string()
    }

    fn copy(self: &Rc<Self>) {
        match vellum_core::io::copy_text(&self.current_text()) {
            Ok(()) => self.flash("已复制到剪贴板", false, false),
            Err(err) => self.flash(&format!("复制失败: {err}"), false, true),
        }
    }

    fn translate(self: &Rc<Self>) {
        if self.translating.get() {
            return;
        }
        let text = self.current_text();
        if text.trim().is_empty() {
            self.flash("没有文本可翻译", false, true);
            return;
        }

        self.translating.set(true);
        if let Some(button) = &self.translate_button {
            button.set_sensitive(false);
        }
        self.flash("翻译中…", true, false);

        let id = self.id;
        std::thread::spawn(move || {
            let config = Config::load();
            match vellum_text::translate(&text, &config.llm) {
                Ok(translated) => post(id, Update::Translation(translated)),
                Err(err) => post(
                    id,
                    Update::Flash {
                        message: format!("翻译失败: {err}"),
                        busy: false,
                        error: true,
                    },
                ),
            }
        });
    }
}

/// Icon plus label, so the buttons read the same as the overlay toolbar.
fn action_button(label: &str, icon: &str) -> Button {
    let content = GtkBox::new(Orientation::Horizontal, 7);
    content.append(&Image::from_icon_name(icon));
    content.append(&Label::new(Some(label)));
    Button::builder()
        .child(&content)
        .tooltip_text(label)
        .build()
}

/// Recognises `image`, optionally translates it, and shows the outcome.
pub fn run_text_action(image: Rgb8, translate: bool) -> i32 {
    let mode = if translate {
        Mode::Translate
    } else {
        Mode::Ocr
    };
    let placeholder = if translate {
        "识别并翻译中…"
    } else {
        "识别中…"
    };
    let app = application();
    let payload = RefCell::new(Some(image));
    app.connect_activate(move |app| {
        let Some(image) = payload.borrow_mut().take() else {
            return;
        };
        let window = ResultWindow::new(app, mode, placeholder, placeholder, true, false);
        window.present();
        let id = window.id;
        std::thread::spawn(move || run_pipeline(id, image, translate));
    });
    run(&app)
}

/// The OCR (and optional translation) pipeline, off the main loop.
fn run_pipeline(id: u64, image: Rgb8, translate: bool) {
    let config = Config::load();
    let text = match vellum_text::recognize(&image, &config.ocr) {
        Ok(text) => text,
        Err(err) => {
            post(
                id,
                Update::Result {
                    text: format!("[错误] {err}"),
                    status: "处理失败".to_string(),
                    busy: false,
                    usable: false,
                },
            );
            return;
        }
    };

    if text.trim().is_empty() {
        post(
            id,
            Update::Result {
                text: "（未识别到文字）".to_string(),
                status: "未识别到文字".to_string(),
                busy: false,
                usable: false,
            },
        );
        return;
    }

    if !translate {
        post(
            id,
            Update::Result {
                text,
                status: String::new(),
                busy: false,
                usable: true,
            },
        );
        return;
    }

    // Show the recognised text while the translation runs: if translation
    // fails, the user still has something to copy.
    post(
        id,
        Update::Result {
            text: text.clone(),
            status: "翻译中…".to_string(),
            busy: true,
            usable: false,
        },
    );
    match vellum_text::translate(&text, &config.llm) {
        Ok(translated) => post(
            id,
            Update::Result {
                text: translated,
                status: String::new(),
                busy: false,
                usable: true,
            },
        ),
        Err(err) => post(
            id,
            Update::Result {
                text,
                status: format!("翻译失败: {err}"),
                busy: false,
                usable: true,
            },
        ),
    }
}

fn application() -> Application {
    Application::builder()
        .application_id(APP_ID)
        // See the module docs: a second text action must be its own process.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build()
}

fn run(app: &Application) -> i32 {
    let empty: [String; 0] = [];
    i32::from(app.run_with_args(&empty).get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_mode_has_its_own_copy() {
        assert_ne!(Mode::Ocr.title(), Mode::Translate.title());
        assert_ne!(Mode::Ocr.subtitle(), Mode::Translate.subtitle());
        assert_ne!(Mode::Ocr.badge(), Mode::Translate.badge());
    }
}

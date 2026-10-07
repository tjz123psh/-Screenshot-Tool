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
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gtk4::gdk::Key;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, EventControllerKey, FlowBox,
    Image, Label, Orientation, PolicyType, ScrolledWindow, SelectionMode, Separator, Spinner,
    TextView, WrapMode, gio, glib,
};
use pango::EllipsizeMode;
use vellum_core::Rgb8;
use vellum_core::config::Config;

use crate::document::{Document, SharedDocument, Snapshot};
use crate::ui_job::{JobState, WorkerSlot};
use crate::{theme, ui_job};
use vellum_text::api::RequestControl;

const APP_ID: &str = "ai.vellum.result";
const WIDTH: i32 = 640;
const HEIGHT: i32 = 460;
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
    /// Own windows until close-request. Workers carry only data; main-loop
    /// callbacks use weak references and request generations. Without this
    /// owner, a window would lose its controller after activate returns.
    static WINDOWS: RefCell<Vec<(u64, Rc<ResultWindow>)>> = const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const DOCUMENT_POLL: Duration = Duration::from_millis(250);

/// A revision alone is not an identity: two separate captures can both be v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImageVersion {
    document_id: u64,
    revision: u64,
}

impl From<&Snapshot> for ImageVersion {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            document_id: snapshot.document_id,
            revision: snapshot.revision,
        }
    }
}

#[derive(Default)]
struct SessionVersions {
    current: Option<ImageVersion>,
    text: Option<ImageVersion>,
}

impl SessionVersions {
    fn observe(&mut self, current: Option<ImageVersion>) -> bool {
        let changed = self.current != current;
        self.current = current;
        changed
    }

    fn is_stale(&self) -> bool {
        self.text.is_some() && self.text != self.current
    }

    fn accepts(&self, requested: Option<ImageVersion>) -> bool {
        self.current == requested
    }

    fn label(&self) -> String {
        match (self.current, self.text) {
            (Some(current), Some(text)) if self.is_stale() => format!(
                "旧版文字：来自修改前图片 v{}；当前图片 v{} · 请重新识别",
                text.revision, current.revision
            ),
            (Some(current), Some(_)) => format!("文字来自图片 v{}", current.revision),
            (Some(current), None) => format!("当前图片 v{} · 尚未获得识别结果", current.revision),
            (None, _) => "独立文字结果".into(),
        }
    }
}

fn document_version(document: &SharedDocument) -> ImageVersion {
    let document = document.borrow();
    ImageVersion {
        document_id: document.id(),
        revision: document.revision(),
    }
}

/// This is the only image input path to OCR. The document exposes a composed
/// crop, never its editable source; workers own only the immutable pixel copy.
fn recognition_snapshot(document: &SharedDocument) -> Result<Snapshot, String> {
    document.borrow().snapshot()
}

fn recognize_snapshot(
    snapshot: Snapshot,
    config: Config,
    control: &RequestControl,
) -> Result<(String, String, Config), String> {
    vellum_text::ocr::recognize_with_control(
        &snapshot.image,
        &config.api,
        &config.ocr,
        &config.llm,
        control,
    )
    .map(|recognized| (recognized.text, engine_label(recognized.engine), config))
    .map_err(|error| format!("识别失败：{error}"))
}

/// Text-only captures end with their result window, not an implicit image viewer.
/// Once the user enters the image workspace, retain its save/copy/discard guard.
/// Derived OCR/translation windows share this choice, including later promotion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultRoute {
    StandaloneText,
    SharedImage,
}

fn enter_image_workspace(
    route: &Cell<ResultRoute>,
    present: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    present()?;
    route.set(ResultRoute::SharedImage);
    Ok(())
}

/// Keep shutdown separate so a failed shared-image handoff cannot cancel work
/// or release state. Text output must never mark the image as saved or copied.
fn finish_close_after_handoff(
    route: ResultRoute,
    document: Option<&SharedDocument>,
    present: impl FnOnce(SharedDocument) -> Result<(), String>,
    finish: impl FnOnce(),
) -> Result<(), String> {
    if route == ResultRoute::SharedImage
        && let Some(document) = document
    {
        // End this borrow before presenting a viewer, which reads the same doc.
        let pending = document.borrow().needs_output_confirmation();
        if pending {
            present(document.clone()).map_err(|error| format!(
                "图片尚未保存或复制，无法转到查看器：{error}；结果窗口、文字和图片均已保留，请重试"
            ))?;
        }
    }
    finish();
    Ok(())
}

/// A failed stage is retried alone; body text is never replaced by an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Recognize,
    Translate,
}

struct TextState {
    mode: Mode,
    body: String,
    stage: Option<Stage>,
    retry: Option<Stage>,
    edited: bool,
}

impl TextState {
    fn new(mode: Mode, text: &str) -> Self {
        Self {
            mode,
            body: text.into(),
            stage: None,
            retry: None,
            edited: false,
        }
    }

    fn begin(&mut self, stage: Stage) {
        self.stage = Some(stage);
        self.retry = None;
    }

    fn failed(&mut self) {
        self.retry = self.stage.take();
    }

    fn complete(&mut self, text: String) {
        self.body = text;
        self.stage = None;
        self.retry = None;
        self.edited = false;
    }

    fn can_translate(&self) -> bool {
        !self.body.trim().is_empty()
            && (self.mode == Mode::Ocr
                || self.retry == Some(Stage::Translate)
                || (self.edited && self.retry == Some(Stage::Recognize)))
    }
}

struct ResultWindow {
    id: u64,
    app: Application,
    window: ApplicationWindow,
    text: TextView,
    status: Label,
    spinner: Spinner,
    copy_button: Button,
    translate_button: Button,
    retry_button: Button,
    cancel_button: Button,
    copy_status: Label,
    document: Option<SharedDocument>,
    route: Rc<Cell<ResultRoute>>,
    versions: RefCell<SessionVersions>,
    version_label: Label,
    view_button: Button,
    edit_button: Button,
    state: RefCell<TextState>,
    updating_text: Cell<bool>,
    text_sync_pending: Cell<bool>,
    request_job: RefCell<JobState>,
    request_slot: WorkerSlot,
    control: RefCell<Option<RequestControl>>,
    copy_job: RefCell<JobState>,
    /// Set from `close-request`. Late updates from a worker must not resurrect
    /// a window the user dismissed.
    closed: Cell<bool>,
}

impl ResultWindow {
    fn new(
        app: &Application,
        mode: Mode,
        text: &str,
        status: &str,
        document: Option<SharedDocument>,
        text_version: Option<ImageVersion>,
        route: Rc<Cell<ResultRoute>>,
    ) -> Rc<Self> {
        theme::install_default();

        let (mw, mh) = gtk4::gdk::Display::default()
            .and_then(|display| display.monitors().item(0))
            .and_then(|monitor| monitor.downcast::<gtk4::gdk::Monitor>().ok())
            .map(|monitor| (monitor.geometry().width(), monitor.geometry().height()))
            .unwrap_or((1440, 900));
        let window = ApplicationWindow::builder()
            .application(app)
            .default_width(WIDTH.min((mw - 64).max(320)))
            .default_height(HEIGHT.min((mh - 96).max(240)))
            // A fixed first-map hint avoids a full-height tiling column.
            // Resizing is enabled only after the compositor floats this window.
            .resizable(false)
            .title(mode.title())
            .build();
        window.add_css_class("vellum-window");
        window.add_css_class("vellum-result");

        let root = GtkBox::new(Orientation::Vertical, 0);

        // The header is ours rather than a compositor title bar: the target
        // session runs without server-side decorations, so a plain window
        // would show no title at all.
        let header = GtkBox::new(Orientation::Horizontal, 8);
        header.set_margin_top(10);
        header.set_margin_bottom(8);
        header.set_margin_start(12);
        header.set_margin_end(12);

        let badge = Label::new(Some(mode.badge()));
        badge.add_css_class("vellum-status-chip");
        badge.set_valign(Align::Center);
        header.append(&badge);

        let titles = GtkBox::new(Orientation::Vertical, 2);
        titles.set_hexpand(true);
        let title = Label::builder().label(mode.title()).xalign(0.0).build();
        title.add_css_class("vellum-title");
        title.set_ellipsize(EllipsizeMode::End);
        let subtitle = Label::builder().label(mode.subtitle()).xalign(0.0).build();
        subtitle.add_css_class("vellum-dim");
        subtitle.set_ellipsize(EllipsizeMode::End);
        subtitle.set_tooltip_text(Some(mode.subtitle()));
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
        // The header doubles as the drag handle: vellum draws its own title bar
        // (the session runs without server-side decorations), so without a
        // WindowHandle there would be no way to move the window at all.
        root.append(&crate::drag::draggable(&header));

        let divider = Separator::new(Orientation::Horizontal);
        divider.add_css_class("vellum-divider");
        root.append(&divider);

        let version_row = GtkBox::new(Orientation::Horizontal, 8);
        version_row.set_margin_start(12);
        version_row.set_margin_end(12);
        version_row.set_margin_top(6);
        let version_label = Label::new(None);
        version_label.set_xalign(0.0);
        version_label.set_hexpand(true);
        version_label.set_ellipsize(EllipsizeMode::End);
        version_label.add_css_class("vellum-dim");
        let view_button = Button::with_label("查看原图");
        let edit_button = Button::with_label("继续编辑");
        view_button.set_tooltip_text(Some("查看同一会话的当前图片，不重新截图"));
        edit_button.set_tooltip_text(Some("修改标注或裁切；提交后可重新识别当前成品"));
        version_row.append(&version_label);
        version_row.append(&view_button);
        version_row.append(&edit_button);
        root.append(&version_row);

        let shell = GtkBox::new(Orientation::Vertical, 0);
        shell.add_css_class("vellum-text-shell");
        shell.set_margin_top(8);
        shell.set_margin_bottom(8);
        shell.set_margin_start(12);
        shell.set_margin_end(12);
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

        // Long OCR lines wrap inside the viewport rather than growing the window.
        let scroller = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .hscrollbar_policy(PolicyType::Never)
            .propagate_natural_width(false)
            .propagate_natural_height(false)
            .child(&view)
            .build();
        shell.append(&scroller);
        root.append(&shell);

        // Feedback has its own row; long errors never compete with the actions.
        let footer = GtkBox::new(Orientation::Vertical, 6);
        footer.set_margin_bottom(10);
        footer.set_margin_start(12);
        footer.set_margin_end(12);

        let status_row = GtkBox::new(Orientation::Horizontal, 8);
        status_row.set_hexpand(true);
        status_row.set_valign(Align::Center);
        let spinner = Spinner::new();
        let status_label = Label::builder().label(status).xalign(0.0).build();
        status_label.add_css_class("vellum-dim");
        status_label.set_ellipsize(EllipsizeMode::End);
        status_label.set_hexpand(true);
        status_label.set_tooltip_text(Some(status));
        status_row.append(&spinner);
        status_row.append(&status_label);
        footer.append(&status_row);

        let copy_status = Label::new(None);
        copy_status.set_xalign(0.0);
        copy_status.set_wrap(true);
        copy_status.set_wrap_mode(pango::WrapMode::WordChar);
        copy_status.set_margin_start(12);
        copy_status.set_margin_end(12);
        copy_status.set_visible(false);
        root.append(&copy_status);

        // GTK wraps actions when narrowed; no resize timer or pixel rendering.
        let buttons = FlowBox::builder()
            .selection_mode(SelectionMode::None)
            .min_children_per_line(1)
            .max_children_per_line(4)
            .column_spacing(4)
            .row_spacing(4)
            .halign(Align::End)
            .build();
        let copy_button = action_button("复制", "edit-copy-symbolic");
        let translate_button = action_button("翻译", "preferences-desktop-locale-symbolic");
        copy_button.add_css_class("suggested-action");
        let retry_button = Button::with_label("重试识别");
        let cancel_button = Button::with_label("取消");
        cancel_button
            .set_tooltip_text(Some("停止接收本次结果；正在运行的请求可能需要等待超时结束"));
        buttons.insert(&copy_button, -1);
        buttons.insert(&retry_button, -1);
        buttons.insert(&translate_button, -1);
        buttons.insert(&cancel_button, -1);
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
            retry_button,
            cancel_button,
            copy_status,
            versions: RefCell::new(SessionVersions {
                current: document.as_ref().map(document_version),
                text: text_version,
            }),
            document,
            route,
            version_label,
            view_button,
            edit_button,
            state: RefCell::new(TextState::new(mode, text)),
            updating_text: Cell::new(false),
            text_sync_pending: Cell::new(false),
            request_job: RefCell::new(JobState::default()),
            request_slot: WorkerSlot::default(),
            control: RefCell::new(None),
            copy_job: RefCell::new(JobState::default()),
            closed: Cell::new(false),
        });

        WINDOWS.with(|slot| slot.borrow_mut().push((result.id, result.clone())));
        result.sync_actions();
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

        let this = Rc::downgrade(self);
        self.translate_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.translate(None);
            }
        });
        let this = Rc::downgrade(self);
        self.retry_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.recognize();
            }
        });
        let this = Rc::downgrade(self);
        self.cancel_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.cancel_request();
            }
        });
        let this = Rc::downgrade(self);
        self.view_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade()
                && let Some(document) = &this.document
                && let Err(error) = enter_image_workspace(&this.route, || {
                    crate::preview::open_document(&this.app, document.clone())
                })
            {
                this.flash(&format!("无法打开查看器：{error}"), true);
            }
        });
        let this = Rc::downgrade(self);
        self.edit_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade()
                && let Some(document) = &this.document
                && let Err(error) = enter_image_workspace(&this.route, || {
                    crate::editor::open(&this.app, document.clone())
                })
            {
                this.flash(&format!("无法打开编辑窗口：{error}"), true);
            }
        });
        let this = Rc::downgrade(self);
        self.text.buffer().connect_changed(move |_| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if this.closed.get() || this.updating_text.get() {
                return;
            }
            this.state.borrow_mut().edited = true;
            this.observe_document();
            {
                let mut versions = this.versions.borrow_mut();
                if versions.text.is_none() {
                    versions.text = versions.current;
                }
            }
            if this.request_job.borrow().is_busy() {
                this.cancel_request();
            }
            // Coalesce a burst of buffer changes (paste/replace/typing) into one
            // full text copy and action refresh. Cancellation above stays immediate.
            if !this.text_sync_pending.replace(true) {
                let weak = Rc::downgrade(&this);
                glib::idle_add_local_once(move || {
                    if let Some(window) = weak.upgrade()
                        && !window.closed.get()
                    {
                        window.sync_text_state();
                        window.sync_actions();
                    }
                });
            }
        });

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

        // Weak on purpose: the registry below owns the strong reference, and a
        // strong one here would be a cycle that never frees.
        let this = Rc::downgrade(self);
        let id = self.id;
        self.window.connect_close_request(move |_| {
            if let Some(this) = this.upgrade() {
                let result = finish_close_after_handoff(
                    this.route.get(),
                    this.document.as_ref(),
                    |document| crate::preview::open_document(&this.app, document),
                    || {
                        this.closed.set(true);
                        if let Some(control) = this.control.borrow_mut().take() {
                            control.cancel();
                        }
                        this.request_job.borrow_mut().close();
                        this.copy_job.borrow_mut().close();
                    },
                );
                if let Err(error) = result {
                    this.flash(&error, true);
                    return glib::Propagation::Stop;
                }
            }
            // Drop our own ownership so the struct can go. Holding the last
            // reference here would keep every closed window alive for the
            // lifetime of the process.
            WINDOWS.with(|slot| slot.borrow_mut().retain(|(known, _)| *known != id));
            glib::Propagation::Proceed
        });

        // Watch only the cheap identity/revision, never render/copy pixels on a
        // timer. The weak owner disappears on close, and request callbacks also
        // check the live revision so safety never relies on this polling delay.
        if self.document.is_some() {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local(DOCUMENT_POLL, move || {
                let Some(window) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                if window.closed.get() {
                    return glib::ControlFlow::Break;
                }
                window.observe_document();
                glib::ControlFlow::Continue
            });
        }

        // Floating rather than tiled: a text result is something you read
        // beside the window you captured, not a new column in the scroll.
        let weak = Rc::downgrade(self);
        self.window.connect_map(move |_| {
            let weak = weak.clone();
            glib::timeout_add_local_once(FLOAT_DELAY, move || {
                let Some(window) = weak.upgrade().filter(|window| !window.closed.get()) else {
                    return;
                };
                // Looked up by pid rather than acting on the focused window:
                // the user may have moved on during the delay above. Retried on
                // the main loop because the compositor's client list can lag the
                // map, and a miss must not fall back to the user's window.
                crate::own_window::float_and_resize(&window.window);
            });
        });
    }

    fn recognize_after_first_frame(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        crate::opening::after_first_frame(&self.window, move || {
            crate::trace::mark("text-window-first-frame");
            glib::idle_add_local_once(move || {
                if let Some(window) = weak.upgrade().filter(|window| !window.closed.get()) {
                    window.recognize();
                }
            });
        });
    }

    fn present(self: &Rc<Self>) {
        self.window.present();
        theme::snapshot_for_review(&self.window);
    }

    fn current_document_version(&self) -> Option<ImageVersion> {
        self.document.as_ref().map(document_version)
    }

    fn observe_document(self: &Rc<Self>) {
        if self.closed.get() {
            return;
        }
        let current = self.current_document_version();
        if !self.versions.borrow_mut().observe(current) {
            return;
        }
        if self.request_job.borrow().is_busy() {
            self.cancel_request();
        }
        if self.document.is_some() {
            self.state.borrow_mut().retry = Some(Stage::Recognize);
            self.copy_status.set_visible(false);
            self.flash(
                "图片已更新；旧文字已保留，请识别当前图片后再复制或翻译",
                false,
            );
        }
        self.sync_actions();
    }

    fn request_is_current(self: &Rc<Self>, ticket: u64, version: Option<ImageVersion>) -> bool {
        self.observe_document();
        self.request_job.borrow().is_current(ticket) && self.versions.borrow().accepts(version)
    }

    fn sync_actions(&self) {
        if self.closed.get() {
            return;
        }
        let busy = self.request_job.borrow().is_busy() || self.request_slot.is_busy();
        let state = self.state.borrow();
        let versions = self.versions.borrow();
        let stale = versions.is_stale();
        let mut version_label = versions.label();
        if state.edited {
            version_label.push_str(" · 文字已手动编辑");
        }
        self.version_label.set_label(&version_label);
        self.version_label.set_tooltip_text(Some(&version_label));
        if stale {
            self.version_label.add_css_class("vellum-error");
        } else {
            self.version_label.remove_css_class("vellum-error");
        }
        self.view_button.set_visible(self.document.is_some());
        self.edit_button.set_visible(self.document.is_some());
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
        self.text.set_editable(true);
        self.text.set_cursor_visible(true);
        self.copy_button.set_sensitive(
            !stale && !state.body.trim().is_empty() && !self.copy_job.borrow().is_busy(),
        );
        set_action_visible(
            &self.translate_button,
            state.mode == Mode::Ocr
                || state.can_translate()
                || state.retry == Some(Stage::Translate)
                || state.stage == Some(Stage::Translate),
        );
        self.translate_button
            .set_sensitive(!stale && state.can_translate() && !busy);
        set_action_label(
            &self.translate_button,
            if state.retry == Some(Stage::Translate) {
                "重试翻译"
            } else {
                "翻译"
            },
        );
        set_action_visible(&self.retry_button, self.document.is_some());
        self.retry_button.set_label(if stale {
            "识别新版"
        } else if state.retry == Some(Stage::Recognize) {
            "重试识别"
        } else {
            "重新识别"
        });
        self.retry_button.set_sensitive(!busy);
        set_action_visible(&self.cancel_button, self.request_job.borrow().is_busy());
    }

    fn flash(&self, message: &str, error: bool) {
        if self.closed.get() {
            return;
        }
        self.status.set_label(message);
        self.status.set_tooltip_text(Some(message));
        if error {
            self.status.add_css_class("vellum-error");
        } else {
            self.status.remove_css_class("vellum-error");
        }
        self.sync_actions();
    }

    fn sync_text_state(&self) {
        if self.text_sync_pending.replace(false) {
            self.state.borrow_mut().body = self.current_text();
        }
    }

    fn current_text(&self) -> String {
        let buffer = self.text.buffer();
        let (start, end) = buffer.bounds();
        buffer.text(&start, &end, false).to_string()
    }

    fn show_body(&self) {
        self.text_sync_pending.set(false);
        let text = self.state.borrow().body.clone();
        self.updating_text.set(true);
        self.text.buffer().set_text(&text);
        self.updating_text.set(false);
        self.sync_actions();
    }

    fn copy(self: &Rc<Self>) {
        self.observe_document();
        if self.versions.borrow().is_stale() {
            self.flash("文字来自旧版图片；请重新识别当前图片后再复制", true);
            return;
        }
        let source_version = self.versions.borrow().text;
        let text = self.current_text();
        if text.trim().is_empty() {
            return;
        }
        let Some(ticket) = self.copy_job.borrow_mut().begin() else {
            return;
        };
        self.copy_status.set_label("复制中…");
        self.copy_status.set_visible(true);
        self.sync_actions();
        let snapshot = text.clone();
        let weak = Rc::downgrade(self);
        let completed = weak.clone();
        ui_job::run(
            move || vellum_core::io::copy_text(&text),
            move || {
                weak.upgrade()
                    .is_some_and(|window| window.copy_job.borrow().is_current(ticket))
            },
            move |result| {
                let Some(window) = completed.upgrade() else {
                    return;
                };
                if !window.copy_job.borrow_mut().finish(ticket) {
                    return;
                }
                let result = result
                    .map_err(|error| error.to_string())
                    .and_then(|result| result.map_err(|error| error.to_string()));
                let message = match result {
                    Ok(()) if source_version != window.current_document_version() => {
                        "已复制操作开始时的旧版文字；图片已更新，请重新识别".into()
                    }
                    Ok(()) if snapshot == window.current_text() => "已复制到剪贴板".into(),
                    Ok(()) => "已复制修改前的文字；当前修改请重新复制".into(),
                    Err(error) => format!("复制失败：{error}（可重试）"),
                };
                window.copy_status.set_label(&message);
                window.copy_status.set_visible(true);
                window.copy_status.set_tooltip_text(Some(&message));
                window.sync_actions();
            },
        );
    }

    fn cancel_request(self: &Rc<Self>) {
        if !self.request_job.borrow().is_busy() {
            return;
        }
        if let Some(control) = self.control.borrow_mut().take() {
            control.cancel();
        }
        let cancelled_generation = self.request_job.borrow().generation();
        self.request_job.borrow_mut().cancel();
        self.state.borrow_mut().failed();
        self.flash("已取消接收结果，正在等待当前请求结束…", false);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(window) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if window.closed.get() {
                return glib::ControlFlow::Break;
            }
            if window.request_slot.is_busy() {
                return glib::ControlFlow::Continue;
            }
            // Do not overwrite a newer request's status if it started meanwhile.
            if window.request_job.borrow().is_latest(cancelled_generation) {
                window.flash(
                    if window.versions.borrow().is_stale() {
                        "已取消旧版任务；请识别当前图片，原文字已保留"
                    } else {
                        "已取消，文字已保留，可重试"
                    },
                    false,
                );
            }
            window.sync_actions();
            glib::ControlFlow::Break
        });
    }

    fn begin_request(&self, stage: Stage, control: RequestControl) -> Option<u64> {
        if self.request_slot.is_busy() {
            return None;
        }
        let ticket = self.request_job.borrow_mut().begin()?;
        self.state.borrow_mut().begin(stage);
        *self.control.borrow_mut() = Some(control);
        self.flash(
            if stage == Stage::Recognize {
                "识别中…"
            } else {
                "翻译中…"
            },
            false,
        );
        Some(ticket)
    }

    fn request_failed(&self, message: &str) {
        self.state.borrow_mut().failed();
        self.flash(message, true);
    }

    fn recognize(self: &Rc<Self>) {
        self.sync_text_state();
        self.observe_document();
        let Some(document) = &self.document else {
            return;
        };
        if self.request_slot.is_busy() || self.request_job.borrow().is_busy() {
            return;
        }
        let config = Config::load();
        let control = RequestControl::new(pipeline_budget(&config, self.state.borrow().mode));
        let Some(ticket) = self.begin_request(Stage::Recognize, control.clone()) else {
            return;
        };
        let worker_control = control.limited_to(ocr_budget(&config));
        let snapshot = match recognition_snapshot(document) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.request_job.borrow_mut().finish(ticket);
                self.control.borrow_mut().take();
                self.request_failed(&format!("无法生成识别图片：{error}（可重试）"));
                return;
            }
        };
        let version = Some(ImageVersion::from(&snapshot));
        let weak = Rc::downgrade(self);
        let completed = weak.clone();
        ui_job::run_with_slot(
            &self.request_slot,
            move || recognize_snapshot(snapshot, config, &worker_control),
            move || {
                weak.upgrade()
                    .is_some_and(|window| window.request_is_current(ticket, version))
            },
            move |result| {
                let Some(window) = completed.upgrade() else {
                    return;
                };
                if !window.request_is_current(ticket, version)
                    || !window.request_job.borrow_mut().finish(ticket)
                {
                    return;
                }
                window.control.borrow_mut().take();
                let result = result
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                match result {
                    Ok((text, _, _)) if text.trim().is_empty() => {
                        window.request_failed("未识别到文字，可重试识别或直接输入")
                    }
                    Ok((text, engine, config)) => {
                        // A same-image retry must not destroy text manually corrected
                        // before it began. Put its fresh OCR in another window instead.
                        let edited = {
                            let state = window.state.borrow();
                            state.edited && !state.body.trim().is_empty()
                        };
                        if edited {
                            {
                                let mut state = window.state.borrow_mut();
                                state.stage = None;
                                state.retry = None;
                            }
                            let fresh = Self::new(
                                &window.app,
                                Mode::Ocr,
                                &text,
                                &engine,
                                window.document.clone(),
                                version,
                                window.route.clone(),
                            );
                            fresh.present();
                            window.flash("识别完成；编辑内容已保留，新识别结果已另开窗口", false);
                            return;
                        }
                        let translate = window.state.borrow().mode == Mode::Translate;
                        window.state.borrow_mut().complete(text);
                        window.versions.borrow_mut().text = version;
                        window.show_body();
                        window.flash(&engine, false);
                        if translate {
                            window.translate(Some((config, control)));
                        }
                    }
                    Err(error) => window.request_failed(&error),
                }
            },
        );
    }

    /// With a supplied control this is the translation stage of the same OCR
    /// request; a user retry gets fresh settings and a fresh total budget.
    fn translate(self: &Rc<Self>, continuation: Option<(Config, RequestControl)>) {
        self.sync_text_state();
        self.observe_document();
        if self.versions.borrow().is_stale() {
            self.flash("文字来自旧版图片；请识别当前成品后再翻译", true);
            return;
        }
        let version = self.current_document_version();
        if self.request_slot.is_busy() || self.request_job.borrow().is_busy() {
            return;
        }
        let text = self.current_text();
        if text.trim().is_empty() {
            self.flash("没有文本可翻译", true);
            return;
        }
        let (config, control) = continuation.unwrap_or_else(|| {
            let config = Config::load();
            let control = RequestControl::new(Duration::from_secs(config.api.timeout_s.max(1)));
            (config, control)
        });
        let Some(ticket) = self.begin_request(Stage::Translate, control.clone()) else {
            return;
        };
        let control = control.limited_to(Duration::from_secs(config.api.timeout_s.max(1)));
        let weak = Rc::downgrade(self);
        let completed = weak.clone();
        ui_job::run_with_slot(
            &self.request_slot,
            move || {
                vellum_text::llm::translate_with_control(&text, &config.api, &config.llm, &control)
                    .map_err(|error| format!("翻译失败：{error}"))
            },
            move || {
                weak.upgrade()
                    .is_some_and(|window| window.request_is_current(ticket, version))
            },
            move |result| {
                let Some(window) = completed.upgrade() else {
                    return;
                };
                if !window.request_is_current(ticket, version)
                    || !window.request_job.borrow_mut().finish(ticket)
                {
                    return;
                }
                window.control.borrow_mut().take();
                let result = result
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                match result {
                    Ok(translated) => {
                        let transport = translated.transport.label();
                        let replace = window.state.borrow().mode == Mode::Translate;
                        if replace {
                            window.state.borrow_mut().complete(translated.text);
                            window.versions.borrow_mut().text = version;
                            window.show_body();
                        } else {
                            {
                                let mut state = window.state.borrow_mut();
                                state.stage = None;
                                state.retry = None;
                            }
                            let fresh = Self::new(
                                &window.app,
                                Mode::Translate,
                                &translated.text,
                                &transport,
                                window.document.clone(),
                                version,
                                window.route.clone(),
                            );
                            fresh.present();
                        }
                        window.flash(&transport, false);
                    }
                    Err(error) => window.request_failed(&error),
                }
            },
        );
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

// FlowBox wraps each action in a child; hide that child too so an unavailable
// action never leaves an empty slot or increases the minimum window width.
fn set_action_visible(button: &Button, visible: bool) {
    button.set_visible(visible);
    if let Some(child) = button.parent().and_downcast::<gtk4::FlowBoxChild>() {
        child.set_visible(visible);
    }
}

fn set_action_label(button: &Button, text: &str) {
    if let Some(label) = button
        .child()
        .and_then(|child| child.downcast::<GtkBox>().ok())
        .and_then(|content| content.last_child())
        .and_then(|child| child.downcast::<Label>().ok())
    {
        label.set_label(text);
    }
    button.set_tooltip_text(Some(text));
}

/// Open OCR/translation beside the viewer/editor in the existing application.
/// This does not recapture, start another main loop or consume a handoff ACK.
pub fn open_document(app: &Application, document: SharedDocument, translate: bool) {
    let requested_id = document.borrow().id();
    let requested_mode = if translate {
        Mode::Translate
    } else {
        Mode::Ocr
    };
    // Repeated viewer clicks must not create an unlimited number of workers.
    // Prefer the newest matching result; preserve independent source/translation
    // windows and never replace a user's manually corrected text implicitly.
    let existing = WINDOWS.with(|windows| {
        windows.borrow().iter().rev().find_map(|(_, window)| {
            (!window.closed.get()
                && window.state.borrow().mode == requested_mode
                && window
                    .current_document_version()
                    .is_some_and(|version| version.document_id == requested_id))
            .then(|| Rc::clone(window))
        })
    });
    if let Some(window) = existing {
        window.route.set(ResultRoute::SharedImage);
        window.observe_document();
        window.present();
        return;
    }
    let window = document_window(app, document, translate, ResultRoute::SharedImage);
    window.recognize_after_first_frame();
    window.present();
}

fn document_window(
    app: &Application,
    document: SharedDocument,
    translate: bool,
    route: ResultRoute,
) -> Rc<ResultWindow> {
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
    ResultWindow::new(
        app,
        mode,
        "",
        placeholder,
        Some(document),
        None,
        Rc::new(Cell::new(route)),
    )
}

/// A toolbar result in the capture's already running GTK application. Keep the
/// standalone close semantics and never consume a process handoff receipt.
pub(crate) fn open_captured_document(
    app: &Application,
    document: SharedDocument,
    translate: bool,
) -> ApplicationWindow {
    let result = document_window(app, document, translate, ResultRoute::StandaloneText);
    result.recognize_after_first_frame();
    result.present();
    result.window.clone()
}

/// Initial standalone entrypoint. Only this first window acknowledges the
/// process handoff; additional same-session windows must never consume it.
pub fn run_document(document: SharedDocument, translate: bool) -> i32 {
    let app = application();
    let payload = RefCell::new(Some(document));
    app.connect_activate(move |app| {
        let Some(document) = payload.borrow_mut().take() else {
            return;
        };
        let window = document_window(app, document, translate, ResultRoute::StandaloneText);
        crate::handoff::connect_ready(&window.window);
        window.recognize_after_first_frame();
        window.present();
    });
    run(&app)
}

/// Compatibility entrypoint for an already flattened, covered raster. It gets
/// a new document identity; an editable in-memory session uses run_document.
pub fn run_text_action(image: Rgb8, translate: bool) -> i32 {
    match Document::from_raster(image) {
        Ok(document) => run_document(Rc::new(RefCell::new(document)), translate),
        Err(error) => {
            let app = application();
            app.connect_activate(move |app| {
                let window = ResultWindow::new(
                    app,
                    Mode::Ocr,
                    "",
                    &error,
                    None,
                    None,
                    Rc::new(Cell::new(ResultRoute::StandaloneText)),
                );
                window.present();
                // No READY: the sender must retain its recoverable composite.
            });
            let exit = run(&app);
            if exit == 0 { 1 } else { exit }
        }
    }
}

fn ocr_budget(config: &Config) -> Duration {
    if config.ocr.uses_api() {
        Duration::from_secs(config.ocr.api_timeout_s.max(1))
    } else {
        Duration::from_secs(30)
    }
}

/// One total budget for the pipeline, with each stage additionally capped by
/// its existing preference. Model fallback never receives a new deadline.
fn pipeline_budget(config: &Config, mode: Mode) -> Duration {
    let recognition = ocr_budget(config);
    if mode == Mode::Translate {
        recognition.saturating_add(Duration::from_secs(config.api.timeout_s.max(1)))
    } else {
        recognition
    }
}

/// One short line naming the OCR engine that produced the text, so a user who
/// switched engines can see which one actually ran.
fn engine_label(engine: &str) -> String {
    if engine == vellum_core::config::OCR_ENGINE_API {
        "API 视觉识别".to_string()
    } else {
        "内置 Tesseract".to_string()
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
#[path = "result_session_tests.rs"]
mod session_tests;

#[cfg(test)]
#[path = "result_ocr_live_tests.rs"]
mod ocr_live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_translation_failure_keeps_ocr_and_offers_translation_only_retry() {
        let mut state = TextState::new(Mode::Translate, "recognized source");
        state.begin(Stage::Translate);
        state.failed();
        assert_eq!(state.body, "recognized source");
        assert_eq!(state.retry, Some(Stage::Translate));
        assert!(state.can_translate());
        state.begin(Stage::Translate);
        state.complete("translated text".into());
        assert_eq!(state.body, "translated text");
        assert_eq!(state.retry, None);
        assert_eq!(state.stage, None);
    }

    #[test]
    fn ocr_window_translation_failure_allows_retry_and_success() {
        let mut state = TextState::new(Mode::Ocr, "corrected source");
        state.edited = true;
        state.begin(Stage::Translate);
        state.failed();
        assert_eq!(state.body, "corrected source");
        assert!(state.edited);
        assert!(state.can_translate());
        state.begin(Stage::Translate);
        state.stage = None;
        state.retry = None;
        assert!(state.can_translate());
    }

    #[test]
    fn recognition_failure_preserves_user_edited_body_and_retries_same_stage() {
        let mut state = TextState::new(Mode::Translate, "my edits");
        state.edited = true;
        state.begin(Stage::Recognize);
        state.failed();
        assert_eq!(state.body, "my edits");
        assert!(state.edited);
        assert_eq!(state.retry, Some(Stage::Recognize));
    }

    #[test]
    fn manually_corrected_text_after_ocr_failure_can_be_translated() {
        let mut state = TextState::new(Mode::Translate, "");
        state.begin(Stage::Recognize);
        state.failed();
        state.body = "manually recovered source".into();
        state.edited = true;
        assert!(state.can_translate());
        assert_eq!(state.retry, Some(Stage::Recognize));
    }

    #[test]
    fn empty_ocr_cannot_be_sent_as_translation() {
        let mut state = TextState::new(Mode::Translate, "  ");
        state.begin(Stage::Recognize);
        state.failed();
        assert!(!state.can_translate());
    }

    #[test]
    fn pipeline_budget_respects_both_existing_timeout_preferences() {
        let mut config = Config::default();
        config.api.timeout_s = 11;
        config.ocr.api_timeout_s = 7;
        config.ocr.engine = vellum_core::config::OCR_ENGINE_API.into();
        assert_eq!(pipeline_budget(&config, Mode::Ocr), Duration::from_secs(7));
        assert_eq!(
            pipeline_budget(&config, Mode::Translate),
            Duration::from_secs(18)
        );
        config.ocr.engine = vellum_core::config::OCR_ENGINE_BUILTIN.into();
        assert_eq!(pipeline_budget(&config, Mode::Ocr), Duration::from_secs(30));
        assert_eq!(
            pipeline_budget(&config, Mode::Translate),
            Duration::from_secs(41)
        );
    }

    #[test]
    fn each_mode_has_its_own_copy() {
        assert_ne!(Mode::Ocr.title(), Mode::Translate.title());
        assert_ne!(Mode::Ocr.subtitle(), Mode::Translate.subtitle());
        assert_ne!(Mode::Ocr.badge(), Mode::Translate.badge());
    }
}

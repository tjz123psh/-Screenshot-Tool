//! The settings panel behind `vellum panel.
//!
//! Three decisions here are load-bearing:
//!
//! * The window is `NON_UNIQUE and draws its own header, exactly like the
//!   result window. Under GTK's default single-instance behaviour a second
//!   `vellum panel would only forward `activate to the first process, which
//!   re-presents the values it loaded at startup and silently discards whatever
//!   config file was edited in between; and the session has no server-side
//!   decorations, so without our own header there would be no title and no way
//!   to move the window.
//! * Nothing is written until 保存. A panel that saved on every keystroke would
//!   rewrite (and chmod) the config while the user is still typing, and a
//!   half-typed key or URL would be live for the next screenshot.
//! * The connection test runs on a worker thread. `ureq is blocking, and a slow
//!   endpoint on the main loop would freeze the window that is supposed to
//!   report the result.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gtk4::gdk::{Key, ModifierType};
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, Entry,
    EventControllerKey, Image, Label, Orientation, PolicyType, ScrolledWindow, SpinButton, Spinner,
    Stack, StackTransitionType, Switch, ToggleButton, gio, glib,
};
use pango::EllipsizeMode;
use vellum_core::config::{
    ApiConfig, Config, DEFAULT_API_BASE_URL, DEFAULT_API_KEY_ENV, DEFAULT_API_TIMEOUT_S,
    DEFAULT_LLM_MODEL, DEFAULT_OCR_LANGS, DEFAULT_TARGET_LANG, OCR_ENGINE_API, OCR_ENGINE_BUILTIN,
};
use vellum_core::prefs::{self, Preferences};

use crate::controls;
use crate::model_picker::ModelPicker;
use crate::theme;

const APP_ID: &str = "ai.vellum.panel";
/// Wide enough for the 184 px sidebar plus the 640 px form column, and deep
/// enough that the tallest page (OCR) scrolls only for its last row.
const WIDTH: i32 = 900;
/// Tall enough that the API page's model card sits above the fold: at 760 the
/// second picker fell below it, and a primary action the user has to discover by
/// scrolling is a defect, not a density win. The sidebar and the capped form
/// column stay put; only the window grew.
const HEIGHT: i32 = 840;
/// Niri needs the window mapped before it can be floated; the same 60 ms the
/// result window uses keeps both windows behaving alike.
const FLOAT_DELAY: Duration = Duration::from_millis(60);

thread_local! {
    /// Live panels, so a worker thread can address one without holding an `Rc.
    /// `glib::idle_add_once demands `Send, and GTK widgets are not; the worker
    /// carries a plain `u64 and the main loop resolves it here.
    ///
    /// The registry owns the panel outright: a `Weak reference here would leave
    /// nothing keeping the struct alive (the `Rc inside `activate dies when
    /// that closure returns), and every worker update would find a dead pointer.
    static PANELS: RefCell<Vec<(u64, Rc<Panel>)>> = const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// What a worker thread can ask the main loop to do.
enum Update {
    /// The connection test finished. `Err is the endpoint's own message, kept
    /// verbatim because it usually names the missing piece (key, URL, quota).
    Probe(Result<Vec<String>, String>),
    /// 「获取模型」 finished: the same request, but the list is also poured into
    /// the two model pickers.
    Models(Result<Vec<String>, String>),
}

/// Hands `update to the main loop for the panel with `id.
fn post(id: u64, update: Update) {
    glib::idle_add_once(move || {
        let target = PANELS.with(|slot| {
            slot.borrow()
                .iter()
                .find(|(known, _)| *known == id)
                .map(|(_, panel)| Rc::clone(panel))
        });
        if let Some(panel) = target {
            panel.apply(update);
        }
    });
}

/// How a status message should read.
///
/// Progress is neither success nor failure: colouring "正在测试连接…" green
/// would claim an outcome that is still unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flash {
    Info,
    Success,
    Error,
}

impl Flash {
    fn class(self) -> Option<&'static str> {
        match self {
            Flash::Info => None,
            Flash::Success => Some("vellum-success"),
            Flash::Error => Some("vellum-error"),
        }
    }
}

/// Every editable value on the three pages, as plain data.
///
/// Kept free of GTK types on purpose: the mapping onto [`Config] and
/// [`Preferences] is the part of a settings panel that can be wrong in a way a
/// test catches, and GTK widgets cannot be built without a display.
#[derive(Clone, Debug, PartialEq)]
struct FormValues {
    base_url: String,
    api_key: String,
    api_key_env: String,
    timeout_s: u64,
    proxy: String,
    model: String,
    target_lang: String,
    fallback_models: String,
    glossary: String,
    engine: String,
    langs: String,
    preprocess: bool,
    upscale: f32,
    api_model: String,
    api_timeout_s: u64,
    save_after_capture: bool,
    copy_after_capture: bool,
}

impl FormValues {
    /// The values the panel shows for an existing config: what is on disk, not
    /// the defaults, so opening the panel never looks like a reset.
    fn from_config(cfg: &Config, prefs: &Preferences) -> Self {
        Self {
            base_url: cfg.api.base_url.clone(),
            api_key: cfg.api.api_key.clone(),
            api_key_env: cfg.api.api_key_env.clone(),
            timeout_s: cfg.api.timeout_s,
            proxy: cfg.api.proxy.clone(),
            model: cfg.llm.model.clone(),
            target_lang: cfg.llm.target_lang.clone(),
            fallback_models: fallback_models_to_text(&cfg.llm.fallback_models),
            glossary: glossary_to_text(&cfg.llm.glossary),
            engine: cfg.ocr.engine.clone(),
            langs: cfg.ocr.langs.clone(),
            preprocess: cfg.ocr.preprocess,
            upscale: cfg.ocr.upscale,
            api_model: cfg.ocr.api_model.clone(),
            api_timeout_s: cfg.ocr.api_timeout_s,
            save_after_capture: prefs.save,
            copy_after_capture: prefs.copy,
        }
    }

    /// Applies the panel's fields onto a full config.
    ///
    /// `base is the config as it was on disk: the panel only owns the api, llm
    /// and ocr sections, so reusing the loaded document keeps [longshot] — and
    /// anything a future version adds — exactly as the user wrote it.
    fn to_config(&self, base: &Config) -> Config {
        let mut cfg = base.clone();
        cfg.api.base_url = trimmed_or(&self.base_url, DEFAULT_API_BASE_URL);
        cfg.api.api_key = self.api_key.trim().to_string();
        cfg.api.api_key_env = trimmed_or(&self.api_key_env, DEFAULT_API_KEY_ENV);
        cfg.api.timeout_s = valid_timeout(self.timeout_s);
        // Empty means "read the environment"; "none" forces a direct connection.
        cfg.api.proxy = self.proxy.trim().to_string();
        cfg.llm.model = trimmed_or(&self.model, DEFAULT_LLM_MODEL);
        cfg.llm.target_lang = trimmed_or(&self.target_lang, DEFAULT_TARGET_LANG);
        cfg.llm.fallback_models = fallback_models_from_text(&self.fallback_models);
        cfg.llm.glossary = glossary_from_text(&self.glossary);
        // Anything that is not the API engine is the built-in one: writing an
        // unknown string would make the loader silently keep the old engine.
        cfg.ocr.engine = if self.engine == OCR_ENGINE_API {
            OCR_ENGINE_API.to_string()
        } else {
            OCR_ENGINE_BUILTIN.to_string()
        };
        cfg.ocr.langs = trimmed_or(&self.langs, DEFAULT_OCR_LANGS);
        cfg.ocr.preprocess = self.preprocess;
        cfg.ocr.upscale = valid_upscale(self.upscale);
        cfg.ocr.api_model = self.api_model.trim().to_string();
        cfg.ocr.api_timeout_s = valid_timeout(self.api_timeout_s);
        cfg
    }

    fn to_preferences(&self) -> Preferences {
        Preferences {
            save: self.save_after_capture,
            copy: self.copy_after_capture,
        }
    }
}

/// Splits the comma-separated fallback list.
///
/// The full-width comma and newlines are accepted too, because the field is
/// typed by hand from documentation that uses either. Duplicates are dropped
/// while keeping the first occurrence: the order is the whole point of the list,
/// and retrying the same refused model only doubles the wait.
/// Split a panel field into list entries.
///
/// All four separators a user might reach for, de-duplicated, with empties
/// dropped: a stray comma should not become a glossary entry.
fn split_list(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for item in text.split([',', '，', ';', '；', '\n', '\r', '\t']) {
        let item = item.trim();
        if item.is_empty() || seen.contains(item) {
            continue;
        }
        seen.insert(item.to_string());
        items.push(item.to_string());
    }
    items
}

fn fallback_models_from_text(text: &str) -> Vec<String> {
    split_list(text)
}

fn glossary_to_text(entries: &[String]) -> String {
    entries.join(", ")
}

/// Glossary entries are arbitrary terms, so unlike model ids they are never
/// run through the legacy-prefix stripper.
fn glossary_from_text(text: &str) -> Vec<String> {
    split_list(text)
}

fn fallback_models_to_text(models: &[String]) -> String {
    models.join(", ")
}

/// An empty field is not an empty setting: it means "use the default", and the
/// loader would otherwise keep the old value with no visible reason.
fn trimmed_or(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The loader silently drops `timeout_s = 0; the panel must not write a value
/// that would look like the edit was forgotten.
fn valid_timeout(seconds: u64) -> u64 {
    seconds.max(1)
}

/// Same for `upscale < 1.0. A non-finite value can only come from a broken
/// widget read, and 1.0 is the documented floor.
fn valid_upscale(factor: f32) -> f32 {
    if factor.is_finite() {
        factor.max(1.0)
    } else {
        1.0
    }
}

/// Whether the API can be reached with what the user has entered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Credentials {
    Ready,
    Missing,
}

/// A local runtime (Ollama, vLLM, LM Studio) needs no key, so a missing key
/// there is not a problem the header should raise.
fn credentials(key_source: Option<&'static str>, loopback: bool) -> Credentials {
    if key_source.is_some() || loopback {
        Credentials::Ready
    } else {
        Credentials::Missing
    }
}

/// (chip text, css class) for the header.
fn chip_style(state: Credentials) -> (&'static str, &'static str) {
    match state {
        Credentials::Ready => ("API 已就绪", "vellum-success"),
        Credentials::Missing => ("缺少密钥", "vellum-error"),
    }
}

/// One line naming where the key comes from, or what to do when there is none.
fn key_hint(source: Option<&'static str>, loopback: bool, env_name: &str) -> String {
    match source {
        Some(source) => format!("密钥来源：{source}"),
        None if loopback => "本地接口不需要密钥".to_string(),
        None => format!("尚未找到密钥：可填在上面的输入框，或设置环境变量 {env_name}"),
    }
}

/// A probe result as one readable line: `(message, is_error)`.
///
/// The configured model is checked against the returned list because reachable
/// plus wrong model is its own failure mode: a Gemini OpenAI-compatible endpoint
/// answers `/models` happily and then 404s every request for `gpt-4o-mini`.
fn probe_message(result: &Result<Vec<String>, String>, model: &str) -> (String, bool) {
    match result {
        // A provider that does not implement /models is still reachable; saying
        // 失败 here would send the user hunting for a problem that is not one.
        Ok(models) if models.is_empty() => ("连接成功，但接口未返回模型列表".to_string(), false),
        Ok(models) => {
            let model = model.trim();
            if !model.is_empty() && !models.iter().any(|known| known == model) {
                let preview: Vec<&str> = models.iter().take(3).map(String::as_str).collect();
                return (
                    // A hint, not a failure: some gateways omit models they
                    // still serve (Gemini answered 200 for a name its /models
                    // never listed), so "not in the list" is not proof of a
                    // wrong name.
                    format!(
                        "接口可达（{} 个模型）；列表里没有 {model}，可用示例：{}",
                        models.len(),
                        preview.join(" / ")
                    ),
                    false,
                );
            }
            (format!("连接成功 · {} 个模型可用", models.len()), false)
        }
        Err(err) => (format!("连接失败：{err}"), true),
    }
}

/// The widgets of the three pages, grouped so reading and repopulating the form
/// stays in one place.
struct FormWidgets {
    base_url: Entry,
    key: controls::SecretEntry,
    key_env: Entry,
    timeout: SpinButton,
    proxy: Entry,
    model: ModelPicker,
    target_lang: Entry,
    fallback: Entry,
    glossary: Entry,
    engine_builtin: CheckButton,
    engine_api: CheckButton,
    langs: Entry,
    preprocess: Switch,
    upscale: SpinButton,
    api_model: ModelPicker,
    api_timeout: SpinButton,
    save_switch: Switch,
    copy_switch: Switch,
}

impl FormWidgets {
    fn build() -> Self {
        let base_url = Entry::builder()
            .placeholder_text(DEFAULT_API_BASE_URL)
            .hexpand(true)
            .build();
        // The reveal toggle is drawn inside the field now (controls::SecretEntry),
        // so the row no longer needs a detached 显示 button beside it.
        let key = controls::SecretEntry::new("sk-…（留空则读环境变量）");
        let key_env = Entry::builder()
            .placeholder_text(DEFAULT_API_KEY_ENV)
            .hexpand(true)
            .build();

        let timeout = SpinButton::with_range(1.0, 600.0, 5.0);
        timeout.set_numeric(true);
        timeout.set_digits(0);
        timeout.set_value(DEFAULT_API_TIMEOUT_S as f64);
        timeout.set_size_request(140, -1);

        // The environment-variable rules live in the tooltip and the placeholder
        // rather than in a paragraph under the field: the same information, read
        // when the user is actually looking at the field.
        let proxy = Entry::builder()
            .placeholder_text("http://127.0.0.1:7890")
            .tooltip_text("留空读取 HTTPS_PROXY / ALL_PROXY；填 none 强制直连")
            .hexpand(true)
            .build();

        // The model field is the one place where typing by hand is not enough:
        // every endpoint names its models differently, so the same widget offers
        // the fetched list next to free text.
        let model = ModelPicker::new(
            "翻译模型",
            Some("可从接口获取后选择，也可以直接手写"),
            DEFAULT_LLM_MODEL,
        );
        let target_lang = Entry::builder()
            .placeholder_text(DEFAULT_TARGET_LANG)
            .tooltip_text("翻译后输出的语言")
            .hexpand(true)
            .build();
        let fallback = Entry::builder()
            .placeholder_text("留空表示不重试其他模型")
            .hexpand(true)
            .build();
        let glossary = Entry::builder()
            .placeholder_text("如 Nexus, Vellum=vellum 截图工具")
            .hexpand(true)
            .build();
        glossary.set_tooltip_text(Some(
            "写 term 表示原样保留不翻译；写 term=译法 表示固定用这个译法。逗号是分隔符，词条本身不能含逗号",
        ));

        // Short labels: the segmented control reads as a choice, and the
        // trade-off between the two engines belongs in the row's own hint, not
        // in a parenthetical glued to each option.
        let engine_builtin = CheckButton::with_label("内置 Tesseract");
        let engine_api = CheckButton::with_label("API 视觉模型");
        engine_api.set_group(Some(&engine_builtin));
        engine_builtin.set_active(true);

        let langs = Entry::builder()
            .placeholder_text(DEFAULT_OCR_LANGS)
            .hexpand(true)
            .build();
        let preprocess = Switch::builder().active(true).valign(Align::Center).build();

        let upscale = SpinButton::with_range(1.0, 8.0, 0.5);
        upscale.set_numeric(true);
        upscale.set_digits(1);
        // The unit label makes the range obvious; the tooltip explains the cost.
        upscale.set_tooltip_text(Some("识别前把选区图片放大的倍数：越高越准，也越慢"));
        upscale.set_value(3.0);
        upscale.set_size_request(140, -1);

        let api_model = ModelPicker::new(
            "OCR 视觉模型",
            Some("留空表示复用上面的翻译模型"),
            "留空复用翻译模型",
        );
        let api_timeout = SpinButton::with_range(1.0, 600.0, 5.0);
        api_timeout.set_numeric(true);
        api_timeout.set_digits(0);
        api_timeout.set_value(DEFAULT_API_TIMEOUT_S as f64);
        api_timeout.set_size_request(140, -1);

        let save_switch = Switch::builder().active(true).valign(Align::Center).build();
        let copy_switch = Switch::builder().active(true).valign(Align::Center).build();

        let form = Self {
            base_url,
            key,
            key_env,
            timeout,
            proxy,
            model,
            target_lang,
            fallback,
            glossary,
            engine_builtin,
            engine_api,
            langs,
            preprocess,
            upscale,
            api_model,
            api_timeout,
            save_switch,
            copy_switch,
        };

        // The engine choice decides which half of the page matters; the wiring
        // lives here so repopulating only has to call `sync_engine`.
        let langs = form.langs.clone();
        let api_model = form.api_model.root.clone();
        let api_timeout = form.api_timeout.clone();
        form.engine_api.connect_toggled(move |button| {
            sync_engine_widgets(button.is_active(), &langs, &api_model, &api_timeout);
        });
        form.sync_engine();
        form
    }

    fn values(&self) -> FormValues {
        FormValues {
            base_url: self.base_url.text().to_string(),
            api_key: self.key.text(),
            api_key_env: self.key_env.text().to_string(),
            timeout_s: self.timeout.value().round() as u64,
            proxy: self.proxy.text().to_string(),
            model: self.model.text().to_string(),
            target_lang: self.target_lang.text().to_string(),
            fallback_models: self.fallback.text().to_string(),
            glossary: self.glossary.text().to_string(),
            engine: if self.engine_api.is_active() {
                OCR_ENGINE_API
            } else {
                OCR_ENGINE_BUILTIN
            }
            .to_string(),
            langs: self.langs.text().to_string(),
            preprocess: self.preprocess.is_active(),
            upscale: self.upscale.value() as f32,
            api_model: self.api_model.text().to_string(),
            api_timeout_s: self.api_timeout.value().round() as u64,
            save_after_capture: self.save_switch.is_active(),
            copy_after_capture: self.copy_switch.is_active(),
        }
    }

    fn populate(&self, cfg: &Config, prefs: &Preferences) {
        let values = FormValues::from_config(cfg, prefs);
        self.base_url.set_text(&values.base_url);
        self.key.set_text(&values.api_key);
        self.key_env.set_text(&values.api_key_env);
        self.timeout.set_value(values.timeout_s as f64);
        self.proxy.set_text(&values.proxy);
        self.model.set_text(&values.model);
        self.target_lang.set_text(&values.target_lang);
        self.fallback.set_text(&values.fallback_models);
        self.glossary.set_text(&values.glossary);
        let api = values.engine == OCR_ENGINE_API;
        self.engine_api.set_active(api);
        self.engine_builtin.set_active(!api);
        self.langs.set_text(&values.langs);
        self.preprocess.set_active(values.preprocess);
        self.upscale.set_value(f64::from(values.upscale));
        self.api_model.set_text(&values.api_model);
        self.api_timeout.set_value(values.api_timeout_s as f64);
        self.save_switch.set_active(values.save_after_capture);
        self.copy_switch.set_active(values.copy_after_capture);
        self.sync_engine();
    }

    fn sync_engine(&self) {
        sync_engine_widgets(
            self.engine_api.is_active(),
            &self.langs,
            &self.api_model.root,
            &self.api_timeout,
        );
    }

    /// Fires when any field that decides the header chip changes.
    fn connect_credentials_changed(&self, callback: Rc<dyn Fn()>) {
        let url = Rc::clone(&callback);
        self.base_url.connect_changed(move |_| url());
        let env = Rc::clone(&callback);
        self.key_env.connect_changed(move |_| env());
        self.key.connect_changed(callback);
    }
}

/// Greys out the half of the OCR page the chosen engine does not use. Hiding the
/// fields instead would make the page jump as the radio changes.
fn sync_engine_widgets(
    api: bool,
    langs: &impl IsA<gtk4::Widget>,
    api_model: &impl IsA<gtk4::Widget>,
    api_timeout: &impl IsA<gtk4::Widget>,
) {
    langs.set_sensitive(!api);
    api_model.set_sensitive(api);
    api_timeout.set_sensitive(api);
}

/// The pages in the order the sidebar lists them.
///
/// `show_page` matches a page name against this table to find the nav item that
/// has to be checked, so the visible page and the highlight cannot drift apart.
const PAGE_NAMES: [&str; 3] = ["api", "text", "capture"];

/// The widgets of the connection-test row.
///
/// Grouped rather than passed one by one: `api_page` is otherwise a small zoo of
/// borrowed widgets, and the dot/line/spinner/button always travel together.
struct ProbeWidgets {
    dot: GtkBox,
    status: Label,
    spinner: Spinner,
    button: Button,
}

struct Panel {
    id: u64,
    window: ApplicationWindow,
    /// Kept so the page shortcuts below can switch it.
    stack: Stack,
    /// The three sidebar buttons, in `PAGE_NAMES` order.
    nav_items: Vec<ToggleButton>,
    /// The document as loaded, so saving preserves the sections the panel does
    /// not own (it edits three of the four).
    base: RefCell<Config>,
    /// The title-bar pill. Its inner label keeps the old chip semantics:
    /// `refresh_credentials` rewrites the text and swaps
    /// "vellum-success"/"vellum-error" as the key source changes.
    chip: controls::StatusPill,
    key_hint: Label,
    status: Label,
    /// The whole probe block — dot, status line, spinner and 测试连接 button —
    /// kept together because it is built before the pages and borrowed by
    /// `api_page`, which already carries the form and the key row.
    probe: ProbeWidgets,
    /// Result line for 「获取模型」, which shares the probe request.
    models_status: Label,
    fetch_button: Button,
    /// Last list the endpoint returned, so typing a model that is not in it can
    /// be flagged before the user saves.
    fetched_models: RefCell<Vec<String>>,
    save_button: Button,
    reset_button: Button,
    form: FormWidgets,
    /// Set from `close-request`; a probe that lands afterwards must not touch a
    /// window the user dismissed.
    closed: Cell<bool>,
    /// Guards against a second connection test while one is in flight.
    probing: Cell<bool>,
}

impl Panel {
    fn new(app: &Application) -> Rc<Self> {
        theme::install_default();

        let base = Config::load();
        let prefs = prefs::load();
        let form = FormWidgets::build();

        // resizable(false) is not cosmetic: it is what makes the panel an
        // independent window instead of a layout participant.
        //
        // MEASURED on Hyprland with the scrolling layout: a resizable panel maps
        // as a tiling COLUMN (926x996) first, which scrolls the whole desktop
        // sideways (firefox moved 940px), and the float requested 60ms later
        // removes the column but leaves the scroll. With resizable(false) the
        // window declares a fixed size, and both Hyprland and niri open a
        // fixed-size window floating from its first frame: no tiled frame, no
        // scroll, and no compositor window rule needed. Same shape as the
        // layer-shell surfaces, without giving up a normal toplevel (drag,
        // blur).
        let window = ApplicationWindow::builder()
            .application(app)
            .default_width(WIDTH)
            .default_height(HEIGHT)
            .resizable(false)
            .title("设置")
            .build();
        window.add_css_class("vellum-window");
        // Panel-only glass hook: the compositor blurs whatever shows through, but
        // .vellum-window is shared with the pin and result windows and .vellum-card
        // with the long-shot panel, so translucency lives on a class only this
        // window carries.
        window.add_css_class("vellum-glass");

        let root = GtkBox::new(Orientation::Vertical, 0);

        // The title bar is ours rather than a compositor's: the target session
        // runs without server-side decorations, so without our own header there
        // would be no title and no way to move the window.
        // Only one title, no subtitle: the header states where we are, the
        // sidebar and the cards state what can be changed.
        let title = Label::builder().label("设置").xalign(0.0).build();
        title.add_css_class("vellum-title");

        let chip = controls::StatusPill::new("", "vellum-success");

        let close = Button::builder()
            .child(&Image::from_icon_name("window-close-symbolic"))
            .tooltip_text("关闭 (Esc)")
            .valign(Align::Center)
            .build();
        close.add_css_class("vellum-quiet");
        close.add_css_class("vellum-icon-button");

        let title_bar = GtkBox::new(Orientation::Horizontal, 12);
        title_bar.add_css_class("vellum-titlebar-inner");
        title_bar.set_margin_top(12);
        title_bar.set_margin_bottom(10);
        title_bar.set_margin_start(18);
        title_bar.set_margin_end(12);
        title_bar.append(&title);
        let bar_spacer = GtkBox::new(Orientation::Horizontal, 0);
        bar_spacer.set_hexpand(true);
        title_bar.append(&bar_spacer);
        title_bar.append(&chip.root);
        title_bar.append(&close);

        // The whole strip drags the window: children keep their own gestures, so
        // the close button still clicks.
        let titlebar = crate::drag::draggable(&title_bar);
        titlebar.add_css_class("vellum-titlebar");
        root.append(&titlebar);

        // --- left navigation --------------------------------------------------
        // Group captions split the three pages into "接口与模型" and "截图"; one
        // radio group spans both, so exactly one page is ever selected.
        let sidebar = GtkBox::new(Orientation::Vertical, 2);
        sidebar.add_css_class("vellum-sidebar");
        sidebar.set_size_request(184, -1);
        sidebar.set_margin_top(8);
        sidebar.set_margin_bottom(8);
        sidebar.set_margin_start(10);
        sidebar.set_margin_end(6);
        sidebar.append(&nav_section("接口与模型"));

        let nav_api = nav_item("模型接入", "network-server-symbolic");
        let nav_text = nav_item("翻译与 OCR", "accessories-dictionary-symbolic");
        sidebar.append(&nav_api);
        sidebar.append(&nav_text);
        sidebar.append(&nav_section("截图"));
        let nav_capture = nav_item("截图行为", "camera-photo-symbolic");
        sidebar.append(&nav_capture);

        nav_text.set_group(Some(&nav_api));
        nav_capture.set_group(Some(&nav_api));

        let nav_spacer = GtkBox::new(Orientation::Vertical, 0);
        nav_spacer.set_vexpand(true);
        sidebar.append(&nav_spacer);

        let nav_items = vec![nav_api, nav_text, nav_capture];

        // --- pages ------------------------------------------------------------
        // Page-local widgets first: the pages take them by reference.
        let key_hint = Label::builder().label("").xalign(0.0).wrap(true).build();
        key_hint.add_css_class("vellum-caption");
        let probe_status = Label::builder().label("尚未测试").xalign(0.0).build();
        probe_status.add_css_class("vellum-status-line");
        probe_status.set_ellipsize(EllipsizeMode::End);
        probe_status.set_hexpand(true);
        // "尚未测试" is neither ready nor missing yet, so the dot starts neutral.
        let probe_dot = controls::status_dot(controls::DOT_INFO);
        let probe_spinner = Spinner::new();
        let test_button =
            controls::secondary_button("测试连接", Some("network-transmit-receive-symbolic"));
        test_button.set_valign(Align::Center);
        test_button.set_tooltip_text(Some("请求 /models 验证地址与密钥"));
        let probe = ProbeWidgets {
            dot: probe_dot,
            status: probe_status,
            spinner: probe_spinner,
            button: test_button,
        };

        let models_status = Label::builder().label("尚未获取模型").xalign(0.0).build();
        models_status.add_css_class("vellum-status-line");
        models_status.set_ellipsize(EllipsizeMode::End);
        let fetch_button = controls::secondary_button("获取模型", Some("view-refresh-symbolic"));
        fetch_button.set_tooltip_text(Some("请求接口的 /models 并填入下面的模型选择器"));

        let stack = Stack::builder()
            .transition_type(StackTransitionType::SlideLeftRight)
            .vexpand(true)
            .build();
        stack.add_titled(
            &api_page(&form, &key_hint, &probe, &models_status, &fetch_button),
            Some("api"),
            "模型接入",
        );
        stack.add_titled(&text_page(&form), Some("text"), "翻译与 OCR");
        stack.add_titled(&capture_page(&form), Some("capture"), "截图行为");

        let content = GtkBox::new(Orientation::Vertical, 0);
        content.add_css_class("vellum-content-column");
        content.set_hexpand(true);
        content.append(&stack);

        let body = GtkBox::new(Orientation::Horizontal, 0);
        body.set_vexpand(true);
        body.append(&sidebar);
        body.append(&content);
        root.append(&body);

        // --- footer -----------------------------------------------------------
        // The footer keeps a fixed slot for feedback, so the buttons do not jump
        // sideways when a message appears.
        let status = Label::builder().label("").xalign(0.0).build();
        status.add_css_class("vellum-status");
        status.add_css_class("vellum-status-line");
        status.set_ellipsize(EllipsizeMode::End);
        status.set_hexpand(true);

        let reset_button = controls::secondary_button("恢复默认", None);
        reset_button.set_tooltip_text(Some("把三个页面填回内置默认值，保存后才写入"));
        let save_button = controls::primary_button("保存更改");
        save_button.set_tooltip_text(Some("保存 (Ctrl+S)，下一次截图生效"));

        let footer = GtkBox::new(Orientation::Horizontal, 12);
        footer.add_css_class("vellum-footer");
        footer.set_margin_top(12);
        footer.set_margin_bottom(14);
        footer.set_margin_start(18);
        footer.set_margin_end(18);
        footer.append(&status);
        footer.append(&reset_button);
        footer.append(&save_button);
        root.append(&footer);

        window.set_child(Some(&root));

        let panel = Rc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            window,
            stack: stack.clone(),
            nav_items,
            base: RefCell::new(base),
            chip,
            key_hint,
            status,
            probe,
            models_status,
            fetch_button,
            fetched_models: RefCell::new(Vec::new()),
            save_button,
            reset_button,
            form,
            closed: Cell::new(false),
            probing: Cell::new(false),
        });

        panel.form.populate(&panel.base.borrow(), &prefs);
        panel.refresh_credentials();

        // Acceptance/screenshot aid, next to VELLUM_PANEL_OPEN_PICKER: open a
        // chosen page without driving the UI. It goes through `show_page`, so the
        // sidebar highlight follows the environment variable too.
        let initial = std::env::var("VELLUM_PANEL_PAGE").unwrap_or_default();
        panel.show_page(match initial.as_str() {
            "text" => "text",
            "capture" => "capture",
            _ => "api",
        });

        // Acceptance aid, next to VELLUM_PANEL_PAGE: fetch the model list and
        // open a picker, so the dropdown can be captured on a session where no
        // pointer can be injected.
        if let Ok(target) = std::env::var("VELLUM_PANEL_OPEN_PICKER") {
            panel.fetch_models();
            let weak = Rc::downgrade(&panel);
            glib::timeout_add_local_once(Duration::from_millis(4000), move || {
                if let Some(panel) = weak.upgrade() {
                    if target == "api" {
                        panel.form.api_model.popup();
                    } else {
                        panel.form.model.popup();
                    }
                }
            });
        }

        PANELS.with(|slot| slot.borrow_mut().push((panel.id, panel.clone())));
        panel.connect(&close);
        panel
    }

    fn connect(self: &Rc<Self>, close: &Button) {
        let window = self.window.clone();
        close.connect_clicked(move |_| window.close());

        // The sidebar and the shortcuts share one entry point, so the checked nav
        // item and the visible page can never disagree.
        for (item, name) in self.nav_items.iter().zip(PAGE_NAMES) {
            let this = Rc::downgrade(self);
            item.connect_toggled(move |button| {
                if button.is_active()
                    && let Some(this) = this.upgrade()
                {
                    this.show_page(name);
                }
            });
        }

        let this = Rc::downgrade(self);
        self.save_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.save();
            }
        });

        let this = Rc::downgrade(self);
        self.reset_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.restore_defaults();
            }
        });

        let this = Rc::downgrade(self);
        self.probe.button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.test_connection();
            }
        });

        let this = Rc::downgrade(self);
        self.fetch_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.fetch_models();
            }
        });

        // Live header feedback while typing: the user should not have to save a
        // key to learn that it is not being picked up.
        let this = Rc::downgrade(self);
        self.form.connect_credentials_changed(Rc::new(move || {
            if let Some(this) = this.upgrade() {
                this.refresh_credentials();
            }
        }));

        let this = Rc::downgrade(self);
        self.form.model.connect_changed(Rc::new(move || {
            if let Some(this) = this.upgrade() {
                this.refresh_model_hint();
            }
        }));

        let keys = EventControllerKey::new();
        let this = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if key == Key::Escape {
                if let Some(this) = this.upgrade() {
                    this.window.close();
                }
                return glib::Propagation::Stop;
            }
            if matches!(key, Key::s | Key::S) && modifiers.contains(ModifierType::CONTROL_MASK) {
                if let Some(this) = this.upgrade() {
                    this.save();
                }
                return glib::Propagation::Stop;
            }
            // Ctrl+1 / Ctrl+2 / Ctrl+3 jump to a page: the form is long enough
            // that reaching for the sidebar with the mouse is annoying.
            if modifiers.contains(ModifierType::CONTROL_MASK)
                && let Some(this) = this.upgrade()
            {
                let page = match key {
                    Key::_1 | Key::KP_1 => Some("api"),
                    Key::_2 | Key::KP_2 => Some("text"),
                    Key::_3 | Key::KP_3 => Some("capture"),
                    _ => None,
                };
                if let Some(page) = page {
                    this.show_page(page);
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);

        // Weak on purpose: the registry owns the strong reference, and a strong
        // one here would be a cycle that never frees.
        let this = Rc::downgrade(self);
        let id = self.id;
        self.window.connect_close_request(move |_| {
            if let Some(this) = this.upgrade() {
                this.closed.set(true);
            }
            // Drop our own ownership so the struct can go.
            PANELS.with(|slot| slot.borrow_mut().retain(|(known, _)| *known != id));
            glib::Propagation::Proceed
        });

        // Fallback for a compositor that does not float fixed-size windows on
        // its own. vellum never writes the user's compositor config (DESIGN.md
        // §8), so this is how the panel floated before the fixed-size hint
        // above existed; it is kept because it costs one no-op dispatch on a
        // compositor that already floated the window. Looked up by pid rather
        // than acting on the focused window, because focus may have moved
        // during the delay.
        self.window.connect_map(|_| {
            glib::timeout_add_local_once(FLOAT_DELAY, || {
                crate::own_window::float_own_window_soon();
            });
        });
    }

    /// Switches the visible page and brings the sidebar with it.
    ///
    /// Every entry point — the sidebar, Ctrl+1/2/3 and VELLUM_PANEL_PAGE — goes
    /// through here: moving the stack alone would leave the checked nav item
    /// pointing at a page that is no longer on screen.
    fn show_page(&self, name: &str) {
        let Some(index) = PAGE_NAMES.iter().position(|page| *page == name) else {
            return;
        };
        self.stack.set_visible_child_name(name);
        if let Some(item) = self.nav_items.get(index)
            && !item.is_active()
        {
            // `set_active` emits `toggled`, whose handler calls back into
            // `show_page` with the same name; the second call is a no-op.
            item.set_active(true);
        }
    }

    fn present(self: &Rc<Self>) {
        self.window.present();
    }

    fn apply(self: &Rc<Self>, update: Update) {
        if self.closed.get() {
            return;
        }
        match update {
            Update::Probe(result) => self.show_probe(&result),
            Update::Models(result) => self.show_models(&result),
        }
    }

    /// The api section as it currently stands in the widgets.
    fn api_config(&self) -> ApiConfig {
        let values = self.form.values();
        ApiConfig {
            base_url: trimmed_or(&values.base_url, DEFAULT_API_BASE_URL),
            api_key: values.api_key.trim().to_string(),
            api_key_env: trimmed_or(&values.api_key_env, DEFAULT_API_KEY_ENV),
            timeout_s: valid_timeout(values.timeout_s),
            proxy: values.proxy.trim().to_string(),
        }
    }

    /// Rewrites the title-bar pill and the key-source line from the live widgets.
    fn refresh_credentials(&self) {
        let api = self.api_config();
        let source = api.key_source();
        let loopback = api.targets_loopback();
        let (text, class) = chip_style(credentials(source, loopback));
        // Text and state class move together: a pill that is briefly green with
        // "缺少密钥" in it is worse than no pill at all.
        self.chip.set_state(text, class);
        self.key_hint
            .set_label(&key_hint(source, loopback, &api.api_key_env));
    }

    fn save(self: &Rc<Self>) {
        let values = self.form.values();
        let config = values.to_config(&self.base.borrow());
        match config.save() {
            Ok(()) => {
                let stored = prefs::store(&values.to_preferences());
                *self.base.borrow_mut() = config;
                self.refresh_credentials();
                match stored {
                    Ok(()) => self.flash("已保存 · 下一次截图生效", Flash::Success),
                    // The API settings did land; only the tray preferences did
                    // not. Saying "保存失败" would be wrong.
                    Err(err) => self.flash(
                        &format!("接口设置已保存，但输出偏好写入失败：{err}"),
                        Flash::Error,
                    ),
                }
            }
            Err(err) => self.flash(&format!("保存失败：{err}"), Flash::Error),
        }
    }

    fn restore_defaults(&self) {
        self.form
            .populate(&Config::default(), &Preferences::default());
        self.refresh_credentials();
        self.flash("已填入默认值，点「保存」后生效", Flash::Info);
    }

    fn test_connection(self: &Rc<Self>) {
        if self.probing.get() {
            return;
        }
        let api = self.api_config();
        let timeout = Duration::from_secs(api.timeout_s);

        self.probing.set(true);
        self.probe.button.set_sensitive(false);
        self.probe.spinner.set_visible(true);
        self.probe.spinner.set_spinning(true);
        self.set_probe_status("正在测试连接…", Flash::Info);

        let id = self.id;
        std::thread::spawn(move || {
            let result = vellum_text::api::probe(&api, timeout);
            post(id, Update::Probe(result));
        });
    }

    fn show_probe(&self, result: &Result<Vec<String>, String>) {
        self.probing.set(false);
        self.probe.button.set_sensitive(true);
        self.probe.spinner.set_spinning(false);
        self.probe.spinner.set_visible(false);
        let (message, error) = probe_message(result, &self.form.model.text());
        self.set_probe_status(&message, if error { Flash::Error } else { Flash::Success });
    }

    /// Same request as the connection test, but the answer is also poured into
    /// the two model pickers — this is the whole point of the button: the
    /// endpoint is the only authority on its own model names.
    fn fetch_models(&self) {
        if self.probing.get() {
            return;
        }
        let api = self.api_config();
        let timeout = Duration::from_secs(api.timeout_s.clamp(5, 60));

        self.probing.set(true);
        self.fetch_button.set_sensitive(false);
        self.models_status.remove_css_class("vellum-error");
        self.models_status.remove_css_class("vellum-success");
        self.models_status.set_label("正在获取模型…");

        let id = self.id;
        std::thread::spawn(move || {
            let result = vellum_text::api::probe(&api, timeout);
            post(id, Update::Models(result));
        });
    }

    fn show_models(&self, result: &Result<Vec<String>, String>) {
        self.probing.set(false);
        self.fetch_button.set_sensitive(true);
        match result {
            Ok(models) if models.is_empty() => {
                self.models_status
                    .set_label("接口可达，但没有返回模型列表；请手动填写模型名");
            }
            Ok(models) => {
                self.form.model.set_models(models);
                self.form.api_model.set_models(models);
                *self.fetched_models.borrow_mut() = models.clone();
                self.refresh_model_hint();
            }
            Err(err) => {
                self.models_status.add_css_class("vellum-error");
                self.models_status.set_label(&format!("获取失败：{err}"));
            }
        }
    }

    /// Live feedback under the model section. A typed name that the endpoint
    /// never reported is exactly the mistake this panel exists to prevent (a
    /// Gemini endpoint 404s `gpt-4o-mini`), so it is worth saying before saving.
    fn refresh_model_hint(&self) {
        let fetched = self.fetched_models.borrow();
        if fetched.is_empty() {
            return;
        }
        let typed = self.form.model.text();
        let typed = typed.trim();
        for class in ["vellum-success", "vellum-error"] {
            self.models_status.remove_css_class(class);
        }
        if typed.is_empty() {
            self.models_status
                .set_label(&format!("已获取 {} 个模型；翻译模型还没填", fetched.len()));
        } else if fetched.iter().any(|known| known == typed) {
            self.models_status.add_css_class("vellum-success");
            self.models_status.set_label(&format!(
                "已获取 {} 个模型；{typed} 在列表里",
                fetched.len()
            ));
        } else {
            // Same softening as the connection test: not every gateway lists
            // every model it serves.
            self.models_status.set_label(&format!(
                "已获取 {} 个模型，其中没有 {typed}（部分服务会漏报）；可从右侧列表改选",
                fetched.len()
            ));
        }
    }

    fn set_probe_status(&self, message: &str, kind: Flash) {
        self.probe.status.set_label(message);
        for class in ["vellum-success", "vellum-error"] {
            self.probe.status.remove_css_class(class);
        }
        if let Some(class) = kind.class() {
            self.probe.status.add_css_class(class);
        }
        // The dot restates the outcome: still running is neither, so it keeps
        // the neutral class and only the two resolved states recolour it.
        for class in [
            controls::DOT_READY,
            controls::DOT_MISSING,
            controls::DOT_INFO,
        ] {
            self.probe.dot.remove_css_class(class);
        }
        self.probe.dot.add_css_class(match kind {
            Flash::Success => controls::DOT_READY,
            Flash::Error => controls::DOT_MISSING,
            Flash::Info => controls::DOT_INFO,
        });
    }

    fn flash(&self, message: &str, kind: Flash) {
        self.status.set_label(message);
        for class in ["vellum-success", "vellum-error"] {
            self.status.remove_css_class(class);
        }
        if let Some(class) = kind.class() {
            self.status.add_css_class(class);
        }
    }
}

/// A sidebar group caption: quiet and wide-tracked, labelling the pages below it
/// without competing with them.
fn nav_section(text: &str) -> Label {
    let label = Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("vellum-nav-section");
    label.set_margin_top(8);
    label.set_margin_bottom(2);
    label.set_margin_start(10);
    label
}

/// One page entry in the sidebar. The caller puts them in a group, so exactly
/// one is ever checked.
fn nav_item(label: &str, icon: &str) -> ToggleButton {
    let content = GtkBox::new(Orientation::Horizontal, 10);
    let image = Image::from_icon_name(icon);
    image.set_pixel_size(15);
    content.append(&image);
    content.append(&Label::new(Some(label)));
    let button = ToggleButton::builder()
        .child(&content)
        .hexpand(true)
        .halign(Align::Fill)
        .build();
    button.add_css_class("vellum-nav-item");
    button
}

/// The inner column of one page: a fixed 640 px form, centred in the content
/// column.
///
/// A row that spans the whole 900 px window reads as a spreadsheet; capping the
/// column keeps each label next to the control it names.
fn page_box() -> GtkBox {
    let content = GtkBox::new(Orientation::Vertical, 10);
    content.add_css_class("vellum-page-content");
    content.set_halign(Align::Center);
    content.set_size_request(640, -1);
    content.set_margin_top(8);
    content.set_margin_bottom(16);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content
}

/// Pages scroll rather than grow: the OCR page is the tallest, and a window
/// whose content does not fit would silently clip its own footer.
fn page(content: &GtkBox) -> ScrolledWindow {
    let scroller = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vexpand(true)
        .child(content)
        .build();
    scroller.add_css_class("vellum-page");
    scroller
}

/// The key field plus the live "where the key comes from" line.
///
/// The caption changes with what is typed, so it cannot be a static subtitle;
/// the row shape is the one `action_row_stacked` builds, with the caption appended
/// after the field.
fn key_row(form: &FormWidgets, key_hint: &Label) -> GtkBox {
    let row = GtkBox::new(Orientation::Vertical, 7);
    row.add_css_class("vellum-row");
    row.add_css_class("vellum-row-stacked");
    let title = Label::builder().label("API 密钥").xalign(0.0).build();
    title.add_css_class("vellum-row-title");
    row.append(&title);
    row.append(&form.key.root);
    row.append(key_hint);
    row
}

/// Probe feedback and its action on one line: the spinner leads the status line,
/// so the row does not reflow when a test starts.
fn probe_row(probe: &ProbeWidgets) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 10);
    row.add_css_class("vellum-row");
    row.append(&probe.dot);
    row.append(&probe.spinner);
    row.append(&probe.status);
    row.append(&probe.button);
    row
}

/// Grey out the rows that only affect the built-in engine.
///
/// They are not dead settings under the API engine: `recognize()` falls back to
/// Tesseract whenever the vision call fails, and that path does read the
/// language pack, the upscale factor and the preprocess switch. What they do not
/// do is shape the API attempt itself, so the row is disabled — with the hint
/// saying why — instead of sitting there looking like it applies to the vision
/// model. Tuning the fallback means selecting the built-in engine, which is what
/// a user does when the fallback is what they are relying on.
fn scope_to_builtin_engine(form: &FormWidgets, rows: &[GtkBox]) {
    let rows: Vec<GtkBox> = rows.to_vec();
    let api = form.engine_api.clone();
    let update = Rc::new({
        let rows = rows.clone();
        move || {
            let on_api = api.is_active();
            for row in &rows {
                row.set_sensitive(!on_api);
            }
        }
    });
    update();
    {
        let update = update.clone();
        form.engine_builtin.connect_toggled(move |_| update());
    }
    {
        let update = update.clone();
        form.engine_api.connect_toggled(move |_| update());
    }
}

/// The OCR engine choice as one segmented control.
///
/// The CheckButtons keep their group and their `is_active` semantics; only the
/// shell changes, so `sync_engine_widgets` still greys out the half that the
/// chosen engine does not use.
fn engine_row(form: &FormWidgets) -> GtkBox {
    let row = GtkBox::new(Orientation::Vertical, 7);
    row.add_css_class("vellum-row");
    row.add_css_class("vellum-row-stacked");

    let head = GtkBox::new(Orientation::Horizontal, 10);
    let title = Label::builder().label("识别引擎").xalign(0.0).build();
    title.add_css_class("vellum-row-title");
    title.set_hexpand(true);
    head.append(&title);
    head.append(&controls::segmented(&[
        &form.engine_builtin,
        &form.engine_api,
    ]));
    row.append(&head);

    let hint = Label::builder()
        .label("内置引擎离线可用，API 失败时也回退到它；下方三项只作用于内置引擎")
        .xalign(0.0)
        .wrap(true)
        .build();
    hint.add_css_class("vellum-row-sub");
    row.append(&hint);
    row
}

fn api_page(
    form: &FormWidgets,
    key_hint: &Label,
    probe: &ProbeWidgets,
    models_status: &Label,
    fetch_button: &Button,
) -> ScrolledWindow {
    let content = page_box();

    let (api_card, api_body, _) = controls::card(
        "接口",
        Some("翻译与 API OCR 共用；任何 OpenAI 兼容服务都可以"),
    );
    let api_rows = controls::row_group();
    // The URL is the widest value on the page, and the key carries the reveal
    // toggle inside it: both own their row instead of sharing a half-width one.
    controls::push_row(
        &api_rows,
        &controls::action_row_stacked("接口地址", None, &form.base_url),
    );
    controls::push_row(&api_rows, &key_row(form, key_hint));
    controls::push_row(
        &api_rows,
        &controls::action_row("密钥环境变量", None, &form.key_env),
    );
    controls::push_row(
        &api_rows,
        &controls::action_row(
            "单次请求超时",
            None,
            &controls::stepper(&form.timeout, "秒"),
        ),
    );
    controls::push_row(
        &api_rows,
        &controls::action_row_stacked(
            "HTTP 代理",
            // The environment-variable rules are in the field's tooltip; product
            // copy stays out of the way of the setting itself.
            Some("留空则自动读取系统代理设置"),
            &form.proxy,
        ),
    );
    controls::push_row(&api_rows, &probe_row(probe));
    api_body.append(&api_rows);
    content.append(&api_card);

    let (model_card, model_body, model_head) =
        controls::card("模型", Some("从接口读回真实模型名，无需手写"));
    model_head.append(fetch_button);
    let model_rows = controls::row_group();
    controls::push_row(&model_rows, models_status);
    controls::push_row(&model_rows, &form.model.root);
    controls::push_row(&model_rows, &form.api_model.root);
    model_body.append(&model_rows);
    content.append(&model_card);

    page(&content)
}

/// Translation and OCR live on one page: they share the same endpoint and the
/// same model field, so splitting them only forced the user to remember which
/// tab a setting was on.
fn text_page(form: &FormWidgets) -> ScrolledWindow {
    let content = page_box();

    let (translate_card, translate_body, _) =
        controls::card("翻译", Some("识别出的文本直接发给上面的接口"));
    let translate_rows = controls::row_group();
    controls::push_row(
        &translate_rows,
        &controls::action_row("目标语言", None, &form.target_lang),
    );
    controls::push_row(
        &translate_rows,
        &controls::action_row_stacked(
            "备用模型",
            Some("逗号分隔；只有主模型被上游明确拒绝时才依次尝试"),
            &form.fallback,
        ),
    );
    // A screenshot is full of text that must survive translation unchanged:
    // identifiers, paths, flags, product names.
    controls::push_row(
        &translate_rows,
        &controls::action_row_stacked(
            "术语表",
            Some("逗号分隔；写 term 原样保留，写 term=译法 固定译法"),
            &form.glossary,
        ),
    );
    translate_body.append(&translate_rows);
    content.append(&translate_card);

    let (ocr_card, ocr_body, _) = controls::card(
        "OCR",
        Some("内置 Tesseract 离线识别，或把选区图片交给视觉模型"),
    );
    let ocr_rows = controls::row_group();
    controls::push_row(&ocr_rows, &engine_row(form));

    // These three only affect the built-in engine: recognize_api() hands the raw
    // crop to the vision model, so an upscale or a preprocess set here would do
    // nothing while still looking active. They are greyed out under the API
    // engine instead of implying an effect that does not exist.
    let langs_row = controls::action_row("Tesseract 语言包", None, &form.langs);
    let upscale_row = controls::action_row(
        "放大倍数",
        // A glyph unit (the multiplication sign) reads as a broken icon at this
        // size, so the unit is spelled out like the two second-valued steppers.
        None,
        &controls::stepper(&form.upscale, "倍"),
    );
    let preprocess_row = controls::action_row(
        "图像预处理",
        Some("自适应灰度、极性与对比度，提升困难场景的识别率"),
        &form.preprocess,
    );
    controls::push_row(&ocr_rows, &langs_row);
    controls::push_row(&ocr_rows, &upscale_row);
    controls::push_row(&ocr_rows, &preprocess_row);
    scope_to_builtin_engine(form, &[langs_row, upscale_row, preprocess_row]);
    controls::push_row(
        &ocr_rows,
        &controls::action_row(
            "视觉模型超时",
            Some("仅 API 引擎"),
            &controls::stepper(&form.api_timeout, "秒"),
        ),
    );
    ocr_body.append(&ocr_rows);
    content.append(&ocr_card);

    page(&content)
}

/// The capture switches used to live at the bottom of the OCR page; they are
/// their own page now, so the sidebar entry matches what the page contains.
fn capture_page(form: &FormWidgets) -> ScrolledWindow {
    let content = page_box();

    let (capture_card, capture_body, _) =
        controls::card("截图行为", Some("与托盘菜单共用同一份设置"));
    let capture_rows = controls::row_group();
    controls::push_row(
        &capture_rows,
        &controls::action_row(
            "截图后保存",
            Some("关闭后区域截图与长截图都不再写入文件"),
            &form.save_switch,
        ),
    );
    controls::push_row(
        &capture_rows,
        &controls::action_row(
            "截图后复制",
            Some("关闭后不再写入剪贴板"),
            &form.copy_switch,
        ),
    );
    capture_body.append(&capture_rows);
    content.append(&capture_card);

    page(&content)
}

/// Opens the settings panel and runs until the window is closed.
pub fn run_panel() -> i32 {
    let app = application();
    app.connect_activate(|app| {
        Panel::new(app).present();
    });
    run(&app)
}

fn application() -> Application {
    Application::builder()
        .application_id(APP_ID)
        // See the module docs: a second panel must be its own process.
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

    fn values() -> FormValues {
        FormValues::from_config(&Config::default(), &Preferences::default())
    }

    #[test]
    fn fallback_models_are_trimmed_deduplicated_and_keep_their_order() {
        assert_eq!(
            fallback_models_from_text(" a, b，c;b\n\td "),
            vec!["a", "b", "c", "d"]
        );
        assert!(fallback_models_from_text("  ,  ， ;\n").is_empty());
        assert_eq!(fallback_models_to_text(&["a".into(), "b".into()]), "a, b");
    }

    #[test]
    fn blank_fields_fall_back_to_the_documented_defaults() {
        let mut form = values();
        form.base_url = "   ".into();
        form.api_key_env = String::new();
        form.model = String::new();
        form.target_lang = String::new();
        form.langs = String::new();

        let cfg = form.to_config(&Config::default());
        assert_eq!(cfg.api.base_url, DEFAULT_API_BASE_URL);
        assert_eq!(cfg.api.api_key_env, DEFAULT_API_KEY_ENV);
        assert_eq!(cfg.llm.model, DEFAULT_LLM_MODEL);
        assert_eq!(cfg.llm.target_lang, DEFAULT_TARGET_LANG);
        assert_eq!(cfg.ocr.langs, DEFAULT_OCR_LANGS);
    }

    /// The widget layer, not just `FormValues`.
    ///
    /// A field that `from_config` fills but `populate` never writes into the
    /// widget reads back empty, and the next save writes that emptiness over the
    /// user's config. That is exactly how `[llm].glossary` was being lost: the
    /// FormValues round-trip test below cannot see it, because it never touches a
    /// widget.
    #[test]
    fn the_form_widgets_round_trip_every_field_including_the_glossary() {
        crate::test_support::with_gtk(|| {
            // A widget needs a display; without one there is nothing to check.

            let cfg = Config::from_toml_str(
                r#"
            [api]
            base_url = "http://localhost:11434/v1"
            api_key = "sk-panel"
            timeout_s = 12
            [llm]
            model = "qwen2.5:7b"
            target_lang = "English"
            fallback_models = ["qwen2.5:3b"]
            glossary = ["Nexus", "API=接口"]
            [ocr]
            engine = "api"
            langs = "eng"
            upscale = 2.5
            api_model = "llava"
            api_timeout_s = 90
            "#,
            );
            let prefs = Preferences {
                save: false,
                copy: true,
            };

            let form = FormWidgets::build();
            form.populate(&cfg, &prefs);
            let values = form.values();

            // Named one by one: a field that is read but never populated fails here.
            assert_eq!(values.glossary, "Nexus, API=接口");
            assert_eq!(values.fallback_models, "qwen2.5:3b");
            assert_eq!(values.target_lang, "English");
            assert_eq!(values.model, "qwen2.5:7b");
            assert_eq!(values.langs, "eng");
            assert_eq!(values.upscale, 2.5);
            assert_eq!(values.engine, OCR_ENGINE_API);
            assert_eq!(values.api_model, "llava");

            // The path the user actually takes: open the panel, press save.
            assert_eq!(values.to_config(&cfg), cfg);
        });
    }
    #[test]
    fn a_filled_form_round_trips_through_the_config() {
        let cfg = Config::from_toml_str(
            r#"
            [api]
            base_url = "http://localhost:11434/v1"
            api_key = "sk-panel"
            api_key_env = "MY_KEY"
            timeout_s = 12
            [llm]
            model = "qwen2.5:7b"
            target_lang = "English"
            fallback_models = ["qwen2.5:3b", "qwen2.5:1b"]
            [ocr]
            engine = "api"
            langs = "eng"
            preprocess = false
            upscale = 2.5
            api_model = "llava"
            api_timeout_s = 90
            "#,
        );
        let prefs = Preferences {
            save: false,
            copy: true,
        };
        let form = FormValues::from_config(&cfg, &prefs);
        assert_eq!(form.to_config(&Config::default()), cfg);
        assert_eq!(form.to_preferences(), prefs);
    }

    #[test]
    fn saving_preserves_the_sections_the_panel_does_not_own() {
        let base = Config::from_toml_str(
            r#"
            [longshot]
            poll_ms = 5
            min_shift_px = 6
            max_diff = 7.5
            "#,
        );
        let form = values();
        let saved = form.to_config(&base);
        assert_eq!(saved.longshot, base.longshot);
        // The base document itself is a template, not an output.
        assert_eq!(base.llm.model, DEFAULT_LLM_MODEL);
        assert!(!saved.llm.model.is_empty());
    }

    #[test]
    fn numbers_the_loader_would_drop_are_clamped_first() {
        assert_eq!(valid_timeout(0), 1);
        assert_eq!(valid_timeout(60), 60);
        assert_eq!(valid_upscale(0.25), 1.0);
        assert_eq!(valid_upscale(3.0), 3.0);
        assert_eq!(valid_upscale(f32::NAN), 1.0);

        let mut form = values();
        form.timeout_s = 0;
        form.api_timeout_s = 0;
        form.upscale = 0.4;
        let cfg = form.to_config(&Config::default());
        assert_eq!(cfg.api.timeout_s, 1);
        assert_eq!(cfg.ocr.api_timeout_s, 1);
        assert_eq!(cfg.ocr.upscale, 1.0);
        // The point of the clamp: a written-then-loaded config keeps the value.
        assert_eq!(Config::from_toml_str(&cfg.to_toml_string()), cfg);
    }

    #[test]
    fn an_unknown_engine_is_written_as_the_builtin_one() {
        let mut form = values();
        form.engine = "tesseract-ng".into();
        assert_eq!(
            form.to_config(&Config::default()).ocr.engine,
            OCR_ENGINE_BUILTIN
        );
        form.engine = OCR_ENGINE_API.into();
        assert_eq!(
            form.to_config(&Config::default()).ocr.engine,
            OCR_ENGINE_API
        );
    }

    #[test]
    fn the_chip_reports_missing_and_ready_credentials() {
        assert_eq!(credentials(None, false), Credentials::Missing);
        assert_eq!(credentials(Some("配置文件"), false), Credentials::Ready);
        // A local runtime answers without a key, so it is not "missing".
        assert_eq!(credentials(None, true), Credentials::Ready);

        assert_eq!(
            chip_style(Credentials::Missing),
            ("缺少密钥", "vellum-error")
        );
        assert_eq!(
            chip_style(Credentials::Ready),
            ("API 已就绪", "vellum-success")
        );
    }

    #[test]
    fn the_key_hint_names_the_environment_variable_it_will_read() {
        assert!(key_hint(Some("环境变量 VELLUM_API_KEY"), false, "X").contains("VELLUM_API_KEY"));
        assert!(key_hint(Some("配置文件"), false, "X").contains("配置文件"));
        assert!(key_hint(None, true, "X").contains("本地接口"));
        assert!(key_hint(None, false, "MY_KEY").contains("MY_KEY"));
    }

    #[test]
    fn a_probe_result_becomes_one_readable_line() {
        let (message, error) = probe_message(&Ok(vec!["a".into(), "b".into()]), "a");
        assert!(message.contains('2'), "{message}");
        assert!(!error);

        // A provider without /models is reachable, not broken.
        let (message, error) = probe_message(&Ok(Vec::new()), "a");
        assert!(!error, "{message}");
        assert!(message.contains("连接成功"), "{message}");

        let (message, error) = probe_message(&Err("401 unauthorized".into()), "a");
        assert!(error);
        assert!(message.contains("401 unauthorized"), "{message}");
    }

    /// Reachable plus wrong model is its own failure: a Gemini
    /// OpenAI-compatible endpoint answers /models and then 404s gpt-4o-mini.
    #[test]
    fn a_model_missing_from_the_list_is_reported() {
        let models = Ok(vec![
            "models/gemini-2.5-flash".to_string(),
            "models/gemini-flash-latest".to_string(),
        ]);
        // Neutral, not an error: some gateways omit models they still serve.
        let (message, error) = probe_message(&models, "gpt-4o-mini");
        assert!(!error, "{message}");
        assert!(message.contains("gpt-4o-mini"), "{message}");
        assert!(message.contains("gemini-2.5-flash"), "{message}");

        let (message, error) = probe_message(&models, "models/gemini-2.5-flash");
        assert!(!error, "{message}");

        // An empty field is "not configured yet", not a mismatch.
        let (message, error) = probe_message(&models, "   ");
        assert!(!error, "{message}");
    }

    #[test]
    fn progress_is_neither_success_nor_failure() {
        assert_eq!(Flash::Info.class(), None);
        assert_eq!(Flash::Success.class(), Some("vellum-success"));
        assert_eq!(Flash::Error.class(), Some("vellum-error"));
    }
}

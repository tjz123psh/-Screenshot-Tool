//! Independent, bounded-surface image viewer. The capture process may exit while
//! this window stays open; ordinary scrolling never changes the image pixels.
use crate::{
    document::{Document, SharedDocument},
    imaging, theme,
};
use gtk4::gdk::{Key, ModifierType};
use gtk4::prelude::*;
use gtk4::{
    Adjustment, Align, Application, ApplicationWindow, Box as GtkBox, Button, DrawingArea,
    EventControllerKey, EventControllerScroll, EventControllerScrollFlags, GestureClick,
    GestureDrag, Grid, Image, Label, Orientation, Scrollbar, gio, glib,
};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use vellum_core::Rgb8;
use vellum_core::image_limits::{EDIT_LIMITS, VIEWER_LIMITS};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ExportState {
    #[default]
    NotRequested,
    Done,
    Failed,
    Uncertain,
}
impl ExportState {
    pub(crate) fn argument(self) -> &'static str {
        match self {
            Self::NotRequested => "off",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
    fn parse(value: Option<String>) -> Self {
        match value.as_deref() {
            Some("done") => Self::Done,
            Some("failed") => Self::Failed,
            Some("uncertain") => Self::Uncertain,
            _ => Self::NotRequested,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct OutputReport {
    pub save: ExportState,
    pub copy: ExportState,
}
impl OutputReport {
    pub(crate) fn from_args(args: &[String]) -> Self {
        Self {
            save: ExportState::parse(crate::flag_value(args, "--save-status")),
            copy: ExportState::parse(crate::flag_value(args, "--copy-status")),
        }
    }
    fn save_text(self) -> &'static str {
        match self.save {
            ExportState::NotRequested => "保存：未自动保存",
            ExportState::Done => "保存：已完成",
            ExportState::Failed => "保存：失败，可另存为重试",
            ExportState::Uncertain => "保存：已写入，持久化未确认",
        }
    }
    fn copy_text(self) -> &'static str {
        match self.copy {
            ExportState::NotRequested => "复制：未自动复制",
            ExportState::Done => "复制：已完成",
            ExportState::Failed => "复制：失败，可重试",
            ExportState::Uncertain => "复制：未确认，可重试",
        }
    }
}

const PAD: f64 = 24.0;
const MIN_SCALE: f64 = 0.025;
const MAX_SCALE: f64 = 8.0;
const TILE_ROWS: usize = 256;
const CACHE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
struct Viewport {
    width: usize,
    height: usize,
    vw: f64,
    vh: f64,
    scale: f64,
    x: f64,
    y: f64,
    fit: bool,
}
impl Viewport {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            vw: 1.0,
            vh: 1.0,
            scale: 1.0,
            x: 0.0,
            y: 0.0,
            fit: true,
        }
    }
    fn limits(&self) -> (f64, f64) {
        (
            (self.width as f64 * self.scale + 2.0 * PAD - self.vw).max(0.0),
            (self.height as f64 * self.scale + 2.0 * PAD - self.vh).max(0.0),
        )
    }
    fn clamp(&mut self) {
        let (x, y) = self.limits();
        self.x = self.x.clamp(0.0, x);
        self.y = self.y.clamp(0.0, y);
    }
    fn origin(&self) -> (f64, f64) {
        (
            ((self.vw - self.width as f64 * self.scale) / 2.0).max(PAD) - self.x,
            PAD - self.y,
        )
    }
    fn resize(&mut self, w: f64, h: f64) {
        let old = self.scale;
        self.vw = w.max(1.0);
        self.vh = h.max(1.0);
        if self.fit {
            self.scale = ((self.vw - 2.0 * PAD) / self.width.max(1) as f64).clamp(MIN_SCALE, 1.0);
            self.y *= self.scale / old;
        }
        self.clamp();
    }
    fn fit_width(&mut self) {
        self.fit = true;
        self.resize(self.vw, self.vh);
        self.x = 0.0;
    }
    fn zoom(&mut self, scale: f64, pointer: (f64, f64)) {
        let origin = self.origin();
        let image_point = (
            (pointer.0 - origin.0) / self.scale,
            (pointer.1 - origin.1) / self.scale,
        );
        self.scale = scale.clamp(MIN_SCALE, MAX_SCALE);
        self.fit = false;
        let base_x = ((self.vw - self.width as f64 * self.scale) / 2.0).max(PAD);
        self.x = base_x + image_point.0 * self.scale - pointer.0;
        self.y = PAD + image_point.1 * self.scale - pointer.1;
        self.clamp();
    }
    fn scroll(&mut self, x: f64, y: f64) {
        self.x += x;
        self.y += y;
        self.clamp();
    }
    fn visible_rows(&self) -> (usize, usize) {
        let top = ((self.y - PAD).max(0.0) / self.scale).floor() as usize;
        let bottom = ((self.y + self.vh - PAD).max(0.0) / self.scale).ceil() as usize;
        (top.min(self.height), bottom.min(self.height))
    }
}

struct Tile {
    row: usize,
    surface: cairo::ImageSurface,
    bytes: usize,
}
#[derive(Default)]
struct TileCache {
    tiles: VecDeque<Tile>,
    bytes: usize,
}
impl TileCache {
    fn get(&mut self, image: &Rgb8, row: usize) -> Option<cairo::ImageSurface> {
        if let Some(index) = self.tiles.iter().position(|tile| tile.row == row) {
            let tile = self.tiles.remove(index)?;
            let surface = tile.surface.clone();
            self.tiles.push_back(tile);
            return Some(surface);
        }
        let end = (row + TILE_ROWS).min(image.height);
        if row >= end {
            return None;
        }
        let surface = imaging::to_surface(&image.rows_slice(row, end)).ok()?;
        let bytes = surface.stride() as usize * (end - row);
        while self.bytes + bytes > CACHE_BYTES {
            let Some(old) = self.tiles.pop_front() else {
                break;
            };
            self.bytes -= old.bytes;
        }
        // A tile exceeding the budget may be drawn, but is never retained.
        if bytes <= CACHE_BYTES {
            self.bytes += bytes;
            self.tiles.push_back(Tile {
                row,
                surface: surface.clone(),
                bytes,
            });
        }
        Some(surface)
    }
}

fn overview(image: &Rgb8) -> Rgb8 {
    let w = image.width.clamp(1, 120);
    let h = (image.height.saturating_mul(w) / image.width.max(1)).clamp(1, 1000);
    let mut thumb = Rgb8::new(w, h);
    for y in 0..h {
        let row = image.row(y * image.height / h);
        for x in 0..w {
            let sx = x * image.width / w;
            thumb.row_mut(y)[x * 3..x * 3 + 3].copy_from_slice(&row[sx * 3..sx * 3 + 3]);
        }
    }
    thumb
}

struct Viewer {
    window: ApplicationWindow,
    area: DrawingArea,
    map: DrawingArea,
    image: RefCell<Arc<Rgb8>>,
    document: Option<SharedDocument>,
    revision: Cell<u64>,
    dimensions: Label,
    thumb: RefCell<Option<cairo::ImageSurface>>,
    view: RefCell<Viewport>,
    tiles: RefCell<TileCache>,
    horizontal: Adjustment,
    vertical: Adjustment,
    hbar: Scrollbar,
    vbar: Scrollbar,
    syncing: Cell<bool>,
    zoom_label: Label,
    position: Label,
    status: Label,
    save_status: Label,
    copy_status: Label,
    outputs: Cell<OutputReport>,
    memory_only: bool,
    overview_only: Cell<bool>,
    allow_close: Cell<bool>,
    close_question: Cell<bool>,
    busy: Cell<bool>,
    actions: Vec<Button>,
}
thread_local! { static LIVE: RefCell<Vec<Rc<Viewer>>> = const { RefCell::new(Vec::new()) }; }
fn raster_document(image: &Rgb8) -> Option<SharedDocument> {
    EDIT_LIMITS.check(image.width, image.height, 4).ok()?;
    Document::from_raster(image.clone())
        .ok()
        .map(|doc| Rc::new(RefCell::new(doc)))
}
pub fn run(image: Rgb8, incomplete: bool, outputs: OutputReport) -> i32 {
    run_with_document(image, None, incomplete, outputs)
}
pub(crate) fn run_with_document(
    image: Rgb8,
    document: Option<SharedDocument>,
    incomplete: bool,
    outputs: OutputReport,
) -> i32 {
    let document = document.or_else(|| raster_document(&image));
    // A caller-owned PNG or explicitly reopened recovery PNG remains on disk.
    // Private handoff files, by contrast, may be removed after READY.
    if !crate::handoff::has_pending_receiver()
        && let Some(document) = &document
    {
        let mut document = document.borrow_mut();
        let revision = document.revision();
        document.mark_saved(revision);
    }
    run_session(Arc::new(image), document, incomplete, outputs, false)
}
/// Open another view of the same committed image revision, never another main loop.
pub(crate) fn open_document(app: &Application, document: SharedDocument) -> Result<(), String> {
    let id = document.borrow().id();
    let existing = LIVE.with(|live| {
        live.borrow()
            .iter()
            .find(|viewer| {
                viewer
                    .document
                    .as_ref()
                    .is_some_and(|doc| doc.borrow().id() == id)
            })
            .cloned()
    });
    if let Some(viewer) = existing {
        if !viewer.refresh_document() {
            return Err("当前成品渲染失败；请保留现有窗口后重试".into());
        }
        viewer.window.present();
        return Ok(());
    }
    let snapshot = document
        .borrow()
        .snapshot()
        .map_err(|_| "当前成品渲染失败；请保留现有窗口后重试".to_string())?;
    match Viewer::new(
        app,
        snapshot.image,
        Some(document),
        false,
        OutputReport::default(),
        false,
    ) {
        Ok(viewer) => {
            present(viewer, false);
            Ok(())
        }
        Err(_) => Err("无法建立图片查看窗口；请保留现有窗口后重试".into()),
    }
}
fn present(viewer: Rc<Viewer>, acknowledge: bool) {
    LIVE.with(|live| live.borrow_mut().push(viewer.clone()));
    if acknowledge {
        crate::handoff::connect_ready(&viewer.window);
    }
    viewer.window.present();
    theme::snapshot_for_review(&viewer.window);
}
/// Exceptional fallback: hold RGB in this process, not on disk.
pub(crate) fn run_memory_recovery(image: Rgb8, incomplete: bool, outputs: OutputReport) -> i32 {
    run_memory_document(image, None, incomplete, outputs)
}
pub(crate) fn run_memory_document(
    image: Rgb8,
    document: Option<SharedDocument>,
    incomplete: bool,
    outputs: OutputReport,
) -> i32 {
    crate::handoff::disarm();
    if let Some(app) = gio::Application::default().and_downcast::<Application>() {
        for window in app.windows() {
            window.set_visible(false);
        }
    }
    let document = document.or_else(|| raster_document(&image));
    run_session(Arc::new(image), document, incomplete, outputs, true)
}
fn run_session(
    image: Arc<Rgb8>,
    document: Option<SharedDocument>,
    incomplete: bool,
    outputs: OutputReport,
    memory_only: bool,
) -> i32 {
    if image.width == 0
        || image.height == 0
        || (!memory_only && VIEWER_LIMITS.check(image.width, image.height, 4).is_err())
    {
        vellum_core::io::notify(
            "Vellum 无法预览",
            "图片尺寸超出安全预览范围；交接图片仍保留，可运行 vellum recover list 查找",
            "normal",
        );
        crate::handoff::reject_current("dimensions");
        return 1;
    }
    let app = Application::builder()
        .application_id("ai.vellum.preview")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let image = RefCell::new(Some(image));
    let failed = Rc::new(Cell::new(false));
    let failed_activate = failed.clone();
    app.connect_activate(move |app| {
        let Some(image) = image.borrow_mut().take() else {
            return;
        };
        match Viewer::new(
            app,
            image,
            document.clone(),
            incomplete,
            outputs,
            memory_only,
        ) {
            Ok(viewer) => present(viewer, true),
            Err(_) => {
                crate::handoff::reject_current("window");
                failed_activate.set(true);
                app.quit();
            }
        }
    });
    let args: [String; 0] = [];
    let code = i32::from(app.run_with_args(&args).get());
    if failed.get() { 1 } else { code }
}

fn button(icon: &str, text: &str, tooltip: &str) -> Button {
    let b = Button::new();
    let row = GtkBox::new(Orientation::Horizontal, 7);
    let image = Image::from_icon_name(icon);
    image.set_pixel_size(16);
    row.append(&image);
    if !text.is_empty() {
        row.append(&Label::new(Some(text)));
    }
    b.set_child(Some(&row));
    b.update_property(&[gtk4::accessible::Property::Label(tooltip)]);
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("vellum-icon-button");
    b
}

impl Viewer {
    fn new(
        app: &Application,
        image: Arc<Rgb8>,
        document: Option<SharedDocument>,
        incomplete: bool,
        outputs: OutputReport,
        memory_only: bool,
    ) -> anyhow::Result<Rc<Self>> {
        let outputs = if let Some(document) = &document {
            let mut document = document.borrow_mut();
            let revision = document.revision();
            if outputs.save == ExportState::Done {
                document.mark_saved(revision);
            }
            if outputs.copy == ExportState::Done {
                document.mark_copied(revision);
            }
            shared_outputs(&document, outputs)
        } else {
            outputs
        };
        theme::install_default();
        let (mw, mh) = gtk4::gdk::Display::default()
            .and_then(|d| d.monitors().item(0))
            .and_then(|m| m.downcast::<gtk4::gdk::Monitor>().ok())
            .map(|m| (m.geometry().width(), m.geometry().height()))
            .unwrap_or((1440, 900));
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Vellum · 图片预览")
            .default_width((mw - 100).clamp(420, 980))
            .default_height((mh - 100).clamp(320, 740))
            .resizable(false)
            .build();
        window.add_css_class("vellum-window");
        window.add_css_class("vellum-preview");
        let root = GtkBox::new(Orientation::Vertical, 0);
        let header = GtkBox::new(Orientation::Horizontal, 12);
        header.add_css_class("vellum-preview-header");
        let brand = Label::new(Some("Vellum"));
        brand.add_css_class("vellum-title");
        header.append(&crate::controls::brand_mark(28));
        header.append(&brand);
        let dimensions = Label::new(Some(&format!(
            "图片预览  ·  {} × {}",
            image.width, image.height
        )));
        dimensions.set_ellipsize(pango::EllipsizeMode::End);
        dimensions.add_css_class("vellum-dim");
        dimensions.set_hexpand(true);
        dimensions.set_xalign(0.0);
        header.append(&dimensions);
        let close = button("window-close-symbolic", "", "关闭预览（Esc）");
        close.add_css_class("vellum-quiet");
        header.append(&close);
        root.append(&crate::drag::draggable(&header));
        let bar = GtkBox::new(Orientation::Horizontal, 8);
        bar.add_css_class("vellum-preview-toolbar");
        let minus = button("zoom-out-symbolic", "", "缩小（−）");
        let plus = button("zoom-in-symbolic", "", "放大（+ / Ctrl+滚轮）");
        let zoom_label = Label::new(Some("100%"));
        zoom_label.set_width_chars(5);
        let fit = button(
            "zoom-fit-best-symbolic",
            "适应宽度",
            "按窗口宽度显示（Ctrl+0）",
        );
        let original = button("zoom-original-symbolic", "100%", "原始像素大小（1）");
        for b in [&minus, &plus] {
            bar.append(b);
        }
        bar.append(&zoom_label);
        bar.append(&fit);
        bar.append(&original);
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        bar.append(&spacer);
        let copy = button("edit-copy-symbolic", "复制", "复制完整图片（Ctrl+C）");
        let save = button(
            "document-save-as-symbolic",
            "另存为",
            "将完整图片另存为 PNG（Ctrl+S）",
        );
        save.add_css_class("suggested-action");
        let pin = button("view-pin-symbolic", "钉图", "将完整图片作为浮动参考图");
        if mw < 960 {
            for b in [&fit, &original, &pin, &copy, &save] {
                if let Some(child) = b
                    .child()
                    .and_then(|c| c.last_child())
                    .and_then(|c| c.downcast::<Label>().ok())
                {
                    child.set_visible(false);
                }
            }
        }
        bar.append(&pin);
        bar.append(&copy);
        bar.append(&save);
        root.append(&bar);
        let session_bar = GtkBox::new(Orientation::Horizontal, 8);
        session_bar.add_css_class("vellum-preview-toolbar");
        let edit = button(
            "document-edit-symbolic",
            "继续编辑",
            "编辑本次会话的标注对象",
        );
        let crop = button(
            "crop-symbolic",
            "裁切",
            "在编辑器裁切；超大图可先裁出可编辑区域",
        );
        let ocr = button("edit-find-symbolic", "识别文字", "识别当前版本的遮挡后成品");
        let translate = button(
            "accessories-dictionary-symbolic",
            "翻译",
            "识别并翻译当前版本的遮挡后成品",
        );
        for button in [&edit, &crop, &ocr, &translate] {
            session_bar.append(button);
        }
        if document.is_none() {
            edit.set_tooltip_text(Some("图片超出编辑预算；请先裁切一部分"));
            ocr.set_tooltip_text(Some("请先裁切到编辑预算内再识别"));
        }
        root.append(&session_bar);
        let exports = GtkBox::new(Orientation::Horizontal, 16);
        exports.set_margin_start(18);
        exports.set_margin_end(18);
        exports.set_margin_top(6);
        exports.set_margin_bottom(6);
        let save_status = Label::new(Some(outputs.save_text()));
        let copy_status = Label::new(Some(outputs.copy_text()));
        save_status.set_wrap(true);
        copy_status.set_wrap(true);
        save_status.set_xalign(0.0);
        copy_status.set_xalign(0.0);
        exports.append(&save_status);
        exports.append(&copy_status);
        root.append(&exports);
        if memory_only {
            window.set_title(Some("Vellum · 未保存的恢复图片"));
            let warning = Label::new(Some(
                "无法写入恢复副本；图片当前仅保留在此窗口。请保持窗口打开并另存为。此时截图服务会保持忙碌。",
            ));
            warning.add_css_class("vellum-warning");
            warning.set_wrap(true);
            warning.set_xalign(0.0);
            warning.set_margin_start(18);
            warning.set_margin_end(18);
            root.append(&warning);
        }
        if incomplete {
            let warning = Label::new(Some(vellum_stitch::INCOMPLETE_WARNING));
            warning.add_css_class("vellum-warning");
            warning.set_wrap(true);
            warning.set_xalign(0.0);
            warning.set_margin_top(10);
            warning.set_margin_bottom(10);
            warning.set_margin_start(18);
            warning.set_margin_end(18);
            root.append(&warning);
        }
        let horizontal = Adjustment::new(0.0, 0.0, 1.0, 44.0, 300.0, 1.0);
        let vertical = Adjustment::new(0.0, 0.0, 1.0, 44.0, 300.0, 1.0);
        let area = DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_focusable(true);
        area.add_css_class("vellum-preview-stage");
        let map = DrawingArea::new();
        map.set_content_width(94);
        map.set_vexpand(true);
        map.add_css_class("vellum-preview-minimap");
        map.set_tooltip_text(Some("全文概览：点击或拖动快速定位"));
        map.set_visible(mw >= 960);
        let grid = Grid::new();
        grid.set_hexpand(true);
        grid.set_vexpand(true);
        let hbar = Scrollbar::new(Orientation::Horizontal, Some(&horizontal));
        let vbar = Scrollbar::new(Orientation::Vertical, Some(&vertical));
        grid.attach(&area, 0, 0, 1, 1);
        grid.attach(&vbar, 1, 0, 1, 1);
        grid.attach(&map, 2, 0, 1, 1);
        grid.attach(&hbar, 0, 1, 1, 1);
        root.append(&grid);
        let footer = GtkBox::new(Orientation::Horizontal, 12);
        footer.add_css_class("vellum-preview-status");
        let status = Label::new(Some("滚轮浏览  ·  Ctrl+滚轮缩放"));
        status.set_hexpand(true);
        status.set_xalign(0.0);
        status.set_ellipsize(pango::EllipsizeMode::Middle);
        let position = Label::new(Some("顶部"));
        position.set_halign(Align::End);
        footer.append(&status);
        footer.append(&position);
        root.append(&footer);
        window.set_child(Some(&root));
        // A Cairo allocation failure must not discard the only RGB buffer.
        // Save/copy controls remain usable even if no thumbnail can be drawn.
        let thumb = imaging::to_surface(&overview(&image)).ok();
        if thumb.is_none() {
            status.set_text("缩略预览不可用，原图仍保留，可另存为或复制");
        }
        let view = RefCell::new(Viewport::new(image.width, image.height));
        let overview_only =
            image.width > 16384 || image.width.saturating_mul(image.height) > 180_000_000;
        let revision = document.as_ref().map_or(0, |doc| doc.borrow().revision());
        let this = Rc::new(Self {
            window,
            area,
            map,
            image: RefCell::new(image),
            document,
            revision: Cell::new(revision),
            dimensions,
            thumb: RefCell::new(thumb),
            view,
            tiles: RefCell::new(TileCache::default()),
            horizontal,
            vertical,
            hbar,
            vbar,
            syncing: Cell::new(false),
            zoom_label,
            position,
            status,
            save_status,
            copy_status,
            outputs: Cell::new(outputs),
            memory_only,
            overview_only: Cell::new(overview_only),
            allow_close: Cell::new(false),
            close_question: Cell::new(false),
            busy: Cell::new(false),
            actions: vec![
                copy.clone(),
                save.clone(),
                pin.clone(),
                edit.clone(),
                crop.clone(),
                ocr.clone(),
                translate.clone(),
            ],
        });
        for (button, action) in [(&edit, 0), (&crop, 1), (&ocr, 2), (&translate, 3)] {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(viewer) = weak.upgrade() {
                    viewer.document_action(action);
                }
            });
        }
        let weak = Rc::downgrade(&this);
        close.connect_clicked(move |_| {
            if let Some(v) = weak.upgrade() {
                v.window.close();
            }
        });
        let weak = Rc::downgrade(&this);
        this.window.connect_close_request(move |window| {
            if let Some(v) = weak.upgrade()
                && v.busy.get()
            {
                v.status.set_text("正在导出完整图片，请稍候…");
                return glib::Propagation::Stop;
            }
            if let Some(v) = weak.upgrade()
                && !v.allow_close.get()
                && (v.document.as_ref().is_some_and(|doc| doc.borrow().needs_output_confirmation())
                    || (v.memory_only && v.outputs.get().save != ExportState::Done))
            {
                if !v.close_question.replace(true) {
                    let dialog = gtk4::AlertDialog::builder()
                        .message("尚未确认图片已保存")
                        .detail("当前版本尚未保存或复制。关闭最后一个持图窗口会丢失未输出的修改；建议继续编辑并保存。")
                        .buttons(["继续编辑", "仍然关闭"])
                        .cancel_button(0)
                        .default_button(0)
                        .build();
                    let asked_revision = v.document.as_ref().map_or(v.revision.get(), |doc| doc.borrow().revision());
                    let weak = Rc::downgrade(&v);
                    dialog.choose(Some(&v.window), None::<&gio::Cancellable>, move |answer| {
                        if let Some(v) = weak.upgrade() {
                            v.close_question.set(false);
                            if answer == Ok(1) {
                                let current = v.document.as_ref().map_or(v.revision.get(), |doc| doc.borrow().revision());
                                if export_matches_revision(current, asked_revision) {
                                    v.allow_close.set(true);
                                    v.window.close();
                                } else {
                                    v.status.set_text("确认期间图片已修改，请检查当前版本后重新关闭");
                                }
                            }
                        }
                    });
                }
                return glib::Propagation::Stop;
            }
            LIVE.with(|live| live.borrow_mut().retain(|viewer| viewer.window != *window));
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(&this);
        this.window.connect_map(move |_| {
            let weak = weak.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                if let Some(v) = weak.upgrade() {
                    crate::own_window::float_own_window_soon();
                    v.area.grab_focus();
                }
            });
        });
        let weak = Rc::downgrade(&this);
        this.area.set_draw_func(move |_, cr, w, h| {
            if let Some(v) = weak.upgrade() {
                v.draw(cr, w, h);
            }
        });
        let weak = Rc::downgrade(&this);
        this.map.set_draw_func(move |_, cr, w, h| {
            if let Some(v) = weak.upgrade() {
                v.draw_map(cr, w, h);
            }
        });
        let weak = Rc::downgrade(&this);
        this.area.connect_resize(move |_, w, h| {
            if let Some(v) = weak.upgrade() {
                v.view.borrow_mut().resize(f64::from(w), f64::from(h));
                v.sync();
            }
        });
        for (adjustment, is_vertical) in [(&this.horizontal, false), (&this.vertical, true)] {
            let weak = Rc::downgrade(&this);
            adjustment.connect_value_changed(move |a| {
                if let Some(v) = weak.upgrade() {
                    if v.syncing.get() {
                        return;
                    }
                    {
                        let mut view = v.view.borrow_mut();
                        if is_vertical {
                            view.y = a.value();
                        } else {
                            view.x = a.value();
                        }
                        view.clamp();
                    }
                    v.sync();
                }
            });
        }
        let scroll = EventControllerScroll::new(EventControllerScrollFlags::BOTH_AXES);
        let weak = Rc::downgrade(&this);
        scroll.connect_scroll(move |controller, dx, dy| {
            if let Some(v) = weak.upgrade() {
                let modifiers = controller.current_event_state();
                if modifiers.contains(ModifierType::CONTROL_MASK) {
                    v.zoom_by(1.12f64.powf(-dy));
                } else {
                    let step = if controller.unit() == gtk4::gdk::ScrollUnit::Wheel {
                        48.0
                    } else {
                        1.0
                    };
                    let mut view = v.view.borrow_mut();
                    if modifiers.contains(ModifierType::SHIFT_MASK) {
                        view.scroll(dy * step, dx * step);
                    } else {
                        view.scroll(dx * step, dy * step);
                    }
                    drop(view);
                    v.sync();
                }
            }
            glib::Propagation::Stop
        });
        this.area.add_controller(scroll);
        for (b, factor) in [(&minus, 1.0 / 1.2), (&plus, 1.2)] {
            let weak = Rc::downgrade(&this);
            b.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.zoom_by(factor);
                }
            });
        }
        let weak = Rc::downgrade(&this);
        fit.connect_clicked(move |_| {
            if let Some(v) = weak.upgrade() {
                v.view.borrow_mut().fit_width();
                v.sync();
            }
        });
        let weak = Rc::downgrade(&this);
        original.connect_clicked(move |_| {
            if let Some(v) = weak.upgrade() {
                v.set_zoom(1.0);
            }
        });
        let weak = Rc::downgrade(&this);
        copy.connect_clicked(move |_| {
            if let Some(v) = weak.upgrade() {
                v.export(None, false);
            }
        });
        let weak = Rc::downgrade(&this);
        pin.connect_clicked(move |_| {
            if let Some(v) = weak.upgrade() {
                v.export(None, true);
            }
        });
        let weak = Rc::downgrade(&this);
        save.connect_clicked(move |_| {
            if let Some(v) = weak.upgrade() {
                v.save_as();
            }
        });
        let click = GestureClick::new();
        let weak = Rc::downgrade(&this);
        click.connect_pressed(move |_, _, _, y| {
            if let Some(v) = weak.upgrade() {
                v.navigate(y);
            }
        });
        this.map.add_controller(click);
        let drag = GestureDrag::new();
        let start = Rc::new(Cell::new(0.0));
        let begin = start.clone();
        drag.connect_drag_begin(move |_, _, y| begin.set(y));
        let weak = Rc::downgrade(&this);
        drag.connect_drag_update(move |_, _, dy| {
            if let Some(v) = weak.upgrade() {
                v.navigate(start.get() + dy);
            }
        });
        this.map.add_controller(drag);
        let keys = EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(v) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let ctrl = mods.contains(ModifierType::CONTROL_MASK);
            match key {
                Key::F12 if std::env::var("VELLUM_UI_DEMO").as_deref() == Ok("1") => {
                    theme::snapshot_for_review(&v.window)
                }
                Key::Escape => v.window.close(),
                Key::plus | Key::equal | Key::KP_Add => v.zoom_by(1.2),
                Key::minus | Key::KP_Subtract => v.zoom_by(1.0 / 1.2),
                Key::_1 => v.set_zoom(1.0),
                Key::_0 if ctrl => {
                    v.view.borrow_mut().fit_width();
                    v.sync();
                }
                Key::c | Key::C if ctrl => v.export(None, false),
                Key::s | Key::S if ctrl => v.save_as(),
                Key::Home => {
                    v.view.borrow_mut().y = 0.0;
                    v.sync();
                }
                Key::End => {
                    let end = v.view.borrow().limits().1;
                    v.view.borrow_mut().y = end;
                    v.sync();
                }
                Key::Page_Down | Key::Page_Up | Key::Down | Key::Up => {
                    let amount = if key == Key::Page_Down || key == Key::Page_Up {
                        v.view.borrow().vh * 0.85
                    } else {
                        48.0
                    };
                    let sign = if key == Key::Page_Up || key == Key::Up {
                        -1.0
                    } else {
                        1.0
                    };
                    v.view.borrow_mut().scroll(0.0, sign * amount);
                    v.sync();
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        this.window.add_controller(keys);
        let weak = Rc::downgrade(&this);
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            let Some(viewer) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            viewer.refresh_document();
            glib::ControlFlow::Continue
        });
        Ok(this)
    }
    /// Refresh only committed revisions. Every consumer receives this composed
    /// snapshot; the private source image is never reachable from the viewer.
    fn refresh_document(&self) -> bool {
        let Some(document) = &self.document else {
            return true;
        };
        let document = document.borrow();
        if document.revision() == self.revision.get() {
            let outputs = shared_outputs(&document, self.outputs.get());
            self.outputs.set(outputs);
            self.save_status.set_text(outputs.save_text());
            self.copy_status.set_text(outputs.copy_text());
            return true;
        }
        let snapshot = match document.snapshot() {
            Ok(snapshot) => snapshot,
            Err(_) => {
                self.status
                    .set_text("当前图片渲染失败；未导出，请保留编辑会话后重试");
                return false;
            }
        };
        let outputs = shared_outputs(&document, OutputReport::default());
        drop(document);
        self.revision.set(snapshot.revision);
        let image = snapshot.image;
        self.dimensions.set_text(&format!(
            "图片版本 {} · {} × {}",
            snapshot.revision, image.width, image.height
        ));
        self.overview_only
            .set(VIEWER_LIMITS.check(image.width, image.height, 4).is_err());
        self.thumb
            .replace(imaging::to_surface(&overview(&image)).ok());
        self.view.replace(Viewport::new(image.width, image.height));
        self.view
            .borrow_mut()
            .resize(f64::from(self.area.width()), f64::from(self.area.height()));
        self.tiles.replace(TileCache::default());
        self.image.replace(image);
        self.outputs.set(outputs);
        self.save_status.set_text(outputs.save_text());
        self.copy_status.set_text(outputs.copy_text());
        self.status
            .set_text("图片已修改，请重新保存、复制或识别当前版本");
        self.sync();
        true
    }
    fn document_action(self: &Rc<Self>, action: u8) {
        if self.busy.get() || !self.refresh_document() {
            return;
        }
        let Some(app) = self.window.application() else {
            return;
        };
        if let Some(document) = &self.document {
            if action <= 1 {
                if crate::editor::open(&app, document.clone()).is_err() {
                    self.status.set_text("无法打开编辑器，请保留当前图片后重试");
                }
            } else {
                crate::result::open_document(&app, document.clone(), action == 3);
            }
        } else if action <= 1 {
            self.crop_large_image();
        } else {
            self.status
                .set_text("图片超出编辑与识别预算，请先裁切所需区域");
        }
    }
    fn crop_large_image(self: &Rc<Self>) {
        let Some(app) = self.window.application() else {
            return;
        };
        let image = self.image.borrow().clone();
        let dialog = ApplicationWindow::builder()
            .application(&app)
            .title("裁切到可编辑范围")
            .transient_for(&self.window)
            .modal(true)
            .default_width(390)
            .build();
        let root = GtkBox::new(Orientation::Vertical, 10);
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        root.set_margin_start(16);
        root.set_margin_end(16);
        let hint = Label::new(Some(
            "输入成品图片上的像素范围。原图保持不变；裁切结果作为新的可编辑图片。",
        ));
        hint.set_wrap(true);
        root.append(&hint);
        let x = gtk4::SpinButton::with_range(0.0, (image.width - 1) as f64, 1.0);
        let y = gtk4::SpinButton::with_range(0.0, (image.height - 1) as f64, 1.0);
        let width = gtk4::SpinButton::with_range(1.0, image.width as f64, 1.0);
        let height = gtk4::SpinButton::with_range(1.0, image.height as f64, 1.0);
        let start_y = self.view.borrow().visible_rows().0.min(image.height - 1);
        y.set_value(start_y as f64);
        let w = image.width.min(4096);
        width.set_value(w as f64);
        height.set_value(
            (image.height - start_y)
                .min(EDIT_LIMITS.max_pixels / w)
                .min(4096) as f64,
        );
        for (name, spin) in [
            ("左侧 X", &x),
            ("顶部 Y", &y),
            ("宽度", &width),
            ("高度", &height),
        ] {
            let row = GtkBox::new(Orientation::Horizontal, 8);
            let label = Label::new(Some(name));
            label.set_hexpand(true);
            label.set_xalign(0.0);
            row.append(&label);
            row.append(spin);
            root.append(&row);
        }
        let error = Label::new(None);
        error.set_wrap(true);
        root.append(&error);
        let apply = Button::with_label("裁切并编辑");
        root.append(&apply);
        let weak_dialog = dialog.downgrade();
        apply.connect_clicked(move |_| {
            let cropped = bounded_crop(
                &image,
                x.value() as usize,
                y.value() as usize,
                width.value() as usize,
                height.value() as usize,
            );
            let document = cropped.and_then(Document::from_raster);
            match document {
                Ok(document) => {
                    let document = Rc::new(RefCell::new(document));
                    if let Err(message) = open_document(&app, document.clone()) {
                        error.set_text(&message);
                        return;
                    }
                    if crate::editor::open(&app, document).is_err() {
                        error.set_text("编辑器未能打开，裁切结果已在新预览中保留");
                        return;
                    }
                    if let Some(dialog) = weak_dialog.upgrade() {
                        dialog.close();
                    }
                }
                Err(message) => error.set_text(&message),
            }
        });
        dialog.set_child(Some(&root));
        dialog.present();
    }
    fn sync(&self) {
        self.syncing.set(true);
        let view = self.view.borrow();
        let (mx, my) = view.limits();
        self.hbar.set_visible(mx > 0.5);
        self.vbar.set_visible(my > 0.5);
        self.horizontal
            .configure(view.x, 0.0, mx + view.vw, 48.0, view.vw * 0.85, view.vw);
        self.vertical
            .configure(view.y, 0.0, my + view.vh, 48.0, view.vh * 0.85, view.vh);
        self.zoom_label
            .set_text(&format!("{:.0}%", view.scale * 100.0));
        let (top, bottom) = view.visible_rows();
        self.position
            .set_text(&format!("{}–{} / {} px", top, bottom, view.height));
        self.syncing.set(false);
        self.area.queue_draw();
        self.map.queue_draw();
    }
    fn zoom_by(&self, factor: f64) {
        let scale = self.view.borrow().scale * factor;
        self.set_zoom(scale);
    }
    fn set_zoom(&self, scale: f64) {
        let mut view = self.view.borrow_mut();
        let centre = (view.vw / 2.0, view.vh / 2.0);
        view.zoom(scale, centre);
        drop(view);
        self.sync();
    }
    fn navigate(&self, y: f64) {
        let fraction =
            ((y - 12.0) / (f64::from(self.map.height()) - 24.0).max(1.0)).clamp(0.0, 1.0);
        let max = self.view.borrow().limits().1;
        self.view.borrow_mut().y = fraction * max;
        self.sync();
    }
    fn draw(&self, cr: &cairo::Context, w: i32, h: i32) {
        cr.set_source_rgb(0.125, 0.133, 0.118);
        cr.paint().ok();
        if self.overview_only.get() {
            // An oversized original still gets save/copy controls in memory
            // recovery without allocating a full-size Cairo surface or tile.
            let thumb = self.thumb.borrow();
            let Some(thumb) = thumb.as_ref() else {
                return;
            };
            let scale = (f64::from(w) / f64::from(thumb.width()))
                .min(f64::from(h) / f64::from(thumb.height()));
            cr.save().ok();
            cr.scale(scale, scale);
            cr.set_source_surface(thumb, 0.0, 0.0).ok();
            cr.paint().ok();
            cr.restore().ok();
            return;
        }
        let view = self.view.borrow();
        let origin = view.origin();
        let (start, end) = view.visible_rows();
        cr.save().ok();
        cr.rectangle(0.0, 0.0, f64::from(w), f64::from(h));
        cr.clip();
        cr.translate(origin.0, origin.1);
        cr.scale(view.scale, view.scale);
        // Tile boundaries share device pixels: do not antialias their clipping
        // rectangles against the dark stage or fractional zoom would show seams.
        cr.set_antialias(cairo::Antialias::None);
        let image = self.image.borrow();
        let mut tiles = self.tiles.borrow_mut();
        for row in ((start / TILE_ROWS) * TILE_ROWS..end).step_by(TILE_ROWS) {
            if let Some(tile) = tiles.get(&image, row) {
                cr.set_source_surface(&tile, 0.0, row as f64).ok();
                cr.source().set_extend(cairo::Extend::Pad);
                cr.source().set_filter(if view.scale >= 3.0 {
                    cairo::Filter::Nearest
                } else {
                    cairo::Filter::Good
                });
                cr.rectangle(0.0, row as f64, image.width as f64, tile.height() as f64);
                cr.fill().ok();
            }
        }
        cr.restore().ok();
    }
    fn draw_map(&self, cr: &cairo::Context, w: i32, h: i32) {
        let thumb = self.thumb.borrow();
        let Some(thumb) = thumb.as_ref() else {
            return;
        };
        cr.set_source_rgb(0.15, 0.16, 0.135);
        cr.paint().ok();
        let width = (f64::from(w) - 20.0).max(1.0);
        let height = (f64::from(h) - 24.0).max(1.0);
        cr.save().ok();
        cr.translate(10.0, 12.0);
        cr.scale(
            width / f64::from(thumb.width()),
            height / f64::from(thumb.height()),
        );
        cr.set_source_surface(thumb, 0.0, 0.0).ok();
        cr.paint_with_alpha(0.58).ok();
        cr.restore().ok();
        let view = self.view.borrow();
        let (top, bottom) = view.visible_rows();
        let y = 12.0 + height * top as f64 / view.height as f64;
        let band = (height * (bottom - top) as f64 / view.height as f64).max(4.0);
        crate::paint::fill_rounded(
            cr,
            crate::paint::Bounds::new(8.0, y, width + 4.0, band),
            3.0,
            (0.78, 0.70, 0.52, 0.24),
        );
        crate::paint::stroke_rounded(
            cr,
            crate::paint::Bounds::new(8.0, y, width + 4.0, band),
            3.0,
            1.5,
            (0.87, 0.79, 0.63, 1.0),
        );
    }
    fn save_as(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let dialog = gtk4::FileDialog::builder()
            .title("另存为 PNG")
            .initial_name("Vellum.png")
            .build();
        let filter = gtk4::FileFilter::new();
        filter.set_name(Some("PNG 图片"));
        filter.add_mime_type("image/png");
        let filters = gio::ListStore::new::<gtk4::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&filter));
        let weak = Rc::downgrade(self);
        dialog.save(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                if let Some(v) = weak.upgrade() {
                    match result {
                        Ok(file) => {
                            if let Some(path) = file.path() {
                                v.export(Some(path), false);
                            } else {
                                v.status.set_text("请选择本地文件位置");
                            }
                        }
                        Err(error) => {
                            if !error.matches(gtk4::DialogError::Dismissed)
                                && !error.matches(gtk4::DialogError::Cancelled)
                            {
                                v.status.set_text(&format!("无法选择位置：{error}"));
                            }
                        }
                    }
                }
            },
        );
    }
    fn update_output(&self, saving: bool, pin: bool, state: ExportState) {
        if state == ExportState::Done
            && let Some(document) = &self.document
        {
            let mut document = document.borrow_mut();
            if saving {
                document.mark_saved(self.revision.get());
            } else if !pin {
                document.mark_copied(self.revision.get());
            }
        }
        let mut report = self.outputs.get();
        if saving {
            report.save = state;
        } else if !pin {
            report.copy = state;
        }
        self.outputs.set(report);
        self.save_status.set_text(report.save_text());
        self.copy_status.set_text(report.copy_text());
    }
    fn export(self: &Rc<Self>, path: Option<PathBuf>, pin: bool) {
        if !self.refresh_document() {
            return;
        }
        if self.busy.replace(true) {
            return;
        }
        for button in &self.actions {
            button.set_sensitive(false);
        }
        self.status.set_text("正在处理完整图片…");
        let image = self.image.borrow().clone();
        let revision = self.revision.get();
        let saving = path.is_some();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (result, state) = if let Some(path) = path {
                match image.save_png_result(&path) {
                    Ok(()) => (Ok("已保存完整图片".to_string()), ExportState::Done),
                    Err(error) if vellum_core::io::committed_save_path(&error).is_some() => {
                        // Preserve a second durable copy when the commit cannot
                        // promise persistence. Never call this “not saved”.
                        let retained = image
                            .to_png()
                            .ok()
                            .and_then(|png| crate::handoff::Asset::create(&png).ok());
                        let message = if let Some(asset) = retained {
                            format!(
                                "图片已写入，持久化未确认；恢复副本 {} 可通过 vellum recover list 查看",
                                asset.id()
                            )
                        } else {
                            "图片已写入，持久化未确认；额外恢复副本写入失败，请保持窗口打开并另存为"
                                .to_string()
                        };
                        (Ok(message), ExportState::Uncertain)
                    }
                    Err(error) => (
                        Err(crate::region_save_failure_message(&error).to_string()),
                        ExportState::Failed,
                    ),
                }
            } else if pin {
                if crate::spawn_detached_image(&image, &["pin-file", "--cleanup"], "pinned") == 0 {
                    (Ok("已打开钉图".to_string()), ExportState::Done)
                } else {
                    (
                        Err("钉图未确认；原图仍在预览中，也可运行 vellum recover list".to_string()),
                        ExportState::Failed,
                    )
                }
            } else {
                match vellum_core::io::copy_image(&image) {
                    Ok(()) => (Ok("已复制完整图片".to_string()), ExportState::Done),
                    Err(error) => (
                        Err(crate::region_copy_failure_message(&error).to_string()),
                        ExportState::Failed,
                    ),
                }
            };
            let _ = tx.send((result, state));
        });
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(std::time::Duration::from_millis(30), move || {
            let Some(v) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match rx.try_recv() {
                Ok((result, state)) => {
                    v.refresh_document();
                    if export_matches_revision(v.revision.get(), revision) {
                        v.update_output(saving, pin, state);
                        v.status
                            .set_text(&result.unwrap_or_else(|e| format!("操作失败：{e}")));
                    } else {
                        v.status
                            .set_text("操作针对旧版图片；当前图已修改，请重新保存或复制");
                    }
                    v.busy.set(false);
                    for button in &v.actions {
                        button.set_sensitive(true);
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(_) => {
                    v.refresh_document();
                    if export_matches_revision(v.revision.get(), revision) {
                        v.update_output(saving, pin, ExportState::Failed);
                    }
                    v.status.set_text("导出任务未能完成，请重试");
                    v.busy.set(false);
                    for button in &v.actions {
                        button.set_sensitive(true);
                    }
                    glib::ControlFlow::Break
                }
            }
        });
    }
}

fn shared_outputs(document: &Document, mut outputs: OutputReport) -> OutputReport {
    if outputs.save == ExportState::NotRequested
        && document.saved_revision() == Some(document.revision())
    {
        outputs.save = ExportState::Done;
    }
    if outputs.copy == ExportState::NotRequested
        && document.copied_revision() == Some(document.revision())
    {
        outputs.copy = ExportState::Done;
    }
    outputs
}
fn export_matches_revision(current: u64, attempted: u64) -> bool {
    current == attempted
}
fn bounded_crop(
    image: &Rgb8,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<Rgb8, String> {
    EDIT_LIMITS
        .check(width, height, 4)
        .map_err(|error| error.to_string())?;
    if x.checked_add(width).is_none_or(|end| end > image.width)
        || y.checked_add(height).is_none_or(|end| end > image.height)
        || image
            .width
            .checked_mul(image.height)
            .and_then(|pixels| pixels.checked_mul(3))
            != Some(image.data.len())
    {
        return Err("裁切范围必须完全位于当前成品图片内".into());
    }
    let bytes = EDIT_LIMITS
        .check(width, height, 3)
        .map_err(|error| error.to_string())?;
    let mut data = Vec::new();
    data.try_reserve_exact(bytes)
        .map_err(|_| "没有足够内存建立裁切结果")?;
    for row in y..y + height {
        data.extend_from_slice(&image.row(row)[x * 3..(x + width) * 3]);
    }
    Ok(Rgb8::from_raw(width, height, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_view_then_committed_edit_retains_shared_pending_state() {
        let document = Rc::new(RefCell::new(
            Document::from_raster(Rgb8::new(8, 8)).unwrap(),
        ));
        let revision = document.borrow().revision();
        document.borrow_mut().mark_saved(revision); // Existing untouched PNG.
        assert!(!document.borrow().needs_output_confirmation());
        let old_view = document.clone();
        drop(old_view); // Original viewer closed while another window retained doc.
        document
            .borrow_mut()
            .set_crop(vellum_core::Rect::new(1, 1, 6, 6))
            .unwrap();
        let new_view = document.clone();
        assert!(new_view.borrow().needs_output_confirmation());
        assert_eq!(
            shared_outputs(&new_view.borrow(), OutputReport::default()).save,
            ExportState::NotRequested
        );
        new_view.borrow_mut().mark_saved(revision); // Late completion of old export.
        assert!(new_view.borrow().needs_output_confirmation());
        let current = new_view.borrow().revision();
        new_view.borrow_mut().mark_saved(current);
        assert!(!new_view.borrow().needs_output_confirmation());
        drop(new_view);
        assert_eq!(
            shared_outputs(&document.borrow(), OutputReport::default()).save,
            ExportState::Done
        );
    }
    #[test]
    fn export_completion_cannot_mark_a_new_revision_saved() {
        assert!(export_matches_revision(7, 7));
        assert!(!export_matches_revision(8, 7));
        assert!(!export_matches_revision(7, 8));
    }
    #[test]
    fn crop_validates_bounds_budget_and_copies_only_selected_pixels() {
        let image = Rgb8::from_raw(4, 3, (0..36).collect());
        let cropped = bounded_crop(&image, 1, 1, 2, 2).unwrap();
        assert_eq!((cropped.width, cropped.height), (2, 2));
        assert_eq!(cropped.pixel(0, 0), image.pixel(1, 1));
        assert_eq!(cropped.pixel(1, 1), image.pixel(2, 2));
        assert!(bounded_crop(&image, 3, 0, 2, 1).is_err());
        assert!(bounded_crop(&image, usize::MAX, 0, 2, 1).is_err());
        assert!(bounded_crop(&image, 0, 0, 0, 1).is_err());
        assert!(bounded_crop(&image, 0, 0, 16384, 16384).is_err());
    }
    #[test]
    fn output_feedback_keeps_save_copy_and_durability_separate() {
        let report = OutputReport {
            save: ExportState::Done,
            copy: ExportState::Failed,
        };
        assert!(report.save_text().contains("已完成"));
        assert!(report.copy_text().contains("失败"));
        let report = OutputReport {
            save: ExportState::Uncertain,
            copy: ExportState::Done,
        };
        assert!(report.save_text().contains("已写入"));
        assert!(report.save_text().contains("未确认"));
        assert!(!report.save_text().contains("失败"));
        assert!(report.copy_text().contains("已完成"));
    }
    #[test]
    fn long_image_fits_width_and_starts_at_top() {
        let mut v = Viewport::new(1200, 50000);
        v.resize(848.0, 600.0);
        assert!((v.scale - 2.0 / 3.0).abs() < 0.001);
        assert_eq!(v.y, 0.0);
        assert!(v.limits().1 > 30000.0);
        assert_eq!(v.visible_rows().0, 0);
    }
    #[test]
    fn scroll_clamps_both_ends() {
        let mut v = Viewport::new(900, 10000);
        v.resize(600.0, 500.0);
        v.scroll(-500.0, -500.0);
        assert_eq!((v.x, v.y), (0.0, 0.0));
        v.scroll(1e8, 1e8);
        assert_eq!((v.x, v.y), v.limits());
        assert_eq!(v.visible_rows().1, 10000);
    }
    #[test]
    fn zoom_keeps_centre_image_point() {
        let mut v = Viewport::new(1600, 10000);
        v.resize(800.0, 600.0);
        v.scroll(0.0, 900.0);
        let p = (400.0, 300.0);
        let o = v.origin();
        let iy = (p.1 - o.1) / v.scale;
        v.zoom(v.scale * 1.5, p);
        assert!(((p.1 - v.origin().1) / v.scale - iy).abs() < 0.001);
    }
    #[test]
    fn overview_allocation_is_bounded() {
        let image = Rgb8::new(4, 50000);
        let small = overview(&image);
        assert!(small.width <= 120 && small.height <= 1000);
    }
    #[test]
    fn tile_cache_never_builds_a_full_height_surface() {
        let image = Rgb8::new(100, 40000);
        let mut cache = TileCache::default();
        for row in (0..image.height).step_by(TILE_ROWS) {
            assert!(cache.get(&image, row).unwrap().height() <= TILE_ROWS as i32);
            assert!(cache.bytes <= CACHE_BYTES);
        }
    }
}

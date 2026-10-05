//! A small session editor reusing Annotator and the safe Document compositor.
use crate::annotate::{Annotator, PALETTE, Stroke, TEXT_SIZE_RANGE, Tool, WIDTH_RANGE};
use crate::document::{EditDraft, SharedDocument};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, DrawingArea, DropDown, Entry,
    EventControllerKey, EventControllerMotion, GestureClick, Label, Orientation, ScrolledWindow,
    SpinButton, glib,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vellum_core::geom::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Select,
    Draw(Tool),
    Crop,
}

type Point = (f64, f64);
type CropDrag = (Point, Point);

struct Drag {
    index: usize,
    original: Stroke,
    start: (f64, f64),
    resize: bool,
}

thread_local! {
    static WINDOWS: RefCell<Vec<Rc<EditorWindow>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn open(app: &Application, document: SharedDocument) -> Result<(), String> {
    let id = document.borrow().id();
    if let Some(window) = WINDOWS.with(|windows| {
        windows
            .borrow()
            .iter()
            .find(|window| !window.closed.get() && window.document.borrow().id() == id)
            .cloned()
    }) {
        window.window.present();
        return Ok(());
    }
    let editor = EditorWindow::new(app, document)?;
    WINDOWS.with(|windows| windows.borrow_mut().push(editor.clone()));
    editor.window.present();
    Ok(())
}

struct EditorWindow {
    document: SharedDocument,
    draft: RefCell<EditDraft>,
    annotator: RefCell<Annotator>,
    window: ApplicationWindow,
    area: DrawingArea,
    status: Label,
    undo_button: Button,
    redo_button: Button,
    tool_control: DropDown,
    color_control: DropDown,
    size_control: SpinButton,
    syncing_properties: Cell<bool>,
    text_entry: Entry,
    changing_entry: Cell<bool>,
    mode: Cell<Mode>,
    selected: Cell<Option<usize>>,
    drag: RefCell<Option<Drag>>,
    crop_drag: Cell<Option<CropDrag>>,
    zoom: Cell<f64>,
    closed: Cell<bool>,
    discard: Cell<bool>,
    asking_close: Cell<bool>,
}

impl EditorWindow {
    fn new(app: &Application, document: SharedDocument) -> Result<Rc<Self>, String> {
        crate::theme::install_default();
        let draft = document.borrow().draft();
        let crop = draft.viewport();
        let annotator = draft.annotator();
        let zoom = (900.0 / f64::from(crop.w))
            .min(560.0 / f64::from(crop.h))
            .clamp(0.05, 1.0);
        let parent = app.active_window();
        let window = ApplicationWindow::builder()
            .application(app)
            .title("编辑当前截图")
            .default_width(1024)
            .default_height(740)
            .build();
        window.add_css_class("vellum-window");
        if let Some(parent) = parent.filter(|parent| parent != window.upcast_ref::<gtk4::Window>())
        {
            window.set_transient_for(Some(&parent));
            window.set_modal(false);
        }
        let root = GtkBox::new(Orientation::Vertical, 8);
        root.set_margin_top(12);
        root.set_margin_bottom(12);
        root.set_margin_start(12);
        root.set_margin_end(12);
        let title = GtkBox::new(Orientation::Horizontal, 8);
        let label = Label::new(Some("本次会话 · 修改后点“应用”更新查看器"));
        label.set_hexpand(true);
        label.set_xalign(0.0);
        title.append(&label);
        let apply = Button::with_label("应用修改");
        apply.add_css_class("suggested-action");
        let close = Button::with_label("关闭");
        title.append(&apply);
        title.append(&close);
        root.append(&crate::drag::draggable(&title));
        let tools = GtkBox::new(Orientation::Horizontal, 8);
        let tool = DropDown::from_strings(&[
            "选择 / 移动",
            "裁切选区",
            "画笔",
            "箭头",
            "矩形",
            "椭圆",
            "文字",
            "实色遮挡",
            "马赛克",
            "模糊",
        ]);
        tools.append(&tool);
        let color = DropDown::from_strings(&["红", "黄", "绿", "蓝", "黑", "白", "自定义（保留）"]);
        tools.append(&color);
        let size = SpinButton::with_range(1.0, 120.0, 1.0);
        size.set_value(4.0);
        size.set_tooltip_text(Some("线宽 / 文字字号"));
        tools.append(&size);
        let undo = Button::with_label("撤销");
        let redo = Button::with_label("重做");
        let delete = Button::with_label("删除对象");
        tools.append(&undo);
        tools.append(&redo);
        tools.append(&delete);
        tools.append(&Label::new(Some("缩放%")));
        let zoom_control = SpinButton::with_range(5.0, 400.0, 5.0);
        zoom_control.set_value(zoom * 100.0);
        tools.append(&zoom_control);
        let toolbar_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Never)
            .child(&tools)
            .build();
        root.append(&toolbar_scroll);
        let text_entry = Entry::new();
        text_entry.set_max_length(crate::document::MAX_TEXT_BYTES as i32);
        text_entry.set_placeholder_text(Some("输入或修改文字，按 Enter 完成"));
        text_entry.set_visible(false);
        root.append(&text_entry);
        let area = DrawingArea::new();
        area.set_focusable(true);
        area.set_halign(gtk4::Align::Center);
        area.set_valign(gtk4::Align::Center);
        let scroll = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&area)
            .build();
        root.append(&scroll);
        let status = Label::new(Some(
            "选择对象可拖动；右下角调整大小；双击文字可修改。原PNG里的旧标注是背景。",
        ));
        status.set_wrap(true);
        status.set_xalign(0.0);
        root.append(&status);
        window.set_child(Some(&root));
        let this = Rc::new(Self {
            document,
            draft: RefCell::new(draft),
            annotator: RefCell::new(annotator),
            window,
            area,
            status,
            undo_button: undo.clone(),
            redo_button: redo.clone(),
            tool_control: tool.clone(),
            color_control: color.clone(),
            size_control: size.clone(),
            syncing_properties: Cell::new(false),
            text_entry,
            changing_entry: Cell::new(false),
            mode: Cell::new(Mode::Select),
            selected: Cell::new(None),
            drag: RefCell::new(None),
            crop_drag: Cell::new(None),
            zoom: Cell::new(zoom),
            closed: Cell::new(false),
            discard: Cell::new(false),
            asking_close: Cell::new(false),
        });
        this.resize_canvas();
        this.sync_history();
        this.sync_properties();
        this.connect_canvas();
        this.connect_close();
        let weak = Rc::downgrade(&this);
        apply.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.apply();
            }
        });
        let weak = Rc::downgrade(&this);
        close.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.window.close();
            }
        });
        let weak = Rc::downgrade(&this);
        undo.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.history(false);
            }
        });
        let weak = Rc::downgrade(&this);
        redo.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.history(true);
            }
        });
        let weak = Rc::downgrade(&this);
        delete.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.delete();
            }
        });
        let weak = Rc::downgrade(&this);
        this.tool_control.connect_selected_notify(move |combo| {
            if let Some(this) = weak.upgrade() {
                if !this.commit_canvas() {
                    return;
                }
                this.drag.borrow_mut().take();
                this.crop_drag.take();
                this.selected.set(None);
                this.text_entry.set_visible(false);
                let next_mode = [
                    Mode::Select,
                    Mode::Crop,
                    Mode::Draw(Tool::Pen),
                    Mode::Draw(Tool::Arrow),
                    Mode::Draw(Tool::Rect),
                    Mode::Draw(Tool::Ellipse),
                    Mode::Draw(Tool::Text),
                    Mode::Draw(Tool::Cover),
                    Mode::Draw(Tool::Mosaic),
                    Mode::Draw(Tool::Blur),
                ]
                .get(combo.selected() as usize)
                .copied()
                .unwrap_or(Mode::Select);
                switch_mode(
                    &this.mode,
                    &this.selected,
                    &mut this.annotator.borrow_mut(),
                    next_mode,
                );
                this.sync_properties();
                this.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(&this);
        color.connect_selected_notify(move |combo| {
            if let Some(this) = weak.upgrade() {
                if this.syncing_properties.get() {
                    return;
                }
                let index = this.selected.get();
                let mut object =
                    index.and_then(|index| this.annotator.borrow().objects().get(index).cloned());
                let target = change_color(
                    this.mode.get(),
                    &mut this.annotator.borrow_mut(),
                    object.as_mut(),
                    combo.selected() as usize,
                );
                if target == PropertyTarget::Selected
                    && let (Some(index), Some(object)) = (index, object)
                {
                    this.annotator.borrow_mut().replace_object(index, object);
                    this.commit_canvas();
                }
                this.sync_properties();
                this.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(&this);
        size.connect_value_changed(move |spin| {
            if let Some(this) = weak.upgrade() {
                if this.syncing_properties.get() {
                    return;
                }
                let index = this.selected.get();
                let mut object =
                    index.and_then(|index| this.annotator.borrow().objects().get(index).cloned());
                let target = change_size(
                    this.mode.get(),
                    &mut this.annotator.borrow_mut(),
                    object.as_mut(),
                    spin.value(),
                );
                if target == PropertyTarget::Selected
                    && let (Some(index), Some(object)) = (index, object)
                {
                    this.annotator.borrow_mut().replace_object(index, object);
                    // A continuous size adjustment becomes one history action
                    // when committed, not one entry for every spin event.
                    this.undo_button.set_sensitive(true);
                    this.redo_button.set_sensitive(false);
                }
                this.sync_properties();
                this.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(&this);
        zoom_control.connect_value_changed(move |spin| {
            if let Some(this) = weak.upgrade() {
                this.zoom.set(spin.value() / 100.0);
                this.resize_canvas();
            }
        });
        let weak = Rc::downgrade(&this);
        this.text_entry.connect_changed(move |entry| {
            if let Some(this) = weak.upgrade() {
                if this.changing_entry.get() {
                    return;
                }
                let text = entry.text().to_string();
                if text.len() > crate::document::MAX_TEXT_BYTES {
                    this.status.set_label("文字超过16KiB限制，请缩短");
                    return;
                }
                if let Some(index) = this.selected.get() {
                    let object = this.annotator.borrow().objects().get(index).cloned();
                    if let Some(mut object) = object
                        && object.tool == Tool::Text
                    {
                        object.text = text;
                        this.annotator.borrow_mut().replace_object(index, object);
                    }
                } else {
                    this.annotator.borrow_mut().replace_editing_text(&text);
                }
                this.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(&this);
        this.text_entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.commit_canvas();
                this.text_entry.set_visible(false);
                this.area.grab_focus();
            }
        });
        Ok(this)
    }

    fn sync_properties(&self) {
        let properties = {
            let annotator = self.annotator.borrow();
            let selected = self
                .selected
                .get()
                .and_then(|index| annotator.objects().get(index));
            property_state(self.mode.get(), selected, &annotator)
        };
        // GTK set_value/set_selected emit signals synchronously. They are a
        // display refresh here, never a request to modify a document or brush.
        self.syncing_properties.set(true);
        self.color_control.set_visible(properties.color.is_some());
        self.color_control.set_sensitive(properties.color.is_some());
        if let Some(color) = properties.color {
            self.color_control.set_selected(color);
        }
        self.size_control.set_visible(properties.size.is_some());
        self.size_control.set_sensitive(properties.size.is_some());
        if let Some((size, range)) = properties.size {
            self.size_control.set_range(range.0, range.1);
            self.size_control.set_value(size);
            self.size_control
                .set_tooltip_text(Some(if range == TEXT_SIZE_RANGE {
                    "文字字号（10–120）"
                } else {
                    "线宽（1–24）"
                }));
        }
        self.syncing_properties.set(false);
    }

    fn resize_canvas(&self) {
        let crop = self.draft.borrow().viewport();
        self.area
            .set_content_width((f64::from(crop.w) * self.zoom.get()).ceil().max(1.0) as i32);
        self.area
            .set_content_height((f64::from(crop.h) * self.zoom.get()).ceil().max(1.0) as i32);
        self.area.queue_draw();
    }
    fn point(&self, x: f64, y: f64) -> (f64, f64) {
        let crop = self.draft.borrow().viewport();
        (
            (x / self.zoom.get() + f64::from(crop.x))
                .clamp(f64::from(crop.x), f64::from(crop.x2())),
            (y / self.zoom.get() + f64::from(crop.y))
                .clamp(f64::from(crop.y), f64::from(crop.y2())),
        )
    }

    fn connect_canvas(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.area.set_draw_func(move |_, cr, _, _| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let crop = this.draft.borrow().viewport();
            cr.scale(this.zoom.get(), this.zoom.get());
            cr.translate(-f64::from(crop.x), -f64::from(crop.y));
            cr.rectangle(
                f64::from(crop.x),
                f64::from(crop.y),
                f64::from(crop.w),
                f64::from(crop.h),
            );
            cr.clip();
            this.draft.borrow().draw_background(cr);
            this.annotator.borrow().draw(cr);
            cr.set_source_rgb(0.2, 0.6, 1.0);
            cr.set_line_width(1.5 / this.zoom.get());
            if let Some(index) = this.selected.get()
                && let Some(object) = this.annotator.borrow().objects().get(index)
            {
                let (x, y, w, h) = bounds(object);
                cr.rectangle(x, y, w, h);
                let _ = cr.stroke();
                let s = 8.0 / this.zoom.get();
                cr.rectangle(x + w - s / 2.0, y + h - s / 2.0, s, s);
                let _ = cr.fill();
            }
            if let Some((start, end)) = this.crop_drag.get() {
                cr.rectangle(
                    start.0.min(end.0),
                    start.1.min(end.1),
                    (end.0 - start.0).abs(),
                    (end.1 - start.1).abs(),
                );
                let _ = cr.stroke();
            }
        });
        let click = GestureClick::new();
        click.set_button(1);
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |_, count, x, y| {
            if let Some(this) = weak.upgrade() {
                this.press(count, this.point(x, y));
            }
        });
        let weak = Rc::downgrade(self);
        click.connect_released(move |_, _, x, y| {
            if let Some(this) = weak.upgrade() {
                this.release(this.point(x, y));
            }
        });
        self.area.add_controller(click);
        let motion = EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            if let Some(this) = weak.upgrade() {
                this.motion(this.point(x, y));
            }
        });
        self.area.add_controller(motion);
        let keys = EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
            if ctrl && (key == gtk4::gdk::Key::z || key == gtk4::gdk::Key::Z) {
                this.history(shift);
                return glib::Propagation::Stop;
            }
            if ctrl && key == gtk4::gdk::Key::y {
                this.history(true);
                return glib::Propagation::Stop;
            }
            if ctrl && key == gtk4::gdk::Key::Return {
                this.apply();
                return glib::Propagation::Stop;
            }
            if !this.text_entry.has_focus()
                && (key == gtk4::gdk::Key::Delete || key == gtk4::gdk::Key::BackSpace)
            {
                this.delete();
                return glib::Propagation::Stop;
            }
            if key == gtk4::gdk::Key::Escape {
                this.window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);
    }

    fn press(&self, count: i32, point: (f64, f64)) {
        if !self.commit_canvas() {
            return;
        }
        self.area.grab_focus();
        self.text_entry.set_visible(false);
        match self.mode.get() {
            Mode::Crop => {
                self.crop_drag.set(Some((point, point)));
                self.selected.set(None);
            }
            Mode::Draw(Tool::Text) => {
                // Drawing text always creates a new object. Existing text is
                // edited with Select + double-click, so property targets never
                // silently switch between the old object and future brush.
                self.selected.set(None);
                self.annotator.borrow_mut().press(point.0, point.1);
                self.show_text_entry("");
            }
            Mode::Draw(_) => {
                self.selected.set(None);
                self.annotator.borrow_mut().press(point.0, point.1);
            }
            Mode::Select => {
                let selected = self.selected.get();
                let resize = selected
                    .and_then(|index| {
                        self.annotator.borrow().objects().get(index).map(|object| {
                            let (x, y, w, h) = bounds(object);
                            ((point.0 - x - w).abs() < 10.0 / self.zoom.get())
                                && ((point.1 - y - h).abs() < 10.0 / self.zoom.get())
                        })
                    })
                    .unwrap_or(false);
                let hit = if resize {
                    selected
                } else {
                    hit_test(
                        self.annotator.borrow().objects(),
                        point,
                        6.0 / self.zoom.get(),
                    )
                };
                self.selected.set(hit);
                if let Some(index) = hit {
                    let object = self.annotator.borrow().objects()[index].clone();
                    if count >= 2 && object.tool == Tool::Text {
                        self.edit_text();
                    } else {
                        *self.drag.borrow_mut() = Some(Drag {
                            index,
                            original: object,
                            start: point,
                            resize,
                        });
                    }
                }
            }
        }
        self.report_limit();
        self.sync_properties();
        self.area.queue_draw();
    }

    fn motion(&self, point: (f64, f64)) {
        if let Some(drag) = self.drag.borrow().as_ref() {
            let object = transformed(
                &drag.original,
                (point.0 - drag.start.0, point.1 - drag.start.1),
                drag.resize,
            );
            self.annotator
                .borrow_mut()
                .replace_object(drag.index, object);
        } else if let Some((start, _)) = self.crop_drag.get() {
            self.crop_drag.set(Some((start, point)));
        } else if matches!(self.mode.get(), Mode::Draw(_)) {
            self.annotator.borrow_mut().motion(point.0, point.1);
        }
        self.report_limit();
        self.sync_properties();
        self.area.queue_draw();
    }

    fn release(&self, point: (f64, f64)) {
        if self.drag.borrow().is_some() {
            self.motion(point);
        }
        if self.drag.borrow_mut().take().is_some() {
            self.commit_canvas();
        } else if let Some((start, _)) = self.crop_drag.take() {
            let crop = drag_crop(start, point, self.draft.borrow().viewport());
            if let Some(crop) = crop {
                let result = self.draft.borrow_mut().crop(crop);
                match result {
                    Ok(()) => {
                        self.rebuild();
                        self.status
                            .set_label("已裁切草稿，可撤销；点应用更新查看器");
                    }
                    Err(error) => self.status.set_label(&error),
                }
            }
        } else if matches!(self.mode.get(),Mode::Draw(tool) if tool!=Tool::Text) {
            self.annotator.borrow_mut().release(point.0, point.1);
            self.commit_canvas();
        }
        self.area.queue_draw();
    }

    fn report_limit(&self) {
        if let Some(message) = self.annotator.borrow_mut().take_limit_notice() {
            self.status.set_label(&message);
        }
    }
    fn show_text_entry(&self, text: &str) {
        self.changing_entry.set(true);
        self.text_entry.set_text(text);
        self.changing_entry.set(false);
        self.text_entry.set_visible(true);
        self.text_entry.grab_focus();
    }
    fn edit_text(&self) {
        if let Some(index) = self.selected.get() {
            let text = self.annotator.borrow().objects()[index].text.clone();
            self.show_text_entry(&text);
            self.sync_properties();
        }
    }

    fn commit_canvas(&self) -> bool {
        if gtk4::prelude::WidgetExt::is_visible(&self.text_entry)
            && self.text_entry.text().len() > crate::document::MAX_TEXT_BYTES
        {
            self.status.set_label("文字超过16KiB限制，请缩短后再应用");
            return false;
        }
        let objects = self
            .annotator
            .borrow()
            .snapshot_objects(Rect::new(0, 0, 1, 1));
        let result = self.draft.borrow_mut().replace_objects(objects.clone());
        match result {
            Ok(()) => {
                self.annotator.borrow_mut().replace_objects(objects);
                self.sync_history();
                true
            }
            Err(error) => {
                self.status.set_label(&error);
                false
            }
        }
    }
    fn rebuild(&self) {
        let (tool, color, width, text_size) = {
            let annotator = self.annotator.borrow();
            (
                annotator.tool(),
                annotator.color_index().unwrap_or(0),
                annotator.width(),
                annotator.text_size(),
            )
        };
        let mut annotator = self.draft.borrow().annotator();
        annotator.set_tool(tool);
        annotator.set_color_index(color);
        annotator.set_width(width);
        annotator.set_text_size(text_size);
        *self.annotator.borrow_mut() = annotator;
        self.selected.set(None);
        self.text_entry.set_visible(false);
        self.resize_canvas();
        self.sync_history();
        self.sync_properties();
    }
    fn sync_history(&self) {
        let draft = self.draft.borrow();
        self.undo_button.set_sensitive(draft.can_undo());
        self.redo_button.set_sensitive(draft.can_redo());
    }
    fn history(&self, redo: bool) {
        if self.crop_drag.take().is_some() {
            self.area.queue_draw();
            return;
        }
        self.drag.borrow_mut().take();
        if !self.commit_canvas() {
            return;
        }
        if redo {
            self.draft.borrow_mut().redo();
        } else {
            self.draft.borrow_mut().undo();
        }
        self.rebuild();
        self.status.set_label("草稿已更新，点应用更新查看器");
    }
    fn delete(&self) {
        if let Some(index) = self.selected.get() {
            if !self.commit_canvas() {
                return;
            }
            let mut objects = self.draft.borrow().objects().to_vec();
            if index < objects.len() {
                objects.remove(index);
                if let Err(error) = self.draft.borrow_mut().replace_objects(objects) {
                    self.status.set_label(&error);
                    return;
                }
                self.rebuild();
            }
        }
    }
    fn apply(&self) {
        self.drag.borrow_mut().take();
        self.crop_drag.take();
        if !self.commit_canvas() {
            return;
        }
        let result = self.document.borrow_mut().commit(&self.draft.borrow());
        match result {
            Ok(()) => {
                if let Err(error) = self
                    .draft
                    .borrow_mut()
                    .acknowledge_commit(&self.document.borrow())
                {
                    self.status.set_label(&error);
                    return;
                }
                self.rebuild();
                self.status
                    .set_label("修改已应用但尚未保存；请在查看器保存或复制");
                if let Err(error) = self.restore_viewer() {
                    self.status.set_label(&format!(
                        "修改已应用，但查看器打开失败：{error}。图片仍保留在当前编辑器，请勿退出。"
                    ));
                }
            }
            Err(error) => self.status.set_label(&error),
        }
    }
    fn restore_viewer(&self) -> Result<(), String> {
        let app = self
            .window
            .application()
            .ok_or("编辑器应用已关闭，无法交付图片")?;
        crate::preview::open_document(&app, self.document.clone())
    }

    fn dirty(&self) -> bool {
        (gtk4::prelude::WidgetExt::is_visible(&self.text_entry)
            && self.text_entry.text().len() > crate::document::MAX_TEXT_BYTES)
            || self.draft.borrow().is_dirty()
            || self
                .annotator
                .borrow()
                .snapshot_objects(Rect::new(0, 0, 1, 1))
                != self.draft.borrow().objects()
    }

    fn connect_close(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if this.dirty() && !this.discard.get() {
                if !this.asking_close.replace(true) {
                    let dialog = gtk4::AlertDialog::builder()
                        .modal(true)
                        .message("放弃未应用的修改？")
                        .detail("已应用不等于已保存。未应用的对象和裁切草稿将被丢弃。")
                        .build();
                    dialog.set_buttons(&["继续编辑", "放弃修改"]);
                    dialog.set_cancel_button(0);
                    dialog.set_default_button(0);
                    let weak = Rc::downgrade(&this);
                    dialog.choose(
                        Some(&this.window),
                        None::<&gtk4::gio::Cancellable>,
                        move |response| {
                            if let Some(this) = weak.upgrade() {
                                this.asking_close.set(false);
                                if response == Ok(1) {
                                    this.discard.set(true);
                                    this.window.close();
                                }
                            }
                        },
                    );
                }
                return glib::Propagation::Stop;
            }
            let needs_output = this.document.borrow().needs_output_confirmation();
            if let Err(error) = ensure_receiver_before_close(needs_output, || this.restore_viewer())
            {
                this.status.set_label(&format!(
                    "尚未保存，查看器打开失败：{error}。已保留当前窗口，请重试。"
                ));
                return glib::Propagation::Stop;
            }
            this.closed.set(true);
            let id = this.document.borrow().id();
            WINDOWS.with(|windows| {
                windows
                    .borrow_mut()
                    .retain(|window| window.document.borrow().id() != id)
            });
            glib::Propagation::Proceed
        });
    }
}

#[derive(Debug, PartialEq)]
struct Properties {
    color: Option<u32>,
    size: Option<(f64, (f64, f64))>,
}

#[derive(Debug, PartialEq, Eq)]
enum PropertyTarget {
    None,
    Brush,
    Selected,
}

fn palette_index(color: (f64, f64, f64)) -> u32 {
    PALETTE
        .iter()
        .position(|candidate| {
            (candidate.0 - color.0).abs() < 1e-6
                && (candidate.1 - color.1).abs() < 1e-6
                && (candidate.2 - color.2).abs() < 1e-6
        })
        .unwrap_or(PALETTE.len()) as u32
}

fn switch_mode(
    mode: &Cell<Mode>,
    selected: &Cell<Option<usize>>,
    annotator: &mut Annotator,
    next: Mode,
) {
    selected.set(None);
    if let Mode::Draw(tool) = next {
        annotator.set_tool(tool);
    }
    mode.set(next);
}

fn property_state(mode: Mode, selected: Option<&Stroke>, annotator: &Annotator) -> Properties {
    let (tool, color, width, text_size) = match mode {
        Mode::Select => match selected {
            Some(object) => (object.tool, object.color, object.width, object.size),
            None => {
                return Properties {
                    color: None,
                    size: None,
                };
            }
        },
        Mode::Draw(tool) => (
            tool,
            annotator.color(),
            annotator.width(),
            annotator.text_size(),
        ),
        Mode::Crop => {
            return Properties {
                color: None,
                size: None,
            };
        }
    };
    Properties {
        color: tool.supports_color().then(|| palette_index(color)),
        size: tool.supports_size().then_some(if tool == Tool::Text {
            (text_size, TEXT_SIZE_RANGE)
        } else {
            (width, WIDTH_RANGE)
        }),
    }
}

fn change_size(
    mode: Mode,
    annotator: &mut Annotator,
    object: Option<&mut Stroke>,
    value: f64,
) -> PropertyTarget {
    if !value.is_finite() {
        return PropertyTarget::None;
    }
    match mode {
        Mode::Draw(tool) if tool.supports_size() => {
            if tool == Tool::Text {
                annotator.set_text_size(value);
            } else {
                annotator.set_width(value);
            }
            PropertyTarget::Brush
        }
        Mode::Select => {
            if let Some(object) = object
                && object.tool.supports_size()
            {
                if object.tool == Tool::Text {
                    object.size = value.clamp(TEXT_SIZE_RANGE.0, TEXT_SIZE_RANGE.1);
                } else {
                    object.width = value.clamp(WIDTH_RANGE.0, WIDTH_RANGE.1);
                }
                PropertyTarget::Selected
            } else {
                PropertyTarget::None
            }
        }
        _ => PropertyTarget::None,
    }
}

fn change_color(
    mode: Mode,
    annotator: &mut Annotator,
    object: Option<&mut Stroke>,
    index: usize,
) -> PropertyTarget {
    let Some(color) = PALETTE.get(index).copied() else {
        return PropertyTarget::None;
    };
    match mode {
        Mode::Draw(tool) if tool.supports_color() => {
            annotator.set_color_index(index);
            PropertyTarget::Brush
        }
        Mode::Select => {
            if let Some(object) = object
                && object.tool.supports_color()
            {
                object.color = color;
                PropertyTarget::Selected
            } else {
                PropertyTarget::None
            }
        }
        _ => PropertyTarget::None,
    }
}

fn ensure_receiver_before_close(
    needs_output: bool,
    restore: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if needs_output { restore() } else { Ok(()) }
}

fn bounds(object: &Stroke) -> (f64, f64, f64, f64) {
    let Some(&(x, y)) = object.points.first() else {
        return (0.0, 0.0, 1.0, 1.0);
    };
    if object.tool == Tool::Text {
        let lines = object.text.lines().count().max(1);
        let chars = object
            .text
            .lines()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(1)
            .max(1);
        return (
            x,
            y,
            (chars as f64 * object.size * 0.7).max(10.0),
            (lines as f64 * object.size * 1.4).max(10.0),
        );
    }
    let (minx, maxx, miny, maxy) = object
        .points
        .iter()
        .fold((x, x, y, y), |(minx, maxx, miny, maxy), &(x, y)| {
            (minx.min(x), maxx.max(x), miny.min(y), maxy.max(y))
        });
    (minx, miny, (maxx - minx).max(1.0), (maxy - miny).max(1.0))
}

fn hit_test(objects: &[Stroke], point: (f64, f64), tolerance: f64) -> Option<usize> {
    let contains = |object: &Stroke| {
        let (x, y, w, h) = bounds(object);
        point.0 >= x - tolerance
            && point.0 <= x + w + tolerance
            && point.1 >= y - tolerance
            && point.1 <= y + h + tolerance
    };
    objects
        .iter()
        .enumerate()
        .rev()
        .find(|(_, object)| object.tool == Tool::Cover && contains(object))
        .or_else(|| {
            objects
                .iter()
                .enumerate()
                .rev()
                .find(|(_, object)| contains(object))
        })
        .map(|(index, _)| index)
}

fn transformed(object: &Stroke, delta: (f64, f64), resize: bool) -> Stroke {
    let mut object = object.clone();
    if resize {
        let (x, y, w, h) = bounds(&object);
        let sx = ((w + delta.0) / w).clamp(0.05, 20.0);
        let sy = ((h + delta.1) / h).clamp(0.05, 20.0);
        for point in &mut object.points {
            point.0 = x + (point.0 - x) * sx;
            point.1 = y + (point.1 - y) * sy;
        }
        if object.tool == Tool::Text {
            object.size = (object.size * sx.max(sy)).clamp(10.0, 120.0);
        }
    } else {
        for point in &mut object.points {
            point.0 += delta.0;
            point.1 += delta.1;
        }
    }
    object
}

fn drag_crop(start: (f64, f64), end: (f64, f64), viewport: Rect) -> Option<Rect> {
    let x = start.0.min(end.0).floor().max(f64::from(viewport.x)) as i32;
    let y = start.1.min(end.1).floor().max(f64::from(viewport.y)) as i32;
    let x2 = start.0.max(end.0).ceil().min(f64::from(viewport.x2())) as i32;
    let y2 = start.1.max(end.1).ceil().min(f64::from(viewport.y2())) as i32;
    let crop = Rect::new(x, y, x2 - x, y2 - y);
    crop.valid().then_some(crop)
}

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;

//! An in-memory editable screenshot. Only snapshot() is a sharing boundary.
//! Private session bytes contain source pixels: anonymous transport only, never
//! a recovery image, sidecar, exported PNG metadata, or default persistent history.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use crate::{
    annotate::{self, Annotator, Stroke, Tool},
    imaging,
};
use cairo::{Context, Format, ImageSurface};
use serde::{Deserialize, Serialize};
use vellum_core::{Rgb8, geom::Rect, image_limits::EDIT_LIMITS};

pub(crate) type SharedDocument = Rc<RefCell<Document>>;
pub(crate) const MAX_SESSION_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_OBJECTS: usize = 1024;
pub(crate) const MAX_POINTS_PER_OBJECT: usize = 4096;
pub(crate) const MAX_TOTAL_POINTS: usize = 65_536;
pub(crate) const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_TOTAL_TEXT_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_HISTORY_STEPS: usize = 32;
const MAGIC: &[u8; 8] = b"VLMSES01";
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static PROCESS_NONCE: OnceLock<Result<u64, ()>> = OnceLock::new();

fn new_document_id() -> Result<u64, String> {
    let nonce = PROCESS_NONCE
        .get_or_init(|| {
            let mut bytes = [0u8; 8];
            let mut filled = 0;
            while filled < bytes.len() {
                // SAFETY: the remaining slice is writable for precisely this length.
                let count = unsafe {
                    libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
                };
                if count < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                if count <= 0 {
                    return Err(());
                }
                filled += count as usize;
            }
            Ok(u64::from_ne_bytes(bytes))
        })
        .as_ref()
        .map_err(|_| "无法分配安全的会话标识")?;
    allocate_document_id(*nonce, &NEXT_ID)
}

// `fetch_update` was only renamed to `try_update` after our declared minimum
// Rust version, so the older name stays until the MSRV moves past that.
#[allow(deprecated)]
fn allocate_document_id(nonce: u64, counter: &AtomicU64) -> Result<u64, String> {
    loop {
        let sequence = counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "会话标识计数已耗尽")?;
        // Addition by a fixed nonce is a bijection on u64; do not merge zero
        // with one. Skip the sole zero result and refuse counter wraparound.
        let id = nonce.wrapping_add(sequence);
        if id != 0 {
            return Ok(id);
        }
    }
}

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub document_id: u64,
    pub revision: u64,
    pub image: Arc<Rgb8>,
}

#[derive(Clone, PartialEq)]
struct EditState {
    objects: Vec<Stroke>,
    crop: Rect,
}

impl EditState {
    fn bytes(&self) -> usize {
        self.objects
            .iter()
            .map(|object| {
                std::mem::size_of::<Stroke>() + object.points.len() * 16 + object.text.len()
            })
            .sum::<usize>()
            + std::mem::size_of::<Self>()
    }
}

pub(crate) struct Document {
    id: u64,
    revision: u64,
    saved_revision: Option<u64>,
    copied_revision: Option<u64>,
    // Deliberately private: ordinary consumers receive only Snapshot.
    source: Arc<Rgb8>,
    base: ImageSurface,
    state: EditState,
    cache: RefCell<Option<Arc<Rgb8>>>,
}

impl Document {
    pub fn from_raster(image: Rgb8) -> Result<Self, String> {
        Self::from_selection(image, Vec::new())
    }

    pub fn from_selection(source: Rgb8, objects: Vec<Stroke>) -> Result<Self, String> {
        validate_source(&source)?;
        validate_objects(&objects, source.width, source.height)?;
        let crop = Rect::new(0, 0, source.width as i32, source.height as i32);
        let base = imaging::to_surface(&source).map_err(|error| error.to_string())?;
        let id = new_document_id()?;
        Ok(Self {
            id,
            revision: 1,
            saved_revision: None,
            copied_revision: None,
            source: Arc::new(source),
            base,
            state: EditState { objects, crop },
            cache: RefCell::new(None),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn saved_revision(&self) -> Option<u64> {
        self.saved_revision
    }
    pub fn copied_revision(&self) -> Option<u64> {
        self.copied_revision
    }
    pub fn mark_saved(&mut self, revision: u64) {
        if revision == self.revision {
            self.saved_revision = Some(revision);
        }
    }
    pub fn mark_copied(&mut self, revision: u64) {
        if revision == self.revision {
            self.copied_revision = Some(revision);
        }
    }
    pub fn has_current_output(&self) -> bool {
        self.saved_revision == Some(self.revision) || self.copied_revision == Some(self.revision)
    }
    pub fn needs_output_confirmation(&self) -> bool {
        !self.has_current_output()
    }

    pub fn snapshot(&self) -> Result<Snapshot, String> {
        if self.cache.borrow().is_none() {
            let image = render(&self.base, &self.state)?;
            *self.cache.borrow_mut() = Some(Arc::new(image));
        }
        Ok(Snapshot {
            document_id: self.id,
            revision: self.revision,
            image: self
                .cache
                .borrow()
                .as_ref()
                .expect("rendered snapshot")
                .clone(),
        })
    }

    /// Test convenience; runtime cropping stays in an undoable EditDraft.
    #[cfg(test)]
    pub fn set_crop(&mut self, crop: Rect) -> Result<(), String> {
        validate_crop(crop, self.source.width, self.source.height)?;
        if self.state.crop != crop {
            self.bump_revision()?;
            self.state.crop = crop;
        }
        Ok(())
    }

    pub fn draft(&self) -> EditDraft {
        EditDraft {
            document_id: self.id,
            base_revision: self.revision,
            source: self.source.clone(),
            base: self.base.clone(),
            initial: self.state.clone(),
            state: self.state.clone(),
            undo: VecDeque::new(),
            redo: Vec::new(),
        }
    }

    pub fn commit(&mut self, draft: &EditDraft) -> Result<(), String> {
        if self.id != draft.document_id || self.revision != draft.base_revision {
            return Err("图片已在其它窗口修改；请保留当前草稿并重新打开编辑器".into());
        }
        validate_objects(&draft.state.objects, self.source.width, self.source.height)?;
        validate_crop(draft.state.crop, self.source.width, self.source.height)?;
        if self.state != draft.state {
            self.bump_revision()?;
            self.state = draft.state.clone();
        }
        Ok(())
    }

    fn bump_revision(&mut self) -> Result<(), String> {
        self.revision = self.revision.checked_add(1).ok_or("图片版本已耗尽")?;
        self.cache.borrow_mut().take();
        self.saved_revision = None;
        self.copied_revision = None;
        Ok(())
    }

    /// SENSITIVE: this is an editable session, not a shareable image.
    /// Only session_transfer's bounded sealed anonymous descriptor may carry it.
    pub fn encode_session(&self) -> Result<Vec<u8>, String> {
        let metadata = SessionMetadata {
            id: self.id,
            revision: self.revision,
            width: self.source.width,
            height: self.source.height,
            crop: [
                self.state.crop.x,
                self.state.crop.y,
                self.state.crop.w,
                self.state.crop.h,
            ],
            objects: self.state.objects.clone(),
        };
        let json = serde_json::to_vec(&metadata).map_err(|_| "无法编码编辑会话")?;
        if json.len() > MAX_METADATA_BYTES {
            return Err("编辑对象数据超过会话预算".into());
        }
        let len = 24usize
            .checked_add(json.len())
            .and_then(|len| len.checked_add(self.source.data.len()))
            .filter(|len| *len <= MAX_SESSION_BYTES)
            .ok_or("编辑会话超过传输预算")?;
        let mut bytes = Vec::with_capacity(len);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(json.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(self.source.data.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&json);
        bytes.extend_from_slice(&self.source.data);
        Ok(bytes)
    }

    pub fn decode_session(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 24 || bytes.len() > MAX_SESSION_BYTES || &bytes[..8] != MAGIC {
            return Err("编辑会话格式或大小无效".into());
        }
        let metadata_len = usize::try_from(u64::from_le_bytes(bytes[8..16].try_into().unwrap()))
            .map_err(|_| "编辑元数据长度无效")?;
        let raw_len = usize::try_from(u64::from_le_bytes(bytes[16..24].try_into().unwrap()))
            .map_err(|_| "编辑图片长度无效")?;
        let split = 24usize
            .checked_add(metadata_len)
            .ok_or("编辑会话长度溢出")?;
        if metadata_len > MAX_METADATA_BYTES || split.checked_add(raw_len) != Some(bytes.len()) {
            return Err("编辑会话长度不一致".into());
        }
        let metadata: SessionMetadata =
            serde_json::from_slice(&bytes[24..split]).map_err(|_| "编辑对象数据无效")?;
        EDIT_LIMITS
            .check(metadata.width, metadata.height, 4)
            .map_err(|error| error.to_string())?;
        let expected = EDIT_LIMITS
            .check(metadata.width, metadata.height, 3)
            .map_err(|error| error.to_string())?;
        if raw_len != expected
            || metadata.id == 0
            || metadata.revision == 0
            || metadata.revision == u64::MAX
        {
            return Err("编辑图片大小或版本无效".into());
        }
        validate_objects(&metadata.objects, metadata.width, metadata.height)?;
        let [x, y, w, h] = metadata.crop;
        let crop = Rect::new(x, y, w, h);
        validate_crop(crop, metadata.width, metadata.height)?;
        let source = Rgb8::from_raw(metadata.width, metadata.height, bytes[split..].to_vec());
        let mut document = Self::from_selection(source, metadata.objects)?;
        document.id = metadata.id;
        document.revision = metadata.revision;
        document.state.crop = crop;
        Ok(document)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionMetadata {
    id: u64,
    revision: u64,
    width: usize,
    height: usize,
    crop: [i32; 4],
    objects: Vec<Stroke>,
}

/// A local uncommitted working copy. Source buffers are shared; history stores
/// bounded object/crop metadata only, never another full screenshot per motion.
pub(crate) struct EditDraft {
    document_id: u64,
    base_revision: u64,
    source: Arc<Rgb8>,
    base: ImageSurface,
    initial: EditState,
    state: EditState,
    undo: VecDeque<EditState>,
    redo: Vec<EditState>,
}

impl EditDraft {
    pub fn viewport(&self) -> Rect {
        self.state.crop
    }
    pub fn objects(&self) -> &[Stroke] {
        &self.state.objects
    }
    pub fn is_dirty(&self) -> bool {
        self.state != self.initial
    }

    /// Applying changes does not destroy this window's undo/redo history.
    pub fn acknowledge_commit(&mut self, document: &Document) -> Result<(), String> {
        if self.document_id != document.id || self.state != document.state {
            return Err("当前草稿与提交结果不一致".into());
        }
        self.base_revision = document.revision;
        self.initial = self.state.clone();
        Ok(())
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn annotator(&self) -> Annotator {
        let mut annotator = Annotator::new();
        annotator.begin_canvas(self.state.crop, &self.base);
        annotator.replace_objects(self.state.objects.clone());
        annotator
    }

    pub fn draw_background(&self, cr: &Context) {
        let _ = cr.set_source_surface(&self.base, 0.0, 0.0);
        let _ = cr.paint();
    }

    pub fn replace_objects(&mut self, objects: Vec<Stroke>) -> Result<(), String> {
        validate_objects(&objects, self.source.width, self.source.height)?;
        self.record(EditState {
            objects,
            crop: self.state.crop,
        });
        Ok(())
    }

    pub fn crop(&mut self, crop: Rect) -> Result<(), String> {
        validate_crop(crop, self.source.width, self.source.height)?;
        self.record(EditState {
            objects: self.state.objects.clone(),
            crop,
        });
        Ok(())
    }

    fn record(&mut self, state: EditState) {
        if state == self.state {
            return;
        }
        self.undo
            .push_back(std::mem::replace(&mut self.state, state));
        self.redo.clear();
        self.trim_history();
    }

    fn trim_history(&mut self) {
        while self.undo.len() + self.redo.len() > MAX_HISTORY_STEPS
            || self
                .undo
                .iter()
                .chain(self.redo.iter())
                .map(EditState::bytes)
                .sum::<usize>()
                > MAX_HISTORY_BYTES
        {
            if self.undo.pop_front().is_none() {
                if self.redo.is_empty() {
                    break;
                }
                self.redo.remove(0);
            }
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(state) = self.undo.pop_back() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.state, state));
        self.trim_history();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(state) = self.redo.pop() else {
            return false;
        };
        self.undo
            .push_back(std::mem::replace(&mut self.state, state));
        self.trim_history();
        true
    }
}

fn validate_source(source: &Rgb8) -> Result<(), String> {
    EDIT_LIMITS
        .check(source.width, source.height, 4)
        .map_err(|error| error.to_string())?;
    let expected = EDIT_LIMITS
        .check(source.width, source.height, 3)
        .map_err(|error| error.to_string())?;
    if source.data.len() != expected {
        return Err("图片像素长度与尺寸不一致".into());
    }
    Ok(())
}

fn validate_crop(crop: Rect, width: usize, height: usize) -> Result<(), String> {
    if !crop.valid()
        || crop.x < 0
        || crop.y < 0
        || i64::from(crop.x) + i64::from(crop.w) > width as i64
        || i64::from(crop.y) + i64::from(crop.h) > height as i64
    {
        return Err("裁切范围必须位于当前源选区内且至少为1像素".into());
    }
    Ok(())
}

pub(crate) fn validate_objects(
    objects: &[Stroke],
    width: usize,
    height: usize,
) -> Result<(), String> {
    if objects.len() > MAX_OBJECTS {
        return Err("标注对象超过1024个限制".into());
    }
    let mut points = 0usize;
    let mut text = 0usize;
    for object in objects {
        points = points.saturating_add(object.points.len());
        text = text.saturating_add(object.text.len());
        let count_ok = match object.tool {
            Tool::Pen => !object.points.is_empty(),
            Tool::Text => object.points.len() == 1,
            Tool::Pick => false,
            _ => object.points.len() == 2,
        };
        if !count_ok
            || object.points.len() > MAX_POINTS_PER_OBJECT
            || points > MAX_TOTAL_POINTS
            || object.text.len() > MAX_TEXT_BYTES
            || text > MAX_TOTAL_TEXT_BYTES
            || !object.width.is_finite()
            || !(1.0..=24.0).contains(&object.width)
            || !object.size.is_finite()
            || !(10.0..=120.0).contains(&object.size)
            || [object.color.0, object.color.1, object.color.2]
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            || object.points.iter().any(|(x, y)| {
                !x.is_finite()
                    || !y.is_finite()
                    || *x < -(width as f64)
                    || *x > 2.0 * width as f64
                    || *y < -(height as f64)
                    || *y > 2.0 * height as f64
            })
        {
            return Err("标注几何、文字或点数超出安全限制".into());
        }
    }
    Ok(())
}

fn render(base: &ImageSurface, state: &EditState) -> Result<Rgb8, String> {
    let mut surface = ImageSurface::create(Format::ARgb32, state.crop.w, state.crop.h)
        .map_err(|error| error.to_string())?;
    {
        let cr = Context::new(&surface).map_err(|error| error.to_string())?;
        cr.translate(-f64::from(state.crop.x), -f64::from(state.crop.y));
        cr.set_source_surface(base, 0.0, 0.0)
            .map_err(|error| error.to_string())?;
        cr.paint().map_err(|error| error.to_string())?;
        for object in &state.objects {
            annotate::draw_stroke(&cr, object, false, "", Some(base));
        }
        // Opaque covers MUST remain above all ink and sampled source effects.
        for object in state
            .objects
            .iter()
            .filter(|object| object.tool == Tool::Cover)
        {
            annotate::draw_stroke(&cr, object, false, "", None);
        }
    }
    imaging::from_surface(&mut surface).map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod tests;

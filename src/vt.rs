//! Herdr's view of a core terminal: libghostty-vt fed with the relayed bytes,
//! read back as frames to draw, and asked to encode keys and mouse input.

use ghostty_sys::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl From<GhosttyColorRgb> for Rgb {
    fn from(c: GhosttyColorRgb) -> Self {
        Rgb(c.r, c.g, c.b)
    }
}

impl Rgb {
    /// WCAG relative luminance.
    fn luminance(self) -> f32 {
        let ch = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(self.0) + 0.7152 * ch(self.1) + 0.0722 * ch(self.2)
    }
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (x, y) = (a.luminance(), b.luminance());
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// Ghostty's graphics ranges retain intentional low contrast, as do our sprites.
fn preserves_graphics_color(text: &str) -> bool {
    crate::sprites::is_sprite(text)
        || text.chars().next().is_some_and(|c| matches!(c as u32,
            0x2500..=0x259f | 0xe0b0..=0xe0d7 | 0x1fb00..=0x1fbff | 0x1cc00..=0x1cebf))
}

/// `fg`, or near-white / near-black when it would vanish into `bg`.
fn readable(fg: Rgb, bg: Rgb, minimum: f32) -> Rgb {
    if minimum <= 1.0 || contrast(fg, bg) >= minimum {
        return fg;
    }
    let (light, dark) = (Rgb(0xf2, 0xf2, 0xf4), Rgb(0x1b, 0x1b, 0x1e));
    if contrast(light, bg) >= contrast(dark, bg) { light } else { dark }
}

#[derive(Clone, PartialEq)]
pub struct Theme {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub selection_bg: Rgb,
    pub palette16: [Rgb; 16],
    pub minimum_contrast: f32,
}

/// The terminal colors from settings.json's theme.
pub fn theme() -> Theme {
    use crate::settings::rgb;
    let t = crate::settings::theme();
    let get = |k: &str| rgb(t.get(k).map(String::as_str).unwrap_or("#ff00ff"));
    let mut palette16 = [Rgb::default(); 16];
    for (i, c) in palette16.iter_mut().enumerate() {
        *c = get(&format!("ansi{i}"));
    }
    Theme { fg: get("terminal_foreground"), bg: get("background"), cursor: get("cursor"), selection_bg: get("selection"), palette16, minimum_contrast: crate::settings::get().minimum_contrast }
}

#[derive(Clone, Default, PartialEq)]
pub struct Cell {
    /// Empty for blank cells and for the tail half of a wide character.
    pub text: String,
    pub fg: Rgb,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub strike: bool,
    pub wide: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
    Hollow,
}

#[derive(Clone, Copy)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub color: Rgb,
}

pub struct Frame {
    /// A row that didn't change since the last frame is the same `Rc`.
    pub rows: Vec<Rc<Vec<Cell>>>,
    /// Every row changed (output scrolled, a resize, new colors): none is kept.
    pub all: bool,
    pub cursor: Option<Cursor>,
    /// The cursor's cell even when the program hides it (agents draw their
    /// own): where input methods put their candidate window.
    pub cursor_cell: Option<(u16, u16)>,
    pub bg: Rgb,
}

/// Holds what the terminal's callbacks write into; boxed so its address is stable.
struct Shared {
    state: GhosttyRenderState,
    held_since: Option<Instant>,
    title_changed: bool,
    /// Replies the terminal writes back to the program (e.g. kitty graphics acks).
    replies: Vec<u8>,
    clipboard_write: bool,
    clipboard: [Option<String>; 2],
}

/// One kitty graphics image on screen, in cells of the viewport.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub image: u32,
    /// Changes whenever the image's pixels do (cache key).
    pub generation: u64,
    pub col: i32,
    pub row: i32,
    pub width: u32,
    pub height: u32,
    pub source: [u32; 4],
    pub offset: [u32; 2],
    /// Placeholder geometry uses integer cells; the view may use fractional cells.
    pub cell_pixels: Option<[u32; 2]>,
    pub z: i32,
}

impl Placement {
    pub fn pixel_geometry(&self, cell_width: f32, cell_height: f32) -> [f32; 4] {
        let [sx, sy] = self.cell_pixels.map_or([1.0, 1.0], |[w, h]| [cell_width / w as f32, cell_height / h as f32]);
        [
            self.col as f32 * cell_width + self.offset[0] as f32 * sx,
            self.row as f32 * cell_height + self.offset[1] as f32 * sy,
            self.width as f32 * sx,
            self.height as f32 * sy,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyVirtualPlacementSpec {
    image_id: u32,
    placement_id: u32,
    columns: u32,
    rows: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyVirtualCell {
    x: u16,
    y: u16,
    image_id_low: u32,
    image_id_high: Option<u32>,
    placement_id: u32,
    row: Option<u32>,
    col: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyVirtualRun {
    x: u16,
    y: u16,
    image_id_low: u32,
    image_id_high: Option<u32>,
    placement_id: u32,
    row: u32,
    col: u32,
    width: u32,
}

static KITTY_PLACEHOLDER_DIACRITICS: OnceLock<HashMap<u32, u32>> = OnceLock::new();

fn kitty_virtual_cell(x: u16, y: u16, grid_ref: &GhosttyGridRef) -> Option<KittyVirtualCell> {
    let mut graphemes = vec![0u32; 4];
    let mut len = 0;
    let mut style = GhosttyStyle { size: std::mem::size_of::<GhosttyStyle>(), ..Default::default() };
    unsafe {
        let result = ghostty_grid_ref_graphemes(grid_ref, graphemes.as_mut_ptr(), graphemes.len(), &mut len);
        if result == GHOSTTY_OUT_OF_SPACE {
            graphemes.resize(len, 0);
            if ghostty_grid_ref_graphemes(grid_ref, graphemes.as_mut_ptr(), graphemes.len(), &mut len) != GHOSTTY_SUCCESS {
                return None;
            }
        } else if result != GHOSTTY_SUCCESS {
            return None;
        }
        if len == 0 || graphemes[0] != 0x10eeee {
            return None;
        }
        if ghostty_grid_ref_style(grid_ref, &mut style) != GHOSTTY_SUCCESS {
            return None;
        }
    }
    graphemes.truncate(len);
    Some(KittyVirtualCell {
        x,
        y,
        image_id_low: kitty_placeholder_color_to_id(style.fg_color),
        image_id_high: graphemes.get(3).and_then(|c| kitty_placeholder_diacritic_index(*c)).filter(|high| *high <= 255),
        placement_id: kitty_placeholder_color_to_id(style.underline_color),
        row: graphemes.get(1).and_then(|c| kitty_placeholder_diacritic_index(*c)),
        col: graphemes.get(2).and_then(|c| kitty_placeholder_diacritic_index(*c)),
    })
}

fn kitty_placeholder_color_to_id(color: GhosttyStyleColor) -> u32 {
    unsafe {
        match color.tag {
            GHOSTTY_STYLE_COLOR_PALETTE => u32::from(color.value.palette),
            GHOSTTY_STYLE_COLOR_RGB => {
                let rgb = color.value.rgb;
                (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
            }
            _ => 0,
        }
    }
}

fn kitty_virtual_placement_geometry(
    run: KittyVirtualRun,
    spec: KittyVirtualPlacementSpec,
    image_width: u32,
    image_height: u32,
    cell_width: u32,
    cell_height: u32,
    generation: u64,
) -> Option<Placement> {
    if image_width == 0 || image_height == 0 {
        return None;
    }
    let cols = if spec.columns == 0 { image_width.div_ceil(cell_width) } else { spec.columns };
    let rows = if spec.rows == 0 { image_height.div_ceil(cell_height) } else { spec.rows };
    if run.col >= cols || run.row >= rows {
        return None;
    }
    let (grid_width, grid_height) = (f64::from(cols) * f64::from(cell_width), f64::from(rows) * f64::from(cell_height));
    let scale = (grid_width / f64::from(image_width)).min(grid_height / f64::from(image_height));
    let (image_left, image_top) = ((grid_width - f64::from(image_width) * scale) / 2.0, (grid_height - f64::from(image_height) * scale) / 2.0);
    let (left, top) = (f64::from(run.col) * f64::from(cell_width), f64::from(run.row) * f64::from(cell_height));
    let (right, bottom) = (f64::from((run.col + run.width).min(cols)) * f64::from(cell_width), top + f64::from(cell_height));
    let (x0, y0) = (left.max(image_left), top.max(image_top));
    let (x1, y1) = (right.min(image_left + f64::from(image_width) * scale), bottom.min(image_top + f64::from(image_height) * scale));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let sx0 = (((x0 - image_left) / scale).round() as u32).min(image_width - 1);
    let sy0 = (((y0 - image_top) / scale).round() as u32).min(image_height - 1);
    let sx1 = (((x1 - image_left) / scale).round() as u32).clamp(sx0 + 1, image_width);
    let sy1 = (((y1 - image_top) / scale).round() as u32).clamp(sy0 + 1, image_height);
    // Round shared edges, not slice lengths, so neighboring runs meet exactly.
    let width = (x1.round() - x0.round()) as u32;
    let height = (y1.round() - y0.round()) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Placement {
        image: spec.image_id,
        generation,
        col: i32::from(run.x),
        row: i32::from(run.y),
        width,
        height,
        source: [sx0, sy0, sx1 - sx0, sy1 - sy0],
        offset: [(x0.round() - left) as u32, (y0.round() - top) as u32],
        cell_pixels: Some([cell_width, cell_height]),
        // Ghostty gives placeholder images a fixed layer, ignoring the virtual placement's z.
        z: -1,
    })
}

fn kitty_placeholder_diacritic_index(codepoint: u32) -> Option<u32> {
    let map = KITTY_PLACEHOLDER_DIACRITICS.get_or_init(|| {
        // Reuse Ghostty's vendored table so Herdr decodes the same placeholder
        // row/column diacritics that libghostty accepts.
        let source =
            include_str!("../vendor/ghostty/src/terminal/kitty/graphics_unicode.zig");
        let mut map = HashMap::new();
        let mut in_table = false;
        for line in source.lines() {
            let line = line.trim();
            if line.starts_with("const diacritics:") {
                in_table = true;
                continue;
            }
            if !in_table {
                continue;
            }
            if line == "};" {
                break;
            }
            let Some(hex) = line
                .strip_prefix("0x")
                .and_then(|value| value.strip_suffix(','))
            else {
                continue;
            };
            if let Ok(value) = u32::from_str_radix(hex, 16) {
                map.insert(value, map.len() as u32);
            }
        }
        map
    });
    map.get(&codepoint).copied()
}


impl KittyVirtualRun {
    fn from_cell(cell: KittyVirtualCell) -> Self {
        Self {
            x: cell.x,
            y: cell.y,
            image_id_low: cell.image_id_low,
            image_id_high: cell.image_id_high,
            placement_id: cell.placement_id,
            row: cell.row.unwrap_or(0),
            col: cell.col.unwrap_or(0),
            width: 1,
        }
    }

    fn append(&mut self, cell: KittyVirtualCell) -> bool {
        if self.image_id_low != cell.image_id_low
            || self.placement_id != cell.placement_id
            || cell.row.is_some_and(|row| row != self.row)
            || cell.col.is_some_and(|col| col != self.col + self.width)
            || cell
                .image_id_high
                .is_some_and(|high| Some(high) != self.image_id_high)
        {
            return false;
        }
        self.width += 1;
        true
    }

    fn image_id(self) -> u32 {
        self.image_id_low | (self.image_id_high.unwrap_or(0) << 24)
    }

}

unsafe extern "C" fn on_write_pty(_t: GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize) {
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    if !data.is_null() && len > 0 {
        shared.replies.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
    }
}

const MAX_CLIPBOARD_BYTES: usize = 1 << 20;

unsafe extern "C" fn on_clipboard_write(_t: GhosttyTerminal, userdata: *mut c_void, request: *const GhosttyClipboardWrite) {
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    let request = unsafe { &*request };
    if !shared.clipboard_write || request.size < std::mem::size_of::<GhosttyClipboardWrite>() {
        return;
    }
    let slot = match request.location {
        GHOSTTY_CLIPBOARD_LOCATION_STANDARD => 0,
        GHOSTTY_CLIPBOARD_LOCATION_PRIMARY if cfg!(target_os = "linux") => 1,
        _ => return,
    };
    let text = if request.contents_len == 0 {
        ""
    } else {
        if request.contents_len != 1 || request.contents.is_null() {
            return;
        }
        let content = unsafe { &*request.contents };
        if content.mime.ptr.is_null() || content.data.len > MAX_CLIPBOARD_BYTES {
            return;
        }
        let mime = unsafe { std::slice::from_raw_parts(content.mime.ptr, content.mime.len) };
        if mime != b"text/plain" {
            return;
        }
        let bytes = if content.data.len == 0 {
            &[][..]
        } else if content.data.ptr.is_null() {
            return;
        } else {
            unsafe { std::slice::from_raw_parts(content.data.ptr, content.data.len) }
        };
        let Ok(text) = std::str::from_utf8(bytes) else { return };
        text
    };
    // Keep only the last write per destination, bounding a whole output batch.
    shared.clipboard[slot] = Some(text.to_owned());
    if let Some(reply) = request.reply {
        let result = GhosttyClipboardWriteReply {
            size: std::mem::size_of::<GhosttyClipboardWriteReply>(),
            result: GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS,
            remember: false,
        };
        unsafe { reply(request, &result) };
    }
}

/// PNG decoding for kitty graphics (libghostty-vt leaves it to the embedder).
unsafe extern "C" fn decode_png(_ud: *mut c_void, alloc: *const GhosttyAllocator, data: *const u8, len: usize, out: *mut GhosttySysImage) -> bool {
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA);
    let Ok(mut reader) = decoder.read_info() else { return false };
    let mut buf = vec![0; reader.output_buffer_size()];
    let Ok(info) = reader.next_frame(&mut buf) else { return false };
    let (w, h) = (info.width, info.height);
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::GrayscaleAlpha => buf[..info.buffer_size()].chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Rgb => buf[..info.buffer_size()].chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => buf[..info.buffer_size()].iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        _ => return false,
    };
    let ptr = unsafe { ghostty_alloc(alloc, rgba.len()) };
    if ptr.is_null() {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), ptr, rgba.len());
        *out = GhosttySysImage { width: w, height: h, data: ptr, data_len: rgba.len() };
    }
    true
}

fn install_png_decoder() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let f: GhosttySysDecodePngFn = Some(decode_png);
        ghostty_sys_set(GHOSTTY_SYS_OPT_DECODE_PNG, std::mem::transmute::<GhosttySysDecodePngFn, *const c_void>(f));
    });
}

pub struct Vt {
    term: GhosttyTerminal,
    shared: Box<Shared>,
    rows_it: GhosttyRenderStateRowIterator,
    cells: GhosttyRenderStateRowCells,
    keys: GhosttyKeyEncoder,
    key_event: GhosttyKeyEvent,
    mouse: GhosttyMouseEncoder,
    mouse_event: GhosttyMouseEvent,
    last_mouse_report: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
    theme: Theme,
    /// The built-in 256-color palette, which the theme's 16 colors override.
    base_palette: [GhosttyColorRgb; 256],
    /// Ghostty's own click/double-click/triple-click/drag selection logic.
    gesture: GhosttySelectionGesture,
    press: GhosttySelectionGestureEvent,
    drag: GhosttySelectionGestureEvent,
    release: GhosttySelectionGestureEvent,
    /// The cursor, the kitty images and the colors as last drawn: changes to
    /// them don't mark the render state dirty.
    drawn: (CursorKey, u64, u64),
    /// The rows read last frame by libghostty's row id, with their selection:
    /// one that isn't dirty is the same row and isn't read again.
    seen: HashMap<[u64; 2], (Rc<Vec<Cell>>, Option<(u16, u16)>)>,
    /// The width and colors they were read with: a change reads every row.
    seen_with: (u16, u64),
    placements_cache: RefCell<Option<Vec<Placement>>>,
    /// Whether the program wants win32 key events (Ghostty doesn't track it).
    pub win32: crate::win32_keys::Win32Input,
    /// The find bar's search, made the first time it's used (null until then).
    search: GhosttySearch,
    /// A regex find, which Ghostty's search doesn't do: its matches as of `written`.
    regex: Option<RegexFind>,
    /// Counts writes and resizes: what the screen's text was read at.
    written: u64,
    /// Copy mode's cursor and visual selection, while it's on.
    copy: Option<CopyState>,
}

/// A visual selection in copy mode: characters (v), whole lines (V) or a block (Ctrl+V).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Visual {
    Char,
    Line,
    Block,
}

/// A copy mode cursor move.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Motion {
    Left,
    Right,
    Up(u32),
    Down(u32),
    /// 0
    LineStart,
    /// ^
    FirstNonBlank,
    /// $
    LineEnd,
    /// g, G: the first and last line.
    Top,
    Bottom,
    /// 20G: a line of the history, from 0 at the top.
    Row(u32),
    /// H, M, L: the top, middle and bottom of the view; 3H, 3L: that many
    /// lines in from the top or bottom (2 here).
    ViewTop(u32),
    ViewMiddle,
    ViewBottom(u32),
    /// {, }: the blank line before or after this paragraph.
    ParagraphUp,
    ParagraphDown,
    /// w, b, e (W, B, E with `big`: words are anything between blanks).
    WordNext { big: bool },
    WordPrev { big: bool },
    WordEnd { big: bool },
    /// f, F, t, T: to (or `till` just before) a character on this line, forward
    /// or back. `again`: a repeat (; ,), where t and T go past the character beside them.
    Char { c: char, forward: bool, till: bool, again: bool },
}

/// A place in the text for word moves: a cell, or the end of a line that
/// doesn't wrap (a break between words).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Spot {
    Cell(u32, usize),
    Break(u32),
}

/// Rows read for one word move, by screen row: each cell's (x, character)
/// (wide characters once), and whether the row wraps onto the next.
struct Rows<'a> {
    vt: &'a Vt,
    read: HashMap<u32, (Vec<(u16, char)>, bool)>,
    last: u32,
}

impl Rows<'_> {
    fn row(&mut self, y: u32) -> &(Vec<(u16, char)>, bool) {
        let vt = self.vt;
        self.read.entry(y).or_insert_with(|| vt.row_chars(y))
    }

    /// 0: blank, 1: a word character, 2: other (punctuation); `big`: 1 for anything not blank.
    fn class(&mut self, at: Spot, big: bool) -> u8 {
        let Spot::Cell(y, i) = at else { return 0 };
        let c = self.row(y).0.get(i).map_or(' ', |c| c.1);
        if c.is_whitespace() {
            0
        } else if big || c.is_alphanumeric() || c == '_' {
            1
        } else {
            2
        }
    }

    fn next(&mut self, at: Spot) -> Option<Spot> {
        match at {
            Spot::Cell(y, i) => {
                let (len, wraps) = { let r = self.row(y); (r.0.len(), r.1) };
                if i + 1 < len {
                    Some(Spot::Cell(y, i + 1))
                } else if y >= self.last {
                    None
                } else if wraps {
                    Some(Spot::Cell(y + 1, 0))
                } else {
                    Some(Spot::Break(y))
                }
            }
            Spot::Break(y) => (y < self.last).then_some(Spot::Cell(y + 1, 0)),
        }
    }

    fn prev(&mut self, at: Spot) -> Option<Spot> {
        match at {
            Spot::Cell(y, 0) => {
                let y = y.checked_sub(1)?;
                let (len, wraps) = { let r = self.row(y); (r.0.len(), r.1) };
                Some(if wraps { Spot::Cell(y, len.saturating_sub(1)) } else { Spot::Break(y) })
            }
            Spot::Cell(y, i) => Some(Spot::Cell(y, i - 1)),
            Spot::Break(y) => {
                let len = self.row(y).0.len();
                Some(Spot::Cell(y, len.saturating_sub(1)))
            }
        }
    }

    fn xy(&mut self, at: Spot) -> (u16, u32) {
        match at {
            Spot::Cell(y, i) => (self.row(y).0.get(i).map_or(0, |c| c.0), y),
            Spot::Break(y) => (self.row(y).0.last().map_or(0, |c| c.0), y),
        }
    }

    /// w: past this word and the blanks after it.
    fn word_next(&mut self, mut at: Spot, big: bool) -> Spot {
        let c = self.class(at, big);
        while c != 0 && self.class(at, big) == c {
            match self.next(at) { Some(n) => at = n, None => return at }
        }
        while self.class(at, big) == 0 {
            match self.next(at) { Some(n) => at = n, None => return at }
        }
        at
    }

    /// e: to the end of this word, or the next one when already there.
    fn word_end(&mut self, mut at: Spot, big: bool) -> Spot {
        let Some(n) = self.next(at) else { return at };
        at = n;
        while self.class(at, big) == 0 {
            match self.next(at) { Some(n) => at = n, None => return at }
        }
        let c = self.class(at, big);
        while let Some(n) = self.next(at) {
            if self.class(n, big) != c {
                break;
            }
            at = n;
        }
        at
    }

    /// b: to the start of this word, or the one before when already there.
    fn word_prev(&mut self, mut at: Spot, big: bool) -> Spot {
        let Some(p) = self.prev(at) else { return at };
        at = p;
        while self.class(at, big) == 0 {
            match self.prev(at) { Some(p) => at = p, None => return at }
        }
        let c = self.class(at, big);
        while let Some(p) = self.prev(at) {
            if self.class(p, big) != c {
                break;
            }
            at = p;
        }
        at
    }
}

/// Cells tracked by libghostty, so they stay on their text as output comes in
/// and old history is dropped.
struct CopyState {
    cursor: GhosttyTrackedGridRef,
    /// Where the visual selection started (null: no selection).
    anchor: GhosttyTrackedGridRef,
    visual: Option<Visual>,
    /// The column j/k aim for (u16::MAX: the end of each line, after $).
    column: u16,
    /// The match a / or ? search points at (highlighted as the current one).
    found: Option<Hit>,
}

/// A match's first and last cell, (x, y) in screen rows (0: the top of the scrollback).
type Hit = ((u16, u32), (u16, u32));

struct RegexFind {
    re: fancy_regex::Regex,
    /// Newest first, as Ghostty's search orders them.
    hits: Vec<Hit>,
    selected: Option<usize>,
    at: u64,
}

/// Where a find stands: which match is selected (0 is the newest) of how many.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Found {
    pub selected: Option<usize>,
    pub total: usize,
}

/// What of the cursor a frame shows: (on screen, x, y, visible, style).
type CursorKey = (bool, u16, u16, bool, i32);

/// Where a link shows: (viewport row, first column, last column) per row.
pub type Spans = Vec<(u16, u16, u16)>;

/// A link found in the terminal: what it says, and where it shows.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkHit {
    pub target: String,
    pub spans: Spans,
}

fn hyperlink_uri(r: &GhosttyGridRef) -> Option<String> {
    let mut len = 0usize;
    let mut buf = vec![0u8; 512];
    let mut res = unsafe { ghostty_grid_ref_hyperlink_uri(r, buf.as_mut_ptr(), buf.len(), &mut len) };
    if res == GHOSTTY_OUT_OF_SPACE {
        buf.resize(len, 0);
        res = unsafe { ghostty_grid_ref_hyperlink_uri(r, buf.as_mut_ptr(), buf.len(), &mut len) };
    }
    (res == GHOSTTY_SUCCESS && len > 0).then(|| String::from_utf8_lossy(&buf[..len]).into_owned())
}

#[derive(Clone, Copy)]
pub enum Select {
    Press,
    Drag,
    Release,
}

fn ok(r: GhosttyResult, what: &str) {
    assert!(r == GHOSTTY_SUCCESS, "{what} failed: {r}");
}

unsafe extern "C" fn on_render_hold(_t: GhosttyTerminal, userdata: *mut c_void, held: bool) {
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    if held {
        // Capture the finished frame the program wants on screen before it redraws.
        unsafe { ghostty_render_state_update(shared.state, _t) };
        shared.held_since = Some(Instant::now());
    } else {
        shared.held_since = None;
    }
}

unsafe extern "C" fn on_title(_t: GhosttyTerminal, userdata: *mut c_void) {
    let shared = unsafe { &mut *(userdata as *mut Shared) };
    shared.title_changed = true;
}

const SYNC_OUTPUT: GhosttyMode = 2026;
const BRACKETED_PASTE: GhosttyMode = 2004;
const MAX_HOLD: Duration = Duration::from_millis(1000);
const SCROLLBACK_MAX_LINES: usize = 10_000;
// Bound memory per terminal; wide or heavily styled rows may hit this first.
const SCROLLBACK_MAX_BYTES: usize = 10_000_000;
/// Units per pixel for selection gestures (see `select`).
const SUBPIXEL: f32 = 64.0;

impl Vt {
    pub fn new(cols: u16, rows: u16) -> Vt {
        unsafe {
            let mut term = std::ptr::null_mut();
            ok(ghostty_terminal_new(std::ptr::null(), &mut term, cols.max(1), rows.max(1)), "terminal_new");
            let mut state = std::ptr::null_mut();
            ok(ghostty_render_state_new(std::ptr::null(), &mut state), "render_state_new");
            let mut rows_it = std::ptr::null_mut();
            ok(ghostty_render_state_row_iterator_new(std::ptr::null(), &mut rows_it), "row_iterator_new");
            let mut cells = std::ptr::null_mut();
            ok(ghostty_render_state_row_cells_new(std::ptr::null(), &mut cells), "row_cells_new");
            let mut keys = std::ptr::null_mut();
            ok(ghostty_key_encoder_new(std::ptr::null(), &mut keys), "key_encoder_new");
            let mut key_event = std::ptr::null_mut();
            ok(ghostty_key_event_new(std::ptr::null(), &mut key_event), "key_event_new");
            let mut mouse = std::ptr::null_mut();
            ok(ghostty_mouse_encoder_new(std::ptr::null(), &mut mouse), "mouse_encoder_new");
            let mut mouse_event = std::ptr::null_mut();
            ok(ghostty_mouse_event_new(std::ptr::null(), &mut mouse_event), "mouse_event_new");

            install_png_decoder();
            let mut shared = Box::new(Shared { state, held_since: None, title_changed: false, replies: Vec::new(), clipboard_write: true, clipboard: [None, None] });
            let ud = &mut *shared as *mut Shared as *const c_void;
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_USERDATA, ud);
            let hold: GhosttyTerminalRenderHoldFn = Some(on_render_hold);
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_RENDER_HOLD, std::mem::transmute(hold));
            let title: GhosttyTerminalTitleChangedFn = Some(on_title);
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, std::mem::transmute(title));
            let write: GhosttyTerminalWritePtyFn = Some(on_write_pty);
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_WRITE_PTY, std::mem::transmute(write));
            let clipboard: GhosttyTerminalClipboardWriteFn = Some(on_clipboard_write);
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE, std::mem::transmute::<GhosttyTerminalClipboardWriteFn, *const c_void>(clipboard));
            let max_clipboard: usize = MAX_CLIPBOARD_BYTES;
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE_MAX_BYTES, &max_clipboard as *const _ as *const c_void);
            // No read callback: programs can never read the local clipboard.
            let images: u64 = 128 << 20;
            ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT, &images as *const _ as *const c_void);

            ghostty_render_state_update(state, term);
            let mut colors = GhosttyRenderStateColors { size: std::mem::size_of::<GhosttyRenderStateColors>(), ..Default::default() };
            ghostty_render_state_get(state, GHOSTTY_RENDER_STATE_DATA_COLORS, &mut colors as *mut _ as *mut c_void);

            ok(ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &SCROLLBACK_MAX_BYTES as *const _ as *const c_void), "scrollback_max_bytes");
            ok(ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &SCROLLBACK_MAX_LINES as *const _ as *const c_void), "scrollback_max_lines");

            let mut gesture = std::ptr::null_mut();
            ok(ghostty_selection_gesture_new(std::ptr::null(), &mut gesture), "selection_gesture_new");
            let ev = |t| {
                let mut e = std::ptr::null_mut();
                ok(ghostty_selection_gesture_event_new(std::ptr::null(), &mut e, t), "gesture_event_new");
                e
            };
            let (press, drag, release) = (
                ev(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS),
                ev(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG),
                ev(GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE),
            );
            let mut vt = Vt {
                term,
                shared,
                rows_it,
                cells,
                keys,
                key_event,
                mouse,
                last_mouse_report: Vec::new(),
                mouse_event,
                cols,
                rows,
                theme: theme(),
                base_palette: colors.palette,
                gesture,
                press,
                drag,
                release,
                drawn: Default::default(),
                seen: HashMap::new(),
                seen_with: (0, 0),
                placements_cache: RefCell::new(None),
                win32: Default::default(),
                search: std::ptr::null_mut(),
                regex: None,
                written: 0,
                copy: None,
            };
            vt.apply_theme();
            vt
        }
    }

    /// Switches colors live (settings.json changed).
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.apply_theme();
            // Rows carry the theme's own colors too (the selection's).
            self.seen.clear();
        }
    }

    fn apply_theme(&mut self) {
        let c = |rgb: Rgb| GhosttyColorRgb { r: rgb.0, g: rgb.1, b: rgb.2 };
        let t = &self.theme;
        let (fg, bg, cur) = (c(t.fg), c(t.bg), c(t.cursor));
        let mut palette = self.base_palette;
        for (i, rgb) in t.palette16.iter().enumerate() {
            palette[i] = c(*rgb);
        }
        unsafe {
            ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &fg as *const _ as *const c_void);
            ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &bg as *const _ as *const c_void);
            ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &cur as *const _ as *const c_void);
            ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, &palette as *const _ as *const c_void);
        }
    }


    /// What the terminal wants to say back to the program since the last call.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.shared.replies)
    }

    /// Visible kitty graphics placements, bottom layer first.
    pub fn placements(&self) -> Vec<Placement> {
        if let Some(placements) = self.placements_cache.borrow().as_ref() {
            return placements.clone();
        }
        let mut out = Vec::new();
        unsafe {
            let mut g: GhosttyKittyGraphics = std::ptr::null_mut();
            if ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &mut g as *mut _ as *mut c_void) != GHOSTTY_SUCCESS || g.is_null() {
                return out;
            }
            let mut it: GhosttyKittyGraphicsPlacementIterator = std::ptr::null_mut();
            if ghostty_kitty_graphics_placement_iterator_new(std::ptr::null(), &mut it) != GHOSTTY_SUCCESS {
                return out;
            }
            let mut specs = Vec::new();
            if ghostty_kitty_graphics_get(g, GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR, &mut it as *mut _ as *mut c_void) == GHOSTTY_SUCCESS {
                while ghostty_kitty_graphics_placement_next(it) {
                    let (mut id, mut z, mut virt) = (0u32, 0i32, false);
                    ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID, &mut id as *mut _ as *mut c_void);
                    ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Z, &mut z as *mut _ as *mut c_void);
                    ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IS_VIRTUAL, &mut virt as *mut _ as *mut c_void);
                    let img = ghostty_kitty_graphics_image(g, id);
                    if virt {
                        let (mut placement_id, mut columns, mut rows) = (0u32, 0u32, 0u32);
                        ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_PLACEMENT_ID, &mut placement_id as *mut _ as *mut c_void);
                        ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_COLUMNS, &mut columns as *mut _ as *mut c_void);
                        ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_ROWS, &mut rows as *mut _ as *mut c_void);
                        specs.push(KittyVirtualPlacementSpec { image_id: id, placement_id, columns, rows });
                        continue;
                    }
                    if img.is_null() {
                        continue;
                    }
                    let mut info = GhosttyKittyGraphicsPlacementRenderInfo { size: std::mem::size_of::<GhosttyKittyGraphicsPlacementRenderInfo>(), ..Default::default() };
                    if ghostty_kitty_graphics_placement_render_info(it, img, self.term, &mut info) != GHOSTTY_SUCCESS || !info.viewport_visible {
                        continue;
                    }
                    let mut generation = 0u64;
                    ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_GENERATION, &mut generation as *mut _ as *mut c_void);
                    out.push(Placement {
                        image: id,
                        generation,
                        col: info.viewport_col,
                        row: info.viewport_row,
                        width: info.pixel_width,
                        height: info.pixel_height,
                        source: [info.source_x, info.source_y, info.source_width, info.source_height],
                        offset: [0, 0],
                        cell_pixels: None,
                        z,
                    });
                }
            }
            ghostty_kitty_graphics_placement_iterator_free(it);
            out.extend(self.kitty_virtual_image_placements(g, &specs));
        }
        out.sort_by_key(|p| p.z);
        *self.placements_cache.borrow_mut() = Some(out.clone());
        out
    }

    fn kitty_virtual_image_placements(&self, graphics: GhosttyKittyGraphics, specs: &[KittyVirtualPlacementSpec]) -> Vec<Placement> {
        if specs.is_empty() {
            return Vec::new();
        }
        let (mut width, mut height) = (0u32, 0u32);
        unsafe {
            ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_WIDTH_PX, &mut width as *mut _ as *mut c_void);
            ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_HEIGHT_PX, &mut height as *mut _ as *mut c_void);
        }
        let cell_width = (width / u32::from(self.cols.max(1))).max(1);
        let cell_height = (height / u32::from(self.rows.max(1))).max(1);
        let mut runs = Vec::new();
        for y in 0..self.rows {
            let Some(first) = self.grid_ref(GHOSTTY_POINT_TAG_VIEWPORT, 0, u32::from(y)) else { continue };
            let mut row = GhosttyRow::default();
            let mut placeholders = false;
            unsafe {
                ghostty_grid_ref_row(&first, &mut row);
                ghostty_row_get(row, GHOSTTY_ROW_DATA_KITTY_VIRTUAL_PLACEHOLDER, &mut placeholders as *mut _ as *mut c_void);
            }
            if !placeholders {
                continue;
            }
            let mut current: Option<KittyVirtualRun> = None;
            for x in 0..self.cols {
                let cell = self.grid_ref(GHOSTTY_POINT_TAG_VIEWPORT, x, u32::from(y))
                    .and_then(|grid_ref| kitty_virtual_cell(x, y, &grid_ref));
                match cell {
                    Some(cell) => {
                        if let Some(run) = current.as_mut() {
                            if run.append(cell) {
                                continue;
                            }
                            runs.push(*run);
                        }
                        current = Some(KittyVirtualRun::from_cell(cell));
                    }
                    None => {
                        if let Some(run) = current.take() {
                            runs.push(run);
                        }
                    }
                }
            }
            if let Some(run) = current {
                runs.push(run);
            }
        }
        let mut placements = Vec::new();
        for run in runs {
            let id = run.image_id();
            let Some(spec) = specs.iter().find(|spec| spec.image_id == id
                && (run.placement_id == 0 || spec.placement_id == run.placement_id)) else { continue };
            let image = unsafe { ghostty_kitty_graphics_image(graphics, id) };
            if image.is_null() {
                continue;
            }
            let (mut image_width, mut image_height, mut generation) = (0u32, 0u32, 0u64);
            unsafe {
                ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_IMAGE_DATA_WIDTH, &mut image_width as *mut _ as *mut c_void);
                ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_IMAGE_DATA_HEIGHT, &mut image_height as *mut _ as *mut c_void);
                ghostty_kitty_graphics_image_get(image, GHOSTTY_KITTY_IMAGE_DATA_GENERATION, &mut generation as *mut _ as *mut c_void);
            }
            if let Some(placement) = kitty_virtual_placement_geometry(run, *spec, image_width, image_height, cell_width, cell_height, generation) {
                placements.push(placement);
            }
        }
        placements
    }

    /// An image's pixels as BGRA (what GPUI uploads), cropped to `source`.
    pub fn image_bgra(&self, id: u32, source: [u32; 4]) -> Option<(u32, u32, Vec<u8>)> {
        unsafe {
            let mut g: GhosttyKittyGraphics = std::ptr::null_mut();
            ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &mut g as *mut _ as *mut c_void);
            let img = (!g.is_null()).then(|| ghostty_kitty_graphics_image(g, id)).filter(|i| !i.is_null())?;
            let (mut w, mut h, mut fmt) = (0u32, 0u32, GHOSTTY_KITTY_IMAGE_FORMAT_RGBA);
            let (mut ptr, mut len): (*const u8, usize) = (std::ptr::null(), 0);
            ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_WIDTH, &mut w as *mut _ as *mut c_void);
            ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_HEIGHT, &mut h as *mut _ as *mut c_void);
            ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_FORMAT, &mut fmt as *mut _ as *mut c_void);
            if ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_DATA_PTR, &mut ptr as *mut _ as *mut c_void) != GHOSTTY_SUCCESS || ptr.is_null() {
                return None;
            }
            ghostty_kitty_graphics_image_get(img, GHOSTTY_KITTY_IMAGE_DATA_DATA_LEN, &mut len as *mut _ as *mut c_void);
            let px = std::slice::from_raw_parts(ptr, len);
            let bpp = match fmt {
                GHOSTTY_KITTY_IMAGE_FORMAT_RGB => 3,
                GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA => 2,
                GHOSTTY_KITTY_IMAGE_FORMAT_GRAY => 1,
                _ => 4,
            };
            let [sx, sy, sw, sh] = source;
            let (sw, sh) = (sw.min(w.saturating_sub(sx)), sh.min(h.saturating_sub(sy)));
            if sw == 0 || sh == 0 {
                return None;
            }
            let mut out = Vec::with_capacity((sw * sh * 4) as usize);
            for y in sy..sy + sh {
                for x in sx..sx + sw {
                    let i = ((y * w + x) as usize) * bpp;
                    let p = px.get(i..i + bpp)?;
                    let (r, g, b, a) = match bpp {
                        4 => (p[0], p[1], p[2], p[3]),
                        3 => (p[0], p[1], p[2], 255),
                        2 => (p[0], p[0], p[0], p[1]),
                        _ => (p[0], p[0], p[0], 255),
                    };
                    out.extend_from_slice(&[b, g, r, a]);
                }
            }
            Some((sw, sh, out))
        }
    }

    /// Feeds one mouse step of a selection gesture (cell + pixel position in the grid).
    /// Applies the resulting selection; a plain click clears it.
    pub fn select(&mut self, step: Select, col: u16, row: u16, x: f32, y: f32, cell_w: f32, height: f32) {
        unsafe {
            let mut r = GhosttyGridRef { size: std::mem::size_of::<GhosttyGridRef>(), ..Default::default() };
            let mut p = GhosttyPoint { tag: GHOSTTY_POINT_TAG_VIEWPORT, ..Default::default() };
            p.value.coordinate = GhosttyPointCoordinate { x: col.min(self.cols.saturating_sub(1)), y: row.min(self.rows.saturating_sub(1)) as u32 };
            if ghostty_terminal_grid_ref(self.term, p, &mut r) != GHOSTTY_SUCCESS {
                return;
            }
            // Ghostty takes the cell width as a whole number: at zoom a cell is
            // 9.75 px, and 9 put "the middle of a cell" in the wrong place, so
            // the highlight jumped between cells while dragging. Positions and
            // geometry go in 1/64 px, where the width stays exact.
            let pos = GhosttySurfacePosition { x: (x * SUBPIXEL) as f64, y: (y * SUBPIXEL) as f64 };
            let e = match step {
                Select::Press => self.press,
                Select::Drag => self.drag,
                Select::Release => self.release,
            };
            ghostty_selection_gesture_event_set(e, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF, &r as *const _ as *const c_void);
            ghostty_selection_gesture_event_set(e, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION, &pos as *const _ as *const c_void);
            match step {
                Select::Press => {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
                    ghostty_selection_gesture_event_set(e, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS, &now as *const _ as *const c_void);
                    // GPUI doesn't expose the platform's double-click interval.
                    let interval = 500_000_000u64;
                    let distance = f64::from(4.0 * SUBPIXEL);
                    ghostty_selection_gesture_event_set(e, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS, &interval as *const _ as *const c_void);
                    ghostty_selection_gesture_event_set(e, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE, &distance as *const _ as *const c_void);
                }
                Select::Drag => {
                    let g = GhosttySelectionGestureGeometry {
                        columns: self.cols as u32,
                        cell_width: (cell_w.max(1.0) * SUBPIXEL).round() as u32,
                        padding_left: 0,
                        screen_height: (height.max(1.0) * SUBPIXEL).round() as u32,
                    };
                    ghostty_selection_gesture_event_set(e, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY, &g as *const _ as *const c_void);
                }
                Select::Release => {}
            }
            let mut sel = GhosttySelection { size: std::mem::size_of::<GhosttySelection>(), ..Default::default() };
            match ghostty_selection_gesture_event(self.gesture, self.term, e, &mut sel) {
                GHOSTTY_SUCCESS => {
                    ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel as *const _ as *const c_void);
                }
                _ if matches!(step, Select::Press) => {
                    ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, std::ptr::null());
                }
                _ => {}
            }
        }
    }

    /// The selected text, if anything is selected.
    pub fn selection_text(&self) -> Option<String> {
        unsafe {
            let opts = GhosttyTerminalSelectionFormatOptions {
                size: std::mem::size_of::<GhosttyTerminalSelectionFormatOptions>(),
                emit: GHOSTTY_FORMATTER_FORMAT_PLAIN,
                unwrap: true,
                trim: true,
                ..Default::default()
            };
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0usize;
            if ghostty_terminal_selection_format_alloc(self.term, std::ptr::null(), opts, &mut ptr, &mut len) != GHOSTTY_SUCCESS {
                return None;
            }
            if ptr.is_null() {
                return None;
            }
            let text = String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned();
            ghostty_free(std::ptr::null(), ptr, len);
            (!text.is_empty()).then_some(text)
        }
    }

    fn grid_ref(&self, tag: GhosttyPointTag, x: u16, y: u32) -> Option<GhosttyGridRef> {
        let mut r = GhosttyGridRef { size: std::mem::size_of::<GhosttyGridRef>(), ..Default::default() };
        let mut p = GhosttyPoint { tag, ..Default::default() };
        p.value.coordinate = GhosttyPointCoordinate { x, y };
        (unsafe { ghostty_terminal_grid_ref(self.term, p, &mut r) } == GHOSTTY_SUCCESS).then_some(r)
    }

    /// Screen rows count from the top of the scrollback; the viewport starts here.
    fn viewport_top(&self) -> Option<u32> {
        unsafe {
            let r = self.grid_ref(GHOSTTY_POINT_TAG_VIEWPORT, 0, 0)?;
            let mut p = GhosttyPointCoordinate::default();
            if ghostty_terminal_point_from_grid_ref(self.term, &r, GHOSTTY_POINT_TAG_SCREEN, &mut p) != GHOSTTY_SUCCESS {
                return None;
            }
            Some(p.y)
        }
    }

    /// The link at a viewport cell: a link the program marked (OSC 8), or a
    /// URL or path in the text. Follows lines the terminal wrapped, also into
    /// rows scrolled out of view.
    pub fn link_at(&self, col: u16, row: u16, find: impl FnOnce(&str, usize) -> Option<std::ops::Range<usize>>) -> Option<LinkHit> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        let top = self.viewport_top()?;
        let hovered = top + row as u32;
        let wraps = |y: u32| unsafe {
            let Some(r) = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, 0, y) else { return false };
            let mut line: GhosttyRow = Default::default();
            let mut wrap = false;
            ghostty_grid_ref_row(&r, &mut line) == GHOSTTY_SUCCESS
                && ghostty_row_get(line, GHOSTTY_ROW_DATA_WRAP, &mut wrap as *mut _ as *mut c_void) == GHOSTTY_SUCCESS
                && wrap
        };
        // A URL longer than this many rows isn't one worth finding.
        const REACH: u32 = 32;
        let mut first = hovered;
        while first > 0 && hovered - first < REACH && wraps(first - 1) {
            first -= 1;
        }
        let mut last = hovered;
        while last - hovered < REACH && wraps(last) && self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, 0, last + 1).is_some() {
            last += 1;
        }

        struct Spot {
            y: u32,
            x: u16,
            width: u16,
            byte: usize,
            uri: Option<String>,
        }
        let mut text = String::new();
        let mut spots: Vec<Spot> = Vec::new();
        for y in first..=last {
            let mut r = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, 0, y)?;
            for x in 0..self.cols {
                r.x = x;
                let (wide, linked) = unsafe {
                    let mut cell: GhosttyCell = Default::default();
                    ghostty_grid_ref_cell(&r, &mut cell);
                    let mut wide: GhosttyCellWide = GHOSTTY_CELL_WIDE_NARROW;
                    ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void);
                    let mut linked = false;
                    ghostty_cell_get(cell, GHOSTTY_CELL_DATA_HAS_HYPERLINK, &mut linked as *mut _ as *mut c_void);
                    (wide, linked)
                };
                if wide == GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD {
                    continue;
                }
                let byte = text.len();
                let mut cps = [0u32; 16];
                let mut len = 0usize;
                unsafe { ghostty_grid_ref_graphemes(&r, cps.as_mut_ptr(), cps.len(), &mut len) };
                let before = text.len();
                text.extend(cps[..len.min(cps.len())].iter().filter_map(|c| char::from_u32(*c)));
                if text.len() == before {
                    text.push(' ');
                }
                let uri = linked.then(|| hyperlink_uri(&r)).flatten();
                let width = if wide == GHOSTTY_CELL_WIDE_WIDE { 2 } else { 1 };
                spots.push(Spot { y, x, width, byte, uri });
            }
        }
        let at = spots.iter().position(|s| s.y == hovered && s.x <= col && col < s.x + s.width)?;
        let (target, chosen): (String, Vec<&Spot>) = match spots[at].uri.clone() {
            Some(uri) => {
                let same = |s: &&Spot| s.uri.as_deref() == Some(uri.as_str());
                let start = spots[..at].iter().rposition(|s| !same(&s)).map_or(0, |i| i + 1);
                let end = spots[at..].iter().position(|s| !same(&s)).map_or(spots.len(), |i| at + i);
                (uri, spots[start..end].iter().collect())
            }
            None => {
                let range = find(&text, spots[at].byte)?;
                (text[range.clone()].to_owned(), spots.iter().filter(|s| range.contains(&s.byte)).collect())
            }
        };
        let mut spans = Spans::new();
        for s in chosen {
            let Some(vy) = s.y.checked_sub(top).filter(|vy| *vy < self.rows as u32) else { continue };
            let end = s.x + s.width - 1;
            match spans.last_mut() {
                Some(last) if last.0 == vy as u16 => last.2 = end,
                _ => spans.push((vy as u16, s.x, end)),
            }
        }
        Some(LinkHit { target, spans })
    }

    pub fn clear_selection(&mut self) {
        unsafe { ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, std::ptr::null()) };
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.placements_cache.get_mut().take();
        self.written += 1;
        self.win32.watch(bytes);
        unsafe { ghostty_terminal_vt_write(self.term, bytes.as_ptr(), bytes.len()) };
    }

    pub fn allow_clipboard_write(&mut self, allow: bool) {
        self.shared.clipboard_write = allow;
    }

    pub fn take_clipboard(&mut self) -> [Option<String>; 2] {
        std::mem::take(&mut self.shared.clipboard)
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.placements_cache.get_mut().take();
        self.written += 1;
        self.cols = cols;
        self.rows = rows;
        unsafe { ghostty_terminal_resize(self.term, cols, rows, cell_w.max(1), cell_h.max(1)) };
    }

    pub fn take_title(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.shared.title_changed) {
            return None;
        }
        let mut s = GhosttyString::default();
        unsafe { ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_TITLE, &mut s as *mut _ as *mut c_void) };
        if s.ptr.is_null() {
            return Some(String::new());
        }
        let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    pub fn kitty_keyboard_flags(&self) -> u8 {
        let mut flags = 0u8;
        unsafe { ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS, &mut flags as *mut _ as *mut c_void) };
        flags
    }

    fn get_bool(&self, data: GhosttyTerminalData) -> bool {
        let mut v = false;
        unsafe { ghostty_terminal_get(self.term, data, &mut v as *mut _ as *mut c_void) };
        v
    }

    fn mode(&self, mode: GhosttyMode) -> bool {
        let mut cfg = GhosttyTerminalModeConfig { mode, value: false };
        unsafe { ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_MODE, &mut cfg as *mut _ as *mut c_void) };
        cfg.value
    }

    pub fn mouse_tracking(&self) -> bool {
        self.get_bool(GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING)
    }

    pub fn alt_screen(&self) -> bool {
        let mut screen: GhosttyTerminalScreen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        unsafe { ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &mut screen as *mut _ as *mut c_void) };
        screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE
    }

    pub fn scroll_viewport(&mut self, delta: isize) {
        self.placements_cache.get_mut().take();
        let mut s = GhosttyTerminalScrollViewport { tag: GHOSTTY_SCROLL_VIEWPORT_DELTA, ..Default::default() };
        s.value.delta = delta;
        unsafe { ghostty_terminal_scroll_viewport(self.term, s) };
    }

    /// How far up the view is: (lines above the bottom, lines of history).
    pub fn scrolled_up(&self) -> (u64, u64) {
        let mut sb = GhosttyTerminalScrollbar::default();
        unsafe { ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &mut sb as *mut _ as *mut c_void) };
        let history = sb.total.saturating_sub(sb.len);
        (history.saturating_sub(sb.offset), history)
    }

    pub fn scroll_to_bottom(&mut self) {
        self.placements_cache.get_mut().take();
        let s = GhosttyTerminalScrollViewport { tag: GHOSTTY_SCROLL_VIEWPORT_BOTTOM, ..Default::default() };
        unsafe { ghostty_terminal_scroll_viewport(self.term, s) };
    }

    /// Searches the screen and scrollback for `needle` (ASCII letters in any
    /// case) and selects the newest match, scrolling to it. Empty ends the find.
    pub fn find(&mut self, needle: &str) -> Found {
        self.regex = None;
        if needle.is_empty() {
            self.find_end();
            return Found::default();
        }
        unsafe {
            if self.search.is_null() && ghostty_search_new(std::ptr::null(), &mut self.search, self.term) != GHOSTTY_SUCCESS {
                self.search = std::ptr::null_mut();
                return Found::default();
            }
            let s = GhosttyString { ptr: needle.as_ptr(), len: needle.len() };
            ghostty_search_set(self.search, GHOSTTY_SEARCH_OPT_NEEDLE, &s as *const _ as *const c_void);
        }
        self.find_step(true)
    }

    /// Selects the next match toward older output (`older`) or newer, wrapping.
    pub fn find_step(&mut self, older: bool) -> Found {
        if self.regex.is_some() {
            return self.regex_step(older);
        }
        if self.search.is_null() {
            return Found::default();
        }
        self.placements_cache.get_mut().take();
        let opt = if older { GHOSTTY_SEARCH_OPT_SELECT_NEXT } else { GHOSTTY_SEARCH_OPT_SELECT_PREV };
        unsafe {
            ghostty_search_run(self.search);
            ghostty_search_set(self.search, opt, std::ptr::null());
        }
        self.select_found()
    }

    /// Where the find stands now, after output since the last step (the
    /// selection is left alone: the person may have selected something).
    pub fn found(&mut self) -> Found {
        // Matches are read again on the next step, not every frame (it's the whole scrollback).
        if let Some(rf) = &self.regex {
            return Found { selected: rf.selected, total: rf.hits.len() };
        }
        if self.search.is_null() {
            return Found::default();
        }
        unsafe { ghostty_search_run(self.search) };
        self.read_found()
    }

    fn read_found(&self) -> Found {
        let mut found = Found::default();
        unsafe {
            ghostty_search_get(self.search, GHOSTTY_SEARCH_DATA_TOTAL_MATCHES, &mut found.total as *mut _ as *mut c_void);
            let mut i = 0usize;
            if ghostty_search_get(self.search, GHOSTTY_SEARCH_DATA_SELECTED_INDEX, &mut i as *mut _ as *mut c_void) == GHOSTTY_SUCCESS {
                found.selected = Some(i);
            }
        }
        found
    }

    /// The selected match becomes the terminal's selection, so it's drawn highlighted.
    fn select_found(&mut self) -> Found {
        let found = self.read_found();
        // In copy mode the selection is the visual one; matches only highlight.
        if self.copy.is_some() {
            return found;
        }
        unsafe {
            let mut sel = GhosttySelection { size: std::mem::size_of::<GhosttySelection>(), ..Default::default() };
            if ghostty_search_get(self.search, GHOSTTY_SEARCH_DATA_SELECTED_MATCH, &mut sel as *mut _ as *mut c_void) == GHOSTTY_SUCCESS {
                ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel as *const _ as *const c_void);
            } else {
                ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, std::ptr::null());
            }
        }
        found
    }

    /// Ends the find: no needle, no highlighted match. The view stays where it is.
    pub fn find_end(&mut self) {
        if let Some(c) = self.copy.as_mut() {
            c.found = None;
        }
        let had_regex = self.regex.take().is_some();
        if !self.search.is_null() {
            unsafe { ghostty_search_set(self.search, GHOSTTY_SEARCH_OPT_NEEDLE, std::ptr::null()) };
        }
        if (had_regex || !self.search.is_null()) && self.copy.is_none() {
            self.clear_selection();
        }
    }

    /// Where the find's matches show in the viewport, as (row, first column,
    /// last column) per row: the selected match, and the others.
    pub fn find_spans(&self) -> (Spans, Spans) {
        let (mut current, mut others) = (Spans::new(), Spans::new());
        let rows = self.rows as u32;
        let last = self.cols.saturating_sub(1);
        let add = |spans: &mut Spans, (x0, y0): (u16, i64), (x1, y1): (u16, i64)| {
            for y in y0.max(0)..=y1.min(rows as i64 - 1) {
                let from = if y == y0 { x0 } else { 0 };
                let to = if y == y1 { x1 } else { last };
                spans.push((y as u16, from, to));
            }
        };
        // In copy mode, the current match is the one / or ? went to.
        let copy_found = self.copy.as_ref().map(|c| c.found);
        if let Some(rf) = &self.regex {
            let Some(top) = self.viewport_top() else { return (current, others) };
            let (top, bottom) = (top as i64, top as i64 + rows as i64);
            for (i, &hit) in rf.hits.iter().enumerate() {
                let ((x0, y0), (x1, y1)) = hit;
                let (y0, y1) = (y0 as i64 - top, y1 as i64 - top);
                if y1 < 0 || y0 >= bottom - top {
                    continue;
                }
                let is_current = copy_found.map_or(rf.selected == Some(i), |f| f == Some(hit));
                add(if is_current { &mut current } else { &mut others }, (x0, y0), (x1, y1));
            }
            return (current, others);
        }
        if self.search.is_null() {
            return (current, others);
        }
        // A match's ends in viewport rows; an end above the viewport counts as row -1.
        let ends = |sel: &GhosttySelection| -> Option<((u16, i64), (u16, i64))> {
            let point = |r: &GhosttyGridRef| unsafe {
                let mut p = GhosttyPointCoordinate::default();
                (ghostty_terminal_point_from_grid_ref(self.term, r, GHOSTTY_POINT_TAG_SCREEN, &mut p) == GHOSTTY_SUCCESS).then_some((p.x, p.y as i64))
            };
            let top = self.viewport_top()? as i64;
            let ((x0, y0), (x1, y1)) = (point(&sel.start)?, point(&sel.end)?);
            Some(((x0, y0 - top), (x1, y1 - top)))
        };
        let mut selected = None;
        unsafe {
            let mut sel = GhosttySelection { size: std::mem::size_of::<GhosttySelection>(), ..Default::default() };
            if let Some(found) = copy_found {
                let top = self.viewport_top().unwrap_or(0) as i64;
                selected = found.map(|((x0, y0), (x1, y1))| ((x0, y0 as i64 - top), (x1, y1 as i64 - top)));
            } else if ghostty_search_get(self.search, GHOSTTY_SEARCH_DATA_SELECTED_MATCH, &mut sel as *mut _ as *mut c_void) == GHOSTTY_SUCCESS {
                selected = ends(&sel);
            }
            let mut buf = GhosttySelectionBuffer { ptr: std::ptr::null_mut(), cap: 0, len: 0 };
            ghostty_search_get(self.search, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES, &mut buf as *mut _ as *mut c_void);
            if buf.len > 0 {
                let mut matches = vec![GhosttySelection { size: std::mem::size_of::<GhosttySelection>(), ..Default::default() }; buf.len];
                buf = GhosttySelectionBuffer { ptr: matches.as_mut_ptr(), cap: matches.len(), len: 0 };
                if ghostty_search_get(self.search, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES, &mut buf as *mut _ as *mut c_void) == GHOSTTY_SUCCESS {
                    for m in &matches[..buf.len.min(matches.len())] {
                        if let Some(e) = ends(m).filter(|e| Some(*e) != selected) {
                            add(&mut others, e.0, e.1);
                        }
                    }
                }
            }
        }
        if let Some((a, b)) = selected {
            add(&mut current, a, b);
        }
        (current, others)
    }

    /// Enters copy mode: a cursor where the terminal's is, or at the bottom
    /// left of the view when it's scrolled up.
    pub fn copy_start(&mut self) {
        self.copy_end();
        let last = self.last_row();
        let (x, y) = if self.scrolled_up().0 == 0 {
            let (mut cx, mut cy) = (0u16, 0u16);
            unsafe {
                ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_CURSOR_X, &mut cx as *mut _ as *mut c_void);
                ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_CURSOR_Y, &mut cy as *mut _ as *mut c_void);
            }
            (cx, (last + 1).saturating_sub(self.rows as u32) + cy as u32)
        } else {
            (0, self.viewport_top().unwrap_or(0) + self.rows as u32 - 1)
        };
        let Some(cursor) = self.track(x, y.min(last)) else { return };
        self.copy = Some(CopyState { cursor, anchor: std::ptr::null_mut(), visual: None, column: x, found: None });
    }

    /// Leaves copy mode, dropping a visual selection.
    pub fn copy_end(&mut self) {
        let Some(c) = self.copy.take() else { return };
        unsafe {
            ghostty_tracked_grid_ref_free(c.cursor);
            if !c.anchor.is_null() {
                ghostty_tracked_grid_ref_free(c.anchor);
                ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, std::ptr::null());
            }
        }
    }

    /// The copy cursor, (x, y) in screen rows.
    pub fn copy_cursor(&self) -> Option<(u16, u32)> {
        self.tracked_at(self.copy.as_ref()?.cursor)
    }

    /// The copy cursor's cell in the view, if it's in view.
    pub fn copy_cursor_view(&self) -> Option<(u16, u16)> {
        let (x, y) = self.copy_cursor()?;
        let row = y.checked_sub(self.viewport_top()?)?;
        (row < self.rows as u32).then_some((x, row as u16))
    }

    pub fn copy_visual(&self) -> Option<Visual> {
        self.copy.as_ref()?.visual
    }

    pub fn copy_move(&mut self, m: Motion) {
        let Some((x, y)) = self.copy_cursor() else { return };
        let column = self.copy.as_ref().map_or(x, |c| c.column);
        let last = self.last_row();
        let top = self.viewport_top().unwrap_or(0);
        let rows = self.rows as u32;
        // Vertical moves keep aiming for the column, short lines stop at their end.
        let aim = |vt: &Vt, y: u32| column.min(vt.line_end(y));
        let (x, y, column) = match m {
            Motion::Left => {
                let mut nx = x.saturating_sub(1);
                if nx > 0 && self.spacer(nx, y) {
                    nx -= 1;
                }
                (nx, y, nx)
            }
            Motion::Right => {
                let end = self.line_end(y).max(x);
                let mut nx = (x + 1).min(end);
                if nx < end && self.spacer(nx, y) {
                    nx += 1;
                }
                (nx, y, nx)
            }
            Motion::Up(n) => {
                let ny = y.saturating_sub(n);
                (aim(self, ny), ny, column)
            }
            Motion::Down(n) => {
                let ny = (y + n).min(last);
                (aim(self, ny), ny, column)
            }
            Motion::LineStart => (0, y, 0),
            Motion::FirstNonBlank => {
                let nx = self.blanks(y).iter().position(|b| !b).unwrap_or(0) as u16;
                (nx, y, nx)
            }
            Motion::LineEnd => (self.line_end(y), y, u16::MAX),
            Motion::Top => (0, 0, 0),
            Motion::Bottom => (aim(self, last), last, column),
            Motion::Row(n) => {
                let ny = n.min(last);
                (aim(self, ny), ny, column)
            }
            Motion::ViewTop(n) => {
                let ny = (top + n.min(rows - 1)).min(last);
                (aim(self, ny), ny, column)
            }
            Motion::ViewMiddle => {
                let ny = (top + rows / 2).min(last);
                (aim(self, ny), ny, column)
            }
            Motion::ViewBottom(n) => {
                let ny = (top + rows - 1 - n.min(rows - 1)).min(last);
                (aim(self, ny), ny, column)
            }
            // Past any blank lines here, then the paragraph, as tmux and vi do.
            Motion::ParagraphUp => {
                let mut ny = y.saturating_sub(1);
                while ny > 0 && self.blank(ny) {
                    ny -= 1;
                }
                while ny > 0 && !self.blank(ny) {
                    ny -= 1;
                }
                (0, ny, 0)
            }
            Motion::WordNext { big } | Motion::WordPrev { big } | Motion::WordEnd { big } => {
                let mut r = Rows { vt: self, read: HashMap::new(), last };
                let i = r.row(y).0.iter().rposition(|c| c.0 <= x).unwrap_or(0);
                let at = Spot::Cell(y, i);
                let to = match m {
                    Motion::WordNext { .. } => r.word_next(at, big),
                    Motion::WordPrev { .. } => r.word_prev(at, big),
                    _ => r.word_end(at, big),
                };
                let (nx, ny) = r.xy(to);
                (nx, ny, nx)
            }
            Motion::Char { c, forward, till, again } => {
                let (cells, _) = self.row_chars(y);
                let here = cells.iter().rposition(|cell| cell.0 <= x).unwrap_or(0);
                // A t/T repeated from just before its character goes on to the next one.
                let skip = if till && again { 2 } else { 1 };
                let found = if forward {
                    cells.iter().enumerate().skip(here + skip).find(|(_, cell)| cell.1 == c).map(|(i, _)| if till { i - 1 } else { i })
                } else {
                    cells[..here.saturating_sub(skip - 1)].iter().rposition(|cell| cell.1 == c).map(|i| if till { i + 1 } else { i })
                };
                match found {
                    Some(i) => (cells[i].0, y, cells[i].0),
                    None => (x, y, column),
                }
            }
            Motion::ParagraphDown => {
                let mut ny = (y + 1).min(last);
                while ny < last && self.blank(ny) {
                    ny += 1;
                }
                while ny < last && !self.blank(ny) {
                    ny += 1;
                }
                (0, ny, 0)
            }
        };
        self.copy_set(x, y, column);
    }

    /// Scrolls the view by `lines` (down is positive). `carry`: the cursor moves
    /// as far (Ctrl+D/U/F/B); otherwise it stays, kept in view (Ctrl+E/Y).
    pub fn copy_scroll(&mut self, lines: isize, carry: bool) {
        let Some((x, y)) = self.copy_cursor() else { return };
        let column = self.copy.as_ref().map_or(x, |c| c.column);
        self.scroll_viewport(lines);
        let ny = if carry {
            (y as i64 + lines as i64).clamp(0, self.last_row() as i64) as u32
        } else {
            let top = self.viewport_top().unwrap_or(0);
            y.clamp(top, top + self.rows as u32 - 1)
        };
        self.copy_set(column.min(self.line_end(ny)), ny, column);
    }

    /// Starts a visual selection at the cursor, switches its kind, or (the same
    /// kind again) drops it.
    pub fn copy_select(&mut self, v: Visual) {
        let Some((x, y)) = self.copy_cursor() else { return };
        let Some(c) = self.copy.as_ref() else { return };
        match c.visual {
            Some(now) if now == v => return self.copy_unselect(),
            Some(_) => {}
            None => {
                let Some(anchor) = self.track(x, y) else { return };
                if let Some(c) = self.copy.as_mut() {
                    c.anchor = anchor;
                }
            }
        }
        if let Some(c) = self.copy.as_mut() {
            c.visual = Some(v);
        }
        self.copy_selection();
    }

    pub fn copy_unselect(&mut self) {
        let Some(c) = self.copy.as_mut() else { return };
        c.visual = None;
        let anchor = std::mem::replace(&mut c.anchor, std::ptr::null_mut());
        if !anchor.is_null() {
            unsafe { ghostty_tracked_grid_ref_free(anchor) };
        }
        self.clear_selection();
    }

    /// Moves the cursor (clamped to the screen), scrolls to keep it in view and
    /// stretches the visual selection to it.
    fn copy_set(&mut self, x: u16, y: u32, column: u16) {
        let Some(c) = self.copy.as_mut() else { return };
        c.column = column;
        let cursor = c.cursor;
        let (x, y) = (x.min(self.cols.saturating_sub(1)), y.min(self.last_row()));
        let mut p = GhosttyPoint { tag: GHOSTTY_POINT_TAG_SCREEN, ..Default::default() };
        p.value.coordinate = GhosttyPointCoordinate { x, y };
        unsafe { ghostty_tracked_grid_ref_set(cursor, self.term, p) };
        self.scroll_into_view(y, y);
        self.copy_selection();
    }

    /// Scrolls as little as shows rows y0..=y1 (from y0, if they don't fit).
    fn scroll_into_view(&mut self, y0: u32, y1: u32) {
        let top = self.viewport_top().unwrap_or(0);
        let rows = self.rows as u32;
        if y0 >= top && y1 < top + rows {
            return;
        }
        self.placements_cache.get_mut().take();
        let mut s = GhosttyTerminalScrollViewport { tag: GHOSTTY_SCROLL_VIEWPORT_ROW, ..Default::default() };
        s.value.row = if y0 < top || y1 + 1 - y0 > rows { y0 } else { y1 + 1 - rows } as usize;
        unsafe { ghostty_terminal_scroll_viewport(self.term, s) };
    }

    /// Goes to the find's next match from the copy cursor, toward older output
    /// (up, `?`) or newer (down, `/`), wrapping. `move_cursor`: the cursor goes
    /// to it (Enter, n, N); else the view only shows it (while typing). False
    /// when nothing matches.
    pub fn copy_jump(&mut self, older: bool, move_cursor: bool) -> bool {
        let Some((x, y)) = self.copy_cursor() else { return false };
        let mut hits = self.find_hits();
        // Reading order, newest (furthest down) first.
        let key = |h: &Hit| (h.0 .1, h.0 .0);
        hits.sort_by_key(|h| std::cmp::Reverse(key(h)));
        let at = (y, x);
        let pick = if older {
            hits.iter().find(|h| key(h) < at).or(hits.first())
        } else {
            hits.iter().rev().find(|h| key(h) > at).or(hits.last())
        };
        let hit = pick.copied();
        if let Some(c) = self.copy.as_mut() {
            c.found = hit;
        }
        let Some(((x0, y0), (_, y1))) = hit else { return false };
        if move_cursor {
            self.copy_set(x0, y0, x0);
        } else {
            self.scroll_into_view(y0, y1);
        }
        if self.copy_visual().is_none() {
            self.clear_selection();
        }
        true
    }

    /// Every match of the find (plain or regex), in screen rows.
    fn find_hits(&mut self) -> Vec<Hit> {
        if self.regex.is_some() {
            let written = self.written;
            let mut rf = self.regex.take().unwrap();
            if rf.at != written {
                rf.hits = self.regex_hits(&rf.re);
                rf.at = written;
                rf.selected = rf.selected.filter(|&i| i < rf.hits.len());
            }
            let hits = rf.hits.clone();
            self.regex = Some(rf);
            return hits;
        }
        if self.search.is_null() {
            return Vec::new();
        }
        let point = |r: &GhosttyGridRef| unsafe {
            let mut p = GhosttyPointCoordinate::default();
            (ghostty_terminal_point_from_grid_ref(self.term, r, GHOSTTY_POINT_TAG_SCREEN, &mut p) == GHOSTTY_SUCCESS).then_some((p.x, p.y))
        };
        unsafe {
            ghostty_search_run(self.search);
            let mut buf = GhosttySelectionBuffer { ptr: std::ptr::null_mut(), cap: 0, len: 0 };
            ghostty_search_get(self.search, GHOSTTY_SEARCH_DATA_MATCHES, &mut buf as *mut _ as *mut c_void);
            if buf.len == 0 {
                return Vec::new();
            }
            let mut matches = vec![GhosttySelection { size: std::mem::size_of::<GhosttySelection>(), ..Default::default() }; buf.len];
            buf = GhosttySelectionBuffer { ptr: matches.as_mut_ptr(), cap: matches.len(), len: 0 };
            if ghostty_search_get(self.search, GHOSTTY_SEARCH_DATA_MATCHES, &mut buf as *mut _ as *mut c_void) != GHOSTTY_SUCCESS {
                return Vec::new();
            }
            matches[..buf.len.min(matches.len())].iter().filter_map(|m| Some((point(&m.start)?, point(&m.end)?))).collect()
        }
    }

    /// The visual selection from its start to the cursor, as the terminal's selection.
    fn copy_selection(&mut self) {
        let Some(c) = self.copy.as_ref() else { return };
        let (Some(v), Some(a), Some(b)) = (c.visual, self.tracked_at(c.anchor), self.tracked_at(c.cursor)) else { return };
        let ((x0, y0), (x1, y1)) = match v {
            // Reading order: by row, then column.
            Visual::Char if (a.1, a.0) <= (b.1, b.0) => (a, b),
            Visual::Char => (b, a),
            Visual::Line => ((0, a.1.min(b.1)), (self.cols.saturating_sub(1), a.1.max(b.1))),
            Visual::Block => ((a.0.min(b.0), a.1.min(b.1)), (a.0.max(b.0), a.1.max(b.1))),
        };
        let (Some(start), Some(end)) = (self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, x0, y0), self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, x1, y1)) else { return };
        let sel = GhosttySelection { size: std::mem::size_of::<GhosttySelection>(), start, end, rectangle: v == Visual::Block };
        unsafe { ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel as *const _ as *const c_void) };
    }

    fn track(&self, x: u16, y: u32) -> Option<GhosttyTrackedGridRef> {
        let mut p = GhosttyPoint { tag: GHOSTTY_POINT_TAG_SCREEN, ..Default::default() };
        p.value.coordinate = GhosttyPointCoordinate { x, y };
        let mut r = std::ptr::null_mut();
        (unsafe { ghostty_terminal_grid_ref_track(self.term, p, &mut r) } == GHOSTTY_SUCCESS).then_some(r)
    }

    fn tracked_at(&self, r: GhosttyTrackedGridRef) -> Option<(u16, u32)> {
        if r.is_null() {
            return None;
        }
        let mut p = GhosttyPointCoordinate::default();
        (unsafe { ghostty_tracked_grid_ref_point(r, GHOSTTY_POINT_TAG_SCREEN, &mut p) } == GHOSTTY_SUCCESS).then_some((p.x, p.y))
    }

    /// The last screen row (history and the screen together, from 0).
    fn last_row(&self) -> u32 {
        let mut sb = GhosttyTerminalScrollbar::default();
        unsafe { ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &mut sb as *mut _ as *mut c_void) };
        (sb.total.max(self.rows as u64) - 1) as u32
    }

    /// Which cells of a row show nothing (blank, or the right half of a wide character).
    fn blanks(&self, y: u32) -> Vec<bool> {
        let Some(mut r) = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, 0, y) else { return Vec::new() };
        (0..self.cols)
            .map(|x| {
                r.x = x;
                let mut cps = [0u32; 4];
                let mut len = 0usize;
                unsafe { ghostty_grid_ref_graphemes(&r, cps.as_mut_ptr(), cps.len(), &mut len) };
                len == 0 || cps[..len.min(4)].iter().all(|&c| c == ' ' as u32)
            })
            .collect()
    }

    /// A row's characters by cell (x, character; blank cells ' ', wide ones
    /// once), and whether it wraps onto the next row.
    fn row_chars(&self, y: u32) -> (Vec<(u16, char)>, bool) {
        let Some(mut r) = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, 0, y) else { return (Vec::new(), false) };
        let wraps = unsafe {
            let mut line: GhosttyRow = Default::default();
            let mut wrap = false;
            ghostty_grid_ref_row(&r, &mut line) == GHOSTTY_SUCCESS
                && ghostty_row_get(line, GHOSTTY_ROW_DATA_WRAP, &mut wrap as *mut _ as *mut c_void) == GHOSTTY_SUCCESS
                && wrap
        };
        let mut cells = Vec::with_capacity(self.cols as usize);
        for x in 0..self.cols {
            r.x = x;
            unsafe {
                let mut cell: GhosttyCell = Default::default();
                ghostty_grid_ref_cell(&r, &mut cell);
                let mut wide: GhosttyCellWide = GHOSTTY_CELL_WIDE_NARROW;
                ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void);
                if wide == GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD {
                    continue;
                }
                let mut cps = [0u32; 4];
                let mut len = 0usize;
                ghostty_grid_ref_graphemes(&r, cps.as_mut_ptr(), cps.len(), &mut len);
                let c = if len == 0 { ' ' } else { char::from_u32(cps[0]).unwrap_or(' ') };
                cells.push((x, c));
            }
        }
        (cells, wraps)
    }

    fn blank(&self, y: u32) -> bool {
        self.blanks(y).iter().all(|b| *b)
    }

    /// The last cell with text on a row (0 when it's empty).
    fn line_end(&self, y: u32) -> u16 {
        self.blanks(y).iter().rposition(|b| !b).unwrap_or(0) as u16
    }

    /// The right half of a wide character.
    fn spacer(&self, x: u16, y: u32) -> bool {
        let Some(r) = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, x, y) else { return false };
        unsafe {
            let mut cell: GhosttyCell = Default::default();
            ghostty_grid_ref_cell(&r, &mut cell);
            let mut wide: GhosttyCellWide = GHOSTTY_CELL_WIDE_NARROW;
            ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void);
            wide == GHOSTTY_CELL_WIDE_SPACER_TAIL
        }
    }

    /// Searches the screen and scrollback for a regex (case-insensitive unless
    /// it says `(?-i)`) and selects the newest match, scrolling to it. Matches
    /// don't cross lines; lines the terminal wrapped count as one.
    pub fn find_regex(&mut self, pattern: &str) -> Result<Found, String> {
        if pattern.is_empty() {
            self.find_end();
            return Ok(Found::default());
        }
        let re = fancy_regex::Regex::new(&format!("(?i){pattern}")).map_err(|e| e.to_string())?;
        if !self.search.is_null() {
            unsafe { ghostty_search_set(self.search, GHOSTTY_SEARCH_OPT_NEEDLE, std::ptr::null()) };
        }
        let hits = self.regex_hits(&re);
        let selected = (!hits.is_empty()).then_some(0);
        self.regex = Some(RegexFind { re, hits, selected, at: self.written });
        Ok(self.show_regex())
    }

    fn regex_step(&mut self, older: bool) -> Found {
        let written = self.written;
        let Some(mut rf) = self.regex.take() else { return Found::default() };
        if rf.at != written {
            rf.hits = self.regex_hits(&rf.re);
            rf.at = written;
        }
        let n = rf.hits.len();
        rf.selected = match rf.selected {
            _ if n == 0 => None,
            None => Some(0),
            Some(i) if older => Some((i + 1) % n),
            Some(i) => Some((i.min(n - 1) + n - 1) % n),
        };
        self.regex = Some(rf);
        self.show_regex()
    }

    /// Selects the regex find's selected match and scrolls to it if it's out of view.
    fn show_regex(&mut self) -> Found {
        let Some(rf) = &self.regex else { return Found::default() };
        let found = Found { selected: rf.selected, total: rf.hits.len() };
        let selects = self.copy.is_none();
        let Some(((x0, y0), (x1, y1))) = rf.selected.map(|i| rf.hits[i]) else {
            if selects {
                self.clear_selection();
            }
            return found;
        };
        let (Some(start), Some(end)) = (self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, x0, y0), self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, x1, y1)) else {
            if selects {
                self.clear_selection();
            }
            return found;
        };
        if selects {
            let sel = GhosttySelection { size: std::mem::size_of::<GhosttySelection>(), start, end, rectangle: false };
            unsafe { ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_SELECTION, &sel as *const _ as *const c_void) };
        }
        let top = self.viewport_top().unwrap_or(0);
        if y0 < top || y1 >= top + self.rows as u32 {
            // A third down, so what led up to it shows too.
            self.placements_cache.get_mut().take();
            let mut s = GhosttyTerminalScrollViewport { tag: GHOSTTY_SCROLL_VIEWPORT_ROW, ..Default::default() };
            s.value.row = y0.saturating_sub(self.rows as u32 / 3) as usize;
            unsafe { ghostty_terminal_scroll_viewport(self.term, s) };
        }
        found
    }

    /// Every match of `re`, newest first.
    fn regex_hits(&self, re: &fancy_regex::Regex) -> Vec<Hit> {
        let mut hits = Vec::new();
        for (text, spots) in self.screen_lines() {
            // The cell holding a byte of the line.
            let cell = |byte: usize| spots[spots.partition_point(|s| s.0 <= byte).saturating_sub(1)];
            for m in re.find_iter(&text) {
                let Ok(m) = m else { break };
                if m.start() == m.end() {
                    continue;
                }
                let (_, x0, y0) = cell(m.start());
                let (_, x1, y1) = cell(m.end() - 1);
                hits.push(((x0, y0), (x1, y1)));
            }
        }
        hits.reverse();
        hits
    }

    /// The screen and scrollback as lines of text (rows the terminal wrapped
    /// joined, trailing blanks dropped), with each character's (byte, x, y).
    fn screen_lines(&self) -> Vec<(String, Vec<(usize, u16, u32)>)> {
        let mut lines = Vec::new();
        let (mut text, mut spots) = (String::new(), Vec::new());
        let mut end_line = |text: &mut String, spots: &mut Vec<(usize, u16, u32)>| {
            let keep = text.trim_end().len();
            text.truncate(keep);
            spots.retain(|s: &(usize, u16, u32)| s.0 < keep);
            if !text.is_empty() {
                lines.push((std::mem::take(text), std::mem::take(spots)));
            }
            text.clear();
            spots.clear();
        };
        let mut y = 0u32;
        while let Some(mut r) = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, 0, y) {
            let wraps = unsafe {
                let mut line: GhosttyRow = Default::default();
                let mut wrap = false;
                ghostty_grid_ref_row(&r, &mut line) == GHOSTTY_SUCCESS
                    && ghostty_row_get(line, GHOSTTY_ROW_DATA_WRAP, &mut wrap as *mut _ as *mut c_void) == GHOSTTY_SUCCESS
                    && wrap
            };
            for x in 0..self.cols {
                r.x = x;
                let wide = unsafe {
                    let mut cell: GhosttyCell = Default::default();
                    ghostty_grid_ref_cell(&r, &mut cell);
                    let mut wide: GhosttyCellWide = GHOSTTY_CELL_WIDE_NARROW;
                    ghostty_cell_get(cell, GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void);
                    wide
                };
                if wide == GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD {
                    continue;
                }
                let byte = text.len();
                let mut cps = [0u32; 16];
                let mut len = 0usize;
                unsafe { ghostty_grid_ref_graphemes(&r, cps.as_mut_ptr(), cps.len(), &mut len) };
                text.extend(cps[..len.min(cps.len())].iter().filter_map(|c| char::from_u32(*c)));
                if text.len() == byte {
                    text.push(' ');
                }
                spots.push((byte, x, y));
            }
            if !wraps {
                end_line(&mut text, &mut spots);
            }
            y += 1;
        }
        end_line(&mut text, &mut spots);
        lines
    }

    /// Wraps pasted text the way the program asked (bracketed paste or not).
    pub fn paste_bytes(&self, text: &str) -> Vec<u8> {
        if self.mode(BRACKETED_PASTE) {
            let clean = text.replace("\x1b[201~", "");
            [b"\x1b[200~".as_slice(), clean.as_bytes(), b"\x1b[201~"].concat()
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        }
    }

    /// Whether output since the last frame changed what's on screen: a title,
    /// or a cursor parked where it was, needs no frame.
    pub fn shows_change(&mut self) -> bool {
        if self.shared.held_since.is_some() {
            return true;
        }
        unsafe {
            let state = self.shared.state;
            ghostty_render_state_update(state, self.term);
            let mut dirty: GhosttyRenderStateDirty = GHOSTTY_RENDER_STATE_DIRTY_FULL;
            ghostty_render_state_get(state, GHOSTTY_RENDER_STATE_DATA_DIRTY, &mut dirty as *mut _ as *mut c_void);
            dirty != GHOSTTY_RENDER_STATE_DIRTY_FALSE || self.drawable() != self.drawn
        }
    }

    /// The cursor and the colors (from the updated render state), and the kitty
    /// images' generation.
    unsafe fn drawable(&self) -> (CursorKey, u64, u64) {
        unsafe {
            let mut cur = GhosttyRenderStateCursor { size: std::mem::size_of::<GhosttyRenderStateCursor>(), ..Default::default() };
            ghostty_render_state_get(self.shared.state, GHOSTTY_RENDER_STATE_DATA_CURSOR, &mut cur as *mut _ as *mut c_void);
            let cursor = (cur.viewport_has_value, cur.viewport_x, cur.viewport_y, cur.visible, cur.visual_style as i32);
            let mut c = GhosttyRenderStateColors { size: std::mem::size_of::<GhosttyRenderStateColors>(), ..Default::default() };
            ghostty_render_state_get(self.shared.state, GHOSTTY_RENDER_STATE_DATA_COLORS, &mut c as *mut _ as *mut c_void);
            let mut h = std::hash::DefaultHasher::new();
            let rgb = |h: &mut std::hash::DefaultHasher, x: GhosttyColorRgb| std::hash::Hash::hash(&(x.r, x.g, x.b), h);
            for x in [c.background, c.foreground].into_iter().chain(c.palette).chain(c.cursor_has_value.then_some(c.cursor)) {
                rgb(&mut h, x);
            }
            let colors = std::hash::Hasher::finish(&h);
            let mut generation = 0u64;
            let mut g: GhosttyKittyGraphics = std::ptr::null_mut();
            if ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &mut g as *mut _ as *mut c_void) == GHOSTTY_SUCCESS && !g.is_null() {
                ghostty_kitty_graphics_get(g, GHOSTTY_KITTY_GRAPHICS_DATA_GENERATION, &mut generation as *mut _ as *mut c_void);
            }
            (cursor, generation, colors)
        }
    }

    /// Updates the render state (respecting synchronized output) and reads one frame.
    pub fn frame(&mut self) -> Frame {
        unsafe {
            if let Some(since) = self.shared.held_since
                && since.elapsed() > MAX_HOLD
            {
                let cfg = GhosttyTerminalModeConfig { mode: SYNC_OUTPUT, value: false };
                ghostty_terminal_set(self.term, GHOSTTY_TERMINAL_OPT_MODE, &cfg as *const _ as *const c_void);
                self.shared.held_since = None;
            }
            let state = self.shared.state;
            if self.shared.held_since.is_none() {
                ghostty_render_state_update(state, self.term);
            }

            let mut colors = GhosttyRenderStateColors { size: std::mem::size_of::<GhosttyRenderStateColors>(), ..Default::default() };
            ghostty_render_state_get(state, GHOSTTY_RENDER_STATE_DATA_COLORS, &mut colors as *mut _ as *mut c_void);
            let fg_default = Rgb::from(colors.foreground);
            let bg_default = Rgb::from(colors.background);

            // Rows libghostty keeps clean are taken as read last frame (see
            // "Row Identity" in its render.h); a resize or new colors (which
            // don't mark rows dirty) read them all.
            let with = (self.cols, self.drawable().2);
            let mut dirty: GhosttyRenderStateDirty = GHOSTTY_RENDER_STATE_DIRTY_FULL;
            ghostty_render_state_get(state, GHOSTTY_RENDER_STATE_DATA_DIRTY, &mut dirty as *mut _ as *mut c_void);
            let all = dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL || with != self.seen_with;
            let mut seen = HashMap::with_capacity(self.rows as usize);
            let mut rows = Vec::with_capacity(self.rows as usize);
            ghostty_render_state_get(state, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &mut self.rows_it as *mut _ as *mut c_void);
            while ghostty_render_state_row_iterator_next(self.rows_it) {
                let mut sel = GhosttyRenderStateRowSelection { size: std::mem::size_of::<GhosttyRenderStateRowSelection>(), ..Default::default() };
                let selected = ghostty_render_state_row_get(self.rows_it, GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION, &mut sel as *mut _ as *mut c_void)
                    == GHOSTTY_SUCCESS;
                let selection = selected.then_some((sel.start_x, sel.end_x));
                let mut id = GhosttyRenderStateRowId::default();
                let known = ghostty_render_state_row_get(self.rows_it, GHOSTTY_RENDER_STATE_ROW_DATA_ID, &mut id as *mut _ as *mut c_void) == GHOSTTY_SUCCESS;
                // Everything changed: no row is kept (but each is remembered).
                let mut row_dirty = true;
                if !all {
                    ghostty_render_state_row_get(self.rows_it, GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY, &mut row_dirty as *mut _ as *mut c_void);
                }
                let same = (known && !row_dirty).then(|| self.seen.get(&id.bits)).flatten().filter(|(_, s)| *s == selection);
                let row = match same {
                    Some((row, _)) => row.clone(),
                    None => {
                        ghostty_render_state_row_get(self.rows_it, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, &mut self.cells as *mut _ as *mut c_void);
                        let mut row = Vec::with_capacity(self.cols as usize);
                        let mut x: u16 = 0;
                        while ghostty_render_state_row_cells_next(self.cells) {
                            row.push(self.read_cell(fg_default, selected && x >= sel.start_x && x <= sel.end_x));
                            x += 1;
                        }
                        Rc::new(row)
                    }
                };
                if known {
                    seen.insert(id.bits, (row.clone(), selection));
                }
                rows.push(row);
            }
            (self.seen, self.seen_with) = (seen, with);

            let mut cur = GhosttyRenderStateCursor { size: std::mem::size_of::<GhosttyRenderStateCursor>(), ..Default::default() };
            ghostty_render_state_get(state, GHOSTTY_RENDER_STATE_DATA_CURSOR, &mut cur as *mut _ as *mut c_void);
            let cursor = (cur.visible && cur.viewport_has_value).then(|| Cursor {
                x: if cur.wide_tail { cur.viewport_x.saturating_sub(1) } else { cur.viewport_x },
                y: cur.viewport_y,
                shape: match cur.visual_style {
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Bar,
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShape::Underline,
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => CursorShape::Hollow,
                    _ => CursorShape::Block,
                },
                color: if colors.cursor_has_value { colors.cursor.into() } else { fg_default },
            });

            self.drawn = self.drawable();
            ghostty_render_state_clean(state);
            let cursor_cell = cur.viewport_has_value.then_some((cur.viewport_x, cur.viewport_y));
            Frame { rows, all, cursor, cursor_cell, bg: bg_default }
        }
    }

    unsafe fn read_cell(&mut self, fg_default: Rgb, selected: bool) -> Cell {
        unsafe {
            let c = self.cells;
            let mut raw: GhosttyCell = Default::default();
            ghostty_render_state_row_cells_get(c, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &mut raw as *mut _ as *mut c_void);
            let mut wide: GhosttyCellWide = GHOSTTY_CELL_WIDE_NARROW;
            ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void);

            let mut style = GhosttyStyle { size: std::mem::size_of::<GhosttyStyle>(), ..Default::default() };
            ghostty_render_state_row_cells_get(c, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &mut style as *mut _ as *mut c_void);

            let mut fg = GhosttyColorRgb::default();
            let fg = if ghostty_render_state_row_cells_get(c, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR, &mut fg as *mut _ as *mut c_void)
                == GHOSTTY_SUCCESS
            {
                Rgb::from(fg)
            } else {
                fg_default
            };
            let mut bg = GhosttyColorRgb::default();
            let bg = (ghostty_render_state_row_cells_get(c, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR, &mut bg as *mut _ as *mut c_void)
                == GHOSTTY_SUCCESS)
                .then(|| Rgb::from(bg));

            let (mut fg, mut bg) = (fg, bg);
            if style.inverse {
                let new_fg = bg.unwrap_or(self.theme.bg);
                bg = Some(fg);
                fg = new_fg;
            }
            if selected {
                bg = Some(self.theme.selection_bg);
            }
            let mut text = String::new();
            if wide != GHOSTTY_CELL_WIDE_SPACER_TAIL && wide != GHOSTTY_CELL_WIDE_SPACER_HEAD && !style.invisible {
                let mut len: u32 = 0;
                ghostty_render_state_row_cells_get(c, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, &mut len as *mut _ as *mut c_void);
                if len > 0 {
                    let mut cps = vec![0u32; len as usize];
                    ghostty_render_state_row_cells_get(c, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF, cps.as_mut_ptr() as *mut c_void);
                    text = cps.into_iter().filter_map(char::from_u32).collect();
                }
            }

            if text.starts_with('\u{10eeee}') {
                text.clear();
                style.underline = 0;
                style.strikethrough = false;
            }

            // Contrast adjustment is opt-in; graphics always keep their colors.
            if self.theme.minimum_contrast > 1.0 && !preserves_graphics_color(&text) {
                fg = readable(fg, bg.unwrap_or(self.theme.bg), self.theme.minimum_contrast);
            }

            Cell {
                text,
                fg,
                bg,
                bold: style.bold,
                italic: style.italic,
                faint: style.faint,
                underline: style.underline != 0,
                strike: style.strikethrough,
                wide: wide == GHOSTTY_CELL_WIDE_WIDE,
            }
        }
    }

    /// Encodes one key press with the program's current keyboard modes.
    /// `consumed`: the modifiers that went into making `text` (Shift for ":",
    /// Option for "\u{df}" on a Mac), as Ghostty reports them. Without it the
    /// kitty keyboard protocol reports shift+; and Neovim never sees ":".
    pub fn encode_key(&mut self, key: GhosttyKey, mods: GhosttyMods, consumed: GhosttyMods, text: Option<&str>, unshifted: Option<char>) -> Vec<u8> {
        unsafe {
            ghostty_key_encoder_setopt_from_terminal(self.keys, self.term);
            #[cfg(target_os = "macos")]
            {
                let option = if gpui_macos::option_as_alt() { GHOSTTY_OPTION_AS_ALT_TRUE } else { GHOSTTY_OPTION_AS_ALT_FALSE };
                ghostty_key_encoder_setopt(self.keys, GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT, &option as *const _ as *const c_void);
            }
            let e = self.key_event;
            ghostty_key_event_set_action(e, GHOSTTY_KEY_ACTION_PRESS);
            ghostty_key_event_set_key(e, key);
            ghostty_key_event_set_mods(e, mods);
            let printable = text.is_some_and(|t| !t.is_empty() && !t.chars().any(|c| c.is_control()));
            ghostty_key_event_set_consumed_mods(e, if printable { consumed } else { 0 });
            match text {
                Some(t) if printable => {
                    ghostty_key_event_set_utf8(e, t.as_ptr() as *const _, t.len())
                }
                _ => ghostty_key_event_set_utf8(e, std::ptr::null(), 0),
            }
            ghostty_key_event_set_unshifted_codepoint(e, unshifted.map_or(0, |c| c as u32));
            let mut buf = [0u8; 128];
            let mut len = 0usize;
            let r = ghostty_key_encoder_encode(self.keys, e, buf.as_mut_ptr() as *mut _, buf.len(), &mut len);
            if r != GHOSTTY_SUCCESS {
                return Vec::new();
            }
            buf[..len].to_vec()
        }
    }

    /// Encodes a mouse event for a program that tracks the mouse. Positions are in pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_mouse(
        &mut self,
        action: GhosttyMouseAction,
        button: Option<GhosttyMouseButton>,
        mods: GhosttyMods,
        x: f32,
        y: f32,
        // (columns, rows, cell width, cell height), cells in px.
        size: (u32, u32, f32, f32),
        any_pressed: bool,
    ) -> Vec<u8> {
        // Ghostty takes whole-pixel cells; at zoom a cell is 11.7 px, and
        // rounding it alone made the reported column run ahead of the pointer
        // (2 cells by mid-line). So the position moves into a grid of whole
        // cells: same column and row exactly, pixels (SGR-pixels mode) within
        // a few percent.
        let (cols, rows, cw, ch) = size;
        let (cwi, chi) = (cw.round().max(1.0), ch.round().max(1.0));
        let (x, y) = (x / cw.max(1.0) * cwi, y / ch.max(1.0) * chi);
        unsafe {
            ghostty_mouse_encoder_setopt_from_terminal(self.mouse, self.term);
            let s = GhosttyMouseEncoderSize {
                size: std::mem::size_of::<GhosttyMouseEncoderSize>(),
                screen_width: cols * cwi as u32,
                screen_height: rows * chi as u32,
                cell_width: cwi as u32,
                cell_height: chi as u32,
                ..Default::default()
            };
            ghostty_mouse_encoder_setopt(self.mouse, GHOSTTY_MOUSE_ENCODER_OPT_SIZE, &s as *const _ as *const c_void);
            ghostty_mouse_encoder_setopt(self.mouse, GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED, &any_pressed as *const _ as *const c_void);
            let e = self.mouse_event;
            ghostty_mouse_event_set_action(e, action);
            match button {
                Some(b) => ghostty_mouse_event_set_button(e, b),
                None => ghostty_mouse_event_clear_button(e),
            }
            ghostty_mouse_event_set_mods(e, mods);
            ghostty_mouse_event_set_position(e, GhosttyMousePosition { x, y });
            let mut buf = [0u8; 64];
            let mut len = 0usize;
            let r = ghostty_mouse_encoder_encode(self.mouse, e, buf.as_mut_ptr() as *mut _, buf.len(), &mut len);
            if r != GHOSTTY_SUCCESS || len == 0 {
                return Vec::new();
            }
            // Refreshing the encoder's terminal/size options resets its cell tracking.
            if action == GHOSTTY_MOUSE_ACTION_MOTION && self.last_mouse_report == buf[..len] {
                return Vec::new();
            }
            self.last_mouse_report.clear();
            self.last_mouse_report.extend_from_slice(&buf[..len]);
            buf[..len].to_vec()
        }
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        self.copy_end();
        unsafe {
            if !self.search.is_null() {
                ghostty_search_free(self.search);
            }
            ghostty_selection_gesture_event_free(self.press);
            ghostty_selection_gesture_event_free(self.drag);
            ghostty_selection_gesture_event_free(self.release);
            ghostty_selection_gesture_free(self.gesture, self.term);
            ghostty_mouse_event_free(self.mouse_event);
            ghostty_mouse_encoder_free(self.mouse);
            ghostty_key_event_free(self.key_event);
            ghostty_key_encoder_free(self.keys);
            ghostty_render_state_row_cells_free(self.cells);
            ghostty_render_state_row_iterator_free(self.rows_it);
            ghostty_terminal_free(self.term);
            ghostty_render_state_free(self.shared.state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_retains_five_thousand_styled_lines() {
        for cols in [80, 200] {
            let mut vt = Vt::new(cols, 25);
            for line in 1..=5000 {
                vt.write(format!("\x1b[1;31m{line:04}\x1b[0m {}\r\n", "x".repeat(cols as usize - 6)).as_bytes());
            }
            vt.scroll_viewport(-10_000);
            for first in (1..=5000).step_by(25) {
                let frame = vt.frame();
                for (offset, row) in frame.rows.iter().enumerate() {
                    let text: String = row.iter().map(|c| c.text.as_str()).collect();
                    assert!(text.starts_with(&format!("{:04} ", first + offset)), "{cols} columns: {text:?}");
                }
                vt.scroll_viewport(25);
            }
        }
    }

    #[test]
    fn find_scrolls_back_to_matches_and_selects_them() {
        let mut vt = Vt::new(40, 5);
        for line in 1..=200 {
            let word = if line == 20 || line == 150 { "Needle" } else { "hay" };
            vt.write(format!("{line:03} {word}\r\n").as_bytes());
        }
        let row0 = |vt: &mut Vt| vt.frame().rows[0].iter().map(|c| c.text.as_str()).collect::<String>();
        // Newest first, any ASCII case, scrolled into view.
        let found = vt.find("needle");
        assert_eq!(found, Found { selected: Some(0), total: 2 });
        assert_eq!(vt.selection_text().as_deref(), Some("Needle"));
        // The selected match's cells, in viewport rows; the other one is out of view.
        let (current, others) = vt.find_spans();
        assert_eq!(current.len(), 1);
        let (y, x0, x1) = current[0];
        let row: String = vt.frame().rows[y as usize].iter().map(|c| c.text.as_str()).collect();
        assert_eq!((x0, x1, &row[..10]), (4, 9, "150 Needle"));
        assert!(others.is_empty());
        assert!(vt.frame().rows.iter().any(|r| r.iter().map(|c| c.text.as_str()).collect::<String>().starts_with("150 Needle")));
        assert_eq!(vt.find_step(true), Found { selected: Some(1), total: 2 });
        assert!(vt.frame().rows.iter().any(|r| r.iter().map(|c| c.text.as_str()).collect::<String>().starts_with("020 Needle")), "{}", row0(&mut vt));
        // Wraps past the oldest, and back.
        assert_eq!(vt.find_step(true).selected, Some(0));
        assert_eq!(vt.find_step(false).selected, Some(1));
        // Output since keeps counting.
        vt.write(b"201 needle\r\n");
        assert_eq!(vt.found().total, 3);
        assert_eq!(vt.find("nothing here"), Found { selected: None, total: 0 });
        assert_eq!(vt.selection_text(), None);
        vt.find_end();
        assert_eq!(vt.found(), Found { selected: None, total: 0 });
    }

    #[test]
    fn regex_find_scrolls_back_to_matches_across_wrapped_rows() {
        let mut vt = Vt::new(20, 5);
        for line in 1..=100 {
            match line {
                10 => vt.write(b"error: E0308 mismatched\r\n"),
                // 30 columns: wraps onto a second row, the match straddles it.
                50 => vt.write(format!("{}error: E0599 here\r\n", "x".repeat(10)).as_bytes()),
                _ => vt.write(format!("{line:03} fine\r\n").as_bytes()),
            }
        }
        let shown = |vt: &mut Vt, needle: &str| vt.frame().rows.iter().any(|r| r.iter().map(|c| c.text.as_str()).collect::<String>().contains(needle));
        let found = vt.find_regex(r"ERROR: E0\d{3}").unwrap();
        assert_eq!(found, Found { selected: Some(0), total: 2 });
        assert_eq!(vt.selection_text().as_deref(), Some("error: E0599"));
        assert!(shown(&mut vt, "error: E05") && shown(&mut vt, "99 here"));
        assert_eq!(vt.find_step(true), Found { selected: Some(1), total: 2 });
        assert_eq!(vt.selection_text().as_deref(), Some("error: E0308"));
        assert!(shown(&mut vt, "error: E0308"));
        assert_eq!(vt.find_step(true).selected, Some(0));
        assert_eq!(vt.find_step(false).selected, Some(1));
        // (?-i) asks for case; new output is found on the next step.
        assert_eq!(vt.find_regex(r"(?-i)ERROR").unwrap().total, 0);
        vt.find_regex(r"E0\d+").unwrap();
        vt.write(b"error: E0001\r\n");
        assert_eq!(vt.find_step(true).total, 3);
        // Two in view: the selected one, and the rest.
        vt.find_regex(r"\b\d{3} fine").unwrap();
        let (current, others) = vt.find_spans();
        assert_eq!(current.len(), 1);
        assert!(!others.is_empty() && others.iter().all(|&(y, x0, x1)| y < 5 && (x0, x1) == (0, 7)));
        assert!(vt.find_regex("(unclosed").is_err());
        // Back to plain text drops the regex.
        assert_eq!(vt.find("e0308").total, 1);
        vt.find_end();
        assert_eq!(vt.found(), Found::default());
        assert_eq!(vt.find_spans(), (Spans::new(), Spans::new()));
    }

    #[test]
    fn scrolled_up_counts_lines_above_the_bottom() {
        let mut vt = Vt::new(20, 5);
        for line in 1..=100 {
            vt.write(format!("{line:03}\r\n").as_bytes());
        }
        // 101 lines (the last empty) on a 5-row screen: 96 of history.
        assert_eq!(vt.scrolled_up(), (0, 96));
        vt.scroll_viewport(-10);
        assert_eq!(vt.scrolled_up(), (10, 96));
        vt.scroll_viewport(-10_000);
        assert_eq!(vt.scrolled_up(), (96, 96));
        let top: String = vt.frame().rows[0].iter().map(|c| c.text.as_str()).collect();
        assert!(top.starts_with("001"), "{top:?}");
        vt.scroll_to_bottom();
        assert_eq!(vt.scrolled_up(), (0, 96));
    }

    #[test]
    fn copy_cursor_moves_scrolls_and_selects() {
        let mut vt = Vt::new(20, 5);
        for line in 1..=50 {
            vt.write(format!("{line:03} line\r\n").as_bytes());
        }
        vt.write(b"$ ");
        vt.copy_start();
        // Starts on the terminal's cursor: the prompt, at the bottom (row 50 of 0..=50).
        assert_eq!(vt.copy_cursor(), Some((2, 50)));
        vt.copy_move(Motion::LineStart);
        vt.copy_move(Motion::Up(3));
        assert_eq!(vt.copy_cursor(), Some((0, 47)));
        // $ goes to the last character, and j/k keep to line ends after it.
        vt.copy_move(Motion::LineEnd);
        assert_eq!(vt.copy_cursor(), Some((7, 47)));
        vt.copy_move(Motion::Down(3));
        assert_eq!(vt.copy_cursor(), Some((0, 50)), "the prompt line ends at its $");
        vt.copy_move(Motion::Up(1));
        assert_eq!(vt.copy_cursor(), Some((7, 49)));
        // l stops at the line's end; h at the start.
        vt.copy_move(Motion::Right);
        assert_eq!(vt.copy_cursor(), Some((7, 49)));
        for _ in 0..10 {
            vt.copy_move(Motion::Left);
        }
        assert_eq!(vt.copy_cursor(), Some((0, 49)));
        vt.copy_move(Motion::FirstNonBlank);
        assert_eq!(vt.copy_cursor(), Some((0, 49)));
        // Moving above the view scrolls it.
        vt.copy_move(Motion::Top);
        assert_eq!(vt.copy_cursor(), Some((0, 0)));
        assert_eq!(vt.copy_cursor_view(), Some((0, 0)));
        assert!(vt.frame().rows[0].iter().map(|c| c.text.as_str()).collect::<String>().starts_with("001"));
        vt.copy_move(Motion::ViewBottom(0));
        assert_eq!(vt.copy_cursor(), Some((0, 4)));
        // 20G: line 20, counted from 1 at the top (row 19); past the end, the last line.
        vt.copy_move(Motion::Row(19));
        assert_eq!(vt.copy_cursor(), Some((0, 19)));
        vt.copy_move(Motion::Row(9_999));
        assert_eq!(vt.copy_cursor(), Some((0, 50)));
        vt.copy_move(Motion::Top);
        vt.copy_move(Motion::ViewBottom(0));
        // 2H, 2L: the second line from the top or the bottom of the view.
        vt.copy_move(Motion::ViewTop(1));
        assert_eq!(vt.copy_cursor(), Some((0, 1)));
        vt.copy_move(Motion::ViewBottom(1));
        assert_eq!(vt.copy_cursor(), Some((0, 3)));
        vt.copy_move(Motion::ViewTop(99));
        assert_eq!(vt.copy_cursor(), Some((0, 4)), "no further than the view");
        vt.copy_move(Motion::Bottom);
        assert_eq!(vt.copy_cursor_view(), Some((0, 4)));
        // Ctrl+U carries the cursor; Ctrl+Y keeps it, in view.
        vt.copy_scroll(-2, true);
        assert_eq!(vt.copy_cursor(), Some((0, 48)));
        vt.copy_scroll(-10, false);
        assert_eq!(vt.copy_cursor_view().map(|c| c.1), Some(4));

        // v: characters in reading order, backwards too.
        vt.copy_move(Motion::Top);
        vt.copy_move(Motion::Down(1));
        vt.copy_move(Motion::Right);
        vt.copy_select(Visual::Char);
        vt.copy_move(Motion::Up(1));
        assert_eq!(vt.selection_text().as_deref(), Some("01 line\n00"));
        // V: whole lines; Ctrl+V: a block.
        vt.copy_select(Visual::Line);
        vt.copy_move(Motion::Down(2));
        assert_eq!(vt.copy_visual(), Some(Visual::Line));
        assert_eq!(vt.selection_text().as_deref(), Some("002 line\n003 line"));
        vt.copy_select(Visual::Block);
        vt.copy_move(Motion::Right);
        assert_eq!(vt.selection_text().as_deref(), Some("02\n03"));
        // The same kind again drops it.
        vt.copy_select(Visual::Block);
        assert_eq!((vt.copy_visual(), vt.selection_text()), (None, None));

        // The cursor and the selection stay on their text as output comes in.
        vt.copy_move(Motion::Top);
        vt.copy_select(Visual::Line);
        for line in 51..=60 {
            vt.write(format!("{line:03} line\r\n").as_bytes());
        }
        assert_eq!(vt.copy_cursor(), Some((0, 0)));
        assert_eq!(vt.selection_text().as_deref(), Some("001 line"));
        vt.copy_end();
        assert_eq!((vt.copy_cursor(), vt.selection_text()), (None, None));
    }

    #[test]
    fn copy_mode_searches_and_paragraphs() {
        let mut vt = Vt::new(20, 5);
        // Paragraphs of three lines with a blank line after each; "mark" on 1, 5, 9 ...
        for p in 0..10 {
            for l in 0..3 {
                let word = if l == 1 { "mark" } else { "text" };
                vt.write(format!("{p}{l} {word}\r\n").as_bytes());
            }
            vt.write(b"\r\n");
        }
        // Rows: paragraph p is rows 4p..4p+2, its blank line 4p+3; the prompt row 40.
        vt.copy_start();
        assert_eq!(vt.copy_cursor(), Some((0, 40)));
        vt.copy_move(Motion::ParagraphUp);
        assert_eq!(vt.copy_cursor(), Some((0, 35)), "the blank line above the last paragraph");
        vt.copy_move(Motion::ParagraphUp);
        assert_eq!(vt.copy_cursor(), Some((0, 31)));
        vt.copy_move(Motion::ParagraphDown);
        assert_eq!(vt.copy_cursor(), Some((0, 35)));
        vt.copy_move(Motion::Top);
        vt.copy_move(Motion::ParagraphUp);
        assert_eq!(vt.copy_cursor(), Some((0, 0)), "stops at the top");

        // / from the top: the next match down; n keeps going, N goes back.
        vt.find("mark");
        assert!(vt.copy_jump(false, true));
        assert_eq!(vt.copy_cursor(), Some((3, 1)));
        assert!(vt.copy_jump(false, true));
        assert_eq!(vt.copy_cursor(), Some((3, 5)));
        assert!(vt.copy_jump(true, true));
        assert_eq!(vt.copy_cursor(), Some((3, 1)));
        // ? from the first one wraps to the last.
        assert!(vt.copy_jump(true, true));
        assert_eq!(vt.copy_cursor(), Some((3, 37)));
        assert_eq!(vt.copy_cursor_view(), Some((3, 4)), "scrolled to it");
        // The match it went to is the current highlight.
        let (current, _) = vt.find_spans();
        assert_eq!(current, vec![(4, 3, 6)]);
        // Typing only shows the match: the cursor stays.
        vt.copy_move(Motion::Top);
        assert!(vt.copy_jump(false, false));
        assert_eq!(vt.copy_cursor(), Some((0, 0)));

        // Regex too, and a visual selection survives searching.
        vt.copy_select(Visual::Line);
        vt.find_regex(r"^5\d").unwrap();
        assert_eq!(vt.selection_text().as_deref(), Some("00 text"), "find doesn't take the selection");
        assert!(vt.copy_jump(false, true));
        assert_eq!(vt.copy_cursor(), Some((0, 20)));
        assert!(vt.selection_text().unwrap().starts_with("00 text\n01 mark"));
        assert!(vt.selection_text().unwrap().ends_with("50 text"));
        vt.find_regex("nothing").unwrap();
        assert!(!vt.copy_jump(false, true));
        assert_eq!(vt.copy_cursor(), Some((0, 20)));
        vt.copy_end();
    }

    #[test]
    fn copy_mode_word_and_character_moves() {
        let mut vt = Vt::new(30, 5);
        vt.write(b"foo.bar(baz) qux\r\n");
        vt.write(b"\r\n");
        vt.write(b"  next-line end\r\n");
        // 34 columns on 30: wraps; "wrapped" straddles the rows.
        vt.write(b"aaaaaaaaaaaaaaaaaaaaaaaaa wrapped tail\r\n");
        vt.copy_start();
        vt.copy_move(Motion::Top);
        let at = |vt: &Vt| vt.copy_cursor().unwrap();
        let w = |vt: &mut Vt, m: Motion, n: usize| {
            for _ in 0..n {
                vt.copy_move(m);
            }
            vt.copy_cursor().unwrap()
        };
        // w: words and punctuation apart; W: blanks only. Past the blank line.
        assert_eq!(w(&mut vt, Motion::WordNext { big: false }, 1), (3, 0), "the .");
        assert_eq!(w(&mut vt, Motion::WordNext { big: false }, 1), (4, 0), "bar");
        assert_eq!(w(&mut vt, Motion::WordNext { big: false }, 4), (13, 0), "qux, past ( baz )");
        assert_eq!(w(&mut vt, Motion::WordNext { big: false }, 1), (2, 2), "next, past the blank line and indent");
        vt.copy_move(Motion::Top);
        assert_eq!(w(&mut vt, Motion::WordNext { big: true }, 1), (13, 0));
        assert_eq!(w(&mut vt, Motion::WordNext { big: true }, 2), (12, 2), "end");
        // e and E: word ends; b and B back.
        assert_eq!(w(&mut vt, Motion::WordEnd { big: false }, 1), (14, 2));
        vt.copy_move(Motion::Top);
        assert_eq!(w(&mut vt, Motion::WordEnd { big: false }, 1), (2, 0), "foo");
        assert_eq!(w(&mut vt, Motion::WordEnd { big: false }, 1), (3, 0), "the .");
        assert_eq!(w(&mut vt, Motion::WordEnd { big: true }, 1), (11, 0), "foo.bar(baz)");
        assert_eq!(w(&mut vt, Motion::WordPrev { big: false }, 1), (8, 0), "baz");
        assert_eq!(w(&mut vt, Motion::WordPrev { big: true }, 1), (0, 0));
        // A word the terminal wrapped is one word.
        vt.copy_move(Motion::Down(3));
        assert_eq!(at(&vt), (0, 3));
        assert_eq!(w(&mut vt, Motion::WordNext { big: false }, 1), (26, 3), "wrapped");
        assert_eq!(w(&mut vt, Motion::WordEnd { big: false }, 1), (2, 4), "its end on the next row");
        assert_eq!(w(&mut vt, Motion::WordPrev { big: false }, 1), (26, 3));

        // f/F/t/T on the line; t again (;) goes past the character beside it.
        vt.copy_move(Motion::Top);
        let ch = |c, forward, till, again| Motion::Char { c, forward, till, again };
        assert_eq!(w(&mut vt, ch('b', true, false, false), 1), (4, 0));
        assert_eq!(w(&mut vt, ch('b', true, false, true), 1), (8, 0));
        assert_eq!(w(&mut vt, ch('o', false, false, false), 1), (2, 0));
        vt.copy_move(Motion::LineStart);
        assert_eq!(w(&mut vt, ch('(', true, true, false), 1), (6, 0), "just before (");
        assert_eq!(w(&mut vt, ch('(', true, true, false), 1), (6, 0), "t stays when it's beside it");
        assert_eq!(w(&mut vt, ch('a', true, true, false), 1), (8, 0), "before baz's a");
        assert_eq!(w(&mut vt, ch('a', true, true, true), 1), (8, 0), "no a after it: stays");
        assert_eq!(w(&mut vt, ch('.', false, true, false), 1), (4, 0), "just after .");
        assert_eq!(w(&mut vt, ch('z', true, false, false), 1), (10, 0));
        assert_eq!(w(&mut vt, ch('#', true, false, false), 1), (10, 0), "not on the line: stays");
        vt.copy_end();
    }

    #[test]
    fn styled_text_reaches_the_frame() {
        let mut vt = Vt::new(20, 3);
        vt.write(b"hi \x1b[1;31mred\x1b[0m\r\n\xe4\xb8\xad!");
        let f = vt.frame();
        assert_eq!(f.rows.len(), 3);
        let first: String = f.rows[0].iter().map(|c| c.text.as_str()).collect();
        assert!(first.starts_with("hi red"), "{first:?}");
        assert!(f.rows[0][3].bold);
        assert_eq!(f.rows[0][3].fg, theme().palette16[1]);
        assert!(f.rows[1][0].wide);
        assert_eq!(f.rows[1][0].text, "中");
        assert_eq!(f.rows[1][1].text, "");
        assert_eq!(f.rows[1][2].text, "!");
        let c = f.cursor.unwrap();
        assert_eq!((c.x, c.y), (3, 1));
    }

    #[test]
    fn a_frame_reads_again_only_the_rows_that_changed() {
        let mut vt = Vt::new(20, 4);
        // The cursor ends on the last row: the row it leaves counts as changed.
        vt.write(b"one\r\ntwo\r\nthree\r\n");
        let before = vt.frame();
        vt.write(b"\x1b[2;1Hchanged");
        let after = vt.frame();
        let same = |y: usize| Rc::ptr_eq(&before.rows[y], &after.rows[y]);
        assert!(same(0) && same(2), "untouched rows aren't read again");
        assert!(!same(1));
        assert!(after.rows[1].iter().map(|c| c.text.as_str()).collect::<String>().starts_with("changed"));
        vt.write(b"\x1b]4;1;rgb:12/34/56\x1b\\");
        let recolored = vt.frame();
        assert!(!Rc::ptr_eq(&after.rows[0], &recolored.rows[0]), "a palette change reads every row");
    }

    #[test]
    fn resize_queues_a_report_only_when_the_program_requested_it() {
        let mut vt = Vt::new(20, 4);
        vt.resize(30, 5, 9, 18);
        assert!(vt.take_replies().is_empty(), "a shell receives no resize input");
        vt.write(b"\x1b[?2048h");
        vt.take_replies();
        vt.resize(40, 6, 9, 18);
        assert_eq!(vt.take_replies(), b"\x1b[48;6;40;108;360t");
        assert!(vt.take_replies().is_empty(), "the report is sent only once");
        vt.write(b"\x1b[?2048l");
        vt.resize(50, 7, 9, 18);
        assert!(vt.take_replies().is_empty());
    }

    #[test]
    fn text_that_would_vanish_turns_readable() {
        let (ink, white, dark_red) = (Rgb(0x25, 0x27, 0x2c), Rgb(0xff, 0xff, 0xff), Rgb(0x46, 0x1e, 0x20));
        assert_eq!(readable(ink, white, 1.6), ink);
        assert_ne!(readable(ink, dark_red, 1.6), ink);
        assert_eq!(readable(Rgb(0xfa, 0xfa, 0xfa), white, 1.6), Rgb(0x1b, 0x1b, 0x1e));
        // Dim text keeps its color.
        assert_eq!(readable(Rgb(0x7b, 0x7e, 0x87), white, 1.6), Rgb(0x7b, 0x7e, 0x87));
    }

    #[test]
    fn program_colors_are_unchanged_by_default_and_contrast_reloads_live() {
        let mut vt = Vt::new(8, 4);
        let original = theme();
        assert_eq!(original.minimum_contrast, 1.0);
        vt.write(b"\x1b[38;2;215;153;33;48;2;104;157;106m XX  \x1b[0m\r\n\x1b[38;5;178;48;5;72m XX  \x1b[0m\r\n\x1b[33;46m XX  \x1b[0m");
        let expected = [Rgb(215, 153, 33), Rgb(215, 175, 0), original.palette16[3]];
        let before = vt.frame();
        for (row, fg) in before.rows.iter().zip(expected) {
            assert_eq!(row[1].fg, fg);
        }
        vt.set_theme(Theme { minimum_contrast: 1.6, ..original.clone() });
        let adjusted = vt.frame();
        for (i, fg) in expected.iter().enumerate() {
            assert_ne!(adjusted.rows[i][1].fg, *fg);
            assert!(!Rc::ptr_eq(&before.rows[i], &adjusted.rows[i]));
        }
        vt.set_theme(original);
        let restored = vt.frame();
        for (i, fg) in expected.iter().enumerate() {
            assert_eq!(restored.rows[i][1].fg, *fg);
        }
    }

    #[test]
    fn graphics_keep_low_contrast_colors_in_frames() {
        let mut vt = Vt::new(8, 2);
        vt.set_theme(Theme { minimum_contrast: 1.6, ..theme() });
        let fg = Rgb(30, 31, 40);
        for code in (0x2500..=0x259f).chain(0xe0b0..=0xe0d7).chain(0x1fb00..=0x1fbff).chain(0x1cc00..=0x1cebf).chain([0x23fa]) {
            let ch = char::from_u32(code).unwrap();
            vt.write(format!("\x1b[H\x1b[38;2;30;31;40;48;2;32;33;42m{ch}X").as_bytes());
            let frame = vt.frame();
            assert_eq!(frame.rows[0][0].fg, fg, "{code:x}");
            let text = frame.rows[0].iter().find(|c| c.text == "X").unwrap();
            assert_ne!(text.fg, fg, "ordinary text still gets contrast adjustment");
        }
        for code in [0x24ff, 0x25a0, 0xe0af, 0xe0d8, 0x1faff, 0x1fc00, 0x1cbff, 0x1cec0] {
            assert!(!preserves_graphics_color(&char::from_u32(code).unwrap().to_string()), "{code:x}");
        }
    }

    #[test]
    fn repeated_clicks_select_words_then_lines() {
        let mut vt = Vt::new(30, 3);
        vt.write(b"hello world\r\nsecond line");
        for (x, expected) in [(12.0, None), (14.0, Some("hello")), (13.0, Some("hello world"))] {
            vt.select(Select::Press, 1, 0, x, 5.0, 10.0, 60.0);
            vt.select(Select::Release, 1, 0, x, 5.0, 10.0, 60.0);
            assert_eq!(vt.selection_text().as_deref(), expected);
        }
        // A nearby cell outside the pixel tolerance starts a new gesture.
        vt.select(Select::Press, 2, 0, 27.0, 5.0, 10.0, 60.0);
        vt.select(Select::Release, 2, 0, 27.0, 5.0, 10.0, 60.0);
        assert_eq!(vt.selection_text(), None);
    }

    #[test]
    fn keys_follow_the_program_modes() {
        let mut vt = Vt::new(20, 3);
        assert_eq!(vt.encode_key(GHOSTTY_KEY_A, 0, 0, Some("a"), Some('a')), b"a");
        assert_eq!(vt.encode_key(GHOSTTY_KEY_C, GHOSTTY_MODS_CTRL as u16, 0, None, Some('c')), b"\x03");
        assert_eq!(vt.encode_key(GHOSTTY_KEY_ARROW_UP, 0, 0, None, None), b"\x1b[A");
        vt.write(b"\x1b[?1h"); // application cursor keys
        assert_eq!(vt.encode_key(GHOSTTY_KEY_ARROW_UP, 0, 0, None, None), b"\x1bOA");
    }

    #[test]
    fn a_selection_shown_while_dragging_matches_the_rows_it_covers() {
        let mut vt = Vt::new(20, 6);
        vt.write(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee\r\n");
        let highlighted = |f: &Frame| f.rows.iter().map(|r| r.iter().any(|c| c.bg == Some(theme().selection_bg))).collect::<Vec<_>>();
        vt.frame();
        vt.select(Select::Press, 0, 0, 1.0, 5.0, 10.0, 20.0);
        for row in 1..5 {
            vt.select(Select::Drag, 2, row, 25.0, row as f32 * 20.0 + 5.0, 10.0, 20.0);
            vt.frame();
        }
        vt.select(Select::Drag, 2, 1, 25.0, 25.0, 10.0, 20.0);
        assert_eq!(highlighted(&vt.frame()), [true, true, false, false, false, false], "back up to row 1");
    }

    #[test]
    fn a_drag_ends_under_the_pointer_with_fractional_cells() {
        // At zoom 1.3 a cell is 9.75 px wide: the selection must end where the
        // pointer is, not run ahead of it as x grows.
        let mut vt = Vt::new(80, 2);
        let line: String = (0..80).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        vt.write(line.as_bytes());
        let cw = 9.75;
        let x = 39.0 * cw + 6.0; // past the middle of column 39
        vt.select(Select::Press, 0, 0, 1.0, 5.0, cw, 40.0);
        vt.select(Select::Drag, 39, 0, x, 5.0, cw, 40.0);
        vt.select(Select::Release, 39, 0, x, 5.0, cw, 40.0);
        assert_eq!(vt.selection_text().as_deref(), Some(&line[..40]));
    }

    #[test]
    fn a_cell_joins_the_drag_only_past_its_middle_with_fractional_cells() {
        // Just before the middle of column 39 (9.75 px cells): 39 stays out. A
        // cell width rounded down to 9 puts the pointer past "the middle" and
        // the highlight flickers between cells as the pointer moves.
        let mut vt = Vt::new(80, 2);
        let line: String = (0..80).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        vt.write(line.as_bytes());
        let cw = 9.75;
        let x = 39.0 * cw + 4.0;
        vt.select(Select::Press, 0, 0, 1.0, 5.0, cw, 40.0);
        vt.select(Select::Drag, 39, 0, x, 5.0, cw, 40.0);
        vt.select(Select::Release, 39, 0, x, 5.0, cw, 40.0);
        assert_eq!(vt.selection_text().as_deref(), Some(&line[..39]));
    }

    #[test]
    fn mouse_reports_the_cell_under_the_pointer_with_fractional_cells() {
        // Programs that take the mouse (pi, vim) get the pointer's cell. At
        // zoom 1.3 a cell is 11.7 px; rounded to 11 the column ran ahead.
        let mut vt = Vt::new(80, 2);
        vt.write(b"\x1b[?1000h\x1b[?1006h");
        let (cw, ch) = (11.7, 30.0);
        let x = 39.0 * cw + 5.0; // inside column 39 (1-based 40)
        let bytes = vt.encode_mouse(GHOSTTY_MOUSE_ACTION_PRESS, Some(GHOSTTY_MOUSE_BUTTON_LEFT), 0, x, 10.0, (80, 2, cw, ch), false);
        assert_eq!(String::from_utf8_lossy(&bytes), "\x1b[<0;40;1M");
    }

    #[test]
    fn mouse_hover_tracks_only_changed_cells_and_respects_button_mode() {
        let mut vt = Vt::new(80, 2);
        let size = (80, 2, 10.0, 20.0);
        vt.write(b"\x1b[?1003h\x1b[?1006h");
        assert_eq!(vt.encode_mouse(GHOSTTY_MOUSE_ACTION_MOTION, None, 0, 15.0, 5.0, size, false), b"\x1b[<35;2;1M");
        assert!(vt.encode_mouse(GHOSTTY_MOUSE_ACTION_MOTION, None, 0, 16.0, 5.0, size, false).is_empty());
        assert_eq!(vt.encode_mouse(GHOSTTY_MOUSE_ACTION_MOTION, None, 0, 25.0, 5.0, size, false), b"\x1b[<35;3;1M");
        vt.write(b"\x1b[?1002h");
        assert!(vt.encode_mouse(GHOSTTY_MOUSE_ACTION_MOTION, None, 0, 35.0, 5.0, size, false).is_empty());
        assert_eq!(vt.encode_mouse(GHOSTTY_MOUSE_ACTION_MOTION, Some(GHOSTTY_MOUSE_BUTTON_LEFT), 0, 35.0, 5.0, size, true), b"\x1b[<32;4;1M");
        vt.write(b"\x1b[?1003h\x1b[?1016h");
        assert_eq!(vt.encode_mouse(GHOSTTY_MOUSE_ACTION_MOTION, None, 0, 15.0, 5.0, size, false), b"\x1b[<35;15;5M");
        assert_eq!(vt.encode_mouse(GHOSTTY_MOUSE_ACTION_MOTION, None, 0, 16.0, 5.0, size, false), b"\x1b[<35;16;5M");
    }

    #[test]
    fn dragging_selects_and_copies_text() {
        let mut vt = Vt::new(20, 3);
        vt.write(b"hello world\r\nsecond");
        vt.select(Select::Press, 0, 0, 2.0, 5.0, 10.0, 60.0);
        vt.select(Select::Drag, 4, 0, 46.0, 5.0, 10.0, 60.0);
        vt.select(Select::Release, 4, 0, 46.0, 5.0, 10.0, 60.0);
        assert_eq!(vt.selection_text().as_deref(), Some("hello"));
        vt.clear_selection();
        assert_eq!(vt.selection_text(), None);
    }

    #[test]
    fn kitty_images_are_stored_placed_and_acknowledged() {
        let mut vt = Vt::new(20, 5);
        vt.resize(20, 5, 8, 16);
        // A 1x1 red PNG, transmitted and displayed, asking for a reply.
        vt.write(b"\x1b_Ga=T,f=100,i=7,q=0;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\");
        let p = vt.placements();
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!((p[0].image, p[0].col, p[0].row), (7, 0, 0));
        assert_eq!(p[0].pixel_geometry(9.75, 20.5), [0.0, 0.0, 1.0, 1.0]);
        let (w, h, bgra) = vt.image_bgra(7, p[0].source).unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(&bgra[..], &[0, 0, 255, 255]);
        let reply = String::from_utf8(vt.take_replies()).unwrap();
        assert!(reply.contains("i=7") && reply.contains("OK"), "{reply:?}");
    }

    fn virtual_image(vt: &mut Vt, id: u32, placement: u32) {
        use base64::Engine;
        let pixels = base64::engine::general_purpose::STANDARD.encode([255u8, 0, 0].repeat(32));
        vt.write(format!("\x1b_Ga=T,U=1,f=24,i={id},p={placement},s=8,v=4,c=8,r=4,q=2;{pixels}\x1b\\").as_bytes());
    }

    fn placeholder(row: usize, col: usize, high: Option<usize>) -> String {
        let marks = ['\u{305}', '\u{30d}', '\u{30e}', '\u{310}', '\u{312}', '\u{33d}', '\u{33e}', '\u{33f}'];
        format!("\u{10eeee}{}{}{}", marks[row], marks[col], high.map(|i| marks[i].to_string()).unwrap_or_default())
    }

    #[test]
    fn kitty_placeholders_resolve_slices_and_scroll_with_their_cells() {
        let mut vt = Vt::new(20, 6);
        vt.resize(20, 6, 8, 8);
        virtual_image(&mut vt, 7, 0);
        vt.write(b"\x1b[38;5;7m");
        for row in 0..4 {
            for col in 0..8 {
                vt.write(placeholder(row, col, None).as_bytes());
            }
            vt.write(b"\r\n");
        }
        let frame = vt.frame();
        assert!(frame.rows.iter().flat_map(|r| r.iter()).all(|c| c.text.is_empty()));
        let p = vt.placements();
        assert_eq!(p.len(), 4, "{p:?}");
        for (row, p) in p.iter().enumerate() {
            assert_eq!((p.image, p.col, p.row, p.width, p.height), (7, 0, row as i32, 64, 8));
            assert_eq!(p.source, [0, row as u32, 8, 1]);
            assert_eq!(p.pixel_geometry(9.75, 20.5), [0.0, row as f32 * 20.5, 78.0, 20.5]);
        }
        vt.write(b"\x1b[6;1H\n\n");
        vt.frame();
        let p = vt.placements();
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!((p[0].row, p[0].source), (0, [0, 2, 8, 1]));
        assert_eq!((p[1].row, p[1].source), (1, [0, 3, 8, 1]));
    }

    #[test]
    fn kitty_placeholders_support_partial_runs_and_omitted_diacritics() {
        let mut vt = Vt::new(20, 6);
        vt.resize(20, 6, 8, 8);
        virtual_image(&mut vt, 7, 0);
        vt.write(b"\x1b[38;5;7m");
        vt.write(format!("{}\u{10eeee}  {}", placeholder(2, 3, None), placeholder(2, 7, None)).as_bytes());
        let p = vt.placements();
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!((p[0].col, p[0].width, p[0].source), (0, 16, [3, 2, 2, 1]));
        assert_eq!((p[1].col, p[1].width, p[1].source), (4, 8, [7, 2, 1, 1]));
    }

    #[test]
    fn kitty_placeholders_decode_ids_before_theme_and_contrast() {
        let mut vt = Vt::new(20, 6);
        vt.resize(20, 6, 8, 8);
        virtual_image(&mut vt, 0x01020304, 0x050607);
        virtual_image(&mut vt, 263, 0);
        let mut theme = vt.theme.clone();
        theme.minimum_contrast = 21.0;
        theme.palette16[7] = Rgb(255, 0, 0);
        vt.set_theme(theme);
        vt.write(b"\x1b[38;2;2;3;4;58;2;5;6;7m");
        vt.write(placeholder(1, 2, Some(1)).as_bytes());
        vt.write(b"\x1b[59;38;2;0;1;7m");
        vt.write(placeholder(3, 4, None).as_bytes());
        vt.frame();
        let p = vt.placements();
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!((p[0].image, p[0].source), (0x01020304, [2, 1, 1, 1]));
        assert_eq!((p[1].image, p[1].source), (263, [4, 3, 1, 1]));
    }

    #[test]
    fn kitty_placeholders_without_images_never_draw_text_or_decorations() {
        let mut vt = Vt::new(20, 6);
        vt.write("\x1b[4;9m\u{10eeee}\u{305}\u{30d}".as_bytes());
        let frame = vt.frame();
        let c = &frame.rows[0][0];
        assert!(c.text.is_empty() && !c.underline && !c.strike);
        assert!(vt.placements().is_empty());
    }

    #[test]
    fn kitty_placeholder_cache_tracks_writes_resize_and_viewport() {
        let mut vt = Vt::new(20, 6);
        vt.resize(20, 6, 8, 8);
        virtual_image(&mut vt, 7, 0);
        vt.write(b"\x1b[38;5;7m");
        vt.write(placeholder(0, 0, None).as_bytes());
        let first = vt.placements();
        assert_eq!(first, vt.placements());
        assert!(vt.placements_cache.borrow().is_some());
        vt.resize(20, 6, 16, 16);
        assert!(vt.placements_cache.borrow().is_none());
        assert_eq!(vt.placements()[0].width, 16);
        vt.write(b"\x1b[6;1H\n\n");
        assert!(vt.placements().is_empty());
        vt.scroll_viewport(-2);
        assert_eq!(vt.placements().len(), 1);
        vt.scroll_to_bottom();
        assert!(vt.placements().is_empty());
        vt.scroll_viewport(-2);
        assert_eq!(vt.placements().len(), 1);
        vt.write(b"\x1b_Ga=d,d=I,i=7,q=2\x1b\\");
        assert!(vt.placements().is_empty());
    }

    #[test]
    fn kitty_placeholders_repeat_enlarged_source_pixels_without_holes() {
        let mut vt = Vt::new(20, 6);
        vt.resize(20, 6, 8, 8);
        vt.write(b"\x1b_Ga=T,U=1,f=24,i=7,s=1,v=1,c=4,r=4,q=2;/wAA\x1b\\\x1b[38;5;7m");
        for row in 0..4 {
            for col in 0..4 {
                vt.write(placeholder(row, col, None).as_bytes());
            }
            vt.write(b"\r\n");
        }
        let placements = vt.placements();
        assert_eq!(placements.len(), 4);
        for p in placements {
            assert_eq!(p.source, [0, 0, 1, 1]);
            assert_eq!((p.width, p.height), (32, 8));
        }
    }

    #[test]
    fn kitty_placeholders_use_the_fixed_layer_below_nonnegative_direct_images() {
        let mut vt = Vt::new(20, 6);
        vt.resize(20, 6, 8, 8);
        virtual_image(&mut vt, 7, 1);
        vt.write(b"\x1b_Ga=p,U=1,i=7,p=1,c=8,r=4,z=10,q=2\x1b\\\x1b[38;5;7m");
        vt.write(placeholder(0, 0, None).as_bytes());
        vt.write(b"\x1b[H\x1b_Ga=T,f=24,i=8,s=1,v=1,z=1,q=2;/wAA\x1b\\");
        let placements = vt.placements();
        assert_eq!(placements.len(), 2);
        assert_eq!((placements[0].image, placements[0].z), (7, -1));
        assert_eq!((placements[1].image, placements[1].z), (8, 1));
    }

    #[test]
    fn kitty_placeholder_diacritics_match_the_vendored_table() {
        assert_eq!(kitty_placeholder_diacritic_index(0x305), Some(0));
        assert_eq!(kitty_placeholder_diacritic_index(0x1d244), Some(296));
        assert_eq!(kitty_placeholder_diacritic_index(0x301), None);
    }

    #[test]
    fn kitty_placeholders_center_images_without_stretching() {
        let mut vt = Vt::new(20, 6);
        vt.resize(20, 6, 8, 20);
        virtual_image(&mut vt, 7, 0);
        vt.write(b"\x1b[38;5;7m");
        vt.write(placeholder(1, 0, None).as_bytes());
        vt.write(placeholder(1, 1, None).as_bytes());
        let p = vt.placements();
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].source, [0, 0, 2, 2]);
        assert_eq!((p[0].width, p[0].height, p[0].offset), (16, 16, [0, 4]));
    }

    #[test]
    fn links_follow_wrapped_lines() {
        let mut vt = Vt::new(20, 4);
        vt.write(b"go https://example.com/a/long/path ok");
        let hit = vt.link_at(5, 1, crate::links::find).unwrap();
        assert_eq!(hit.target, "https://example.com/a/long/path");
        assert_eq!(hit.spans, vec![(0, 3, 19), (1, 0, 13)]);
        assert_eq!(vt.link_at(1, 0, crate::links::find), None);
        assert_eq!(vt.link_at(16, 1, crate::links::find), None);
    }

    #[test]
    fn links_wrapped_out_of_view_keep_their_whole_text() {
        let mut vt = Vt::new(10, 2);
        vt.write(b"https://example.com/abcdefgh");
        // Three rows of link on a two-row screen: the first scrolled away.
        let hit = vt.link_at(2, 0, crate::links::find).unwrap();
        assert_eq!(hit.target, "https://example.com/abcdefgh");
        assert_eq!(hit.spans, vec![(0, 0, 9), (1, 0, 7)]);
    }

    #[test]
    fn marked_links_open_their_uri() {
        let mut vt = Vt::new(30, 2);
        vt.write(b"see \x1b]8;;https://herdr.dev\x1b\\the docs\x1b]8;;\x1b\\ now");
        let hit = vt.link_at(6, 0, crate::links::find).unwrap();
        assert_eq!(hit.target, "https://herdr.dev");
        assert_eq!(hit.spans, vec![(0, 4, 11)]);
        assert_eq!(vt.link_at(13, 0, crate::links::find), None);
    }

    #[test]
    fn osc52_writes_are_decoded_and_bounded() {
        use base64::Engine;
        let mut vt = Vt::new(80, 24);
        vt.write(b"\x1b]52;c;aGVs");
        assert_eq!(vt.take_clipboard(), [None, None]);
        vt.write(b"bG8=\x07");
        assert_eq!(vt.take_clipboard(), [Some("hello".into()), None]);
        vt.write(b"\x1b]52;c;8J+YgA==\x1b\\");
        assert_eq!(vt.take_clipboard(), [Some("😀".into()), None]);
        for selector in b"01234567" {
            vt.write(format!("\x1b]52;{};aGk=\x07", *selector as char).as_bytes());
            assert_eq!(vt.take_clipboard(), [Some("hi".into()), None]);
        }
        vt.write(b"\x1b]52;p;aGk=\x07");
        let expected = if cfg!(target_os = "linux") { Some("hi".into()) } else { None };
        assert_eq!(vt.take_clipboard(), [None, expected]);
        for sequence in [b"\x1b]52;c;?\x07".as_slice(), b"\x1b]52;c;%%%\x07", b"\x1b]52;c;/w==\x07", b"\x1b]52;s;aGk=\x07"] {
            vt.write(sequence);
            assert_eq!(vt.take_clipboard(), [None, None]);
            assert!(vt.take_replies().is_empty());
        }
        let limit = base64::engine::general_purpose::STANDARD.encode(vec![b'x'; MAX_CLIPBOARD_BYTES]);
        vt.write(format!("\x1b]52;c;{limit}\x07").as_bytes());
        assert_eq!(vt.take_clipboard()[0].as_ref().map(String::len), Some(MAX_CLIPBOARD_BYTES));
        let huge = base64::engine::general_purpose::STANDARD.encode(vec![b'x'; MAX_CLIPBOARD_BYTES + 1]);
        vt.write(format!("\x1b]52;c;{huge}\x07").as_bytes());
        assert_eq!(vt.take_clipboard(), [None, None]);
        vt.write(b"\x1b]52;c;b2s=\x07");
        assert_eq!(vt.take_clipboard(), [Some("ok".into()), None]);
        vt.allow_clipboard_write(false);
        vt.write(b"\x1b]52;c;aGk=\x07");
        assert_eq!(vt.take_clipboard(), [None, None]);
        vt.allow_clipboard_write(true);
        vt.write(b"\x1b]52;c;YQ==\x07\x1b]52;c;Yg==\x07");
        assert_eq!(vt.take_clipboard(), [Some("b".into()), None]);
        vt.write(b"\x1b]52;c;YQ==\x07\x1b]52;c;\x07");
        assert_eq!(vt.take_clipboard(), [Some(String::new()), None]);
        vt.write(b"\x1b]52;p;\x1b\\");
        let expected = if cfg!(target_os = "linux") { Some(String::new()) } else { None };
        assert_eq!(vt.take_clipboard(), [None, expected]);
    }

    #[test]
    fn title_is_reported_once() {
        let mut vt = Vt::new(20, 3);
        vt.write(b"\x1b]2;hello\x07");
        assert_eq!(vt.take_title().as_deref(), Some("hello"));
        assert_eq!(vt.take_title(), None);
    }
}

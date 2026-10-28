//! The terminal core: libghostty-vt behind a small safe wrapper. One `Term` is
//! the grid, scrollback and modes of one session plus the callbacks that answer
//! the program (device attributes, size reports, ...). Not thread safe: the
//! owner keeps it behind a mutex, the PTY reader and the UI take turns.

#[allow(clippy::all, unsafe_op_in_unsafe_fn)]
pub mod sys;

use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant};

use sys::*;

const fn mode(value: u16) -> GhosttyMode {
    value & 0x7fff
}
const DECCKM: GhosttyMode = mode(1);
const SGR_MOUSE: GhosttyMode = mode(1006);
const ALT_SCROLL: GhosttyMode = mode(1007);
const BRACKETED_PASTE: GhosttyMode = mode(2004);
const SYNC_OUTPUT: GhosttyMode = mode(2026);

/// A synchronized-output hold longer than this is a program that never
/// released it: go back to showing live content (alacritty's bound too).
const HOLD_MAX: Duration = Duration::from_millis(150);

/// What the program asked of the terminal besides drawing.
pub enum Event {
    Bell,
    Title(String),
    Clipboard(String),
}

/// Input-relevant modes, read once per frame.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Modes {
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    /// Any mouse tracking mode is on.
    pub mouse: bool,
    pub sgr_mouse: bool,
    pub alt_screen: bool,
    pub alt_scroll: bool,
    /// Kitty keyboard protocol: disambiguate escape codes.
    pub kitty: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Idx(u8),
    Rgb([u8; 3]),
}

/// One visible cell that has something to draw.
pub struct Cell {
    pub x: u16,
    pub y: u16,
    /// ' ' when the cell has no text.
    pub c: char,
    /// Combining codepoints after `c`.
    pub extra: Option<Vec<char>>,
    pub fg: Color,
    /// Resolved background, None for the default one.
    pub bg: Option<[u8; 3]>,
    pub bold: bool,
    pub faint: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub underline: bool,
    pub strike: bool,
    pub wide: bool,
    pub selected: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Beam,
    Underline,
    HollowBlock,
}

pub struct Frame {
    pub cells: Vec<Cell>,
    /// Viewport cell of the cursor, None when it is scrolled out of view.
    pub cursor: Option<(u16, u16)>,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    /// Text under the cursor (' ' when empty).
    pub cursor_char: char,
    /// The active palette (theme plus OSC 4 changes) and default colors.
    pub palette: [[u8; 3]; 256],
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

/// The text of one grid row: each char with the column it sits in. Wide-char
/// spacers are left out, combining marks share their base char's column.
pub struct RowText {
    pub chars: Vec<char>,
    pub cols: Vec<u16>,
    /// Soft-wrapped: the line continues on the next row.
    pub wrap: bool,
}

/// A position in the whole buffer: `y` counts rows from the top of the
/// scrollback (ghostty's "screen" coordinates).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Pos {
    pub y: u32,
    pub x: u16,
}

struct Ud {
    rs: GhosttyRenderState,
    pty_out: Vec<u8>,
    events: Vec<Event>,
    hold: Option<Instant>,
    size: GhosttySizeReportSize,
    light: bool,
}

pub struct Term {
    t: GhosttyTerminal,
    rs: GhosttyRenderState,
    it: GhosttyRenderStateRowIterator,
    cells: GhosttyRenderStateRowCells,
    ud: Box<Ud>,
    compressed: Option<u64>,
}

// The handles are only touched through &mut self / &self under the owner's lock.
unsafe impl Send for Term {}

fn sized<T>() -> T {
    // Ghostty's sized structs: zeroed, first field = size of the struct.
    let mut v: T = unsafe { std::mem::zeroed() };
    unsafe { *(&mut v as *mut T as *mut usize) = std::mem::size_of::<T>() };
    v
}

fn rgb(c: GhosttyColorRgb) -> [u8; 3] {
    [c.r, c.g, c.b]
}

unsafe extern "C" fn on_write_pty(_t: GhosttyTerminal, ud: *mut c_void, data: *const u8, len: usize) {
    let ud = unsafe { &mut *(ud as *mut Ud) };
    ud.pty_out.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
}

unsafe extern "C" fn on_bell(_t: GhosttyTerminal, ud: *mut c_void) {
    unsafe { &mut *(ud as *mut Ud) }.events.push(Event::Bell);
}

unsafe extern "C" fn on_title(t: GhosttyTerminal, ud: *mut c_void) {
    let mut s = GhosttyString { ptr: ptr::null(), len: 0 };
    let title = unsafe {
        if ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_TITLE, &mut s as *mut _ as *mut c_void) == GHOSTTY_SUCCESS
            && !s.ptr.is_null()
        {
            String::from_utf8_lossy(std::slice::from_raw_parts(s.ptr, s.len)).into_owned()
        } else {
            String::new()
        }
    };
    unsafe { &mut *(ud as *mut Ud) }.events.push(Event::Title(title));
}

unsafe extern "C" fn on_size(_t: GhosttyTerminal, ud: *mut c_void, out: *mut GhosttySizeReportSize) -> bool {
    unsafe { *out = (*(ud as *mut Ud)).size };
    true
}

unsafe extern "C" fn on_color_scheme(_t: GhosttyTerminal, ud: *mut c_void, out: *mut GhosttyColorScheme) -> bool {
    let light = unsafe { (*(ud as *mut Ud)).light };
    unsafe { *out = if light { GHOSTTY_COLOR_SCHEME_LIGHT } else { GHOSTTY_COLOR_SCHEME_DARK } };
    true
}

unsafe extern "C" fn on_device_attributes(
    _t: GhosttyTerminal,
    _ud: *mut c_void,
    out: *mut GhosttyDeviceAttributes,
) -> bool {
    // VT220 with ANSI color, like Ghostty itself reports.
    let a = unsafe { &mut *out };
    a.primary.conformance_level = 62;
    a.primary.features[0] = 22;
    a.primary.num_features = 1;
    a.secondary.device_type = 1;
    a.secondary.firmware_version = 10;
    a.secondary.rom_cartridge = 0;
    a.tertiary.unit_id = 0;
    true
}

unsafe extern "C" fn on_render_hold(t: GhosttyTerminal, ud: *mut c_void, held: bool) {
    let ud = unsafe { &mut *(ud as *mut Ud) };
    if held {
        // The grid now holds the frame the program wants kept on screen.
        unsafe { ghostty_render_state_update(ud.rs, t) };
        ud.hold = Some(Instant::now());
    } else {
        ud.hold = None;
    }
}

unsafe extern "C" fn on_clipboard_write(_t: GhosttyTerminal, ud: *mut c_void, w: *const GhosttyClipboardWrite) {
    let w = unsafe { &*w };
    let contents = if w.contents.is_null() {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(w.contents, w.contents_len) }
    };
    let bytes = |s: GhosttyString| {
        if s.ptr.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(s.ptr, s.len) } }
    };
    let text = contents
        .iter()
        .find(|c| bytes(c.mime).starts_with(b"text/plain"))
        .or(contents.first())
        .map(|c| String::from_utf8_lossy(bytes(c.data)).into_owned());
    if let Some(text) = text {
        unsafe { &mut *(ud as *mut Ud) }.events.push(Event::Clipboard(text));
    }
    if let Some(reply) = w.reply {
        let r = GhosttyClipboardWriteReply {
            size: std::mem::size_of::<GhosttyClipboardWriteReply>(),
            result: GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS,
            remember: false,
        };
        unsafe { reply(w, &r) };
    }
}

impl Term {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        unsafe {
            let mut t = ptr::null_mut();
            assert_eq!(ghostty_terminal_new(ptr::null(), &mut t, cols.max(1), rows.max(1)), GHOSTTY_SUCCESS);
            let mut rs = ptr::null_mut();
            ghostty_render_state_new(ptr::null(), &mut rs);
            let mut it = ptr::null_mut();
            ghostty_render_state_row_iterator_new(ptr::null(), &mut it);
            let mut cells = ptr::null_mut();
            ghostty_render_state_row_cells_new(ptr::null(), &mut cells);
            let ud = Box::new(Ud {
                rs,
                pty_out: Vec::new(),
                events: Vec::new(),
                hold: None,
                size: GhosttySizeReportSize { rows, columns: cols, cell_width: 8, cell_height: 16 },
                light: false,
            });
            let term = Term { t, rs, it, cells, ud, compressed: None };
            let set = |opt, v: *const c_void| ghostty_terminal_set(t, opt, v);
            set(GHOSTTY_TERMINAL_OPT_USERDATA, &*term.ud as *const Ud as *const c_void);
            let write_pty: GhosttyTerminalWritePtyFn = Some(on_write_pty);
            set(GHOSTTY_TERMINAL_OPT_WRITE_PTY, write_pty.map_or(ptr::null(), |f| f as *const c_void));
            let bell: GhosttyTerminalBellFn = Some(on_bell);
            set(GHOSTTY_TERMINAL_OPT_BELL, bell.map_or(ptr::null(), |f| f as *const c_void));
            let title: GhosttyTerminalTitleChangedFn = Some(on_title);
            set(GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, title.map_or(ptr::null(), |f| f as *const c_void));
            let size: GhosttyTerminalSizeFn = Some(on_size);
            set(GHOSTTY_TERMINAL_OPT_SIZE, size.map_or(ptr::null(), |f| f as *const c_void));
            let scheme: GhosttyTerminalColorSchemeFn = Some(on_color_scheme);
            set(GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, scheme.map_or(ptr::null(), |f| f as *const c_void));
            let da: GhosttyTerminalDeviceAttributesFn = Some(on_device_attributes);
            set(GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES, da.map_or(ptr::null(), |f| f as *const c_void));
            let hold: GhosttyTerminalRenderHoldFn = Some(on_render_hold);
            set(GHOSTTY_TERMINAL_OPT_RENDER_HOLD, hold.map_or(ptr::null(), |f| f as *const c_void));
            let clip: GhosttyTerminalClipboardWriteFn = Some(on_clipboard_write);
            set(GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE, clip.map_or(ptr::null(), |f| f as *const c_void));
            // The line limit alone bounds scrollback, as it did before.
            set(GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, ptr::null());
            set(GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &scrollback as *const usize as *const c_void);
            let name = "xterm-256color";
            let s = GhosttyString { ptr: name.as_ptr(), len: name.len() };
            set(GHOSTTY_TERMINAL_OPT_TERMINFO_NAME, &s as *const _ as *const c_void);
            term
        }
    }

    /// Feed program output. Answers to queries collect in `take_pty_out`.
    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.t, bytes.as_ptr(), bytes.len()) }
    }

    pub fn take_pty_out(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.ud.pty_out)
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.ud.events)
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        unsafe { ghostty_terminal_resize(self.t, cols, rows, cell_w, cell_h) };
        self.ud.size = GhosttySizeReportSize { rows, columns: cols, cell_width: cell_w, cell_height: cell_h };
    }

    /// Default colors and palette: what unstyled text uses and what OSC 4/10/11
    /// queries report.
    pub fn set_colors(&mut self, fg: [u8; 3], bg: [u8; 3], cursor: [u8; 3], palette: &[[u8; 3]; 256], light: bool) {
        let c = |v: [u8; 3]| GhosttyColorRgb { r: v[0], g: v[1], b: v[2] };
        let pal: Vec<GhosttyColorRgb> = palette.iter().map(|&v| c(v)).collect();
        unsafe {
            ghostty_terminal_set(self.t, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &c(fg) as *const _ as *const c_void);
            ghostty_terminal_set(self.t, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &c(bg) as *const _ as *const c_void);
            ghostty_terminal_set(self.t, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &c(cursor) as *const _ as *const c_void);
            ghostty_terminal_set(self.t, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, pal.as_ptr() as *const c_void);
        }
        self.ud.light = light;
    }

    fn get<T>(&self, data: GhosttyTerminalData, out: &mut T) -> bool {
        unsafe { ghostty_terminal_get(self.t, data, out as *mut T as *mut c_void) == GHOSTTY_SUCCESS }
    }

    fn mode(&self, mode: GhosttyMode) -> bool {
        let mut c = GhosttyTerminalModeConfig { mode, value: false };
        self.get(GHOSTTY_TERMINAL_DATA_MODE, &mut c) && c.value
    }

    pub fn cols(&self) -> u16 {
        let mut v = 0u16;
        self.get(GHOSTTY_TERMINAL_DATA_COLS, &mut v);
        v
    }

    pub fn rows(&self) -> u16 {
        let mut v = 0u16;
        self.get(GHOSTTY_TERMINAL_DATA_ROWS, &mut v);
        v
    }

    pub fn modes(&self) -> Modes {
        let mut mouse = false;
        self.get(GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, &mut mouse);
        let mut screen: GhosttyTerminalScreen = 0;
        self.get(GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &mut screen);
        let mut kitty = 0u8;
        self.get(GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS, &mut kitty);
        Modes {
            app_cursor: self.mode(DECCKM),
            bracketed_paste: self.mode(BRACKETED_PASTE),
            mouse,
            sgr_mouse: self.mode(SGR_MOUSE),
            alt_screen: screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE,
            alt_scroll: self.mode(ALT_SCROLL),
            kitty: kitty & 1 != 0,
        }
    }

    fn scrollbar(&self) -> GhosttyTerminalScrollbar {
        let mut s = GhosttyTerminalScrollbar { total: 0, offset: 0, len: 0 };
        self.get(GHOSTTY_TERMINAL_DATA_SCROLLBAR, &mut s);
        s
    }

    /// Rows above the screen (scrollback).
    pub fn history(&self) -> usize {
        let s = self.scrollbar();
        s.total.saturating_sub(s.len) as usize
    }

    /// How far the view is scrolled up from the bottom, in rows.
    pub fn display_offset(&self) -> usize {
        let s = self.scrollbar();
        s.total.saturating_sub(s.offset + s.len) as usize
    }

    /// Buffer row shown at the top of the viewport.
    pub fn viewport_top(&self) -> u32 {
        self.scrollbar().offset as u32
    }

    /// All rows: scrollback plus screen.
    pub fn total_rows(&self) -> u32 {
        self.scrollbar().total as u32
    }

    fn scroll_viewport(&mut self, tag: GhosttyTerminalScrollViewportTag, value: isize) {
        let mut sv: GhosttyTerminalScrollViewport = unsafe { std::mem::zeroed() };
        sv.tag = tag;
        sv.value.delta = value;
        unsafe { ghostty_terminal_scroll_viewport(self.t, sv) };
    }

    /// Scroll the view `lines` rows up into history (negative: down).
    pub fn scroll(&mut self, lines: i32) {
        self.scroll_viewport(GHOSTTY_SCROLL_VIEWPORT_DELTA, -(lines as isize));
    }

    pub fn scroll_bottom(&mut self) {
        self.scroll_viewport(GHOSTTY_SCROLL_VIEWPORT_BOTTOM, 0);
    }

    /// Put buffer row `row` at the top of the view (clamped).
    pub fn scroll_to(&mut self, row: u32) {
        let mut sv: GhosttyTerminalScrollViewport = unsafe { std::mem::zeroed() };
        sv.tag = GHOSTTY_SCROLL_VIEWPORT_ROW;
        sv.value.row = row as usize;
        unsafe { ghostty_terminal_scroll_viewport(self.t, sv) };
    }

    /// Snapshot of the viewport to draw. While the program holds output
    /// (synchronized updates) this is the frame from before the hold, unless
    /// the view is scrolled away from the live screen.
    pub fn frame(&mut self) -> Frame {
        unsafe {
            if self.ud.hold.is_some_and(|t| t.elapsed() >= HOLD_MAX) {
                let off = GhosttyTerminalModeConfig { mode: SYNC_OUTPUT, value: false };
                ghostty_terminal_set(self.t, GHOSTTY_TERMINAL_OPT_MODE, &off as *const _ as *const c_void);
                self.ud.hold = None;
            }
            if self.ud.hold.is_none() || self.display_offset() != 0 {
                ghostty_render_state_update(self.rs, self.t);
            }

            let mut colors: GhosttyRenderStateColors = sized();
            ghostty_render_state_get(self.rs, GHOSTTY_RENDER_STATE_DATA_COLORS, &mut colors as *mut _ as *mut c_void);
            let mut cur: GhosttyRenderStateCursor = sized();
            ghostty_render_state_get(self.rs, GHOSTTY_RENDER_STATE_DATA_CURSOR, &mut cur as *mut _ as *mut c_void);
            let cursor = cur.viewport_has_value.then_some((cur.viewport_x, cur.viewport_y));
            let mut frame = Frame {
                cells: Vec::with_capacity(1024),
                cursor,
                cursor_visible: cur.visible,
                cursor_shape: match cur.visual_style {
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Beam,
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShape::Underline,
                    GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => CursorShape::HollowBlock,
                    _ => CursorShape::Block,
                },
                cursor_char: ' ',
                palette: [[0; 3]; 256],
                fg: rgb(colors.foreground),
                bg: rgb(colors.background),
            };
            for (i, c) in colors.palette.iter().enumerate() {
                frame.palette[i] = rgb(*c);
            }

            ghostty_render_state_get(
                self.rs,
                GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                &mut self.it as *mut _ as *mut c_void,
            );
            let keys = [
                GHOSTTY_CELL_DATA_CODEPOINT,
                GHOSTTY_CELL_DATA_CONTENT_TAG,
                GHOSTTY_CELL_DATA_WIDE,
                GHOSTTY_CELL_DATA_HAS_STYLING,
            ];
            let mut y = 0u16;
            let mut gbuf = [0u32; 16];
            while ghostty_render_state_row_iterator_next(self.it) {
                ghostty_render_state_row_get(
                    self.it,
                    GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    &mut self.cells as *mut _ as *mut c_void,
                );
                let mut rsel: GhosttyRenderStateRowSelection = sized();
                let sel = (ghostty_render_state_row_get(
                    self.it,
                    GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                    &mut rsel as *mut _ as *mut c_void,
                ) == GHOSTTY_SUCCESS)
                    .then_some((rsel.start_x, rsel.end_x));
                let mut x = 0u16;
                while ghostty_render_state_row_cells_next(self.cells) {
                    let mut raw: GhosttyCell = 0;
                    ghostty_render_state_row_cells_get(
                        self.cells,
                        GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
                        &mut raw as *mut _ as *mut c_void,
                    );
                    let (mut cp, mut tag, mut wide, mut styled) = (0u32, 0 as GhosttyCellContentTag, 0 as GhosttyCellWide, false);
                    let mut outs: [*mut c_void; 4] = [
                        &mut cp as *mut _ as *mut c_void,
                        &mut tag as *mut _ as *mut c_void,
                        &mut wide as *mut _ as *mut c_void,
                        &mut styled as *mut _ as *mut c_void,
                    ];
                    ghostty_cell_get_multi(raw, keys.len(), keys.as_ptr(), outs.as_mut_ptr(), ptr::null_mut());
                    let selected = sel.is_some_and(|(a, b)| (a..=b).contains(&x));
                    let spacer = wide == GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD;
                    let c = if spacer { ' ' } else { char::from_u32(cp).filter(|c| *c != '\0').unwrap_or(' ') };
                    let is_cursor = cursor == Some((x, y));
                    let has_bg = styled || tag == GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE || tag == GHOSTTY_CELL_CONTENT_BG_COLOR_RGB;
                    if c == ' ' && !has_bg && !selected {
                        x += 1;
                        continue;
                    }
                    let mut bgc: GhosttyColorRgb = std::mem::zeroed();
                    let bg = (has_bg
                        && ghostty_render_state_row_cells_get(
                            self.cells,
                            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                            &mut bgc as *mut _ as *mut c_void,
                        ) == GHOSTTY_SUCCESS)
                        .then(|| rgb(bgc));
                    let mut cell = Cell {
                        x,
                        y,
                        c,
                        extra: None,
                        fg: Color::Default,
                        bg,
                        bold: false,
                        faint: false,
                        inverse: false,
                        hidden: false,
                        underline: false,
                        strike: false,
                        wide: wide == GHOSTTY_CELL_WIDE_WIDE,
                        selected,
                    };
                    if styled {
                        let mut st: GhosttyStyle = sized();
                        ghostty_render_state_row_cells_get(
                            self.cells,
                            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                            &mut st as *mut _ as *mut c_void,
                        );
                        cell.fg = match st.fg_color.tag {
                            GHOSTTY_STYLE_COLOR_PALETTE => Color::Idx(st.fg_color.value.palette),
                            GHOSTTY_STYLE_COLOR_RGB => Color::Rgb(rgb(st.fg_color.value.rgb)),
                            _ => Color::Default,
                        };
                        cell.bold = st.bold;
                        cell.faint = st.faint;
                        cell.inverse = st.inverse;
                        cell.hidden = st.invisible;
                        cell.underline = st.underline != 0;
                        cell.strike = st.strikethrough;
                    }
                    if tag == GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME && !spacer {
                        let mut n = 0u32;
                        ghostty_render_state_row_cells_get(
                            self.cells,
                            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                            &mut n as *mut _ as *mut c_void,
                        );
                        if n > 1 && n as usize <= gbuf.len() {
                            ghostty_render_state_row_cells_get(
                                self.cells,
                                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                                gbuf.as_mut_ptr() as *mut c_void,
                            );
                            cell.extra = Some(gbuf[1..n as usize].iter().filter_map(|&u| char::from_u32(u)).collect());
                        }
                    }
                    if is_cursor {
                        frame.cursor_char = cell.c;
                    }
                    frame.cells.push(cell);
                    x += 1;
                }
                y += 1;
            }
            frame
        }
    }

    fn grid_ref(&self, tag: GhosttyPointTag, x: u16, y: u32) -> Option<GhosttyGridRef> {
        let mut pt: GhosttyPoint = unsafe { std::mem::zeroed() };
        pt.tag = tag;
        pt.value.coordinate = GhosttyPointCoordinate { x, y };
        let mut r: GhosttyGridRef = sized();
        (unsafe { ghostty_terminal_grid_ref(self.t, pt, &mut r) } == GHOSTTY_SUCCESS).then_some(r)
    }

    fn pos_of(&self, r: &GhosttyGridRef) -> Option<Pos> {
        let mut c = GhosttyPointCoordinate { x: 0, y: 0 };
        (unsafe { ghostty_terminal_point_from_grid_ref(self.t, r, GHOSTTY_POINT_TAG_SCREEN, &mut c) }
            == GHOSTTY_SUCCESS)
            .then_some(Pos { y: c.y, x: c.x })
    }

    /// Text of buffer row `y` (0 = top of the scrollback).
    pub fn row_text(&self, y: u32) -> Option<RowText> {
        let r0 = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, 0, y)?;
        let cols = self.cols();
        let mut out = RowText { chars: Vec::with_capacity(cols as usize), cols: Vec::with_capacity(cols as usize), wrap: false };
        unsafe {
            let mut row: GhosttyRow = 0;
            ghostty_grid_ref_row(&r0, &mut row);
            let mut grapheme = false;
            ghostty_row_get(row, GHOSTTY_ROW_DATA_WRAP, &mut out.wrap as *mut _ as *mut c_void);
            ghostty_row_get(row, GHOSTTY_ROW_DATA_GRAPHEME, &mut grapheme as *mut _ as *mut c_void);
            let keys = [GHOSTTY_CELL_DATA_CODEPOINT, GHOSTTY_CELL_DATA_WIDE, GHOSTTY_CELL_DATA_CONTENT_TAG];
            let mut gbuf = [0u32; 16];
            for x in 0..cols {
                // Same row, next column: the ref only differs in x.
                let r = GhosttyGridRef { x, ..r0 };
                let mut cell: GhosttyCell = 0;
                if ghostty_grid_ref_cell(&r, &mut cell) != GHOSTTY_SUCCESS {
                    break;
                }
                let (mut cp, mut wide, mut tag) = (0u32, 0 as GhosttyCellWide, 0 as GhosttyCellContentTag);
                let mut outs: [*mut c_void; 3] =
                    [&mut cp as *mut _ as *mut c_void, &mut wide as *mut _ as *mut c_void, &mut tag as *mut _ as *mut c_void];
                ghostty_cell_get_multi(cell, keys.len(), keys.as_ptr(), outs.as_mut_ptr(), ptr::null_mut());
                if wide == GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == GHOSTTY_CELL_WIDE_SPACER_HEAD {
                    continue;
                }
                out.chars.push(char::from_u32(cp).filter(|c| *c != '\0').unwrap_or(' '));
                out.cols.push(x);
                if grapheme && tag == GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME {
                    let mut n = 0usize;
                    if ghostty_grid_ref_graphemes(&r, gbuf.as_mut_ptr(), gbuf.len(), &mut n) == GHOSTTY_SUCCESS {
                        for &u in gbuf.iter().take(n).skip(1) {
                            if let Some(c) = char::from_u32(u) {
                                out.chars.push(c);
                                out.cols.push(x);
                            }
                        }
                    }
                }
            }
        }
        Some(out)
    }

    /// Lines of text from buffer row `from` to the end, soft wraps joined and
    /// trailing blanks trimmed, like a copy of the whole range would read.
    pub fn text_from(&self, from: u32) -> String {
        let mut out = String::new();
        let mut line = String::new();
        for y in from..self.total_rows() {
            let Some(r) = self.row_text(y) else { break };
            line.extend(r.chars.iter());
            if !r.wrap {
                out.push_str(line.trim_end());
                out.push('\n');
                line.clear();
            }
        }
        out.push_str(line.trim_end());
        out
    }

    // ---- Selection: the terminal owns it and keeps it in place as output
    // scrolls; these build it from buffer positions. ----

    fn select_refs(&mut self, a: GhosttyGridRef, b: GhosttyGridRef) {
        let mut sel: GhosttySelection = sized();
        sel.start = a;
        sel.end = b;
        unsafe { ghostty_terminal_set(self.t, GHOSTTY_TERMINAL_OPT_SELECTION, &sel as *const _ as *const c_void) };
    }

    /// Select from `a` to `b` inclusive (either order).
    pub fn select(&mut self, a: Pos, b: Pos) {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if let (Some(ra), Some(rb)) =
            (self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, a.x, a.y), self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, b.x, b.y))
        {
            self.select_refs(ra, rb);
        }
    }

    pub fn clear_selection(&mut self) {
        unsafe { ghostty_terminal_set(self.t, GHOSTTY_TERMINAL_OPT_SELECTION, ptr::null()) };
    }

    fn ordered_bounds(&self, sel: &GhosttySelection) -> Option<(Pos, Pos)> {
        let mut o: GhosttySelection = sized();
        unsafe { ghostty_terminal_selection_ordered(self.t, sel, GHOSTTY_SELECTION_ORDER_FORWARD, &mut o) };
        Some((self.pos_of(&o.start)?, self.pos_of(&o.end)?))
    }

    /// Bounds of the word (`line == false`) or line at `p`.
    pub fn unit_at(&self, p: Pos, line: bool) -> Option<(Pos, Pos)> {
        let r = self.grid_ref(GHOSTTY_POINT_TAG_SCREEN, p.x, p.y)?;
        let mut sel: GhosttySelection = sized();
        let res = unsafe {
            if line {
                let mut o: GhosttyTerminalSelectLineOptions = sized();
                o.ref_ = r;
                ghostty_terminal_select_line(self.t, &o, &mut sel)
            } else {
                let mut o: GhosttyTerminalSelectWordOptions = sized();
                o.ref_ = r;
                ghostty_terminal_select_word(self.t, &o, &mut sel)
            }
        };
        if res != GHOSTTY_SUCCESS {
            return None;
        }
        self.ordered_bounds(&sel)
    }

    /// Selected text, soft wraps joined, trailing blanks trimmed.
    pub fn selection_text(&self) -> Option<String> {
        let mut opts: GhosttyTerminalSelectionFormatOptions = sized();
        opts.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
        opts.unwrap = true;
        opts.trim = true;
        let (mut p, mut n) = (ptr::null_mut(), 0usize);
        unsafe {
            if ghostty_terminal_selection_format_alloc(self.t, ptr::null(), opts, &mut p, &mut n) != GHOSTTY_SUCCESS {
                return None;
            }
            let s = if p.is_null() { String::new() } else { String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned() };
            ghostty_free(ptr::null(), p, n);
            Some(s)
        }
    }

    /// Compress idle scrollback; a no-op until new output arrives again.
    pub fn compress_idle(&mut self) {
        unsafe {
            let mut token = 0u64;
            ghostty_terminal_compression_activity(self.t, &mut token);
            if self.compressed == Some(token) {
                return;
            }
            for _ in 0..256 {
                let mut r: GhosttyTerminalCompressionResult = 0;
                ghostty_terminal_compress(self.t, GHOSTTY_TERMINAL_COMPRESSION_MODE_INCREMENTAL, &mut r);
                if r != GHOSTTY_TERMINAL_COMPRESSION_RESULT_PENDING {
                    ghostty_terminal_compression_activity(self.t, &mut token);
                    self.compressed = Some(token);
                    return;
                }
            }
        }
    }

    /// Resident bytes of both screens' pages.
    #[cfg(test)]
    pub fn memory(&self) -> u64 {
        let mut m: GhosttyTerminalMemoryUsage = sized();
        self.get(GHOSTTY_TERMINAL_DATA_MEMORY_USAGE, &mut m);
        m.primary_resident_bytes + m.alternate_resident_bytes
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        unsafe {
            ghostty_render_state_row_cells_free(self.cells);
            ghostty_render_state_row_iterator_free(self.it);
            ghostty_render_state_free(self.rs);
            ghostty_terminal_free(self.t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(t: &mut Term, n: usize) {
        for i in 0..n {
            t.write(format!("line {i}\r\n").as_bytes());
        }
    }

    fn row(t: &Term, y: u32) -> String {
        t.row_text(y).map(|r| r.chars.iter().collect::<String>().trim_end().to_string()).unwrap_or_default()
    }

    #[test]
    fn scrollback_rows_and_viewport_share_coordinates() {
        let mut t = Term::new(20, 5, 1000);
        lines(&mut t, 30);
        assert_eq!(t.total_rows(), 31);
        assert_eq!(t.history(), 26);
        assert_eq!(t.display_offset(), 0);
        assert_eq!(row(&t, 0), "line 0");
        // The viewport's top row is the buffer row the frame shows first.
        let top = t.viewport_top();
        assert_eq!(row(&t, top), "line 26");
        t.scroll(3);
        assert_eq!(t.display_offset(), 3);
        let top = t.viewport_top();
        assert_eq!(row(&t, top), "line 23");
        let f = t.frame();
        let first: String = f.cells.iter().filter(|c| c.y == 0).map(|c| c.c).collect();
        assert_eq!(first, "line23");
        t.scroll_bottom();
        assert_eq!(t.display_offset(), 0);
    }

    #[test]
    fn wide_chars_and_wraps() {
        let mut t = Term::new(6, 4, 100);
        t.write("日本ab\r\nabcdefgh".as_bytes());
        let r = t.row_text(0).unwrap();
        assert_eq!(r.chars[..4], ['日', '本', 'a', 'b']);
        assert_eq!(r.cols[..4], [0, 2, 4, 5]);
        assert!(t.row_text(1).unwrap().wrap);
        assert_eq!(t.text_from(0).trim_end(), "日本ab\nabcdefgh");
    }

    #[test]
    fn selection_text_and_words() {
        let mut t = Term::new(30, 4, 100);
        t.write(b"hello brave world\r\nnext line");
        t.select(Pos { y: 0, x: 6 }, Pos { y: 0, x: 10 });
        assert_eq!(t.selection_text().as_deref(), Some("brave"));
        assert_eq!(t.frame().cells.iter().filter(|c| c.selected).count(), 5);
        let (a, b) = t.unit_at(Pos { y: 0, x: 13 }, false).unwrap();
        assert_eq!((a.x, b.x), (12, 16));
        let (a, b) = t.unit_at(Pos { y: 1, x: 2 }, true).unwrap();
        t.select(a, b);
        assert_eq!(t.selection_text().as_deref(), Some("next line"));
        // Output that scrolls keeps the selection on its text.
        t.write(b"\r\nmore\r\nmore\r\nmore\r\n");
        assert_eq!(t.selection_text().as_deref(), Some("next line"));
        t.clear_selection();
        assert_eq!(t.selection_text(), None);
    }

    #[test]
    fn answers_queries() {
        let mut t = Term::new(80, 24, 100);
        t.write(b"\x1b[c");
        assert_eq!(t.take_pty_out(), b"\x1b[?62;22c");
        t.write(b"\x1b[6n");
        t.write(b"\x1b[18t");
        let out = String::from_utf8(t.take_pty_out()).unwrap();
        assert!(out.starts_with("\x1b[1;1R") && out.contains("\x1b[8;24;80t"), "{out:?}");
        t.write(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1h\x1b[?1049h");
        let m = t.modes();
        assert!(m.bracketed_paste && m.mouse && m.sgr_mouse && m.app_cursor && m.alt_screen);
        t.write(b"\x1b]0;hi\x07\x07");
        let ev = t.take_events();
        assert!(matches!(ev.as_slice(), [Event::Title(s), Event::Bell] if s == "hi"));
    }

    #[test]
    fn colors_and_styles() {
        let mut t = Term::new(20, 3, 100);
        t.write(b"\x1b[1;31mR\x1b[0m\x1b[48;2;1;2;3mB\x1b[0m\x1b[7mI");
        let f = t.frame();
        let c = |x| f.cells.iter().find(|c| c.y == 0 && c.x == x).unwrap();
        assert!(c(0).bold && c(0).fg == Color::Idx(1));
        assert_eq!(c(1).bg, Some([1, 2, 3]));
        assert!(c(2).inverse);
        assert_eq!(f.cursor, Some((3, 0)));
    }

    #[test]
    fn held_frame_until_release() {
        let mut t = Term::new(20, 3, 100);
        t.write(b"old");
        t.write(b"\x1b[?2026h\x1b[2K\rnew");
        let text = |t: &mut Term| t.frame().cells.iter().filter(|c| c.y == 0).map(|c| c.c).collect::<String>();
        assert_eq!(text(&mut t), "old");
        t.write(b"\x1b[?2026l");
        assert_eq!(text(&mut t), "new");
    }

    #[test]
    fn idle_compression_shrinks_history() {
        let mut t = Term::new(120, 40, 5000);
        for i in 0..5000 {
            t.write(format!("{i:05} the quick brown fox jumps over the lazy dog, again and again\r\n").as_bytes());
        }
        let before = t.memory();
        t.compress_idle();
        let after = t.memory();
        assert!(after < before, "{before} -> {after}");
        assert!(t.text_from(0).starts_with("00000 the quick"));
    }
}

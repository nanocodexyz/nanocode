use std::{
    io::{self, Stdout, Write, stdout},
    panic,
    sync::{
        Once, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use crate::input::{MouseButton, MouseEvent, MouseEventKind};
use crate::ui::{
    buffer::{Buffer, Cell},
    layout::{Position, Rect},
    style::Color,
    text::Line,
    widgets::Widget,
};

use crate::selection::Selection;

pub mod ansi;
#[cfg(windows)]
#[path = "terminal/console.rs"]
mod device;
#[cfg(unix)]
mod device;

#[cfg(unix)]
pub(crate) use device::readable_now;
pub use device::{Tty, disable_raw_mode, enable_raw_mode, raw_mode_enabled, size, tty};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static MODES: AtomicU8 = AtomicU8::new(0);
static PANIC_HOOK: Once = Once::new();

const MOUSE: u8 = 1;
const KEYBOARD: u8 = 2;
const ALTERNATE: u8 = 4;

#[derive(Clone, Copy, Debug)]
pub struct TerminalOptions {
    pub mouse_capture: bool,
    pub keyboard_enhancement: bool,
    pub alternate_screen: bool,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            mouse_capture: true,
            keyboard_enhancement: true,
            alternate_screen: true,
        }
    }
}

pub struct Canvas<'a> {
    area: Rect,
    buffer: &'a mut Buffer,
    cursor: Option<Position>,
    regions: &'a mut Vec<Rect>,
    spinners: &'a mut Vec<Position>,
    scrollback: &'a mut Vec<Line<'static>>,
}

impl Canvas<'_> {
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.area
    }

    pub const fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    pub fn set_cursor_position(&mut self, position: impl Into<Position>) {
        self.cursor = Some(position.into());
    }

    pub fn selectable(&mut self, area: Rect) {
        self.regions.push(area.intersection(self.area));
    }

    pub fn spinner(&mut self, position: Position) {
        if self.area.contains(position) {
            self.spinners.push(position);
        }
    }

    pub fn scrollback(&mut self, lines: Vec<Line<'static>>) {
        self.scrollback.extend(lines);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameStats {
    pub frames: u64,
    pub skipped: u64,
    pub full_repaints: u64,
    pub bytes: u64,
}

#[derive(Debug)]
pub enum SelectionEvent {
    Ignored,
    Changed,
    Copied(String),
    Failed(io::Error),
}

pub struct TerminalSession {
    output: Stdout,
    previous: Buffer,
    current: Buffer,
    stale: bool,
    cursor: Option<Position>,
    bytes: Vec<u8>,
    truecolor: bool,
    monochrome: bool,
    stats: FrameStats,
    options: TerminalOptions,
    active: bool,
    regions: Vec<Rect>,
    spinners: Vec<Position>,
    selection: Option<Selection>,
    changed: Vec<usize>,
    scrollback: Vec<Line<'static>>,
}

impl TerminalSession {
    pub fn enter(options: TerminalOptions) -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        let mut output = stdout();
        let started = activate(&mut output, options).and_then(|()| {
            if options.alternate_screen {
                Ok(())
            } else {
                preserve(&mut output)
            }
        });
        if let Err(error) = started {
            restore(&mut output);
            return Err(error);
        }
        Ok(Self {
            output,
            previous: Buffer::empty(Rect::default()),
            current: Buffer::empty(Rect::default()),
            stale: true,
            cursor: None,
            bytes: Vec::new(),
            truecolor: truecolor(),
            monochrome: no_color(),
            stats: FrameStats::default(),
            options,
            active: true,
            regions: Vec::new(),
            spinners: Vec::new(),
            selection: None,
            changed: Vec::new(),
            scrollback: Vec::new(),
        })
    }

    #[must_use]
    pub const fn options(&self) -> TerminalOptions {
        self.options
    }

    pub fn draw(&mut self, render: impl FnOnce(&mut Canvas<'_>)) -> io::Result<()> {
        let (columns, rows) = size()?;
        let span = nanocode_trace::info_span!(
            "tui.terminal.draw",
            width = columns,
            height = rows,
            repaint = nanocode_trace::field::Empty,
            frame.skipped = false,
            frame.bytes = nanocode_trace::field::Empty,
            frame.changed_cells = nanocode_trace::field::Empty
        );
        let _entered = span.enter();
        let area = Rect::new(0, 0, columns, rows);
        if area != self.previous.area {
            self.stale = true;
            self.selection = None;
        }
        if area == self.current.area {
            self.current.reset();
        } else {
            self.current = Buffer::empty(area);
        }
        span.record("repaint", self.stale);
        self.regions.clear();
        self.regions.push(area);
        self.spinners.clear();
        let mut canvas = Canvas {
            area,
            buffer: &mut self.current,
            cursor: None,
            regions: &mut self.regions,
            spinners: &mut self.spinners,
            scrollback: &mut self.scrollback,
        };
        render(&mut canvas);
        let cursor = canvas.cursor.filter(|position| area.contains(*position));
        let mut lead = Vec::new();
        if !self.scrollback.is_empty() {
            scroll_off(
                &mut lead,
                &self.scrollback,
                area,
                self.monochrome,
                self.truecolor,
            );
            shift(&mut self.previous, self.scrollback.len());
            self.scrollback.clear();
            self.selection = None;
        }
        if let Some(selection) = &self.selection {
            selection.paint(&mut self.current);
        }
        if self.monochrome {
            monochrome(&mut self.current);
        } else if !self.truecolor {
            downsample(&mut self.current);
        }
        let diff_encode = nanocode_trace::info_span!("tui.terminal.diff_encode").entered();
        let scroll = if self.stale {
            None
        } else {
            scroll(&mut self.previous, &self.current, &mut self.changed)
        };
        let frame = Frame {
            bytes: &mut self.bytes,
            lead: &lead,
            screen: &self.current,
            area,
            stale: self.stale,
            scroll,
            cursor,
            shown: self.cursor,
            monochrome: self.monochrome,
        };
        let encoded = if self.stale {
            frame.encode(self.current.blank_diff_iter())
        } else {
            let width = usize::from(area.width);
            frame.encode(self.changed.iter().flat_map(|&y| {
                self.previous
                    .diff_range(&self.current, y * width..(y + 1) * width)
            }))
        };
        let Some(changed_cells) = encoded else {
            span.record("frame.changed_cells", 0);
            span.record("frame.skipped", true);
            self.stats.skipped += 1;
            std::mem::swap(&mut self.previous, &mut self.current);
            return Ok(());
        };
        span.record("frame.changed_cells", changed_cells);
        drop(diff_encode);
        span.record("frame.bytes", self.bytes.len());
        nanocode_trace::info_span!("tui.terminal.write", bytes = self.bytes.len()).in_scope(
            || {
                self.output.write_all(&self.bytes)?;
                self.output.flush()
            },
        )?;
        self.stats.frames += 1;
        self.stats.bytes += self.bytes.len() as u64;
        if self.stale {
            self.stats.full_repaints += 1;
        }
        self.stale = false;
        self.cursor = cursor;
        std::mem::swap(&mut self.previous, &mut self.current);
        Ok(())
    }

    pub fn animate(&mut self, spinner: &str) -> io::Result<bool> {
        if self.stale || !self.active || self.spinners.is_empty() {
            return Ok(false);
        }
        let (columns, rows) = size()?;
        if Rect::new(0, 0, columns, rows) != self.previous.area {
            return Ok(false);
        }
        let span = nanocode_trace::info_span!(
            "tui.terminal.animate",
            cells = self.spinners.len(),
            frame.bytes = nanocode_trace::field::Empty
        );
        let _entered = span.enter();
        for position in &self.spinners {
            if let Some(cell) = self.previous.cell_mut(*position) {
                cell.set_symbol(spinner);
            }
        }
        self.bytes.clear();
        self.bytes
            .extend_from_slice(ansi::BEGIN_SYNCHRONIZED_UPDATE.as_bytes());
        let previous = &self.previous;
        let synchronized = ansi::draw(
            &mut self.bytes,
            previous,
            self.spinners.iter().filter_map(|position| {
                previous
                    .cell(*position)
                    .map(|cell| (position.x, position.y, cell))
            }),
            self.monochrome,
        );
        if let Some(position) = self.cursor {
            ansi::move_to(&mut self.bytes, position.x, position.y);
        }
        finish(&mut self.bytes, synchronized);
        span.record("frame.bytes", self.bytes.len());
        self.output.write_all(&self.bytes)?;
        self.output.flush()?;
        self.stats.frames += 1;
        self.stats.bytes += self.bytes.len() as u64;
        Ok(true)
    }

    #[must_use]
    pub const fn stats(&self) -> FrameStats {
        self.stats
    }

    pub const fn clear(&mut self) -> io::Result<()> {
        self.stale = true;
        Ok(())
    }

    pub fn close(mut self, lines: &[Line<'_>]) -> io::Result<()> {
        if !self.active || self.options.alternate_screen {
            return Ok(());
        }
        let (columns, rows) = size()?;
        let kept = lines.len().min(usize::from(rows.saturating_sub(1)));
        let (scrolled, kept) = lines.split_at(lines.len() - kept);
        self.bytes.clear();
        self.bytes
            .extend_from_slice(ansi::BEGIN_SYNCHRONIZED_UPDATE.as_bytes());
        scroll_off(
            &mut self.bytes,
            scrolled,
            Rect::new(0, 0, columns, rows),
            self.monochrome,
            self.truecolor,
        );
        paint(
            &mut self.bytes,
            kept,
            columns,
            self.monochrome,
            self.truecolor,
        );
        let below = u16::try_from(kept.len()).expect("kept lines fit the screen");
        ansi::move_to(&mut self.bytes, 0, below);
        self.bytes.extend_from_slice(ansi::CLEAR_BELOW.as_bytes());
        self.bytes
            .extend_from_slice(ansi::END_SYNCHRONIZED_UPDATE.as_bytes());
        execute(&mut self.output, &self.bytes)
    }

    pub fn suspend(&mut self) -> io::Result<()> {
        if self.active {
            restore(&mut self.output);
            self.active = false;
            self.selection = None;
        }
        Ok(())
    }

    pub fn resume(&mut self) -> io::Result<()> {
        if !self.active {
            enable_raw_mode()?;
            activate(&mut self.output, self.options)?;
            self.stale = true;
            self.cursor = None;
            self.active = true;
        }
        Ok(())
    }

    pub fn set_mouse_capture(&mut self, enabled: bool) -> io::Result<()> {
        if enabled == self.options.mouse_capture {
            return Ok(());
        }
        self.options.mouse_capture = enabled;
        if enabled {
            execute(&mut self.output, ansi::ENABLE_MOUSE_CAPTURE.as_bytes())?;
            MODES.fetch_or(MOUSE, Ordering::AcqRel);
        } else {
            execute(&mut self.output, ansi::DISABLE_MOUSE_CAPTURE.as_bytes())?;
            MODES.fetch_and(!MOUSE, Ordering::AcqRel);
            self.selection = None;
        }
        Ok(())
    }

    pub fn select(&mut self, mouse: MouseEvent) -> SelectionEvent {
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let visible = self.selection.as_ref().is_some_and(Selection::active);
                let mut screen = self.previous.clone();
                if let Some(selection) = &self.selection {
                    selection.highlight(&mut screen);
                }
                self.selection = Selection::begin(&self.regions, position, screen);
                if visible {
                    SelectionEvent::Changed
                } else {
                    SelectionEvent::Ignored
                }
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self
                    .selection
                    .as_mut()
                    .is_some_and(|selection| selection.extend(position)) =>
            {
                SelectionEvent::Changed
            }
            MouseEventKind::Up(_) => match self.selection.take() {
                Some(selection) if selection.active() => {
                    let text = selection.text();
                    if text.is_empty() {
                        return SelectionEvent::Changed;
                    }
                    match self.copy(&text) {
                        Ok(()) => SelectionEvent::Copied(text),
                        Err(error) => SelectionEvent::Failed(error),
                    }
                }
                _ => SelectionEvent::Ignored,
            },
            MouseEventKind::Moved => match self.selection.take() {
                Some(selection) if selection.active() => SelectionEvent::Changed,
                _ => SelectionEvent::Ignored,
            },
            _ => SelectionEvent::Ignored,
        }
    }

    pub fn copy(&mut self, text: &str) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        if !["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
            .into_iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        {
            use std::process::{Command, Stdio};

            let mut child = Command::new("pbcopy")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?;
            let written = child
                .stdin
                .take()
                .expect("pbcopy stdin is piped")
                .write_all(text.as_bytes());
            let output = child.wait_with_output()?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "pbcopy exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            return written;
        }
        let mut bytes = Vec::new();
        ansi::copy_to_clipboard(&mut bytes, text);
        execute(&mut self.output, &bytes)
    }

    #[must_use]
    pub const fn mouse_capture(&self) -> bool {
        self.options.mouse_capture
    }

    #[must_use]
    pub const fn truecolor(&self) -> bool {
        self.truecolor
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.active {
            restore(&mut self.output);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Scroll {
    top: u16,
    bottom: u16,
    lines: u16,
    up: bool,
}

struct Frame<'a> {
    bytes: &'a mut Vec<u8>,
    lead: &'a [u8],
    screen: &'a Buffer,
    area: Rect,
    stale: bool,
    scroll: Option<Scroll>,
    cursor: Option<Position>,
    shown: Option<Position>,
    monochrome: bool,
}

impl Frame<'_> {
    fn encode<'cell>(
        self,
        updates: impl Iterator<Item = (u16, u16, &'cell Cell)>,
    ) -> Option<usize> {
        let mut changed_cells = 0;
        let mut updates = updates.inspect(|_| changed_cells += 1).peekable();
        let has_updates = updates.peek().is_some();
        if !self.stale
            && self.lead.is_empty()
            && self.scroll.is_none()
            && !has_updates
            && self.cursor == self.shown
        {
            return None;
        }
        self.bytes.clear();
        self.bytes
            .extend_from_slice(ansi::BEGIN_SYNCHRONIZED_UPDATE.as_bytes());
        self.bytes.extend_from_slice(self.lead);
        if self.stale {
            self.bytes
                .extend_from_slice(ansi::RESET_ATTRIBUTES.as_bytes());
            for row in self.area.top()..self.area.bottom() {
                ansi::move_to(self.bytes, 0, row);
                self.bytes.extend_from_slice(ansi::CLEAR_LINE.as_bytes());
            }
        }
        if let Some(Scroll {
            top,
            bottom,
            lines,
            up,
        }) = self.scroll
        {
            ansi::scroll(self.bytes, top, bottom, lines, up);
        }
        let rows = has_updates && ansi::draw(self.bytes, self.screen, updates, self.monochrome);
        match self.cursor {
            Some(position) => {
                ansi::move_to(self.bytes, position.x, position.y);
                if self.shown.is_none() {
                    self.bytes.extend_from_slice(ansi::SHOW_CURSOR.as_bytes());
                }
            }
            None if self.shown.is_some() => {
                self.bytes.extend_from_slice(ansi::HIDE_CURSOR.as_bytes());
            }
            None => {}
        }
        finish(
            self.bytes,
            self.stale || !self.lead.is_empty() || self.scroll.is_some() || rows,
        );
        Some(changed_cells)
    }
}

fn finish(bytes: &mut Vec<u8>, synchronized: bool) {
    if synchronized {
        bytes.extend_from_slice(ansi::END_SYNCHRONIZED_UPDATE.as_bytes());
    } else {
        bytes.drain(..ansi::BEGIN_SYNCHRONIZED_UPDATE.len());
    }
}

fn scroll(previous: &mut Buffer, current: &Buffer, changed: &mut Vec<usize>) -> Option<Scroll> {
    changed.clear();
    let width = usize::from(current.area.width);
    let rows = previous.content.len().checked_div(width)?;
    changed.extend((0..rows).filter(|&y| row(previous, width, y) != row(current, width, y)));
    let top = *changed.first()?;
    let bottom = *changed.last()? + 1;
    let span = bottom - top;
    let misses = changed.len();
    let kept = |lines: usize, up: bool| {
        span - lines
            - (top..bottom - lines)
                .filter(|&y| {
                    let (from, to) = if up { (y + lines, y) } else { (y, y + lines) };
                    row(previous, width, from) != row(current, width, to)
                })
                .take(misses)
                .count()
    };
    let mut best = (span - misses, None);
    for lines in 1..misses {
        for up in [true, false] {
            let (first, last) = if up {
                ((top + lines, top), (bottom - 1, bottom - 1 - lines))
            } else {
                ((top, top + lines), (bottom - 1 - lines, bottom - 1))
            };
            let anchored = row(previous, width, first.0) == row(current, width, first.1)
                || row(previous, width, last.0) == row(current, width, last.1);
            if anchored {
                let rows = kept(lines, up);
                if rows > best.0 {
                    best = (rows, Some((lines, up)));
                }
            }
        }
    }
    let (lines, up) = best.1?;
    let region = &mut previous.content[top * width..bottom * width];
    let blank = if up {
        region.rotate_left(lines * width);
        (span - lines) * width..span * width
    } else {
        region.rotate_right(lines * width);
        0..lines * width
    };
    region[blank].iter_mut().for_each(Cell::reset);
    changed.clear();
    changed.extend((top..bottom).filter(|&y| row(previous, width, y) != row(current, width, y)));
    Some(Scroll {
        top: top as u16,
        bottom: bottom as u16,
        lines: lines as u16,
        up,
    })
}

fn scroll_off(bytes: &mut Vec<u8>, lines: &[Line<'_>], area: Rect, mono: bool, truecolor: bool) {
    if area.is_empty() {
        return;
    }
    for chunk in lines.chunks(usize::from(area.height)) {
        paint(bytes, chunk, area.width, mono, truecolor);
        ansi::move_to(bytes, 0, area.height - 1);
        bytes.resize(bytes.len() + chunk.len(), b'\n');
    }
}

fn paint(bytes: &mut Vec<u8>, lines: &[Line<'_>], width: u16, mono: bool, truecolor: bool) {
    let height = u16::try_from(lines.len()).expect("painted lines fit the screen");
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    bytes.extend_from_slice(ansi::RESET_ATTRIBUTES.as_bytes());
    for (y, line) in (0..height).zip(lines) {
        ansi::move_to(bytes, 0, y);
        bytes.extend_from_slice(ansi::CLEAR_LINE.as_bytes());
        line.render(Rect::new(0, y, width, 1), &mut buffer);
    }
    if mono {
        monochrome(&mut buffer);
    } else if !truecolor {
        downsample(&mut buffer);
    }
    ansi::draw(bytes, &buffer, buffer.blank_diff_iter(), mono);
}

fn shift(buffer: &mut Buffer, lines: usize) {
    let length = buffer.content.len();
    let moved = lines
        .saturating_mul(usize::from(buffer.area.width))
        .min(length);
    buffer.content.rotate_left(moved);
    buffer.content[length - moved..]
        .iter_mut()
        .for_each(Cell::reset);
}

fn preserve(output: &mut impl Write) -> io::Result<()> {
    let (_, rows) = size()?;
    output.write_all(&vec![b'\n'; usize::from(rows)])?;
    output.flush()
}

fn row(buffer: &Buffer, width: usize, y: usize) -> &[Cell] {
    &buffer.content[y * width..(y + 1) * width]
}

fn execute(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    output.write_all(bytes)?;
    output.flush()
}

fn no_color() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| std::env::var("NO_COLOR").is_ok_and(|value| !value.is_empty()))
}

fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("truecolor") || value.contains("24bit")
    })
}

fn monochrome(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
    }
}

fn downsample(buffer: &mut Buffer) {
    let mut foreground = None;
    let mut background = None;
    for cell in &mut buffer.content {
        if let Color::Rgb(red, green, blue) = cell.fg {
            cell.fg = indexed_color(&mut foreground, red, green, blue);
        }
        if let Color::Rgb(red, green, blue) = cell.bg {
            cell.bg = indexed_color(&mut background, red, green, blue);
        }
    }
}

fn indexed_color(cache: &mut Option<((u8, u8, u8), Color)>, red: u8, green: u8, blue: u8) -> Color {
    let rgb = (red, green, blue);
    if let Some((previous, indexed)) = *cache
        && previous == rgb
    {
        return indexed;
    }
    let indexed = Color::Indexed(ansi256(red, green, blue));
    *cache = Some((rgb, indexed));
    indexed
}

const fn ansi256(red: u8, green: u8, blue: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    const fn level(value: u8) -> u8 {
        if value < 48 {
            0
        } else if value < 115 {
            1
        } else {
            (value - 35) / 40
        }
    }
    const fn distance(left: (u8, u8, u8), right: (u8, u8, u8)) -> u32 {
        let red = left.0.abs_diff(right.0) as u32;
        let green = left.1.abs_diff(right.1) as u32;
        let blue = left.2.abs_diff(right.2) as u32;
        red * red + green * green + blue * blue
    }
    let (r, g, b) = (level(red), level(green), level(blue));
    let cube = (LEVELS[r as usize], LEVELS[g as usize], LEVELS[b as usize]);
    let average = (red as u32 + green as u32 + blue as u32) / 3;
    let gray_index = if average > 238 {
        23
    } else if average < 8 {
        0
    } else {
        (average - 8) / 10
    };
    let gray_level = (8 + gray_index * 10) as u8;
    let gray = (gray_level, gray_level, gray_level);
    if distance((red, green, blue), gray) < distance((red, green, blue), cube) {
        232 + gray_index as u8
    } else {
        16 + 36 * r + 6 * g + b
    }
}

fn activate(output: &mut impl Write, options: TerminalOptions) -> io::Result<()> {
    ACTIVE.store(true, Ordering::Release);
    let mut modes = 0;
    if options.alternate_screen {
        output.write_all(ansi::ENTER_ALTERNATE_SCREEN.as_bytes())?;
        modes |= ALTERNATE;
    }
    output.write_all(ansi::ENABLE_BRACKETED_PASTE.as_bytes())?;
    output.write_all(ansi::HIDE_CURSOR.as_bytes())?;
    #[cfg(unix)]
    for sequence in [
        ansi::ENABLE_COLOR_SCHEME_UPDATES,
        ansi::QUERY_BACKGROUND_COLOR,
        ansi::QUERY_COLOR_SCHEME,
    ] {
        output.write_all(sequence.as_bytes())?;
    }
    if options.mouse_capture {
        output.write_all(ansi::ENABLE_MOUSE_CAPTURE.as_bytes())?;
        modes |= MOUSE;
    }
    if options.keyboard_enhancement {
        output.write_all(ansi::PUSH_DISAMBIGUATE_ESCAPE_CODES.as_bytes())?;
        modes |= KEYBOARD;
    }
    output.flush()?;
    MODES.store(modes, Ordering::Release);
    Ok(())
}

fn restore(output: &mut impl Write) {
    ACTIVE.store(false, Ordering::Release);
    let modes = MODES.swap(0, Ordering::AcqRel);
    drop(disable_raw_mode());
    let mut bytes = Vec::new();
    if modes & KEYBOARD != 0 {
        bytes.extend_from_slice(ansi::POP_KEYBOARD_ENHANCEMENT_FLAGS.as_bytes());
    }
    if modes & MOUSE != 0 {
        bytes.extend_from_slice(ansi::DISABLE_MOUSE_CAPTURE.as_bytes());
    }
    #[cfg(unix)]
    bytes.extend_from_slice(ansi::DISABLE_COLOR_SCHEME_UPDATES.as_bytes());
    for sequence in [ansi::END_SYNCHRONIZED_UPDATE, ansi::DISABLE_BRACKETED_PASTE] {
        bytes.extend_from_slice(sequence.as_bytes());
    }
    if modes & ALTERNATE != 0 {
        bytes.extend_from_slice(ansi::LEAVE_ALTERNATE_SCREEN.as_bytes());
    }
    bytes.extend_from_slice(ansi::SHOW_CURSOR.as_bytes());
    drop(output.write_all(&bytes));
    drop(output.flush());
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if ACTIVE.load(Ordering::Acquire) {
                restore(&mut stdout());
            }
            previous(info);
        }));
    });
}

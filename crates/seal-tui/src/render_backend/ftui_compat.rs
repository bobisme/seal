use std::io;

use ftui_core::event as ft;
use ftui_render::cell::PackedRgba;
use palette::{Mix, Srgba};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Self = Self::new(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Self = Self::new(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Self = Self::new(1.0, 1.0, 1.0, 1.0);

    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub fn from_rgba_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: f32::from(a) / 255.0,
        }
    }

    #[must_use]
    pub fn to_rgba_u8(self) -> (u8, u8, u8, u8) {
        (
            channel_to_u8(self.r),
            channel_to_u8(self.g),
            channel_to_u8(self.b),
            channel_to_u8(self.a),
        )
    }

    #[must_use]
    pub fn from_hex(hex: &str) -> Option<Self> {
        parse_hex_rgba(hex)
    }

    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        color_lerp(self, other, t)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const SHIFT: Self = Self(0b0001);
    pub const ALT: Self = Self(0b0010);
    pub const CTRL: Self = Self(0b0100);
    pub const SUPER: Self = Self(0b1000);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl Default for KeyModifiers {
    fn default() -> Self {
        Self::empty()
    }
}

impl core::ops::BitOr for KeyModifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for KeyModifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    BackTab,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    F(u8),
    Null,
    MediaPlayPause,
    MediaStop,
    MediaNextTrack,
    MediaPrevTrack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyEvent {
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    #[must_use]
    pub const fn key(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::empty(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub x: u32,
    pub y: u32,
    pub button: MouseButton,
    pub kind: MouseEventKind,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl MouseEvent {
    #[must_use]
    pub const fn new(x: u32, y: u32, button: MouseButton, kind: MouseEventKind) -> Self {
        Self {
            x,
            y,
            button,
            kind,
            shift: false,
            ctrl: false,
            alt: false,
        }
    }

    #[must_use]
    pub const fn with_modifiers(mut self, shift: bool, ctrl: bool, alt: bool) -> Self {
        self.shift = shift;
        self.ctrl = ctrl;
        self.alt = alt;
        self
    }

    #[must_use]
    pub const fn is_scroll(&self) -> bool {
        matches!(
            self.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteEvent {
    pub text: String,
}

impl PasteEvent {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeEvent {
    pub width: u16,
    pub height: u16,
}

impl ResizeEvent {
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(ResizeEvent),
    Paste(PasteEvent),
    FocusGained,
    FocusLost,
}

#[must_use]
pub fn event_from_ftui(event: ft::Event) -> Option<Event> {
    match event {
        ft::Event::Key(key) => key_event_from_ftui(key).map(Event::Key),
        ft::Event::Mouse(mouse) => Some(Event::Mouse(mouse_event_from_ftui(mouse))),
        ft::Event::Resize { width, height } => Some(Event::Resize(ResizeEvent::new(width, height))),
        ft::Event::Paste(paste) => Some(Event::Paste(PasteEvent::new(paste.text))),
        ft::Event::Focus(focused) => Some(if focused {
            Event::FocusGained
        } else {
            Event::FocusLost
        }),
        ft::Event::Clipboard(_) | ft::Event::Tick => None,
    }
}

fn key_event_from_ftui(key: ft::KeyEvent) -> Option<KeyEvent> {
    keycode_from_ftui(key.code).map(|code| {
        let mut modifiers = KeyModifiers::empty();
        if key.modifiers.contains(ft::Modifiers::SHIFT) {
            modifiers |= KeyModifiers::SHIFT;
        }
        if key.modifiers.contains(ft::Modifiers::ALT) {
            modifiers |= KeyModifiers::ALT;
        }
        if key.modifiers.contains(ft::Modifiers::CTRL) {
            modifiers |= KeyModifiers::CTRL;
        }
        if key.modifiers.contains(ft::Modifiers::SUPER) {
            modifiers |= KeyModifiers::SUPER;
        }
        KeyEvent::new(code, modifiers)
    })
}

const fn keycode_from_ftui(code: ft::KeyCode) -> Option<KeyCode> {
    Some(match code {
        ft::KeyCode::Char(c) => KeyCode::Char(c),
        ft::KeyCode::Enter => KeyCode::Enter,
        ft::KeyCode::Escape => KeyCode::Esc,
        ft::KeyCode::Backspace => KeyCode::Backspace,
        ft::KeyCode::Tab => KeyCode::Tab,
        ft::KeyCode::BackTab => KeyCode::BackTab,
        ft::KeyCode::Delete => KeyCode::Delete,
        ft::KeyCode::Insert => KeyCode::Insert,
        ft::KeyCode::Home => KeyCode::Home,
        ft::KeyCode::End => KeyCode::End,
        ft::KeyCode::PageUp => KeyCode::PageUp,
        ft::KeyCode::PageDown => KeyCode::PageDown,
        ft::KeyCode::Up => KeyCode::Up,
        ft::KeyCode::Down => KeyCode::Down,
        ft::KeyCode::Left => KeyCode::Left,
        ft::KeyCode::Right => KeyCode::Right,
        ft::KeyCode::F(n) => KeyCode::F(n),
        ft::KeyCode::Null => KeyCode::Null,
        ft::KeyCode::MediaPlayPause
        | ft::KeyCode::MediaStop
        | ft::KeyCode::MediaNextTrack
        | ft::KeyCode::MediaPrevTrack => return None,
    })
}

fn mouse_event_from_ftui(mouse: ft::MouseEvent) -> MouseEvent {
    let (button, kind) = match mouse.kind {
        ft::MouseEventKind::Down(button) => (mouse_button_from_ftui(button), MouseEventKind::Press),
        ft::MouseEventKind::Up(button) => (mouse_button_from_ftui(button), MouseEventKind::Release),
        ft::MouseEventKind::Drag(button) => (mouse_button_from_ftui(button), MouseEventKind::Move),
        ft::MouseEventKind::Moved => (MouseButton::None, MouseEventKind::Move),
        ft::MouseEventKind::ScrollUp => (MouseButton::None, MouseEventKind::ScrollUp),
        ft::MouseEventKind::ScrollDown => (MouseButton::None, MouseEventKind::ScrollDown),
        ft::MouseEventKind::ScrollLeft => (MouseButton::None, MouseEventKind::ScrollLeft),
        ft::MouseEventKind::ScrollRight => (MouseButton::None, MouseEventKind::ScrollRight),
    };

    MouseEvent::new(mouse.x.into(), mouse.y.into(), button, kind).with_modifiers(
        mouse.modifiers.contains(ft::Modifiers::SHIFT),
        mouse.modifiers.contains(ft::Modifiers::CTRL),
        mouse.modifiers.contains(ft::Modifiers::ALT),
    )
}

const fn mouse_button_from_ftui(button: ft::MouseButton) -> MouseButton {
    match button {
        ft::MouseButton::Left => MouseButton::Left,
        ft::MouseButton::Middle => MouseButton::Middle,
        ft::MouseButton::Right => MouseButton::Right,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextAttributes {
    bits: u16,
    link_id: Option<u32>,
}

impl TextAttributes {
    pub const NONE: Self = Self {
        bits: 0,
        link_id: None,
    };
    pub const BOLD: Self = Self {
        bits: 1 << 0,
        link_id: None,
    };
    pub const DIM: Self = Self {
        bits: 1 << 1,
        link_id: None,
    };
    pub const ITALIC: Self = Self {
        bits: 1 << 2,
        link_id: None,
    };
    pub const UNDERLINE: Self = Self {
        bits: 1 << 3,
        link_id: None,
    };
    pub const BLINK: Self = Self {
        bits: 1 << 4,
        link_id: None,
    };
    pub const INVERSE: Self = Self {
        bits: 1 << 5,
        link_id: None,
    };
    pub const HIDDEN: Self = Self {
        bits: 1 << 6,
        link_id: None,
    };
    pub const STRIKETHROUGH: Self = Self {
        bits: 1 << 7,
        link_id: None,
    };

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.bits & other.bits) == other.bits
    }

    #[must_use]
    pub const fn with_link_id(mut self, link_id: u32) -> Self {
        self.link_id = Some(link_id);
        self
    }

    #[must_use]
    pub const fn link_id(self) -> Option<u32> {
        self.link_id
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
            link_id: if self.link_id.is_some() {
                self.link_id
            } else {
                other.link_id
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub fg: Option<Rgba>,
    pub bg: Option<Rgba>,
    pub attributes: TextAttributes,
}

impl Style {
    #[must_use]
    pub const fn fg(color: Rgba) -> Self {
        Self {
            fg: Some(color),
            bg: None,
            attributes: TextAttributes::NONE,
        }
    }

    #[must_use]
    pub const fn with_bg(mut self, bg: Rgba) -> Self {
        self.bg = Some(bg);
        self
    }

    #[must_use]
    pub const fn with_bold(mut self) -> Self {
        self.attributes = self.attributes.union(TextAttributes::BOLD);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellContent {
    Char(char),
    Empty,
    Continuation,
    Grapheme(u32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub content: CellContent,
    pub fg: Rgba,
    pub bg: Rgba,
    pub attributes: TextAttributes,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            content: CellContent::Empty,
            fg: Rgba::WHITE,
            bg: Rgba::TRANSPARENT,
            attributes: TextAttributes::NONE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxStyle {
    pub border_style: Style,
    rounded: bool,
}

impl BoxStyle {
    #[must_use]
    pub const fn rounded(border_style: Style) -> Self {
        Self {
            border_style,
            rounded: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OptimizedBuffer {
    width: u32,
    height: u32,
    cells: Vec<Cell>,
}

impl OptimizedBuffer {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let area = usize::try_from(width.saturating_mul(height)).unwrap_or(0);
        Self {
            width,
            height,
            cells: vec![Cell::default(); area],
        }
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub fn get(&self, x: u32, y: u32) -> Option<&Cell> {
        self.idx(x, y).and_then(|idx| self.cells.get(idx))
    }

    pub fn get_mut(&mut self, x: u32, y: u32) -> Option<&mut Cell> {
        self.idx(x, y).and_then(|idx| self.cells.get_mut(idx))
    }

    pub fn clear(&mut self, color: Rgba) {
        for cell in &mut self.cells {
            cell.content = CellContent::Empty;
            cell.fg = Rgba::WHITE;
            cell.bg = color;
            cell.attributes = TextAttributes::NONE;
        }
    }

    pub fn draw_text(&mut self, x: u32, y: u32, text: &str, style: Style) {
        if y >= self.height {
            return;
        }

        let mut col = x;
        for ch in text.chars() {
            if col >= self.width {
                break;
            }
            if let Some(cell) = self.get_mut(col, y) {
                cell.content = CellContent::Char(ch);
                if let Some(fg) = style.fg {
                    cell.fg = fg;
                }
                if let Some(bg) = style.bg {
                    cell.bg = bg;
                }
                cell.attributes = style.attributes;
            }
            col = col.saturating_add(1);
        }
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Rgba) {
        let x_end = x.saturating_add(width).min(self.width);
        let y_end = y.saturating_add(height).min(self.height);

        for yy in y..y_end {
            for xx in x..x_end {
                if let Some(cell) = self.get_mut(xx, yy) {
                    cell.content = CellContent::Empty;
                    cell.bg = color;
                }
            }
        }
    }

    pub fn draw_box(&mut self, x: u32, y: u32, width: u32, height: u32, style: BoxStyle) {
        if width < 2 || height < 2 {
            return;
        }

        let (tl, tr, bl, br, horiz, vert) = if style.rounded {
            ('╭', '╮', '╰', '╯', '─', '│')
        } else {
            ('┌', '┐', '└', '┘', '─', '│')
        };

        let right = x.saturating_add(width.saturating_sub(1));
        let bottom = y.saturating_add(height.saturating_sub(1));
        self.draw_text(x, y, &tl.to_string(), style.border_style);
        self.draw_text(right, y, &tr.to_string(), style.border_style);
        self.draw_text(x, bottom, &bl.to_string(), style.border_style);
        self.draw_text(right, bottom, &br.to_string(), style.border_style);

        for xx in x.saturating_add(1)..right {
            self.draw_text(xx, y, &horiz.to_string(), style.border_style);
            self.draw_text(xx, bottom, &horiz.to_string(), style.border_style);
        }
        for yy in y.saturating_add(1)..bottom {
            self.draw_text(x, yy, &vert.to_string(), style.border_style);
            self.draw_text(right, yy, &vert.to_string(), style.border_style);
        }
    }

    #[must_use]
    fn idx(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let row = usize::try_from(y).ok()?;
        let width = usize::try_from(self.width).ok()?;
        let col = usize::try_from(x).ok()?;
        Some(row.saturating_mul(width).saturating_add(col))
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct RendererOptions {
    pub use_alt_screen: bool,
    pub hide_cursor: bool,
    pub enable_mouse: bool,
    pub query_capabilities: bool,
}

#[derive(Debug)]
pub struct Renderer {
    buffer: OptimizedBuffer,
    background: Rgba,
    _options: RendererOptions,
}

impl Renderer {
    pub fn new_with_options(width: u32, height: u32, options: RendererOptions) -> io::Result<Self> {
        let mut renderer = Self {
            buffer: OptimizedBuffer::new(width.max(1), height.max(1)),
            background: Rgba::BLACK,
            _options: options,
        };
        renderer.clear();
        Ok(renderer)
    }

    pub const fn set_background(&mut self, color: Rgba) {
        self.background = color;
    }

    #[must_use]
    pub const fn buffer(&mut self) -> &mut OptimizedBuffer {
        &mut self.buffer
    }

    pub fn clear(&mut self) {
        self.buffer.clear(self.background);
    }

    pub const fn invalidate(&mut self) {}

    pub const fn present(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) -> io::Result<()> {
        self.buffer = OptimizedBuffer::new(width.max(1), height.max(1));
        self.buffer.clear(self.background);
        Ok(())
    }

    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.buffer.width, self.buffer.height)
    }
}

#[derive(Debug)]
pub struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {}
}

pub const fn enable_raw_mode() -> io::Result<RawModeGuard> {
    Ok(RawModeGuard)
}

pub fn terminal_size() -> io::Result<(u16, u16)> {
    let columns = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|v| *v > 1);
    let rows = std::env::var("LINES")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|v| *v > 1);

    if let (Some(width), Some(height)) = (columns, rows) {
        return Ok((width, height));
    }

    Err(io::Error::other("terminal size unavailable"))
}

#[must_use]
pub fn color_from_hex(hex: &str) -> Option<Rgba> {
    Rgba::from_hex(hex)
}

#[must_use]
pub const fn color_with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba::new(color.r, color.g, color.b, alpha.clamp(0.0, 1.0))
}

#[must_use]
pub fn color_luminance(color: Rgba) -> f32 {
    color
        .r
        .mul_add(0.299, color.g.mul_add(0.587, color.b * 0.114))
}

#[must_use]
pub fn color_lerp(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let mixed =
        Srgba::new(a.r, a.g, a.b, a.a).mix(Srgba::new(b.r, b.g, b.b, b.a), t.clamp(0.0, 1.0));
    Rgba::new(mixed.red, mixed.green, mixed.blue, mixed.alpha)
}

#[must_use]
pub fn color_blend_over(fg: Rgba, bg: Rgba) -> Rgba {
    let src_a = fg.a.clamp(0.0, 1.0);
    let dst_a = bg.a.clamp(0.0, 1.0);
    let out_a = src_a + dst_a * (1.0 - src_a);

    if out_a <= 0.0 {
        return Rgba::TRANSPARENT;
    }

    let out_r = fg.r.mul_add(src_a, bg.r * dst_a * (1.0 - src_a)) / out_a;
    let out_g = fg.g.mul_add(src_a, bg.g * dst_a * (1.0 - src_a)) / out_a;
    let out_b = fg.b.mul_add(src_a, bg.b * dst_a * (1.0 - src_a)) / out_a;
    Rgba::new(out_r, out_g, out_b, out_a)
}

pub fn buffer_draw_box(
    buffer: &mut OptimizedBuffer,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    style: BoxStyle,
) {
    buffer.draw_box(x, y, width, height, style);
}

pub fn buffer_clear(buffer: &mut OptimizedBuffer, color: Rgba) {
    buffer.clear(color);
}

pub fn buffer_draw_text(buffer: &mut OptimizedBuffer, x: u32, y: u32, text: &str, style: Style) {
    buffer.draw_text(x, y, text, style);
}

pub fn buffer_fill_rect(
    buffer: &mut OptimizedBuffer,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: Rgba,
) {
    buffer.fill_rect(x, y, width, height, color);
}

pub fn buffer_dim_cell_rgb(buffer: &mut OptimizedBuffer, x: u32, y: u32, scale: f32) {
    if let Some(cell) = buffer.get_mut(x, y) {
        cell.fg = Rgba::new(
            cell.fg.r * scale,
            cell.fg.g * scale,
            cell.fg.b * scale,
            cell.fg.a,
        );
        cell.bg = Rgba::new(
            cell.bg.r * scale,
            cell.bg.g * scale,
            cell.bg.b * scale,
            cell.bg.a,
        );
    }
}

#[must_use]
pub fn rgba_to_packed(color: Rgba) -> PackedRgba {
    let (r, g, b, a) = color.to_rgba_u8();
    PackedRgba::rgba(r, g, b, a)
}

#[must_use]
pub fn packed_to_rgba(color: PackedRgba) -> Rgba {
    Rgba::from_rgba_u8(color.r(), color.g(), color.b(), color.a())
}

fn channel_to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn parse_hex_rgba(hex: &str) -> Option<Rgba> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);

    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Rgba::from_rgba_u8(r, g, b, 255))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Rgba::from_rgba_u8(r, g, b, a))
        }
        _ => None,
    }
}

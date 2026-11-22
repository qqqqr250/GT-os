//! A small clean-room desktop shell built from the widget toolkit.
//!
//! The types here are ordinary Rust: a horizontal [`Panel`] with a launcher
//! toggle, [`Clock`] and [`SystemTray`], plus a [`LauncherGrid`] that can be
//! shown above the desktop. [`DesktopShell`] owns the layering and routes
//! events to the visible surface. No Plasma, Qt, HTML or foreign runtime is
//! involved; the clock reads the guest's CMOS-backed realtime syscall.

use crate::{
    Accessible, Button, Canvas, Event, GLYPH_HEIGHT, GLYPH_WIDTH, GridLayout, HorizontalLayout,
    Layout, Point, Rect, Style, Theme, Widget,
};
use alloc::vec;
use alloc::vec::Vec;
use gtos_sys::{KEY_DOWN, KEY_LEFT, KEY_RIGHT, KEY_UP, MOD_SHIFT, realtime_seconds};

/// Carriage return / Enter key code.
const ENTER: u32 = 13;
/// Escape key code used to dismiss the launcher.
const ESCAPE: u32 = 27;
/// Tab key code used for focus cycling.
const TAB: u32 = 9;
/// Height of the panel strip in pixels.
pub const DEFAULT_PANEL_HEIGHT: u32 = 24;
/// Width of the launcher grid in pixels.
pub const DEFAULT_LAUNCHER_WIDTH: u32 = 144;
/// Height of the launcher grid in pixels.
pub const DEFAULT_LAUNCHER_HEIGHT: u32 = 96;
/// Gap between the panel, launcher and screen edges.
pub const DEFAULT_LAUNCHER_GAP: u32 = 4;
/// Number of launcher columns by default.
pub const DEFAULT_LAUNCHER_COLUMNS: usize = 2;
/// Gap between launcher cells by default.
pub const DEFAULT_LAUNCHER_SPACING: u32 = 2;

/// Geometry used when a [`DesktopShell`] is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellLayout {
    /// Height of the top panel.
    pub panel_height: u32,
    /// Width of the launcher grid.
    pub launcher_width: u32,
    /// Height of the launcher grid.
    pub launcher_height: u32,
    /// Gap around the launcher grid.
    pub launcher_gap: u32,
    /// Number of launcher columns.
    pub launcher_columns: usize,
    /// Gap between launcher cells.
    pub launcher_spacing: u32,
}

impl ShellLayout {
    /// Creates a layout from explicit geometry.
    #[must_use]
    pub const fn new(
        panel_height: u32,
        launcher_width: u32,
        launcher_height: u32,
        launcher_gap: u32,
        launcher_columns: usize,
        launcher_spacing: u32,
    ) -> Self {
        Self {
            panel_height,
            launcher_width,
            launcher_height,
            launcher_gap,
            launcher_columns,
            launcher_spacing,
        }
    }
}

impl Default for ShellLayout {
    fn default() -> Self {
        Self::new(
            DEFAULT_PANEL_HEIGHT,
            DEFAULT_LAUNCHER_WIDTH,
            DEFAULT_LAUNCHER_HEIGHT,
            DEFAULT_LAUNCHER_GAP,
            DEFAULT_LAUNCHER_COLUMNS,
            DEFAULT_LAUNCHER_SPACING,
        )
    }
}

const fn digit(value: u64) -> u8 {
    b'0' + (value % 10) as u8
}

const fn format_clock(seconds: u64) -> [u8; 8] {
    let daytime = seconds % 86_400;
    let hour = daytime / 3600;
    let minute = (daytime % 3600) / 60;
    let second = daytime % 60;
    [
        digit(hour / 10),
        digit(hour),
        b':',
        digit(minute / 10),
        digit(minute),
        b':',
        digit(second / 10),
        digit(second),
    ]
}

/// A panel clock showing `HH:MM:SS` in UTC.
pub struct Clock {
    rect: Rect,
    seconds: u64,
    text: [u8; 8],
    theme: Theme,
    style: Style,
}

impl Clock {
    /// Creates a clock from the guest's realtime clock.
    #[must_use]
    pub fn new() -> Self {
        Self::at(realtime_seconds())
    }

    /// Creates a clock at a fixed timestamp; useful for synthetic tests.
    #[must_use]
    pub fn at(seconds: u64) -> Self {
        Self {
            rect: Rect::empty(),
            seconds,
            text: format_clock(seconds),
            theme: Theme::dark(),
            style: Style::new(),
        }
    }

    /// Creates a themed clock at a fixed timestamp.
    #[must_use]
    pub const fn styled(seconds: u64, theme: Theme, style: Style) -> Self {
        Self {
            rect: Rect::empty(),
            seconds,
            text: format_clock(seconds),
            theme,
            style,
        }
    }

    /// The timestamp currently displayed by the clock.
    #[must_use]
    pub const fn seconds(&self) -> u64 {
        self.seconds
    }

    /// The rendered eight-byte `HH:MM:SS` text.
    #[must_use]
    pub const fn text(&self) -> &[u8] {
        &self.text
    }

    /// Updates the displayed timestamp.
    pub fn set_seconds(&mut self, seconds: u64) {
        self.seconds = seconds;
        self.text = format_clock(seconds);
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for Clock {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        canvas.fill_rect_clipped(self.rect, colors.surface, self.rect);
        let text_width = (self.text.len() * GLYPH_WIDTH) as u32;
        let x = self.rect.x + (self.rect.width.saturating_sub(text_width) / 2) as i32;
        let y = self.rect.y + self.rect.height.saturating_sub(GLYPH_HEIGHT as u32) as i32 / 2;
        canvas.draw_text_scaled(
            x,
            y.max(self.rect.y),
            &self.text,
            colors.foreground,
            1,
            self.rect,
        );
    }

    fn accessible_role(&self) -> &'static str {
        "clock"
    }

    fn accessible_label(&self) -> Option<&[u8]> {
        Some(&self.text)
    }
}

/// One application shown in the launcher grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LauncherEntry<'a> {
    /// Text drawn on the launcher button.
    pub label: &'a [u8],
    /// Stable identifier returned when the entry is activated.
    pub id: &'a [u8],
}

impl<'a> LauncherEntry<'a> {
    /// Creates a launcher entry.
    #[must_use]
    pub const fn new(label: &'a [u8], id: &'a [u8]) -> Self {
        Self { label, id }
    }
}

/// An owned launch target parsed from VFS metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherRecord {
    /// Stable identifier returned when the entry is activated.
    pub id: Vec<u8>,
    /// Text drawn on the launcher cell.
    pub label: Vec<u8>,
    /// VFS path of the executable this entry launches.
    pub target: Vec<u8>,
}

impl LauncherRecord {
    /// Creates an owned launch target whose executable path matches its id.
    #[must_use]
    pub fn new(id: &[u8], label: &[u8]) -> Self {
        Self::with_target(id, label, id)
    }

    /// Creates an owned launch target with an explicit executable path.
    #[must_use]
    pub fn with_target(id: &[u8], label: &[u8], target: &[u8]) -> Self {
        Self {
            id: id.to_vec(),
            label: label.to_vec(),
            target: target.to_vec(),
        }
    }

    /// The stable identifier.
    #[must_use]
    pub fn id(&self) -> &[u8] {
        &self.id
    }

    /// The displayed label.
    #[must_use]
    pub fn label(&self) -> &[u8] {
        &self.label
    }

    /// The VFS path of the executable to launch.
    #[must_use]
    pub fn target(&self) -> &[u8] {
        &self.target
    }

    /// Borrows this record as a declarative [`LauncherEntry`].
    #[must_use]
    pub fn entry(&self) -> LauncherEntry<'_> {
        LauncherEntry::new(&self.label, &self.id)
    }
}

/// A session-level request produced by a shell control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellSessionAction {
    /// Lock the current session.
    Lock,
    /// Leave the current session and return to a greeting screen.
    LogOut,
    /// Restart the machine.
    Reboot,
    /// Power the machine down.
    Shutdown,
}

impl ShellSessionAction {
    /// A short stable name for logging and introspection.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::LogOut => "logout",
            Self::Reboot => "reboot",
            Self::Shutdown => "shutdown",
        }
    }
}

/// An intent produced by shell interaction, ready for a host handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellAction {
    /// Launch the metadata entry with this stable identifier.
    Launch(Vec<u8>),
    /// Activate a generic panel applet with this stable identifier.
    Applet(Vec<u8>),
    /// Toggle a system-tray indicator.
    Tray {
        /// Index of the tray indicator.
        index: usize,
        /// Its state after the activation.
        active: bool,
    },
    /// Request a session-level action.
    Session(ShellSessionAction),
}

fn trim_line(mut line: &[u8]) -> &[u8] {
    while line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    line
}

/// Parses `id<TAB>label[<TAB>target]` lines into owned launch targets.
///
/// A missing target defaults to the id, so older two-column metadata remains
/// valid. Blank lines and lines starting with `#` are ignored. This is a
/// clean-room metadata format; no desktop-entry specification or foreign parser
/// is used.
#[must_use]
pub fn parse_launcher_records(bytes: &[u8]) -> Vec<LauncherRecord> {
    let mut records = Vec::new();
    for raw in bytes.split(|&byte| byte == b'\n') {
        let line = trim_line(raw);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let mut fields = line.split(|&byte| byte == b'\t');
        let Some(id) = fields.next() else {
            continue;
        };
        let Some(label) = fields.next() else {
            continue;
        };
        let target = fields
            .next()
            .filter(|field| !field.is_empty())
            .unwrap_or(id);
        if !id.is_empty() && !label.is_empty() {
            records.push(LauncherRecord::with_target(id, label, target));
        }
    }
    records
}

/// Visual state of one launcher cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LauncherCell {
    rect: Rect,
    hovered: bool,
    pressed: bool,
    focused: bool,
}

impl Widget for LauncherCell {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
    }

    fn paint(&self, _canvas: &mut Canvas<'_>) {}
}

/// A grid of launcher buttons with pointer and keyboard selection.
pub struct LauncherGrid {
    rect: Rect,
    records: Vec<LauncherRecord>,
    cells: Vec<LauncherCell>,
    columns: usize,
    spacing: u32,
    focus: Option<usize>,
    pressed: Option<usize>,
    activated: Option<usize>,
    theme: Theme,
    style: Style,
}

impl LauncherGrid {
    /// Builds a launcher from borrowed declarative entries.
    #[must_use]
    pub fn new(
        entries: &[LauncherEntry<'_>],
        columns: usize,
        spacing: u32,
        theme: Theme,
        style: Style,
    ) -> Self {
        let records = entries
            .iter()
            .map(|entry| LauncherRecord::new(entry.id, entry.label))
            .collect();
        Self::from_records(records, columns, spacing, theme, style)
    }

    /// Builds a launcher from owned records, such as VFS metadata.
    #[must_use]
    pub fn from_records(
        records: Vec<LauncherRecord>,
        columns: usize,
        spacing: u32,
        theme: Theme,
        style: Style,
    ) -> Self {
        let cells = vec![LauncherCell::default(); records.len()];
        Self {
            rect: Rect::empty(),
            records,
            cells,
            columns: columns.max(1),
            spacing,
            focus: None,
            pressed: None,
            activated: None,
            theme,
            style,
        }
    }

    /// Number of launcher entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the launcher has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The launch target at `index`.
    #[must_use]
    pub fn record(&self, index: usize) -> Option<&LauncherRecord> {
        self.records.get(index)
    }

    /// The focused entry index, if any.
    #[must_use]
    pub const fn focused_index(&self) -> Option<usize> {
        self.focus
    }

    /// The most recently activated entry index.
    #[must_use]
    pub const fn activated_index(&self) -> Option<usize> {
        self.activated
    }

    /// Takes the most recent activation and returns its stable identifier.
    pub fn take_activation(&mut self) -> Option<&[u8]> {
        let index = self.activated.take()?;
        self.records.get(index).map(LauncherRecord::id)
    }

    /// Focuses an entry, clearing focus from the previous one.
    pub fn focus_index(&mut self, next: Option<usize>) {
        let old = self.focus;
        if old == next {
            return;
        }
        self.focus = next.filter(|index| *index < self.cells.len());
        if let Some(index) = old {
            if let Some(cell) = self.cells.get_mut(index) {
                cell.focused = false;
            }
        }
        if let Some(index) = self.focus {
            if let Some(cell) = self.cells.get_mut(index) {
                cell.focused = true;
            }
        }
    }

    /// Collects launcher accessibility records in paint order.
    #[must_use]
    pub fn accessible(&self) -> Vec<Accessible<'_>> {
        self.records
            .iter()
            .map(|record| Accessible {
                role: "launch target",
                label: &record.label,
            })
            .collect()
    }

    fn move_focus(&mut self, dx: i32, dy: i32) -> bool {
        if self.cells.is_empty() {
            return false;
        }
        let columns = self.columns.min(self.cells.len());
        let rows = self.cells.len().div_ceil(columns);
        let current = self.focus.unwrap_or(0);
        let row = current / columns;
        let column = current % columns;
        let next_row = (row as i32 + dy).clamp(0, rows as i32 - 1) as usize;
        let next_column = (column as i32 + dx).clamp(0, columns as i32 - 1) as usize;
        let mut next = next_row * columns + next_column;
        if next >= self.cells.len() {
            next = self.cells.len() - 1;
        }
        self.focus_index(Some(next));
        true
    }
}

impl Widget for LauncherGrid {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
        let mut cells: Vec<&mut dyn Widget> = self
            .cells
            .iter_mut()
            .map(|cell| cell as &mut dyn Widget)
            .collect();
        GridLayout::new(self.columns, self.spacing).arrange(area, &mut cells);
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        canvas.fill_rect_clipped(self.rect, colors.background, self.rect);
        for rect in [
            Rect::new(self.rect.x, self.rect.y, self.rect.width, 1),
            Rect::new(self.rect.x, self.rect.bottom() - 1, self.rect.width, 1),
            Rect::new(self.rect.x, self.rect.y, 1, self.rect.height),
            Rect::new(self.rect.right() - 1, self.rect.y, 1, self.rect.height),
        ] {
            canvas.fill_rect_clipped(rect, colors.accent, self.rect);
        }
        for (index, cell) in self.cells.iter().enumerate() {
            let face = if cell.pressed {
                colors.surface_pressed
            } else if cell.hovered {
                colors.surface_hover
            } else {
                colors.surface
            };
            canvas.fill_rect_clipped(cell.rect, face, cell.rect);
            let border = if cell.focused {
                colors.accent
            } else {
                colors.border
            };
            for rect in [
                Rect::new(cell.rect.x, cell.rect.y, cell.rect.width, 1),
                Rect::new(cell.rect.x, cell.rect.bottom() - 1, cell.rect.width, 1),
                Rect::new(cell.rect.x, cell.rect.y, 1, cell.rect.height),
                Rect::new(cell.rect.right() - 1, cell.rect.y, 1, cell.rect.height),
            ] {
                canvas.fill_rect_clipped(rect, border, cell.rect);
            }
            if let Some(record) = self.records.get(index) {
                let label = record.label();
                let x = cell.rect.x + 2;
                let y =
                    cell.rect.y + cell.rect.height.saturating_sub(GLYPH_HEIGHT as u32) as i32 / 2;
                canvas.draw_text_scaled(
                    x,
                    y.max(cell.rect.y),
                    label,
                    colors.foreground,
                    1,
                    cell.rect,
                );
            }
        }
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::PointerMove { x, y, .. } => {
                let point = Point::new(x, y);
                let mut handled = false;
                for cell in &mut self.cells {
                    cell.hovered = cell.rect.contains(point);
                    handled |= cell.hovered;
                }
                handled
            }
            Event::PointerDown { x, y, button } => {
                if button != 0 {
                    return false;
                }
                let point = Point::new(x, y);
                let hit = self
                    .cells
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, cell)| cell.rect.contains(point))
                    .map(|(index, _)| index);
                if let Some(index) = hit {
                    self.cells[index].pressed = true;
                    self.pressed = Some(index);
                    self.focus_index(Some(index));
                }
                hit.is_some()
            }
            Event::PointerUp { x, y, button } => {
                if button != 0 {
                    return false;
                }
                for cell in &mut self.cells {
                    cell.pressed = false;
                }
                let Some(index) = self.pressed.take() else {
                    return false;
                };
                let clicked = self.cells[index].rect.contains(Point::new(x, y));
                if clicked {
                    self.activated = Some(index);
                }
                true
            }
            Event::KeyDown { code, .. } => match code {
                KEY_LEFT => self.move_focus(-1, 0),
                KEY_RIGHT => self.move_focus(1, 0),
                KEY_UP => self.move_focus(0, -1),
                KEY_DOWN => self.move_focus(0, 1),
                ENTER => {
                    if let Some(index) = self.focus {
                        self.activated = Some(index);
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            },
            Event::Scroll { .. } | Event::KeyUp { .. } | Event::Ignored => false,
        }
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        if !focused {
            self.focus_index(None);
            self.pressed = None;
            for cell in &mut self.cells {
                cell.hovered = false;
            }
        }
    }

    fn accessible_role(&self) -> &'static str {
        "launcher"
    }
}

/// What activating a tray slot means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    /// A conventional on/off status indicator.
    Indicator,
    /// A session request, such as shutdown.
    Session(ShellSessionAction),
}

/// One indicator in the system tray.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrayItem<'a> {
    /// Short label drawn in the tray slot.
    pub label: &'a [u8],
    /// Whether the indicator starts active.
    pub active: bool,
    /// The action produced when this slot is activated.
    pub action: TrayAction,
}

impl<'a> TrayItem<'a> {
    /// Creates a status indicator.
    #[must_use]
    pub const fn new(label: &'a [u8], active: bool) -> Self {
        Self {
            label,
            active,
            action: TrayAction::Indicator,
        }
    }

    /// Creates a tray slot that requests a session action.
    #[must_use]
    pub const fn session(label: &'a [u8], action: ShellSessionAction) -> Self {
        Self {
            label,
            active: false,
            action: TrayAction::Session(action),
        }
    }
}

/// A row of toggleable tray indicators.
pub struct SystemTray<'a> {
    rect: Rect,
    buttons: Vec<Button<'a>>,
    active: Vec<bool>,
    actions: Vec<TrayAction>,
    focus: Option<usize>,
    pressed: Option<usize>,
    activated: Option<usize>,
    theme: Theme,
    style: Style,
}

impl<'a> SystemTray<'a> {
    /// Builds a tray from `items`.
    #[must_use]
    pub fn new(items: &'a [TrayItem<'a>], theme: Theme, style: Style) -> Self {
        let buttons = items
            .iter()
            .map(|item| Button::styled(item.label, theme, style))
            .collect();
        let active = items.iter().map(|item| item.active).collect();
        let actions = items.iter().map(|item| item.action).collect();
        Self {
            rect: Rect::empty(),
            buttons,
            active,
            actions,
            focus: None,
            pressed: None,
            activated: None,
            theme,
            style,
        }
    }

    /// Number of tray indicators.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buttons.len()
    }

    /// Whether the tray is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buttons.is_empty()
    }

    /// Whether indicator `index` is active.
    #[must_use]
    pub fn active(&self, index: usize) -> Option<bool> {
        self.active.get(index).copied()
    }

    /// The action assigned to tray slot `index`.
    #[must_use]
    pub fn action(&self, index: usize) -> Option<TrayAction> {
        self.actions.get(index).copied()
    }

    /// Toggles indicator `index` and returns its new state.
    pub fn toggle(&mut self, index: usize) -> Option<bool> {
        let state = self.active.get_mut(index)?;
        *state = !*state;
        Some(*state)
    }

    /// The focused indicator index, if any.
    #[must_use]
    pub const fn focused_index(&self) -> Option<usize> {
        self.focus
    }

    /// The most recently activated indicator index.
    #[must_use]
    pub const fn activated_index(&self) -> Option<usize> {
        self.activated
    }

    /// Clears the most recent activation.
    pub fn clear_activation(&mut self) {
        self.activated = None;
    }

    /// Clears a pending pointer press, for example when focus leaves the tray.
    pub fn clear_pressed(&mut self) {
        self.pressed = None;
    }

    fn activate(&mut self, index: usize) {
        self.activated = Some(index);
        if self.actions.get(index) == Some(&TrayAction::Indicator) {
            self.toggle(index);
        }
    }

    /// Focuses an indicator, clearing focus from the previous one.
    pub fn focus_index(&mut self, next: Option<usize>) {
        let old = self.focus;
        if old == next {
            return;
        }
        self.focus = next.filter(|index| *index < self.buttons.len());
        if let Some(index) = old {
            if let Some(button) = self.buttons.get_mut(index) {
                button.set_focus(false);
            }
        }
        if let Some(index) = self.focus {
            if let Some(button) = self.buttons.get_mut(index) {
                button.set_focus(true);
            }
        }
    }

    /// Collects tray accessibility records in paint order.
    #[must_use]
    pub fn accessible(&self) -> Vec<Accessible<'_>> {
        self.buttons
            .iter()
            .enumerate()
            .map(|(index, button)| Accessible {
                role: match self.actions.get(index) {
                    Some(TrayAction::Session(_)) => "session action",
                    _ => "tray item",
                },
                label: button.accessible_label().unwrap_or_default(),
            })
            .collect()
    }
}

impl Widget for SystemTray<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
        let mut widgets: Vec<&mut dyn Widget> = self
            .buttons
            .iter_mut()
            .map(|button| button as &mut dyn Widget)
            .collect();
        HorizontalLayout::new(0).arrange(area, &mut widgets);
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        for button in &self.buttons {
            button.paint(canvas);
        }
        for (index, button) in self.buttons.iter().enumerate() {
            let slot = button.rect();
            let indicator = Rect::new(slot.right() - 4, slot.y + 3, 2, 2);
            let color = if self.active.get(index).copied().unwrap_or(false) {
                colors.accent
            } else {
                colors.dim
            };
            canvas.fill_rect_clipped(indicator, color, slot);
        }
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::PointerMove { .. } => {
                let mut handled = false;
                for button in &mut self.buttons {
                    handled |= button.event(event);
                }
                handled
            }
            Event::PointerDown { .. } => {
                let hit = self
                    .buttons
                    .iter_mut()
                    .enumerate()
                    .rev()
                    .find_map(|(index, button)| button.event(event).then_some(index));
                if let Some(index) = hit {
                    self.pressed = Some(index);
                    self.focus_index(Some(index));
                }
                hit.is_some()
            }
            Event::PointerUp { .. } => {
                for button in &mut self.buttons {
                    let _ = button.event(event);
                }
                let Some(index) = self.pressed.take() else {
                    return false;
                };
                if self.buttons[index].was_clicked() {
                    self.activate(index);
                }
                true
            }
            Event::KeyDown { code: ENTER, .. } => {
                if let Some(index) = self.focus {
                    self.activate(index);
                    true
                } else {
                    false
                }
            }
            Event::Scroll { .. } | Event::KeyDown { .. } | Event::KeyUp { .. } | Event::Ignored => {
                false
            }
        }
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        if !focused {
            self.focus_index(None);
        }
    }

    fn accessible_role(&self) -> &'static str {
        "system tray"
    }
}

/// A generic text applet occupying a panel slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applet<'a> {
    rect: Rect,
    id: &'a [u8],
    label: &'a [u8],
    focused: bool,
    activated: bool,
    theme: Theme,
    style: Style,
}

impl<'a> Applet<'a> {
    /// Creates an applet with an identifier and a short status label.
    #[must_use]
    pub const fn new(id: &'a [u8], label: &'a [u8], theme: Theme, style: Style) -> Self {
        Self {
            rect: Rect::empty(),
            id,
            label,
            focused: false,
            activated: false,
            theme,
            style,
        }
    }

    /// The applet's stable identifier.
    #[must_use]
    pub const fn id(&self) -> &'a [u8] {
        self.id
    }

    /// The applet's displayed label.
    #[must_use]
    pub const fn label(&self) -> &'a [u8] {
        self.label
    }

    /// Whether the applet currently owns keyboard focus.
    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    /// Takes a pending activation produced by pointer or Enter input.
    pub fn take_activation(&mut self) -> Option<&'a [u8]> {
        if self.activated {
            self.activated = false;
            Some(self.id)
        } else {
            None
        }
    }
}

impl Widget for Applet<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        canvas.fill_rect_clipped(self.rect, colors.surface, self.rect);
        let border = if self.focused {
            colors.accent
        } else {
            colors.border
        };
        for rect in [
            Rect::new(self.rect.x, self.rect.y, self.rect.width, 1),
            Rect::new(self.rect.x, self.rect.bottom() - 1, self.rect.width, 1),
            Rect::new(self.rect.x, self.rect.y, 1, self.rect.height),
            Rect::new(self.rect.right() - 1, self.rect.y, 1, self.rect.height),
        ] {
            canvas.fill_rect_clipped(rect, border, self.rect);
        }
        let x = self.rect.x + 2;
        let y = self.rect.y + self.rect.height.saturating_sub(GLYPH_HEIGHT as u32) as i32 / 2;
        canvas.draw_text_scaled(
            x,
            y.max(self.rect.y),
            self.label,
            colors.foreground,
            1,
            self.rect,
        );
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::PointerUp { x, y, button: 0 } => {
                if self.rect.contains(Point::new(x, y)) {
                    self.activated = true;
                    true
                } else {
                    false
                }
            }
            Event::KeyDown { code: ENTER, .. } if self.focused => {
                self.activated = true;
                true
            }
            _ => false,
        }
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn accessible_role(&self) -> &'static str {
        "applet"
    }

    fn accessible_label(&self) -> Option<&[u8]> {
        Some(self.label)
    }
}

/// Which panel control currently owns keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelFocus {
    /// The launcher toggle button.
    Toggle,
    /// A generic applet slot by index.
    Applet(usize),
    /// The system tray.
    Tray,
}

/// The horizontal panel strip at the top of the desktop.
pub struct Panel<'a> {
    rect: Rect,
    toggle: Button<'a>,
    clock: Clock,
    applets: Vec<Applet<'a>>,
    tray: SystemTray<'a>,
    theme: Theme,
    style: Style,
    toggle_pressed: bool,
    toggle_event: bool,
}

impl<'a> Panel<'a> {
    /// Builds a panel with a launcher toggle label and tray indicators.
    #[must_use]
    pub fn new(
        launcher_label: &'a [u8],
        tray_items: &'a [TrayItem<'a>],
        theme: Theme,
        style: Style,
    ) -> Self {
        Self {
            rect: Rect::empty(),
            toggle: Button::styled(launcher_label, theme, style),
            clock: Clock::new(),
            applets: Vec::new(),
            tray: SystemTray::new(tray_items, theme, style),
            theme,
            style,
            toggle_pressed: false,
            toggle_event: false,
        }
    }

    /// Builds a panel with a fixed clock timestamp for synthetic tests.
    #[must_use]
    pub fn at(
        launcher_label: &'a [u8],
        tray_items: &'a [TrayItem<'a>],
        seconds: u64,
        theme: Theme,
        style: Style,
    ) -> Self {
        let mut panel = Self::new(launcher_label, tray_items, theme, style);
        panel.clock.set_seconds(seconds);
        panel
    }

    /// The clock displayed in the panel.
    #[must_use]
    pub const fn clock(&self) -> &Clock {
        &self.clock
    }

    /// The system tray displayed in the panel.
    #[must_use]
    pub const fn tray(&self) -> &SystemTray<'a> {
        &self.tray
    }

    /// Updates the panel clock.
    pub fn set_clock_seconds(&mut self, seconds: u64) {
        self.clock.set_seconds(seconds);
    }

    /// Adds a generic applet to the panel's centre slots.
    pub fn add_applet(&mut self, id: &'a [u8], label: &'a [u8]) {
        let theme = self.theme;
        let style = self.style;
        self.applets.push(Applet::new(id, label, theme, style));
    }

    /// Number of generic applet slots.
    #[must_use]
    pub fn applet_count(&self) -> usize {
        self.applets.len()
    }

    /// The identifier of applet slot `index`.
    #[must_use]
    pub fn applet_id(&self, index: usize) -> Option<&'a [u8]> {
        self.applets.get(index).map(Applet::id)
    }

    /// The label of applet slot `index`.
    #[must_use]
    pub fn applet_label(&self, index: usize) -> Option<&'a [u8]> {
        self.applets.get(index).map(Applet::label)
    }

    /// The rectangle assigned to the system tray.
    #[must_use]
    pub fn tray_rect(&self) -> Rect {
        self.tray.rect()
    }

    /// The action assigned to tray slot `index`.
    #[must_use]
    pub fn tray_action(&self, index: usize) -> Option<TrayAction> {
        self.tray.action(index)
    }

    /// Takes a pending applet activation.
    pub fn take_applet_activation(&mut self) -> Option<&'a [u8]> {
        for applet in &mut self.applets {
            if let Some(id) = applet.take_activation() {
                return Some(id);
            }
        }
        None
    }

    /// Focuses a specific tray slot.
    pub fn focus_tray(&mut self, index: usize) {
        self.set_panel_focus(Some(PanelFocus::Tray));
        if index < self.tray.len() {
            self.tray.focus_index(Some(index));
        }
    }

    /// Clears a tray activation after the host has observed it.
    pub fn clear_tray_activation(&mut self) {
        self.tray.clear_activation();
    }

    /// Which panel control currently owns keyboard focus.
    #[must_use]
    pub fn focus(&self) -> Option<PanelFocus> {
        if self.toggle.is_focused() {
            return Some(PanelFocus::Toggle);
        }
        for (index, applet) in self.applets.iter().enumerate() {
            if applet.is_focused() {
                return Some(PanelFocus::Applet(index));
            }
        }
        if self.tray.focused_index().is_some() {
            return Some(PanelFocus::Tray);
        }
        None
    }

    /// Moves keyboard focus to one panel control, clearing all others.
    pub fn set_panel_focus(&mut self, target: Option<PanelFocus>) {
        self.toggle.set_focus(false);
        for applet in &mut self.applets {
            applet.set_focus(false);
        }
        self.tray.set_focus(false);
        match target {
            Some(PanelFocus::Toggle) => self.toggle.set_focus(true),
            Some(PanelFocus::Applet(index)) => {
                if let Some(applet) = self.applets.get_mut(index) {
                    applet.set_focus(true);
                }
            }
            Some(PanelFocus::Tray) => self.tray.focus_index(Some(0)),
            None => {}
        }
    }

    /// Takes a launcher toggle event produced by pointer or Enter input.
    pub fn take_toggle(&mut self) -> bool {
        let toggled = self.toggle_event;
        self.toggle_event = false;
        toggled
    }

    /// Collects panel accessibility records in paint order.
    #[must_use]
    pub fn accessible(&self) -> Vec<Accessible<'_>> {
        let mut records = vec![Accessible {
            role: self.toggle.accessible_role(),
            label: self.toggle.accessible_label().unwrap_or_default(),
        }];
        records.push(Accessible {
            role: self.clock.accessible_role(),
            label: self.clock.text(),
        });
        for applet in &self.applets {
            records.push(Accessible {
                role: applet.accessible_role(),
                label: applet.label(),
            });
        }
        records.extend(self.tray.accessible());
        records
    }
}

impl Widget for Panel<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
        let inner = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(2),
        );
        let toggle_width = inner.width / 4;
        let tray_width = inner.width / 4;
        let middle_width = inner.width.saturating_sub(toggle_width + tray_width);
        let slot_width = middle_width / (1 + self.applets.len()) as u32;
        self.toggle
            .layout(Rect::new(inner.x, inner.y, toggle_width, inner.height));
        let mut x = inner.x + toggle_width as i32;
        self.clock
            .layout(Rect::new(x, inner.y, slot_width, inner.height));
        x += slot_width as i32;
        for applet in &mut self.applets {
            applet.layout(Rect::new(x, inner.y, slot_width, inner.height));
            x += slot_width as i32;
        }
        self.tray.layout(Rect::new(
            inner.right() - tray_width as i32,
            inner.y,
            tray_width,
            inner.height,
        ));
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        canvas.fill_rect_clipped(self.rect, colors.background, self.rect);
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.bottom() - 1, self.rect.width, 1),
            colors.accent,
            self.rect,
        );
        self.toggle.paint(canvas);
        self.clock.paint(canvas);
        for applet in &self.applets {
            applet.paint(canvas);
        }
        self.tray.paint(canvas);
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::PointerDown { .. } => {
                self.toggle_pressed = false;
                if self.toggle.event(event) {
                    self.toggle_pressed = true;
                    self.set_panel_focus(Some(PanelFocus::Toggle));
                    return true;
                }
                let point = event.position().unwrap_or_default();
                let applet_hit = self
                    .applets
                    .iter_mut()
                    .enumerate()
                    .find_map(|(index, applet)| {
                        let _ = applet.event(event);
                        applet.rect().contains(point).then_some(index)
                    });
                if let Some(index) = applet_hit {
                    self.set_panel_focus(Some(PanelFocus::Applet(index)));
                    return true;
                }
                if self.tray.event(event) {
                    self.set_panel_focus(Some(PanelFocus::Tray));
                    return true;
                }
                false
            }
            Event::PointerUp { .. } => {
                if self.toggle_pressed {
                    self.toggle_pressed = false;
                    let _ = self.toggle.event(event);
                    if self.toggle.was_clicked() {
                        self.toggle_event = true;
                    }
                    return true;
                }
                if self.toggle.event(event) {
                    return true;
                }
                for applet in &mut self.applets {
                    let _ = applet.event(event);
                }
                self.tray.event(event)
            }
            Event::PointerMove { .. } => {
                let toggle_handled = self.toggle.event(event);
                for applet in &mut self.applets {
                    let _ = applet.event(event);
                }
                self.tray.event(event) || toggle_handled
            }
            Event::KeyDown { code: ENTER, .. } if self.toggle.is_focused() => {
                self.toggle_event = true;
                true
            }
            Event::KeyDown { .. } | Event::KeyUp { .. } => {
                let mut handled = self.tray.event(event);
                for applet in &mut self.applets {
                    handled |= applet.event(event);
                }
                handled |= self.toggle.event(event);
                handled
            }
            Event::Scroll { .. } | Event::Ignored => false,
        }
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        if !focused {
            self.set_panel_focus(None);
            self.toggle_pressed = false;
            self.tray.clear_pressed();
        }
    }

    fn accessible_role(&self) -> &'static str {
        "panel"
    }
}

/// Which shell control currently owns keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellFocus {
    /// The panel's launcher toggle.
    Toggle,
    /// A generic panel applet slot by index.
    Applet(usize),
    /// The system tray.
    Tray,
    /// The launcher grid.
    Launcher,
}

impl ShellFocus {
    /// A short stable name for introspection.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Toggle => "toggle",
            Self::Applet(_) => "applet",
            Self::Tray => "tray",
            Self::Launcher => "launcher",
        }
    }
}

/// A minimal desktop shell: a panel plus an optional launcher grid.
pub struct DesktopShell<'a> {
    rect: Rect,
    layout: ShellLayout,
    panel: Panel<'a>,
    launcher: LauncherGrid,
    launcher_visible: bool,
    return_focus: Option<ShellFocus>,
    actions: Vec<ShellAction>,
}

impl<'a> DesktopShell<'a> {
    /// Builds a shell from launcher entries and tray indicators.
    #[must_use]
    pub fn new(
        launcher_label: &'a [u8],
        entries: &'a [LauncherEntry<'a>],
        tray_items: &'a [TrayItem<'a>],
        theme: Theme,
        style: Style,
        layout: ShellLayout,
    ) -> Self {
        Self {
            rect: Rect::empty(),
            layout,
            panel: Panel::new(launcher_label, tray_items, theme, style),
            launcher: LauncherGrid::new(
                entries,
                layout.launcher_columns,
                layout.launcher_spacing,
                theme,
                style,
            ),
            launcher_visible: false,
            return_focus: None,
            actions: Vec::new(),
        }
    }

    /// Builds a shell from owned launch records, such as VFS metadata.
    #[must_use]
    pub fn from_records(
        launcher_label: &'a [u8],
        records: Vec<LauncherRecord>,
        tray_items: &'a [TrayItem<'a>],
        theme: Theme,
        style: Style,
        layout: ShellLayout,
    ) -> Self {
        Self {
            rect: Rect::empty(),
            layout,
            panel: Panel::new(launcher_label, tray_items, theme, style),
            launcher: LauncherGrid::from_records(
                records,
                layout.launcher_columns,
                layout.launcher_spacing,
                theme,
                style,
            ),
            launcher_visible: false,
            return_focus: None,
            actions: Vec::new(),
        }
    }

    /// Builds a shell with a fixed clock timestamp for synthetic tests.
    #[must_use]
    pub fn at(
        launcher_label: &'a [u8],
        entries: &'a [LauncherEntry<'a>],
        tray_items: &'a [TrayItem<'a>],
        seconds: u64,
        theme: Theme,
        style: Style,
        layout: ShellLayout,
    ) -> Self {
        let mut shell = Self::new(launcher_label, entries, tray_items, theme, style, layout);
        shell.panel.set_clock_seconds(seconds);
        shell
    }

    /// Whether the launcher grid is currently visible.
    #[must_use]
    pub const fn launcher_visible(&self) -> bool {
        self.launcher_visible
    }

    /// Shows or hides the launcher grid.
    pub fn set_launcher_visible(&mut self, visible: bool) {
        if visible == self.launcher_visible {
            return;
        }
        self.launcher_visible = visible;
        if visible {
            self.return_focus = Some(self.focus().unwrap_or(ShellFocus::Toggle));
            self.apply_focus(ShellFocus::Launcher);
        } else {
            let target = self.return_focus.take().unwrap_or(ShellFocus::Toggle);
            self.apply_focus(target);
        }
    }

    /// The panel focus remembered for restoration when the launcher closes.
    #[must_use]
    pub const fn focus_memory(&self) -> Option<ShellFocus> {
        self.return_focus
    }

    /// Adds a generic applet to the panel.
    pub fn add_applet(&mut self, id: &'a [u8], label: &'a [u8]) {
        self.panel.add_applet(id, label);
    }

    /// Number of generic applet slots.
    #[must_use]
    pub fn applet_count(&self) -> usize {
        self.panel.applet_count()
    }

    /// The identifier of applet slot `index`.
    #[must_use]
    pub fn applet_id(&self, index: usize) -> Option<&'a [u8]> {
        self.panel.applet_id(index)
    }

    /// The label of applet slot `index`.
    #[must_use]
    pub fn applet_label(&self, index: usize) -> Option<&'a [u8]> {
        self.panel.applet_label(index)
    }

    /// The rectangle assigned to the system tray.
    #[must_use]
    pub fn tray_rect(&self) -> Rect {
        self.panel.tray_rect()
    }

    /// Which control currently owns keyboard focus.
    #[must_use]
    pub fn focus(&self) -> Option<ShellFocus> {
        if self.launcher.focused_index().is_some() && self.launcher_visible {
            return Some(ShellFocus::Launcher);
        }
        self.panel.focus().map(|focus| match focus {
            PanelFocus::Toggle => ShellFocus::Toggle,
            PanelFocus::Applet(index) => ShellFocus::Applet(index),
            PanelFocus::Tray => ShellFocus::Tray,
        })
    }

    fn focus_targets(&self) -> Vec<ShellFocus> {
        let mut targets = vec![ShellFocus::Toggle];
        for index in 0..self.panel.applet_count() {
            targets.push(ShellFocus::Applet(index));
        }
        targets.push(ShellFocus::Tray);
        if self.launcher_visible {
            targets.push(ShellFocus::Launcher);
        }
        targets
    }

    fn apply_focus(&mut self, target: ShellFocus) {
        self.panel.set_panel_focus(None);
        self.launcher.set_focus(false);
        match target {
            ShellFocus::Toggle => self.panel.set_panel_focus(Some(PanelFocus::Toggle)),
            ShellFocus::Applet(index) => {
                self.panel.set_panel_focus(Some(PanelFocus::Applet(index)));
            }
            ShellFocus::Tray => self.panel.set_panel_focus(Some(PanelFocus::Tray)),
            ShellFocus::Launcher => self.launcher.focus_index(Some(0)),
        }
    }

    fn cycle_focus(&mut self, forward: bool) {
        let targets = self.focus_targets();
        if targets.is_empty() {
            return;
        }
        let current = self
            .focus()
            .and_then(|focus| targets.iter().position(|target| *target == focus));
        let next = match current {
            Some(index) => {
                let step = if forward { 1 } else { targets.len() - 1 };
                targets[(index + step) % targets.len()]
            }
            None if forward => targets[0],
            None => targets[targets.len() - 1],
        };
        self.apply_focus(next);
    }

    /// Moves keyboard focus to an explicit shell control.
    pub fn set_shell_focus(&mut self, target: ShellFocus) {
        self.apply_focus(target);
    }

    /// Moves keyboard focus to a specific system-tray slot.
    pub fn focus_tray(&mut self, index: usize) {
        self.panel.focus_tray(index);
    }

    /// Posts an action for a host handler.
    pub fn post_action(&mut self, action: ShellAction) {
        self.actions.push(action);
    }

    /// Posts a session request for a host handler.
    pub fn post_session_action(&mut self, action: ShellSessionAction) {
        self.post_action(ShellAction::Session(action));
    }

    /// Takes all actions produced since the previous call.
    pub fn take_actions(&mut self) -> Vec<ShellAction> {
        core::mem::take(&mut self.actions)
    }

    /// The panel clock.
    #[must_use]
    pub const fn clock(&self) -> &Clock {
        self.panel.clock()
    }

    /// The launcher grid.
    #[must_use]
    pub const fn launcher(&self) -> &LauncherGrid {
        &self.launcher
    }

    /// The system tray.
    #[must_use]
    pub const fn tray(&self) -> &SystemTray<'a> {
        self.panel.tray()
    }

    /// Updates the clock displayed in the panel.
    pub fn set_clock_seconds(&mut self, seconds: u64) {
        self.panel.set_clock_seconds(seconds);
    }

    /// Takes the stable identifier of the most recent launcher activation.
    pub fn take_launcher_activation(&mut self) -> Option<&[u8]> {
        self.launcher.take_activation()
    }

    /// The rectangle assigned to the panel.
    #[must_use]
    pub fn panel_rect(&self) -> Rect {
        Rect::new(
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.layout.panel_height.min(self.rect.height),
        )
    }

    /// The rectangle assigned to the launcher grid.
    #[must_use]
    pub fn launcher_rect(&self) -> Rect {
        let gap = self.layout.launcher_gap;
        let y = self.panel_rect().bottom() + gap as i32;
        let available = self.rect.bottom().saturating_sub(y + gap as i32).max(0) as u32;
        let width = self
            .layout
            .launcher_width
            .min(self.rect.width.saturating_sub(gap.saturating_mul(2)));
        let height = self.layout.launcher_height.min(available);
        Rect::new(self.rect.x + gap as i32, y, width, height)
    }

    /// Collects visible shell accessibility records, with the launcher first
    /// when it is open and the panel painted last.
    #[must_use]
    pub fn accessible(&self) -> Vec<Accessible<'_>> {
        let mut records = Vec::new();
        if self.launcher_visible {
            records.extend(self.launcher.accessible());
        }
        records.extend(self.panel.accessible());
        records
    }
}

impl Widget for DesktopShell<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
        self.panel.layout(self.panel_rect());
        self.launcher.layout(self.launcher_rect());
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let theme = Theme::dark();
        canvas.fill_rect_clipped(self.rect, theme.background, self.rect);
        if self.launcher_visible {
            self.launcher.paint(canvas);
        }
        self.panel.paint(canvas);
    }

    fn event(&mut self, event: Event) -> bool {
        if let Event::KeyDown { code, modifiers } = event {
            if code == TAB {
                self.cycle_focus(modifiers & MOD_SHIFT == 0);
                return true;
            }
        }
        if matches!(event, Event::KeyDown { code: ESCAPE, .. }) && self.launcher_visible {
            self.set_launcher_visible(false);
            return true;
        }
        if self.launcher_visible {
            let before = self.launcher.activated_index();
            let handled = self.launcher.event(event);
            let after = self.launcher.activated_index();
            if handled {
                if after.is_some() && after != before {
                    if let Some(index) = after {
                        if let Some(record) = self.launcher.record(index) {
                            self.actions.push(ShellAction::Launch(record.id().to_vec()));
                        }
                    }
                }
                return true;
            }
        }
        let tray_before = self.tray().activated_index();
        if self.panel.event(event) {
            if self.panel.take_toggle() {
                self.set_launcher_visible(!self.launcher_visible);
            }
            if let Some(id) = self.panel.take_applet_activation() {
                self.actions.push(ShellAction::Applet(id.to_vec()));
            }
            let tray_after = self.tray().activated_index();
            if tray_after.is_some() && tray_after != tray_before {
                if let Some(index) = tray_after {
                    let active = self.tray().active(index).unwrap_or(false);
                    match self.panel.tray_action(index) {
                        Some(TrayAction::Session(action)) => {
                            self.actions.push(ShellAction::Session(action));
                        }
                        Some(TrayAction::Indicator) | None => {
                            self.actions.push(ShellAction::Tray { index, active });
                        }
                    }
                    self.panel.clear_tray_activation();
                }
            }
            return true;
        }
        false
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        if !focused {
            self.panel.set_focus(false);
            self.launcher.set_focus(false);
        }
    }

    fn accessible_role(&self) -> &'static str {
        "desktop shell"
    }
}

//! Declarative view descriptions compiled into concrete widgets.
//!
//! A [`View`] is a small, copyable description of a control or a container plus
//! its style overrides. [`ViewBuilder`] resolves those descriptions against a
//! [`Theme`] and stores the resulting widget tree; [`ViewBuilder::ui`] lends it
//! to an [`Ui`] for layout, painting and event dispatch. Containers own their
//! children and apply a reusable [`Layout`], so views can nest arbitrarily
//! without self-references or a foreign markup language.

use crate::{
    Accessible, Button, Canvas, Event, GridLayout, HorizontalLayout, Label, Layout, Rect, TextArea,
    TextInput, TextView, Theme, Ui, VerticalLayout, Widget, theme::Style,
};
use alloc::vec;
use alloc::vec::Vec;

/// A declarative description of a control or container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View<'a> {
    /// A single line of static text.
    Label {
        /// Text to display.
        text: &'a [u8],
        /// Overrides applied on top of the builder's theme.
        style: Style,
    },
    /// A scrollable, read-only block of text.
    Text {
        /// Newline-separated text to display.
        text: &'a [u8],
        /// Overrides applied on top of the builder's theme.
        style: Style,
    },
    /// A push button.
    Button {
        /// Button caption.
        text: &'a [u8],
        /// Overrides applied on top of the builder's theme.
        style: Style,
    },
    /// A single-line editable field.
    Input {
        /// Maximum number of bytes the field accepts.
        max_len: usize,
        /// Overrides applied on top of the builder's theme.
        style: Style,
    },
    /// A multi-line editable text area.
    TextArea {
        /// Maximum number of bytes the editor accepts.
        max_bytes: usize,
        /// Overrides applied on top of the builder's theme.
        style: Style,
    },
    /// A container that stacks its children vertically.
    Column {
        /// Child views, painted in order.
        children: &'a [View<'a>],
        /// Pixels between adjacent children.
        spacing: u32,
        /// Container background overrides.
        style: Style,
    },
    /// A container that places its children in a row.
    Row {
        /// Child views, painted in order.
        children: &'a [View<'a>],
        /// Pixels between adjacent children.
        spacing: u32,
        /// Container background overrides.
        style: Style,
    },
    /// A container that places its children in a row-major grid.
    Grid {
        /// Child views, painted in order.
        children: &'a [View<'a>],
        /// Number of columns.
        columns: usize,
        /// Pixels between adjacent children.
        spacing: u32,
        /// Container background overrides.
        style: Style,
    },
}

impl<'a> View<'a> {
    /// Describes a static label with style overrides.
    #[must_use]
    pub const fn label(text: &'a [u8], style: Style) -> Self {
        Self::Label { text, style }
    }

    /// Describes a multi-line text block with style overrides.
    #[must_use]
    pub const fn text(text: &'a [u8], style: Style) -> Self {
        Self::Text { text, style }
    }

    /// Describes a button with style overrides.
    #[must_use]
    pub const fn button(text: &'a [u8], style: Style) -> Self {
        Self::Button { text, style }
    }

    /// Describes a text field with style overrides.
    #[must_use]
    pub const fn input(max_len: usize, style: Style) -> Self {
        Self::Input { max_len, style }
    }

    /// Describes a multi-line editor with style overrides.
    #[must_use]
    pub const fn textarea(max_bytes: usize, style: Style) -> Self {
        Self::TextArea { max_bytes, style }
    }

    /// Describes a vertical container with style overrides.
    #[must_use]
    pub const fn column(children: &'a [View<'a>], spacing: u32, style: Style) -> Self {
        Self::Column {
            children,
            spacing,
            style,
        }
    }

    /// Describes a horizontal container with style overrides.
    #[must_use]
    pub const fn row(children: &'a [View<'a>], spacing: u32, style: Style) -> Self {
        Self::Row {
            children,
            spacing,
            style,
        }
    }

    /// Describes a grid container with style overrides.
    #[must_use]
    pub const fn grid(
        children: &'a [View<'a>],
        columns: usize,
        spacing: u32,
        style: Style,
    ) -> Self {
        Self::Grid {
            children,
            columns,
            spacing,
            style,
        }
    }
}

/// The layout strategy stored inside a compiled container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutMode {
    /// Vertical stacking.
    Vertical(u32),
    /// Horizontal row.
    Horizontal(u32),
    /// Row-major grid.
    Grid(usize, u32),
}

impl Layout for LayoutMode {
    fn arrange(&self, area: Rect, widgets: &mut [&mut dyn Widget]) {
        match *self {
            Self::Vertical(spacing) => VerticalLayout::new(spacing).arrange(area, widgets),
            Self::Horizontal(spacing) => HorizontalLayout::new(spacing).arrange(area, widgets),
            Self::Grid(columns, spacing) => {
                GridLayout::new(columns, spacing).arrange(area, widgets);
            }
        }
    }
}

/// A container that owns its children and routes focus into them.
struct Container<'a> {
    rect: Rect,
    children: Vec<BuiltWidget<'a>>,
    layout: LayoutMode,
    focus: Option<usize>,
    theme: Theme,
    style: Style,
}

impl<'a> Container<'a> {
    fn new(layout: LayoutMode, theme: Theme, style: Style) -> Self {
        Self {
            rect: Rect::empty(),
            children: Vec::new(),
            layout,
            focus: None,
            theme,
            style,
        }
    }

    fn push(&mut self, child: BuiltWidget<'a>) {
        self.children.push(child);
    }

    fn child(&self, index: usize) -> Option<&BuiltWidget<'a>> {
        self.children.get(index)
    }

    fn focus_child(&mut self, next: Option<usize>) {
        let old = self.focus;
        if old == next {
            return;
        }
        self.focus = next;
        if let Some(index) = old {
            if let Some(widget) = self.children.get_mut(index) {
                widget.set_focus(false);
            }
        }
        if let Some(index) = next {
            if let Some(widget) = self.children.get_mut(index) {
                widget.set_focus(true);
            }
        }
    }

    fn arrange(&mut self) {
        let mut widgets: Vec<&mut dyn Widget> = self
            .children
            .iter_mut()
            .map(|child| child as &mut dyn Widget)
            .collect();
        self.layout.arrange(self.rect, &mut widgets);
    }

    fn records(&self) -> Vec<Accessible<'_>> {
        let mut records = vec![Accessible {
            role: self.accessible_role(),
            label: self.accessible_label().unwrap_or_default(),
        }];
        for child in &self.children {
            records.extend(child.records());
        }
        records
    }
}

impl Widget for Container<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
        self.arrange();
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        canvas.fill_rect_clipped(self.rect, colors.background, self.rect);
        let mut view = canvas.clipped(self.rect);
        for child in &self.children {
            child.paint(&mut view);
        }
    }

    fn event(&mut self, event: Event) -> bool {
        if matches!(event, Event::KeyDown { .. } | Event::KeyUp { .. }) {
            if let Some(index) = self.focus {
                if let Some(widget) = self.children.get_mut(index) {
                    if widget.event(event) {
                        return true;
                    }
                }
            }
        }
        for (index, widget) in self.children.iter_mut().enumerate().rev() {
            let focusable = widget.accepts_focus();
            if widget.event(event) {
                if matches!(event, Event::PointerDown { .. }) && focusable {
                    self.focus_child(Some(index));
                }
                return true;
            }
        }
        false
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        if !focused {
            self.focus_child(None);
        }
    }

    fn accessible_role(&self) -> &'static str {
        "container"
    }
}

/// A concrete widget produced by compiling a [`View`].
enum BuiltWidget<'a> {
    /// A static label.
    Label(Label<'a>),
    /// A multi-line text block.
    Text(TextView<'a>),
    /// A push button.
    Button(Button<'a>),
    /// A single-line text field.
    Input(TextInput),
    /// A multi-line editor.
    TextArea(TextArea),
    /// A nested container.
    Container(Container<'a>),
}

impl BuiltWidget<'_> {
    fn child(&self, index: usize) -> Option<&Self> {
        match self {
            Self::Container(container) => container.child(index),
            _ => None,
        }
    }

    fn at(&self, path: &[usize]) -> Option<&Self> {
        match path {
            [] => Some(self),
            [index, rest @ ..] => self.child(*index).and_then(|child| child.at(rest)),
        }
    }

    fn records(&self) -> Vec<Accessible<'_>> {
        match self {
            Self::Container(container) => container.records(),
            _ => vec![Accessible {
                role: self.accessible_role(),
                label: self.accessible_label().unwrap_or_default(),
            }],
        }
    }
}

impl Widget for BuiltWidget<'_> {
    fn rect(&self) -> Rect {
        match self {
            Self::Label(widget) => widget.rect(),
            Self::Text(widget) => widget.rect(),
            Self::Button(widget) => widget.rect(),
            Self::Input(widget) => widget.rect(),
            Self::TextArea(widget) => widget.rect(),
            Self::Container(widget) => widget.rect(),
        }
    }

    fn layout(&mut self, area: Rect) {
        match self {
            Self::Label(widget) => widget.layout(area),
            Self::Text(widget) => widget.layout(area),
            Self::Button(widget) => widget.layout(area),
            Self::Input(widget) => widget.layout(area),
            Self::TextArea(widget) => widget.layout(area),
            Self::Container(widget) => widget.layout(area),
        }
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        match self {
            Self::Label(widget) => widget.paint(canvas),
            Self::Text(widget) => widget.paint(canvas),
            Self::Button(widget) => widget.paint(canvas),
            Self::Input(widget) => widget.paint(canvas),
            Self::TextArea(widget) => widget.paint(canvas),
            Self::Container(widget) => widget.paint(canvas),
        }
    }

    fn event(&mut self, event: Event) -> bool {
        match self {
            Self::Label(widget) => widget.event(event),
            Self::Text(widget) => widget.event(event),
            Self::Button(widget) => widget.event(event),
            Self::Input(widget) => widget.event(event),
            Self::TextArea(widget) => widget.event(event),
            Self::Container(widget) => widget.event(event),
        }
    }

    fn accepts_focus(&self) -> bool {
        match self {
            Self::Label(widget) => widget.accepts_focus(),
            Self::Text(widget) => widget.accepts_focus(),
            Self::Button(widget) => widget.accepts_focus(),
            Self::Input(widget) => widget.accepts_focus(),
            Self::TextArea(widget) => widget.accepts_focus(),
            Self::Container(widget) => widget.accepts_focus(),
        }
    }

    fn set_focus(&mut self, focused: bool) {
        match self {
            Self::Label(widget) => widget.set_focus(focused),
            Self::Text(widget) => widget.set_focus(focused),
            Self::Button(widget) => widget.set_focus(focused),
            Self::Input(widget) => widget.set_focus(focused),
            Self::TextArea(widget) => widget.set_focus(focused),
            Self::Container(widget) => widget.set_focus(focused),
        }
    }

    fn accessible_role(&self) -> &'static str {
        match self {
            Self::Label(widget) => widget.accessible_role(),
            Self::Text(widget) => widget.accessible_role(),
            Self::Button(widget) => widget.accessible_role(),
            Self::Input(widget) => widget.accessible_role(),
            Self::TextArea(widget) => widget.accessible_role(),
            Self::Container(widget) => widget.accessible_role(),
        }
    }

    fn accessible_label(&self) -> Option<&[u8]> {
        match self {
            Self::Label(widget) => widget.accessible_label(),
            Self::Text(widget) => widget.accessible_label(),
            Self::Button(widget) => widget.accessible_label(),
            Self::Input(widget) => widget.accessible_label(),
            Self::TextArea(widget) => widget.accessible_label(),
            Self::Container(widget) => widget.accessible_label(),
        }
    }
}

/// Compiles declarative views into themed, possibly nested widgets.
pub struct ViewBuilder<'a> {
    theme: Theme,
    widgets: Vec<BuiltWidget<'a>>,
}

impl<'a> ViewBuilder<'a> {
    /// Creates an empty builder using `theme`.
    #[must_use]
    pub const fn new(theme: Theme) -> Self {
        Self {
            theme,
            widgets: Vec::new(),
        }
    }

    /// The theme all views are resolved against.
    #[must_use]
    pub const fn theme(&self) -> Theme {
        self.theme
    }

    /// Number of compiled top-level widgets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.widgets.len()
    }

    /// Whether no widgets have been compiled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.widgets.is_empty()
    }

    /// Replaces the compiled widgets with the result of building `views`.
    pub fn build(&mut self, views: &[View<'a>]) {
        self.widgets.clear();
        for &view in views {
            self.widgets.push(self.compile(view));
        }
    }

    fn compile(&self, view: View<'a>) -> BuiltWidget<'a> {
        match view {
            View::Label { text, style } => {
                BuiltWidget::Label(Label::new(text, self.theme.resolve(style).foreground))
            }
            View::Text { text, style } => {
                BuiltWidget::Text(TextView::styled(text, self.theme, style))
            }
            View::Button { text, style } => {
                BuiltWidget::Button(Button::styled(text, self.theme, style))
            }
            View::Input { max_len, style } => {
                BuiltWidget::Input(TextInput::styled(max_len, self.theme, style))
            }
            View::TextArea { max_bytes, style } => {
                BuiltWidget::TextArea(TextArea::styled(max_bytes, self.theme, style))
            }
            View::Column {
                children,
                spacing,
                style,
            } => self.compile_container(LayoutMode::Vertical(spacing), children, style),
            View::Row {
                children,
                spacing,
                style,
            } => self.compile_container(LayoutMode::Horizontal(spacing), children, style),
            View::Grid {
                children,
                columns,
                spacing,
                style,
            } => self.compile_container(LayoutMode::Grid(columns, spacing), children, style),
        }
    }

    fn compile_container(
        &self,
        layout: LayoutMode,
        children: &'a [View<'a>],
        style: Style,
    ) -> BuiltWidget<'a> {
        let mut container = Container::new(layout, self.theme, style);
        for &child in children {
            container.push(self.compile(child));
        }
        BuiltWidget::Container(container)
    }

    /// Borrows the compiled widgets as a widget tree.
    #[must_use]
    pub fn ui(&mut self) -> Ui<'_> {
        let mut ui = Ui::new();
        for widget in &mut self.widgets {
            ui.add(widget);
        }
        ui
    }

    fn at(&self, path: &[usize]) -> Option<&BuiltWidget<'a>> {
        let (index, rest) = path.split_first()?;
        self.widgets.get(*index)?.at(rest)
    }

    /// The first visible line of the text view at `path`.
    #[must_use]
    pub fn text_line(&self, path: &[usize]) -> Option<usize> {
        match self.at(path) {
            Some(BuiltWidget::Text(widget)) => Some(widget.line_offset()),
            _ => None,
        }
    }

    /// The current contents of the input at `path`.
    #[must_use]
    pub fn input_text(&self, path: &[usize]) -> Option<&[u8]> {
        match self.at(path) {
            Some(BuiltWidget::Input(widget)) => Some(widget.text()),
            _ => None,
        }
    }

    /// The full text of the editor at `path`.
    #[must_use]
    pub fn text_area_text(&self, path: &[usize]) -> Option<Vec<u8>> {
        match self.at(path) {
            Some(BuiltWidget::TextArea(widget)) => Some(widget.text()),
            _ => None,
        }
    }

    /// The `(line, column)` cursor of the editor at `path`.
    #[must_use]
    pub fn text_area_cursor(&self, path: &[usize]) -> Option<(usize, usize)> {
        match self.at(path) {
            Some(BuiltWidget::TextArea(widget)) => Some(widget.cursor()),
            _ => None,
        }
    }

    /// The first visible line of the editor at `path`.
    #[must_use]
    pub fn text_area_line(&self, path: &[usize]) -> Option<usize> {
        match self.at(path) {
            Some(BuiltWidget::TextArea(widget)) => Some(widget.first_line()),
            _ => None,
        }
    }

    /// Collects accessibility records recursively, in paint order.
    #[must_use]
    pub fn accessible(&self) -> Vec<Accessible<'_>> {
        let mut records = Vec::new();
        for widget in &self.widgets {
            records.extend(widget.records());
        }
        records
    }
}

impl Default for ViewBuilder<'_> {
    fn default() -> Self {
        Self::new(Theme::default())
    }
}

//! The colors of the text output (`full`, `concise`, and `--diff`): Ruff's, from the styles of its
//! diagnostic renderer.

use anstyle::{AnsiColor, Style};
use heki_env::Severity;
use std::fmt;

/// The style of each part of the text output. Every style of [`Stylesheet::plain`] is empty and
/// writes nothing, so its output is the text alone.
#[derive(Debug, Clone, Copy)]
pub struct Stylesheet {
    pub(crate) path: Style,
    /// The `:` after a path, its line, and its column.
    pub(crate) separator: Style,
    pub(crate) rule_code: Style,
    pub(crate) error: Style,
    pub(crate) warning: Style,
    /// The line numbers and the `|` and `=` of a source frame.
    pub(crate) gutter: Style,
    pub(crate) related: Style,
    pub(crate) help: Style,
    /// Removed lines of a diff and its `---` file.
    pub(crate) deletion: Style,
    /// Added lines of a diff and its `+++` file.
    pub(crate) insertion: Style,
}

impl Stylesheet {
    pub const fn plain() -> Self {
        let none = Style::new();
        Self {
            path: none,
            separator: none,
            rule_code: none,
            error: none,
            warning: none,
            gutter: none,
            related: none,
            help: none,
            deletion: none,
            insertion: none,
        }
    }

    /// Ruff's colors: a related location takes the style Ruff gives a sub-diagnostic's `info`.
    pub const fn styled() -> Self {
        let bright_blue = AnsiColor::BrightBlue.on_default().bold();
        Self {
            path: Style::new().bold(),
            separator: AnsiColor::Cyan.on_default(),
            rule_code: AnsiColor::Red.on_default().bold(),
            error: AnsiColor::BrightRed.on_default().bold(),
            warning: AnsiColor::Yellow.on_default().bold(),
            gutter: bright_blue,
            related: bright_blue,
            help: AnsiColor::BrightCyan.on_default().bold(),
            deletion: AnsiColor::Red.on_default(),
            insertion: AnsiColor::Green.on_default(),
        }
    }

    /// The style of a report's severity and of its caret.
    pub(crate) const fn severity(&self, severity: Severity) -> Style {
        match severity {
            Severity::Error => self.error,
            Severity::Warning => self.warning,
        }
    }

    /// `path:line:column:`, as every text output starts a location.
    pub(crate) fn location<'a>(
        &self,
        path: &'a str,
        line: u32,
        column: u32,
    ) -> impl fmt::Display + 'a {
        let (path_style, separator) = (self.path, paint(self.separator, ':'));
        fmt::from_fn(move |f| {
            write!(
                f,
                "{}{separator}{line}{separator}{column}{separator}",
                paint(path_style, path)
            )
        })
    }
}

/// `text` between the code of `style` and its reset; the text alone when `style` is empty.
pub(crate) fn paint<T: fmt::Display>(style: Style, text: T) -> impl fmt::Display {
    fmt::from_fn(move |f| write!(f, "{style}{text}{style:#}"))
}

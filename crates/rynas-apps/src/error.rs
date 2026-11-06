//! Errors. Loading and linting collect every problem of an entry as an [`Issue`] (the
//! file and a precise message), so CI and the catalogue page show them all at once.
//! Rendering and planning stop at the first [`RenderError`], which carries a stable code
//! for the API.

use std::fmt;

/// One problem found in a catalogue entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The app id (the folder name).
    pub app: String,
    /// The file inside the entry, relative to `apps/<id>/` (`app.toml`, `compose.yaml`, …).
    pub file: String,
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}: {}", self.app, self.file, self.message)
    }
}

/// Every problem of one or more entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Issues(pub Vec<Issue>);

impl Issues {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn push(&mut self, app: &str, file: &str, message: impl Into<String>) {
        self.0.push(Issue { app: app.to_owned(), file: file.to_owned(), message: message.into() });
    }

    pub(crate) fn into_result<T>(self, ok: T) -> Result<T, Issues> {
        if self.0.is_empty() { Ok(ok) } else { Err(self) }
    }
}

impl fmt::Display for Issues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, issue) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{issue}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Issues {}

/// A failure while rendering, migrating or planning an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// A form field failed validation (`field`, reason).
    InvalidValue { field: String, reason: String },
    /// A value the form or app.toml requires was not given.
    MissingValue(String),
    /// A value was given for a name the entry does not declare.
    UnknownValue(String),
    /// A value cannot be written safely into `.env` (newline, single quote, NUL).
    UnsafeValue(String),
    /// The compose file references a variable that nothing provides.
    UndefinedVariable(String),
    /// The stack name is not a valid compose project name.
    InvalidStackName(String),
    /// The installed stack is not this app, or its `x-rynas` block is unreadable.
    NotInstalledApp(String),
    /// A migration could not be applied.
    Migration(String),
    /// The entry's compose or overlay is broken (should have been caught by lint).
    Compose(String),
    /// Secret generation failed (no system randomness).
    Random(String),
}

impl RenderError {
    /// A stable code, for `err.<code>` in the API and the UI.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidValue { .. } => "apps.invalid_value",
            Self::MissingValue(_) => "apps.missing_value",
            Self::UnknownValue(_) => "apps.unknown_value",
            Self::UnsafeValue(_) => "apps.unsafe_value",
            Self::UndefinedVariable(_) => "apps.undefined_variable",
            Self::InvalidStackName(_) => "apps.invalid_stack_name",
            Self::NotInstalledApp(_) => "apps.not_installed_app",
            Self::Migration(_) => "apps.migration",
            Self::Compose(_) => "apps.compose",
            Self::Random(_) => "apps.random",
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValue { field, reason } => write!(f, "{field}: {reason}"),
            Self::MissingValue(v) => write!(f, "missing value for {v}"),
            Self::UnknownValue(v) => write!(f, "unknown value {v}"),
            Self::UnsafeValue(v) => write!(f, "value of {v} contains a newline, a single quote or NUL"),
            Self::UndefinedVariable(v) => write!(f, "compose references undefined variable {v}"),
            Self::InvalidStackName(v) => write!(f, "invalid stack name `{v}`"),
            Self::NotInstalledApp(m) => write!(f, "not an installed catalogue app: {m}"),
            Self::Migration(m) => write!(f, "migration failed: {m}"),
            Self::Compose(m) => write!(f, "compose: {m}"),
            Self::Random(m) => write!(f, "secret generation: {m}"),
        }
    }
}

impl std::error::Error for RenderError {}

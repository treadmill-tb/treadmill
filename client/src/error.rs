//! Error shapes a consumer can act on.
//!
//! Operations here return [`anyhow::Error`], so that the context of a failure
//! survives all the way up. Three wrappers carry a decision the caller has to
//! make with it, and are found with [`anyhow::Error::downcast_ref`]:
//! [`Hinted`] (there is something to suggest), [`ExitError`] (the failure has a
//! status of its own), and [`Cancelled`] (a question answered with no, which is
//! not really a failure at all).

/// An error that should exit with a status of its own, rather than 1, so that
/// a script can tell it apart from other failures.
#[derive(Debug)]
pub struct ExitError {
    code: u8,
    message: String,
}

impl ExitError {
    pub fn new(code: u8, message: String) -> Self {
        Self { code, message }
    }

    /// The status to exit with.
    pub fn code(&self) -> u8 {
        self.code
    }
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitError {}

/// An error with a suggestion of what to do about it. The two are kept apart
/// so that a consumer can present the hint in its own way — `tml` puts it on a
/// `hint:` line of its own, below the error.
#[derive(Debug)]
pub struct Hinted {
    error: anyhow::Error,
    hint: String,
}

impl Hinted {
    /// The underlying failure, without the hint.
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }

    /// What to do about it: a sentence or a command.
    pub fn hint(&self) -> &str {
        &self.hint
    }
}

/// `error`, with `hint`: a sentence or a command.
pub fn hinted(error: impl Into<anyhow::Error>, hint: impl Into<String>) -> anyhow::Error {
    Hinted {
        error: error.into(),
        hint: hint.into(),
    }
    .into()
}

impl std::fmt::Display for Hinted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

impl std::error::Error for Hinted {}

/// A question answered with no. Not a failure of the operation, but it did not
/// happen: `tml` reports it as `✗ Cancelled · <what was left unchanged>` rather
/// than as an error, and still exits non-zero.
#[derive(Debug)]
pub struct Cancelled(String);

impl Cancelled {
    /// What was left as it was.
    pub fn unchanged(&self) -> &str {
        &self.0
    }
}

/// See [`Cancelled`]; `unchanged` says what was left as it was.
pub fn cancelled(unchanged: impl Into<String>) -> anyhow::Error {
    Cancelled(unchanged.into()).into()
}

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled: {}", self.0)
    }
}

impl std::error::Error for Cancelled {}

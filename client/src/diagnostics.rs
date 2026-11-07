//! Where this crate reports what a human may want to see.
//!
//! A library cannot know how its consumer presents itself: `tml` writes styled
//! `warning:` lines to stderr and mirrors them into its `-o json` stream, a
//! daemon wants them in its log, and a test harness wants them discarded. So
//! nothing here writes to stderr itself; it reports through a [`Diagnostics`]
//! the caller supplies.

/// A sink for the two kinds of message this crate produces.
///
/// Neither is an error: an operation that reports through here still succeeds.
/// Errors are returned, not reported.
pub trait Diagnostics: Send + Sync {
    /// Something a human should see: a condition the operation continued past,
    /// such as a cache file that had to be discarded.
    fn warn(&self, message: &str);

    /// Detail that is only wanted when a caller asked for it, such as the URL a
    /// WebSocket was opened against. `tml` prints these under `-v`.
    fn detail(&self, message: &str);
}

/// Discards everything. The default for a consumer with nowhere to put it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Silent;

impl Diagnostics for Silent {
    fn warn(&self, _message: &str) {}
    fn detail(&self, _message: &str) {}
}

/// Keeps what it was told, so that a test can assert on it.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Recording {
    pub warnings: std::sync::Mutex<Vec<String>>,
    pub details: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl Recording {
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Diagnostics for Recording {
    fn warn(&self, message: &str) {
        self.warnings.lock().unwrap().push(message.to_string());
    }
    fn detail(&self, message: &str) {
        self.details.lock().unwrap().push(message.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_keeps_nothing() {
        // Only that it is usable as a `Diagnostics` and does not panic.
        let silent: &dyn Diagnostics = &Silent;
        silent.warn("ignored");
        silent.detail("ignored");
    }

    #[test]
    fn a_recording_sink_separates_the_two_kinds() {
        let recording = Recording::default();
        let sink: &dyn Diagnostics = &recording;
        sink.warn("a warning");
        sink.detail("a detail");
        assert_eq!(recording.warnings(), ["a warning"]);
        assert_eq!(*recording.details.lock().unwrap(), ["a detail"]);
    }
}

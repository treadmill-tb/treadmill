//! Undoing what a build did, including when it is interrupted.
//!
//! This is the part of a build that cannot be allowed to fail quietly. A
//! leaked mount or a still-attached NBD device does not spoil the image — it
//! wedges the *host*, and the next build inherits the mess.
//!
//! Three things undo the work, in increasing order of desperation:
//!
//! 1. [`Teardown::run`], called deliberately. A build tears down in the middle
//!    too, before it finalizes its layers, so this is not only an exit path.
//! 2. [`Guard`], which runs it on drop. This covers an error returning out of
//!    the build, which is the common failure.
//! 3. [`tear_down_on_signal`], which covers Ctrl-C and `kill`. A `Drop` guard
//!    does **not** run on a signal, so without this the common way to abandon
//!    a build would be the one way that leaks.
//!
//! Steps are undone innermost first, which falls out of recording them in the
//! order they were done: a mount made after a device was attached is undone
//! before that device is detached.
//!
//! One thing deliberately not attempted: reaping another run's leftovers. A
//! build's state lives in a per-run working directory, so there is nothing to
//! key a cross-run sweep on, and guessing from `/sys/block/nbd*` risks
//! detaching a device some other program is legitimately using. A leaked
//! device shows up as one that [`crate::nbd`] skips as busy.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::exec::Privilege;

/// Something done that has to be undone.
#[derive(Debug, Clone)]
enum Step {
    /// A mount point, and everything mounted under it.
    Mounted(PathBuf),
    /// An attached NBD device, with the pid file its server wrote.
    Attached { device: PathBuf, pid_file: PathBuf },
    /// A directory this build made.
    Created(PathBuf),
}

/// The stack of things a build has to undo.
#[derive(Debug)]
pub struct Teardown {
    privilege: Privilege,
    steps: Vec<Step>,
}

impl Teardown {
    pub fn new(privilege: Privilege) -> Self {
        Teardown {
            privilege,
            steps: Vec::new(),
        }
    }

    /// Record a mount point to be unmounted, with anything under it.
    pub fn mounted(&mut self, path: impl Into<PathBuf>) {
        self.steps.push(Step::Mounted(path.into()));
    }

    /// Record an attached NBD device to be detached.
    pub fn attached(&mut self, device: impl Into<PathBuf>, pid_file: impl Into<PathBuf>) {
        self.steps.push(Step::Attached {
            device: device.into(),
            pid_file: pid_file.into(),
        });
    }

    /// Record a directory to be removed.
    pub fn created(&mut self, path: impl Into<PathBuf>) {
        self.steps.push(Step::Created(path.into()));
    }

    /// Whether anything is outstanding.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Undo everything, innermost first.
    ///
    /// Best effort by design: one step failing must not strand the rest, since
    /// what is left behind is worse than what failed. Every failure is
    /// returned, and a caller that is not already failing should say so.
    pub fn run(&mut self) -> Vec<anyhow::Error> {
        let privilege = self.privilege;
        let mut failures = Vec::new();
        while let Some(step) = self.steps.pop() {
            if let Err(error) = undo(privilege, &step) {
                failures.push(error);
            }
        }
        failures
    }

    /// Give the host back: unmount everything and detach every device, but
    /// leave the directories alone.
    ///
    /// A build does this in the middle rather than at the end. Layers can only
    /// be finalized once the file systems are quiesced and the devices are
    /// gone — but they are finalized *from* the working directory, which has
    /// to outlive that.
    pub fn release_host_state(&mut self) -> Vec<anyhow::Error> {
        let privilege = self.privilege;
        let mut failures = Vec::new();
        let mut kept = Vec::new();
        while let Some(step) = self.steps.pop() {
            match step {
                Step::Created(_) => kept.push(step),
                _ => {
                    if let Err(error) = undo(privilege, &step) {
                        failures.push(error);
                    }
                }
            }
        }
        kept.reverse();
        self.steps = kept;
        failures
    }
}

/// Undo one step.
fn undo(privilege: Privilege, step: &Step) -> Result<()> {
    match step {
        Step::Mounted(path) => crate::mount::unmount_tree(privilege, path),
        Step::Attached { device, pid_file } => crate::nbd::detach(privilege, device, pid_file),
        Step::Created(path) => std::fs::remove_dir_all(path).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(anyhow::Error::new(e).context(format!("remove {}", path.display())))
            }
        }),
    }
}

/// A [`Teardown`] the signal handler can also reach.
pub type Shared = Arc<Mutex<Teardown>>;

/// Share a teardown.
pub fn shared(privilege: Privilege) -> Shared {
    Arc::new(Mutex::new(Teardown::new(privilege)))
}

/// Runs the teardown when it goes out of scope.
///
/// Holding one of these is what makes an early return safe; nothing else in
/// the build has to remember to clean up on the way out.
pub struct Guard {
    teardown: Shared,
}

impl Guard {
    pub fn new(teardown: Shared) -> Self {
        Guard { teardown }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // A poisoned lock means the signal thread panicked mid-teardown.
        // Tearing down anyway is strictly better than not.
        let mut teardown = match self.teardown.lock() {
            Ok(teardown) => teardown,
            Err(poisoned) => poisoned.into_inner(),
        };
        for failure in teardown.run() {
            eprintln!("warning: could not clean up: {failure:#}");
        }
    }
}

/// Tear down and exit on `SIGINT` or `SIGTERM`.
///
/// The signals are blocked process-wide and waited for on a thread of their
/// own, rather than handled. A handler may only call async-signal-safe
/// functions, and tearing down means running `umount` and `qemu-nbd` — so the
/// work has to happen on an ordinary thread.
///
/// Children are unaffected: a terminal delivers `SIGINT` to the whole
/// foreground process group, so Ctrl-C still reaches a long-running
/// `qemu-img` directly.
pub fn tear_down_on_signal(teardown: Shared) -> Result<()> {
    use nix::sys::signal::{SigSet, Signal};

    let mut signals = SigSet::empty();
    signals.add(Signal::SIGINT);
    signals.add(Signal::SIGTERM);
    // Blocked here, and so in every thread spawned after this point.
    signals.thread_block()?;

    std::thread::Builder::new()
        .name("tml-teardown".into())
        .spawn(move || {
            let signal = loop {
                match signals.wait() {
                    Ok(signal) => break signal,
                    // Interrupted by something else; keep waiting.
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(_) => return,
                }
            };
            eprintln!("\nInterrupted; cleaning up...");
            let mut teardown = match teardown.lock() {
                Ok(teardown) => teardown,
                Err(poisoned) => poisoned.into_inner(),
            };
            for failure in teardown.run() {
                eprintln!("warning: could not clean up: {failure:#}");
            }
            // The conventional status for a signal, and what a shell reports.
            std::process::exit(128 + signal as i32);
        })?;
    Ok(())
}

/// Remove a path if it is there, succeeding if it is not.
pub fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("remove {}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_undone_innermost_first() {
        let dir = tempfile::tempdir().unwrap();
        // Three nested directories, recorded outermost first, as a build
        // would: removing the outer one first would take the inner ones with
        // it and the later steps would be removing what is already gone.
        let outer = dir.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();

        let mut teardown = Teardown::new(Privilege::none());
        teardown.created(&outer);
        teardown.created(&inner);
        assert!(teardown.run().is_empty());
        assert!(!outer.exists());
    }

    #[test]
    fn a_missing_directory_is_not_a_failure() {
        let mut teardown = Teardown::new(Privilege::none());
        teardown.created(PathBuf::from("/nonexistent/tml-never-made-this"));
        assert!(teardown.run().is_empty());
    }

    #[test]
    fn running_a_teardown_empties_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let mut teardown = Teardown::new(Privilege::none());
        teardown.created(&target);
        assert!(!teardown.is_empty());
        teardown.run();
        // A build tears down mid-way and then keeps going; a second run must
        // not try to undo the same things again.
        assert!(teardown.is_empty());
        assert!(teardown.run().is_empty());
    }

    #[test]
    fn releasing_host_state_keeps_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();

        let mut teardown = Teardown::new(Privilege::none());
        teardown.created(&work);
        // Nothing privileged to undo here; what matters is what survives.
        assert!(teardown.release_host_state().is_empty());
        assert!(
            work.exists(),
            "layers are finalized from the working directory, so it must outlive the devices"
        );
        assert!(!teardown.is_empty(), "and it is still owed a removal");

        teardown.run();
        assert!(!work.exists());
    }

    #[test]
    fn the_guard_tears_down_on_an_early_return() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();

        let teardown = shared(Privilege::none());
        {
            let _guard = Guard::new(Arc::clone(&teardown));
            teardown.lock().unwrap().created(&target);
        }
        assert!(!target.exists(), "the guard should have removed it");
    }
}

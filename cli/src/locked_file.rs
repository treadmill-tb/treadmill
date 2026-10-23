//! Updating files as atomic, totally ordered read-modify-write steps.

use anyhow::{Context as _, Result};
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

/// Update the file at `path`: read it (`None` if it does not exist), let
/// `change` change its contents, and, if it changed them, replace the file with
/// them, via a temporary file renamed over it.
///
/// The whole step holds an exclusive lock (`flock`) on the file's directory,
/// so that the updates of concurrent processes are totally ordered and none is
/// lost: each reads what the one before it wrote. Readers need no lock, as
/// they find the file either as it was before an update or after it, never in
/// between. Locking the directory, rather than a lock file of its own, leaves
/// no files beside the file that other programs might list, and keeps the lock
/// valid across the rename.
///
/// A file it creates gets `mode` (less the umask); one it replaces keeps its
/// mode, which is in place before the new contents are written. The directory
/// is created, as by [`fs::create_dir_all`], if missing. Leaving the contents
/// `None` leaves a missing file missing.
pub fn update<R>(
    path: &Path,
    mode: u32,
    change: impl FnOnce(&mut Option<String>) -> Result<R>,
) -> Result<R> {
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let lock = fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", dir.display()))?;

    let before = match fs::read_to_string(path) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut contents = before.clone();
    let result = change(&mut contents)?;
    let Some(contents) = contents.filter(|contents| before.as_ref() != Some(contents)) else {
        return Ok(result);
    };

    let tmp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    // Left behind by an update that was interrupted, possibly with another
    // mode.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", tmp.display()));
        }
        _ => {}
    }
    let existing_mode = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(existing_mode.unwrap_or(mode))
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    if let Some(mode) = existing_mode {
        // Unlike the umask-reduced mode `open` applies.
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting the mode of {}", tmp.display()))?;
    }
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(result)
}

/// Replace the file at `path` with `contents`, as [`update`] does.
#[cfg_attr(not(feature = "daemon"), allow(dead_code))]
pub fn replace(path: &Path, mode: u32, contents: &str) -> Result<()> {
    update(path, mode, |current| {
        *current = Some(contents.to_string());
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_updates_are_all_applied() {
        let dir = std::env::temp_dir().join(format!("tml-locked-file-{}", std::process::id()));
        let path = dir.join("counter");
        let _ = fs::remove_dir_all(&dir);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        update(&path, 0o600, |contents| {
                            let n: u32 = contents.as_deref().map_or(0, |c| c.parse().unwrap());
                            *contents = Some((n + 1).to_string());
                            Ok(())
                        })
                        .unwrap();
                    }
                });
            }
        });
        assert_eq!(fs::read_to_string(&path).unwrap(), "200");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Only the file itself is left in its directory.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replaced_files_keep_their_mode() {
        let dir = std::env::temp_dir().join(format!("tml-locked-mode-{}", std::process::id()));
        let path = dir.join("config");
        let _ = fs::remove_dir_all(&dir);
        replace(&path, 0o600, "a").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        replace(&path, 0o600, "b").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "b");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        fs::remove_dir_all(&dir).unwrap();
    }
}

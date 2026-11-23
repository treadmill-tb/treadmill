//! Following `base` from one image to the next.
//!
//! An image is a directory. What it is built on is its `base`, which is one
//! of exactly three things, told apart by how it is written:
//!
//! - `./x` or `../x` — another image directory, relative to this one.
//! - empty or absent — nothing. The image fetches its own vendor image.
//! - anything else — a reference to an image in a registry.
//!
//! That is the whole model. There is no repository, no coordinate and no
//! required directory layout: a chain is a path through the file system, and
//! it works the same whether the directories sit in one tree or in a checkout
//! of something else entirely.
//!
//! A path has to be written as one. `webide` is a registry reference even if
//! a directory of that name happens to sit next door, because guessing from
//! what is on disk would make the meaning of a definition depend on its
//! surroundings.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};

use crate::spec::ImageDir;

/// Deep enough for any chain worth building, shallow enough to catch a cycle.
///
/// CI unrolls one job per level, so this is also the length of that ladder.
pub const MAX_DEPTH: usize = 9;

/// What an image is built on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// Nothing: the image fetches its own vendor image.
    Root,
    /// Another image directory.
    Path(PathBuf),
    /// An image in a registry, pulled rather than built.
    Reference(String),
}

/// Classify the `base` of the image in `dir`.
pub fn base_of(dir: &Path, base: Option<&str>) -> Base {
    match base {
        None | Some("") => Base::Root,
        Some(base) if base.starts_with("./") || base.starts_with("../") => {
            Base::Path(dir.join(base))
        }
        Some(base) => Base::Reference(base.to_string()),
    }
}

/// An image and everything it is built on.
#[derive(Debug)]
pub struct Chain {
    /// The images to build, base first; the last is the one asked for.
    pub links: Vec<ImageDir>,
    /// The registry reference the first link builds onto, if it has one.
    pub lower_ref: Option<String>,
}

/// Follow `dir`'s `base` down to an image that is built on nothing, or on a
/// registry reference.
pub fn chain(dir: &Path) -> Result<Chain> {
    let mut links = Vec::new();
    let mut lower_ref = None;
    let mut seen = BTreeSet::new();
    let mut current = dir.to_path_buf();

    loop {
        // Canonical, so that two spellings of one directory are one link and a
        // cycle is caught however it was written.
        let here = current
            .canonicalize()
            .with_context(|| format!("no such image directory: {}", current.display()))?;
        ensure!(
            seen.insert(here.clone()),
            "{} is built on itself, through {}",
            dir.display(),
            here.display(),
        );
        ensure!(
            links.len() < MAX_DEPTH,
            "{} is more than {MAX_DEPTH} images deep",
            dir.display(),
        );

        let image = ImageDir::open(&here)?;
        let base = base_of(&here, image.spec.base.as_deref());
        links.push(image);

        match base {
            Base::Root => break,
            Base::Reference(reference) => {
                lower_ref = Some(reference);
                break;
            }
            Base::Path(next) => current = next,
        }
    }

    links.reverse();
    Ok(Chain { links, lower_ref })
}

/// How deep `dir` sits: 0 for an image built on nothing or on a registry
/// reference, one more for each local image under it.
pub fn level(dir: &Path) -> Result<usize> {
    Ok(chain(dir)?.links.len() - 1)
}

/// Every image directory at or under `root`, in a stable order.
///
/// An image is a directory with an `image.json` in it. Nothing else is
/// assumed: images may sit anywhere and nest however the tree's author likes.
pub fn find(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = BTreeSet::new();
    walk(root, &mut found)?;
    Ok(found.into_iter().collect())
}

fn walk(dir: &Path, found: &mut BTreeSet<PathBuf>) -> Result<()> {
    if dir.join("image.json").is_file() {
        found.insert(dir.to_path_buf());
    }
    let entries = std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", dir.display()))?;
        let path = entry.path();
        // Symlinks are how a tree shares provisioning fragments between
        // images; following them here would find the same image twice, or
        // wander out of the tree entirely.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `.git`, `.image-cache`, and anything else not meant to be read.
        if name.starts_with('.') {
            continue;
        }
        walk(&path, found)?;
    }
    Ok(())
}

/// Check every image at or under `root`: that each one parses and has what it
/// needs, that every chain terminates, and that no two claim the same name.
///
/// Returns the directories checked.
pub fn check(root: &Path) -> Result<Vec<PathBuf>> {
    let dirs = find(root)?;
    ensure!(
        !dirs.is_empty(),
        "no images under {}: an image is a directory with an image.json in it",
        root.display(),
    );

    let mut names: std::collections::BTreeMap<String, PathBuf> = Default::default();
    for dir in &dirs {
        let chain = chain(dir).with_context(|| format!("image {}", dir.display()))?;
        let image = chain.links.last().expect("a chain is never empty");
        if let Some(other) = names.insert(image.name.clone(), dir.clone())
            && other != *dir
        {
            bail!(
                "{} and {} are both called {}",
                other.display(),
                dir.display(),
                image.name,
            );
        }
    }
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A directory holding one image.
    fn image(dir: &Path, name: &str, base: Option<&str>) {
        fs::create_dir_all(dir).unwrap();
        let base = match base {
            Some(base) => format!(r#", "base": "{base}""#),
            None => String::new(),
        };
        fs::write(
            dir.join("image.json"),
            format!(
                r#"{{"name": "{name}", "title": "T", "arch": "x86_64", "type": "disk"{base}}}"#
            ),
        )
        .unwrap();
        fs::write(dir.join("provision.sh"), "#!/bin/sh\n").unwrap();
        fs::write(
            dir.join("inputs.json"),
            r#"{"vendor_image": {"version": "1", "url": "https://e.invalid/i", "sha256": "00"}}"#,
        )
        .unwrap();
    }

    #[test]
    fn a_base_is_a_path_only_when_written_as_one() {
        let dir = Path::new("/images/webide");
        assert_eq!(base_of(dir, None), Base::Root);
        assert_eq!(base_of(dir, Some("")), Base::Root);
        assert_eq!(
            base_of(dir, Some("../base")),
            Base::Path(PathBuf::from("/images/webide/../base"))
        );
        assert_eq!(
            base_of(dir, Some("./inner")),
            Base::Path(PathBuf::from("/images/webide/./inner"))
        );
        // A bare name is a registry reference, even if a directory of that
        // name sits next door: a definition should not change meaning with
        // its surroundings.
        assert_eq!(
            base_of(dir, Some("ghcr.io/org/image:tag")),
            Base::Reference("ghcr.io/org/image:tag".into())
        );
        assert_eq!(base_of(dir, Some("base")), Base::Reference("base".into()));
    }

    #[test]
    fn a_chain_runs_from_the_root_image_up() {
        let temp = tempfile::tempdir().unwrap();
        image(&temp.path().join("base"), "base", None);
        image(&temp.path().join("tock"), "tock", Some("../base"));
        image(&temp.path().join("webide"), "webide", Some("../tock"));

        let chain = chain(&temp.path().join("webide")).unwrap();
        let names: Vec<&str> = chain.links.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["base", "tock", "webide"]);
        assert_eq!(chain.lower_ref, None);
        assert_eq!(level(&temp.path().join("webide")).unwrap(), 2);
        assert_eq!(level(&temp.path().join("base")).unwrap(), 0);
    }

    #[test]
    fn a_chain_on_a_registry_reference_starts_there() {
        let temp = tempfile::tempdir().unwrap();
        image(
            &temp.path().join("tock"),
            "tock",
            Some("ghcr.io/org/image:tag"),
        );
        let chain = chain(&temp.path().join("tock")).unwrap();
        assert_eq!(chain.links.len(), 1);
        assert_eq!(chain.lower_ref.as_deref(), Some("ghcr.io/org/image:tag"));
        // Deep in no local sense: there is nothing here to build under it.
        assert_eq!(level(&temp.path().join("tock")).unwrap(), 0);
    }

    #[test]
    fn a_chain_may_leave_its_own_tree() {
        // The case this model exists for: an image kept in one repository,
        // built on a base kept in another.
        let temp = tempfile::tempdir().unwrap();
        image(&temp.path().join("elsewhere/base"), "base", None);
        image(
            &temp.path().join("mine/image"),
            "mine",
            Some("../../elsewhere/base"),
        );
        let chain = chain(&temp.path().join("mine/image")).unwrap();
        let names: Vec<&str> = chain.links.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["base", "mine"]);
    }

    #[test]
    fn a_cycle_is_caught_rather_than_followed() {
        let temp = tempfile::tempdir().unwrap();
        image(&temp.path().join("a"), "a", Some("../b"));
        image(&temp.path().join("b"), "b", Some("../a"));
        let error = chain(&temp.path().join("a")).unwrap_err();
        assert!(
            format!("{error:#}").contains("built on itself"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn a_missing_base_is_named() {
        let temp = tempfile::tempdir().unwrap();
        image(&temp.path().join("derived"), "derived", Some("../absent"));
        let error = chain(&temp.path().join("derived")).unwrap_err();
        assert!(
            format!("{error:#}").contains("absent"),
            "the error should name what is missing: {error:#}"
        );
    }

    #[test]
    fn finding_images_ignores_hidden_directories() {
        let temp = tempfile::tempdir().unwrap();
        image(&temp.path().join("images/ubuntu"), "ubuntu", None);
        image(&temp.path().join("images/rpi"), "rpi", None);
        // A build cache holds layouts, not definitions.
        image(&temp.path().join(".image-cache/stale"), "stale", None);

        let found = find(temp.path()).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.strip_prefix(temp.path()).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["images/rpi", "images/ubuntu"]);
    }

    #[test]
    fn two_images_may_not_share_a_name() {
        let temp = tempfile::tempdir().unwrap();
        image(&temp.path().join("one"), "same", None);
        image(&temp.path().join("two"), "same", None);
        let error = check(temp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("same"), "{error:#}");
    }

    #[test]
    fn check_accepts_a_well_formed_tree() {
        let temp = tempfile::tempdir().unwrap();
        image(&temp.path().join("base"), "base", None);
        image(&temp.path().join("tock"), "tock", Some("../base"));
        assert_eq!(check(temp.path()).unwrap().len(), 2);
    }

    #[test]
    fn a_tree_with_no_images_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let error = check(temp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("no images"), "{error:#}");
    }
}

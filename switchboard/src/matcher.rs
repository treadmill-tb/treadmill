//! Group → concrete image matching (`doc/oci-image-migration-plan.md` §8.3).
//!
//! When a job names an image *group* (an OCI index), the switchboard must, after
//! choosing a host, pick the single group member appropriate for that host. The
//! selection axes are the OCI platform (architecture / OS / variant) plus the
//! Treadmill `target` / `board` annotations, denormalized into
//! `image_group_members` at registration time.
//!
//! # Host attribute source (provisional — plan §10 risk)
//!
//! The matcher consumes a [`HostImageAttributes`] describing the chosen host.
//! Where those attributes come from overlaps the still-unspecified job
//! `tag_config` (see [`crate::sql::job`] and `JobRequest::tag_config`, marked
//! "TO BE SPECIFIED"). Until that schema is pinned down, [`HostImageAttributes`]
//! is sourced from the host's free-form `hosts.tags` via the **provisional**
//! `key:value` convention parsed by [`HostImageAttributes::from_tags`]. The
//! matching logic itself ([`select_member`]) is independent of that source and
//! is what the unit tests exercise; only the tag-parsing convention is
//! provisional and **must be revisited** when `tag_config` is specified.

/// The image-selection attributes of a concrete host.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HostImageAttributes {
    /// OCI platform architecture, e.g. `arm64`.
    pub arch: String,
    /// OCI platform OS, e.g. `linux`.
    pub os: String,
    /// OCI platform variant, e.g. `v8` (if the host pins one).
    pub variant: Option<String>,
    /// Treadmill target, e.g. `nbd-netboot` (if the host pins one).
    pub target: Option<String>,
    /// Treadmill board, e.g. `raspberrypi-4` (if the host pins one).
    pub board: Option<String>,
}

impl HostImageAttributes {
    /// Provisional parse of host attributes from `hosts.tags`.
    ///
    /// Recognizes the `key:value` tags `arch:`, `os:`, `variant:`, `target:`,
    /// and `board:`; the last occurrence of a key wins. `arch`/`os` default to
    /// empty (matching no concrete member) if absent. **Provisional**: see the
    /// module docs — this overlaps the unspecified `tag_config` schema.
    pub fn from_tags(tags: &[String]) -> Self {
        let mut attrs = HostImageAttributes {
            arch: String::new(),
            os: String::new(),
            variant: None,
            target: None,
            board: None,
        };
        for tag in tags {
            let Some((key, value)) = tag.split_once(':') else {
                continue;
            };
            match key {
                "arch" => attrs.arch = value.to_string(),
                "os" => attrs.os = value.to_string(),
                "variant" => attrs.variant = Some(value.to_string()),
                "target" => attrs.target = Some(value.to_string()),
                "board" => attrs.board = Some(value.to_string()),
                _ => {}
            }
        }
        attrs
    }
}

/// A selectable group member, as denormalized in `image_group_members`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GroupMember<T> {
    /// The caller's handle for the selected member (e.g. an image id or digest).
    pub handle: T,
    pub arch: String,
    pub os: String,
    pub variant: Option<String>,
    pub target: Option<String>,
    pub board: Option<String>,
}

impl<T> GroupMember<T> {
    /// Whether this member is admissible for `host`.
    ///
    /// `arch` and `os` must match exactly. An axis the member leaves unset
    /// (`variant`/`target`/`board` is `None`) is a wildcard; an axis the member
    /// *does* constrain must equal the host's value, and a host that does not
    /// pin that axis cannot satisfy a member that constrains it.
    fn admissible_for(&self, host: &HostImageAttributes) -> bool {
        fn axis_ok(member: &Option<String>, host: &Option<String>) -> bool {
            match member {
                None => true,
                Some(want) => host.as_deref() == Some(want.as_str()),
            }
        }
        self.arch == host.arch
            && self.os == host.os
            && axis_ok(&self.variant, &host.variant)
            && axis_ok(&self.target, &host.target)
            && axis_ok(&self.board, &host.board)
    }

    /// How specific this member is — the number of optional axes it constrains.
    /// Used to prefer a more specific member over a looser one.
    fn specificity(&self) -> usize {
        self.variant.is_some() as usize
            + self.target.is_some() as usize
            + self.board.is_some() as usize
    }
}

/// Select the group member for `host`, preferring the most specific admissible
/// match. Returns `None` when no member is admissible.
///
/// Ties on specificity resolve to the earliest member in `members` (registration
/// order), so a deterministic choice is always made.
pub fn select_member<'a, T>(
    members: &'a [GroupMember<T>],
    host: &HostImageAttributes,
) -> Option<&'a GroupMember<T>> {
    members
        .iter()
        .filter(|m| m.admissible_for(host))
        .max_by_key(|m| m.specificity())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(
        handle: &str,
        arch: &str,
        os: &str,
        variant: Option<&str>,
        target: Option<&str>,
        board: Option<&str>,
    ) -> GroupMember<String> {
        GroupMember {
            handle: handle.to_string(),
            arch: arch.to_string(),
            os: os.to_string(),
            variant: variant.map(str::to_string),
            target: target.map(str::to_string),
            board: board.map(str::to_string),
        }
    }

    #[test]
    fn parses_provisional_tags() {
        let tags = vec![
            "arch:arm64".to_string(),
            "os:linux".to_string(),
            "variant:v8".to_string(),
            "target:nbd-netboot".to_string(),
            "board:raspberrypi-4".to_string(),
            "unrelated".to_string(),
            "region:us-east".to_string(),
        ];
        let attrs = HostImageAttributes::from_tags(&tags);
        assert_eq!(attrs.arch, "arm64");
        assert_eq!(attrs.os, "linux");
        assert_eq!(attrs.variant.as_deref(), Some("v8"));
        assert_eq!(attrs.target.as_deref(), Some("nbd-netboot"));
        assert_eq!(attrs.board.as_deref(), Some("raspberrypi-4"));
    }

    #[test]
    fn picks_arch_os_match() {
        let members = vec![
            member("amd", "amd64", "linux", None, None, None),
            member("arm", "arm64", "linux", None, None, None),
        ];
        let host = HostImageAttributes {
            arch: "arm64".to_string(),
            os: "linux".to_string(),
            variant: None,
            target: None,
            board: None,
        };
        assert_eq!(select_member(&members, &host).unwrap().handle, "arm");
    }

    #[test]
    fn prefers_more_specific_board_match() {
        let members = vec![
            member("generic", "arm64", "linux", None, Some("nbd-netboot"), None),
            member(
                "rpi4",
                "arm64",
                "linux",
                None,
                Some("nbd-netboot"),
                Some("raspberrypi-4"),
            ),
        ];
        let host = HostImageAttributes {
            arch: "arm64".to_string(),
            os: "linux".to_string(),
            variant: None,
            target: Some("nbd-netboot".to_string()),
            board: Some("raspberrypi-4".to_string()),
        };
        assert_eq!(select_member(&members, &host).unwrap().handle, "rpi4");
    }

    #[test]
    fn host_cannot_satisfy_member_constraining_an_unpinned_axis() {
        let members = vec![member(
            "rpi4",
            "arm64",
            "linux",
            None,
            None,
            Some("raspberrypi-4"),
        )];
        let host = HostImageAttributes {
            arch: "arm64".to_string(),
            os: "linux".to_string(),
            variant: None,
            target: None,
            board: None,
        };
        assert!(select_member(&members, &host).is_none());
    }

    #[test]
    fn no_match_on_arch_mismatch() {
        let members = vec![member("arm", "arm64", "linux", None, None, None)];
        let host = HostImageAttributes {
            arch: "amd64".to_string(),
            os: "linux".to_string(),
            variant: None,
            target: None,
            board: None,
        };
        assert!(select_member(&members, &host).is_none());
    }
}

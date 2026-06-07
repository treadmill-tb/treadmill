//! Group → concrete image matching (`doc/oci-image-migration-plan.md` §8.3).
//!
//! When a job names an image *group* (an OCI index), the switchboard must, after
//! choosing a host, pick the single group member appropriate for that host.
//! Selection is by **host tags only**: each member carries a set of
//! `required_host_tags` (authored on the index descriptor, denormalized into
//! `image_group_members` at registration), and a member is admissible for a host
//! iff the host's tags are a superset of that set. Target (DUT) tags play no part
//! in image selection.
//!
//! Among the admissible members the most *specific* one wins — the one requiring
//! the largest tag set — so a host that satisfies a more constrained member gets
//! it in preference to a looser fallback. Ties on specificity resolve to the
//! earliest member (registration order, carried as `position`), so the choice is
//! always deterministic.

use std::collections::BTreeSet;

/// Collect a host's tags into a set for containment queries. Tags are opaque
/// strings; duplicates collapse.
pub fn host_tag_set(tags: &[String]) -> BTreeSet<String> {
    tags.iter().cloned().collect()
}

/// A selectable group member, as denormalized in `image_group_members`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GroupMember<T> {
    /// The caller's handle for the selected member (e.g. an image id or digest).
    pub handle: T,
    /// Host tags a host must carry (as a superset) for this member to apply.
    pub required_host_tags: Vec<String>,
}

impl<T> GroupMember<T> {
    /// Whether this member is admissible for a host with `host_tags`: every
    /// required tag must be present.
    fn admissible_for(&self, host_tags: &BTreeSet<String>) -> bool {
        self.required_host_tags
            .iter()
            .all(|t| host_tags.contains(t))
    }

    /// How specific this member is — the number of host tags it requires. Used
    /// to prefer a more constrained member over a looser one.
    fn specificity(&self) -> usize {
        self.required_host_tags.len()
    }
}

/// Select the group member for a host with `host_tags`, preferring the most
/// specific admissible match. Returns `None` when no member is admissible.
///
/// Ties on specificity resolve to the earliest member in `members`, so a
/// deterministic choice is always made (callers pass members in registration
/// order).
pub fn select_member<'a, T>(
    members: &'a [GroupMember<T>],
    host_tags: &BTreeSet<String>,
) -> Option<&'a GroupMember<T>> {
    members
        .iter()
        .filter(|m| m.admissible_for(host_tags))
        // Keep the incumbent on ties (earlier registration order wins); only a
        // *strictly* more specific member displaces it.
        .fold(None, |best: Option<&GroupMember<T>>, m| match best {
            Some(b) if b.specificity() >= m.specificity() => Some(b),
            _ => Some(m),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(handle: &str, required: &[&str]) -> GroupMember<String> {
        GroupMember {
            handle: handle.to_string(),
            required_host_tags: required.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn host(tags: &[&str]) -> BTreeSet<String> {
        tags.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn picks_admissible_subset_match() {
        let members = vec![
            member("amd", &["arch=amd64"]),
            member("arm", &["arch=arm64"]),
        ];
        let h = host(&["arch=arm64", "os=linux"]);
        assert_eq!(select_member(&members, &h).unwrap().handle, "arm");
    }

    #[test]
    fn prefers_more_specific_match() {
        let members = vec![
            member("generic", &["arch=arm64"]),
            member("rpi4", &["arch=arm64", "raspberrypi-4"]),
        ];
        let h = host(&["arch=arm64", "raspberrypi-4", "os=linux"]);
        assert_eq!(select_member(&members, &h).unwrap().handle, "rpi4");
    }

    #[test]
    fn unconstrained_member_is_admissible_anywhere() {
        let members = vec![member("any", &[])];
        assert_eq!(
            select_member(&members, &host(&["whatever"]))
                .unwrap()
                .handle,
            "any"
        );
    }

    #[test]
    fn host_missing_a_required_tag_is_not_admissible() {
        let members = vec![member("rpi4", &["arch=arm64", "raspberrypi-4"])];
        let h = host(&["arch=arm64"]);
        assert!(select_member(&members, &h).is_none());
    }

    #[test]
    fn no_match_when_nothing_admissible() {
        let members = vec![member("arm", &["arch=arm64"])];
        let h = host(&["arch=amd64"]);
        assert!(select_member(&members, &h).is_none());
    }

    #[test]
    fn ties_resolve_to_earliest_member() {
        // Two equally-specific admissible members: the earliest (registration
        // order) wins.
        let members = vec![member("first", &["x"]), member("second", &["y"])];
        let h = host(&["x", "y"]);
        assert_eq!(select_member(&members, &h).unwrap().handle, "first");
    }
}

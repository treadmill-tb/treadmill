//! Image-set → concrete image matching.
//!
//! When a job names an image *set*, the switchboard must, after choosing a
//! host, pick the single set member appropriate for that host. The candidate
//! set is the membership of the job's **frozen generation** (the
//! `image_set_members` rows of the generation pinned onto the job at enqueue).
//!
//! A member declares the machine configuration it is built for
//! (`platform_profile`) and, optionally, a CEL refinement of it. It is
//! admissible for a host iff the host's spec advertises that profile and the
//! refinement evaluates true; the **first** admissible member in `index` order
//! wins. Author order is the ranking, because arbitrary predicates admit no
//! specificity order to infer.

use treadmill_rs::host_spec::HostSpecV1;

use crate::predicate::{CelEngine, Engine};

/// A selectable set member, as denormalized in `image_set_members`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GroupMember<T> {
    /// The caller's handle for the selected member (e.g. an image id or digest).
    pub handle: T,
    /// The machine configuration this member is built for, matched by equality
    /// against the host spec's `platform.profiles`.
    pub platform_profile: String,
    /// Optional CEL refinement, evaluated with the host spec bound as `host`.
    pub predicate: Option<String>,
}

impl<T> GroupMember<T> {
    /// Whether this member is admissible for `host`: the host advertises the
    /// member's profile, and the refinement (if any) evaluates true.
    ///
    /// A refinement that errors or fails to compile makes the member
    /// inadmissible rather than failing the job, matching how a job's own
    /// predicate treats a host it cannot evaluate against.
    fn admissible_for(&self, host: &HostSpecV1) -> bool {
        if !host.platform.profiles().contains(&self.platform_profile) {
            return false;
        }
        match self.predicate.as_deref() {
            None => true,
            Some(source) => CelEngine
                .compile(source)
                .ok()
                .and_then(|p| p.eval(host).ok())
                .unwrap_or(false),
        }
    }
}

/// Select the set member to dispatch onto a host. Returns `None` when no member
/// is admissible, which is a *host* rejection: another host may match.
///
/// Members must be supplied in `index` order, which is the order they are
/// tried. A host with no spec advertises no profiles, so it admits nothing.
pub fn select_member<'a, T>(
    members: &'a [GroupMember<T>],
    host: Option<&HostSpecV1>,
) -> Option<&'a GroupMember<T>> {
    let host = host?;
    members.iter().find(|m| m.admissible_for(host))
}

#[cfg(test)]
mod tests {
    use treadmill_rs::host_spec::HostSpecV1;

    use super::*;

    fn member(handle: &str, profile: &str, predicate: Option<&str>) -> GroupMember<String> {
        GroupMember {
            handle: handle.to_string(),
            platform_profile: profile.to_string(),
            predicate: predicate.map(str::to_string),
        }
    }

    /// A virtual host advertising `profiles` with `memory_mb` of RAM.
    fn spec(profiles: &[&str], memory_mb: u32) -> HostSpecV1 {
        use treadmill_rs::host_spec::{Platform, Resources, SpecVersionV1};
        HostSpecV1 {
            spec_version: SpecVersionV1::V1,
            id: uuid::Uuid::nil(),
            name: "h".into(),
            description: None,
            site: "cambridge".into(),
            location: None,
            platform: Platform::Virtual {
                arch: "x86_64".into(),
                profiles: profiles.iter().map(|p| p.to_string()).collect(),
                hypervisor: "qemu".into(),
            },
            resources: Resources {
                cpu_cores: 4,
                memory_mb,
                storage_gb: 64,
            },
            labels: Default::default(),
            duts: vec![],
        }
    }

    #[test]
    fn first_admissible_member_wins() {
        // Author order is the ranking: index 0 is more constrained, index 1 is
        // the catch-all for the same profile.
        let members = vec![
            member(
                "big",
                "q35-virtio-uefi",
                Some("host.resources.memory_mb >= 16384"),
            ),
            member("plain", "q35-virtio-uefi", None),
            member("bios", "q35-virtio-bios", None),
        ];

        let big = spec(&["q35-virtio-uefi"], 16384);
        assert_eq!(select_member(&members, Some(&big)).unwrap().handle, "big");

        // Same profile, too little memory: the refinement fails and the
        // catch-all behind it takes over.
        let small = spec(&["q35-virtio-uefi"], 4096);
        assert_eq!(
            select_member(&members, Some(&small)).unwrap().handle,
            "plain"
        );

        // A host advertising only the other profile skips both of the above.
        let bios = spec(&["q35-virtio-bios"], 16384);
        assert_eq!(select_member(&members, Some(&bios)).unwrap().handle, "bios");
    }

    #[test]
    fn no_member_for_the_hosts_profiles_is_a_rejection() {
        let members = vec![member("arm", "rpi4-uboot-sd", None)];
        let h = spec(&["q35-virtio-uefi"], 8192);
        assert!(select_member(&members, Some(&h)).is_none());
    }

    /// A host with no spec advertises no profiles, so it admits no member.
    #[test]
    fn undescribed_host_admits_nothing() {
        let members = vec![member("any", "q35-virtio-uefi", None)];
        assert!(select_member(&members, None).is_none());
    }

    /// A refinement that errors on this host (or does not compile) makes the
    /// member inadmissible; it never fails the job.
    #[test]
    fn broken_refinement_skips_the_member() {
        let members = vec![
            member("typo", "q35-virtio-uefi", Some("host.no_such_field == 1")),
            member("bad-syntax", "q35-virtio-uefi", Some("host.site ==")),
            member("ok", "q35-virtio-uefi", None),
        ];
        let h = spec(&["q35-virtio-uefi"], 8192);
        assert_eq!(select_member(&members, Some(&h)).unwrap().handle, "ok");
    }

    /// The same image under two profiles is two members, which is what the
    /// primary key change permits.
    #[test]
    fn one_image_may_serve_several_profiles() {
        let members = vec![
            member("img", "q35-virtio-uefi", None),
            member("img", "q35-virtio-bios", None),
        ];
        for profile in ["q35-virtio-uefi", "q35-virtio-bios"] {
            let h = spec(&[profile], 8192);
            assert_eq!(select_member(&members, Some(&h)).unwrap().handle, "img");
        }
    }

    #[test]
    fn an_empty_generation_matches_nothing() {
        let members: Vec<GroupMember<String>> = vec![];
        let h = spec(&["q35-virtio-uefi"], 8192);
        assert!(select_member(&members, Some(&h)).is_none());
    }
}

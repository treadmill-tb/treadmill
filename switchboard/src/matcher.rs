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
//!
//! Members with no profile are the legacy host-tag form, kept only while tags
//! still exist: a generation whose members all lack one is ranked the old way,
//! most-specific-tag-set first with ties going to the earliest member. That
//! branch goes when tags do.

use std::collections::BTreeSet;

use treadmill_rs::host_spec::HostSpecV1;

use crate::predicate::{CelEngine, Engine};

/// Collect a host's tags into a set for containment queries. Tags are opaque
/// strings; duplicates collapse.
pub fn host_tag_set(tags: &[String]) -> BTreeSet<String> {
    tags.iter().cloned().collect()
}

/// A selectable set member, as denormalized in `image_set_members`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GroupMember<T> {
    /// The caller's handle for the selected member (e.g. an image id or digest).
    pub handle: T,
    /// The machine configuration this member is built for, matched by equality
    /// against the host spec's `platform.profiles`.
    pub platform_profile: Option<String>,
    /// Optional CEL refinement, evaluated with the host spec bound as `host`.
    pub predicate: Option<String>,
    /// Legacy: host tags a host must carry (as a superset) for this member to
    /// apply. Consulted only for a generation whose members carry no profile.
    pub required_host_tags: Vec<String>,
}

impl<T> GroupMember<T> {
    /// Whether this member is admissible for `host`: the host advertises the
    /// member's profile, and the refinement (if any) evaluates true.
    ///
    /// A refinement that errors or fails to compile makes the member
    /// inadmissible rather than failing the job, matching how a job's own
    /// predicate treats a host it cannot evaluate against.
    fn admissible_for_spec(&self, host: &HostSpecV1) -> bool {
        let Some(profile) = self.platform_profile.as_deref() else {
            return false;
        };
        if !host.platform.profiles().iter().any(|p| p == profile) {
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

/// Select the set member to dispatch onto a host. Returns `None` when no member
/// is admissible, which is a *host* rejection: another host may match.
///
/// Members must be supplied in `index` order. When any member declares a
/// platform profile the choice is first-match-wins over those; otherwise the
/// generation is the legacy tag-only form and the most specific admissible
/// member wins, ties going to the earliest.
pub fn select_member<'a, T>(
    members: &'a [GroupMember<T>],
    host_tags: &BTreeSet<String>,
    host: Option<&HostSpecV1>,
) -> Option<&'a GroupMember<T>> {
    if members.iter().any(|m| m.platform_profile.is_some()) {
        // An undescribed host advertises no profiles, so it admits nothing here.
        let host = host?;
        return members.iter().find(|m| m.admissible_for_spec(host));
    }

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

/// A host target (DUT) considered for a job's target requirements.
#[derive(Debug, Clone)]
pub struct TargetCandidate<T> {
    /// The caller's handle for the target (e.g. its `target_id`).
    pub handle: T,
    /// The target's opaque tag set.
    pub tags: BTreeSet<String>,
}

/// Decide whether a host can satisfy a job's target (DUT) requirements, and if
/// so how. Each requirement (a set of required tags) must be assigned to a
/// **distinct** candidate target whose tags are a superset of it; a job
/// requesting two identical DUTs therefore needs two matching targets. Returns
/// the chosen handle per requirement (index-aligned with `requirements`), or
/// `None` if no assignment saturating every requirement exists.
///
/// This is maximum bipartite matching (Kuhn's augmenting paths). It is purely an
/// admission gate — nothing about the assignment is persisted; a scheduled job
/// has access to every target physically wired to its host. `requirements` and
/// `targets` are both small (a handful), so the simple O(V·E) algorithm is more
/// than adequate, and trying requirements/candidates in order makes it
/// deterministic.
pub fn match_targets<T: Copy>(
    requirements: &[Vec<String>],
    targets: &[TargetCandidate<T>],
) -> Option<Vec<T>> {
    // adjacency[i] = indices of targets whose tags satisfy requirement i.
    let adjacency: Vec<Vec<usize>> = requirements
        .iter()
        .map(|req| {
            targets
                .iter()
                .enumerate()
                .filter(|(_, t)| req.iter().all(|tag| t.tags.contains(tag)))
                .map(|(j, _)| j)
                .collect()
        })
        .collect();

    // For each target, which requirement (if any) currently claims it.
    let mut target_to_req: Vec<Option<usize>> = vec![None; targets.len()];

    // Augmenting-path search: try to give requirement `req` a target, possibly
    // bumping an already-matched requirement onto a different free target.
    fn augment(
        req: usize,
        adjacency: &[Vec<usize>],
        target_to_req: &mut [Option<usize>],
        visited: &mut [bool],
    ) -> bool {
        for &t in &adjacency[req] {
            if visited[t] {
                continue;
            }
            visited[t] = true;
            let free_or_reassignable = match target_to_req[t] {
                None => true,
                Some(other) => augment(other, adjacency, target_to_req, visited),
            };
            if free_or_reassignable {
                target_to_req[t] = Some(req);
                return true;
            }
        }
        false
    }

    for req in 0..requirements.len() {
        let mut visited = vec![false; targets.len()];
        if !augment(req, &adjacency, &mut target_to_req, &mut visited) {
            return None;
        }
    }

    // Invert target→requirement into requirement→handle.
    let mut chosen: Vec<Option<T>> = vec![None; requirements.len()];
    for (t, claimed) in target_to_req.iter().enumerate() {
        if let Some(req) = claimed {
            chosen[*req] = Some(targets[t].handle);
        }
    }
    // Every requirement was saturated above, so all slots are filled.
    Some(chosen.into_iter().map(|c| c.expect("saturated")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(handle: &str, required: &[&str]) -> GroupMember<String> {
        GroupMember {
            handle: handle.to_string(),
            platform_profile: None,
            predicate: None,
            required_host_tags: required.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A member in the profile form: a required profile plus an optional CEL
    /// refinement.
    fn profile_member(handle: &str, profile: &str, predicate: Option<&str>) -> GroupMember<String> {
        GroupMember {
            handle: handle.to_string(),
            platform_profile: Some(profile.to_string()),
            predicate: predicate.map(str::to_string),
            required_host_tags: vec![],
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
        assert_eq!(select_member(&members, &h, None).unwrap().handle, "arm");
    }

    #[test]
    fn prefers_more_specific_match() {
        let members = vec![
            member("generic", &["arch=arm64"]),
            member("rpi4", &["arch=arm64", "raspberrypi-4"]),
        ];
        let h = host(&["arch=arm64", "raspberrypi-4", "os=linux"]);
        assert_eq!(select_member(&members, &h, None).unwrap().handle, "rpi4");
    }

    #[test]
    fn unconstrained_member_is_admissible_anywhere() {
        let members = vec![member("any", &[])];
        assert_eq!(
            select_member(&members, &host(&["whatever"]), None)
                .unwrap()
                .handle,
            "any"
        );
    }

    #[test]
    fn host_missing_a_required_tag_is_not_admissible() {
        let members = vec![member("rpi4", &["arch=arm64", "raspberrypi-4"])];
        let h = host(&["arch=arm64"]);
        assert!(select_member(&members, &h, None).is_none());
    }

    #[test]
    fn no_match_when_nothing_admissible() {
        let members = vec![member("arm", &["arch=arm64"])];
        let h = host(&["arch=amd64"]);
        assert!(select_member(&members, &h, None).is_none());
    }

    #[test]
    fn ties_resolve_to_earliest_member() {
        // Two equally-specific admissible members: the earliest (registration
        // order) wins.
        let members = vec![member("first", &["x"]), member("second", &["y"])];
        let h = host(&["x", "y"]);
        assert_eq!(select_member(&members, &h, None).unwrap().handle, "first");
    }

    // -- profile-based selection ---------------------------------------------

    #[test]
    fn first_admissible_profile_member_wins() {
        // Author order is the ranking: index 0 is more constrained, index 1 is
        // the catch-all for the same profile.
        let members = vec![
            profile_member(
                "big",
                "q35-virtio-uefi",
                Some("host.resources.memory_mb >= 16384"),
            ),
            profile_member("plain", "q35-virtio-uefi", None),
            profile_member("bios", "q35-virtio-bios", None),
        ];
        let tags = host(&[]);

        let big = spec(&["q35-virtio-uefi"], 16384);
        assert_eq!(
            select_member(&members, &tags, Some(&big)).unwrap().handle,
            "big"
        );

        // Same profile, too little memory: the refinement fails and the
        // catch-all behind it takes over.
        let small = spec(&["q35-virtio-uefi"], 4096);
        assert_eq!(
            select_member(&members, &tags, Some(&small)).unwrap().handle,
            "plain"
        );

        // A host advertising only the other profile skips both of the above.
        let bios = spec(&["q35-virtio-bios"], 16384);
        assert_eq!(
            select_member(&members, &tags, Some(&bios)).unwrap().handle,
            "bios"
        );
    }

    #[test]
    fn no_member_for_the_hosts_profiles_is_a_rejection() {
        let members = vec![profile_member("arm", "rpi4-uboot-sd", None)];
        let h = spec(&["q35-virtio-uefi"], 8192);
        assert!(select_member(&members, &host(&[]), Some(&h)).is_none());
    }

    /// A host with no spec advertises no profiles, so it admits no member of a
    /// profile-carrying generation.
    #[test]
    fn undescribed_host_admits_no_profile_member() {
        let members = vec![profile_member("any", "q35-virtio-uefi", None)];
        assert!(select_member(&members, &host(&["whatever"]), None).is_none());
    }

    /// A refinement that errors on this host (or does not compile) makes the
    /// member inadmissible; it never fails the job.
    #[test]
    fn broken_refinement_skips_the_member() {
        let members = vec![
            profile_member("typo", "q35-virtio-uefi", Some("host.no_such_field == 1")),
            profile_member("bad-syntax", "q35-virtio-uefi", Some("host.site ==")),
            profile_member("ok", "q35-virtio-uefi", None),
        ];
        let h = spec(&["q35-virtio-uefi"], 8192);
        assert_eq!(
            select_member(&members, &host(&[]), Some(&h))
                .unwrap()
                .handle,
            "ok"
        );
    }

    /// The same image under two profiles is two members, which is what the
    /// primary key change permits.
    #[test]
    fn one_image_may_serve_several_profiles() {
        let members = vec![
            profile_member("img", "q35-virtio-uefi", None),
            profile_member("img", "q35-virtio-bios", None),
        ];
        for profile in ["q35-virtio-uefi", "q35-virtio-bios"] {
            let h = spec(&[profile], 8192);
            assert_eq!(
                select_member(&members, &host(&[]), Some(&h))
                    .unwrap()
                    .handle,
                "img"
            );
        }
    }

    /// A mixed generation ignores the tag-only members: any profile present
    /// means the generation is read the new way.
    #[test]
    fn profile_members_take_precedence_over_tag_members() {
        let members = vec![
            member("legacy", &["arch=amd64"]),
            profile_member("modern", "q35-virtio-uefi", None),
        ];
        let h = spec(&["q35-virtio-uefi"], 8192);
        assert_eq!(
            select_member(&members, &host(&["arch=amd64"]), Some(&h))
                .unwrap()
                .handle,
            "modern"
        );
    }

    // -- DUT (target) matching ---------------------------------------------

    fn dut(handle: u32, tags: &[&str]) -> TargetCandidate<u32> {
        TargetCandidate {
            handle,
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn reqs(rs: &[&[&str]]) -> Vec<Vec<String>> {
        rs.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn match_targets_empty_requirements_is_trivially_satisfied() {
        let targets = vec![dut(1, &["a"])];
        assert_eq!(match_targets(&[], &targets), Some(vec![]));
        // Even with no targets at all.
        assert_eq!(
            match_targets(&[], &[] as &[TargetCandidate<u32>]),
            Some(vec![])
        );
    }

    #[test]
    fn match_targets_single_requirement_subset_semantics() {
        // The DUT's tags must be a superset of the requirement.
        let targets = vec![dut(7, &["board=nrf52840dk", "ble", "gpio"])];
        assert_eq!(
            match_targets(&reqs(&[&["board=nrf52840dk", "ble"]]), &targets),
            Some(vec![7])
        );
        // A requirement tag the DUT lacks → no match.
        assert_eq!(
            match_targets(&reqs(&[&["board=nrf52840dk", "usb"]]), &targets),
            None
        );
    }

    #[test]
    fn match_targets_requires_distinct_duts() {
        // Two identical requirements need two distinct matching DUTs.
        let one = vec![dut(1, &["board=nrf52840dk"])];
        assert_eq!(
            match_targets(&reqs(&[&["board=nrf52840dk"], &["board=nrf52840dk"]]), &one),
            None,
            "one DUT cannot satisfy two requirements"
        );
        let two = vec![dut(1, &["board=nrf52840dk"]), dut(2, &["board=nrf52840dk"])];
        let got = match_targets(&reqs(&[&["board=nrf52840dk"], &["board=nrf52840dk"]]), &two)
            .expect("two DUTs satisfy two requirements");
        assert_eq!(got.len(), 2);
        assert_ne!(got[0], got[1], "the two requirements get distinct DUTs");
    }

    #[test]
    fn match_targets_more_requirements_than_targets_fails() {
        let targets = vec![dut(1, &["a"]), dut(2, &["a"])];
        assert_eq!(
            match_targets(&reqs(&[&["a"], &["a"], &["a"]]), &targets),
            None
        );
    }

    #[test]
    fn match_targets_needs_augmenting_path() {
        // The classic case a *greedy* (non-augmenting) matcher gets wrong:
        //   req0 can use D1 or D2; req1 can use only D1.
        // If req0 greedily takes D1, req1 is stuck — but a perfect matching
        // exists (req0→D2, req1→D1). Kuhn's must reassign req0.
        let targets = vec![dut(1, &["shared"]), dut(2, &["shared", "extra"])];
        let got = match_targets(&reqs(&[&["shared"], &["shared", "extra"]]), &targets);
        // req1 requires "extra", so it must take D2; req0 must then take D1.
        assert_eq!(got, Some(vec![1, 2]));

        // Symmetric ordering: req0 needs the scarce DUT.
        let targets = vec![dut(1, &["shared", "extra"]), dut(2, &["shared"])];
        let got = match_targets(&reqs(&[&["shared", "extra"], &["shared"]]), &targets);
        assert_eq!(got, Some(vec![1, 2]));
    }

    #[test]
    fn match_targets_surplus_targets() {
        let targets = vec![dut(1, &["a"]), dut(2, &["b"]), dut(3, &["a", "b"])];
        // One requirement, three DUTs: deterministically takes the first
        // satisfying DUT in order.
        assert_eq!(match_targets(&reqs(&[&["a"]]), &targets), Some(vec![1]));
    }
}

use std::collections::{BTreeMap, BTreeSet, HashSet};
use uuid::Uuid;

pub trait TagMatcher {
    fn matches(&self, tag_set: &BTreeSet<String>) -> bool;
}

pub struct NaiveTagConfig {
    inner: BTreeSet<String>,
}
impl From<&'_ str> for NaiveTagConfig {
    fn from(value: &str) -> Self {
        Self {
            inner: value
                .split(';')
                .map(String::from)
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }
}
impl TagMatcher for NaiveTagConfig {
    fn matches(&self, tag_set: &BTreeSet<String>) -> bool {
        self.inner.is_subset(tag_set)
    }
}

pub fn find_matching_supervisors<M: TagMatcher + for<'a> From<&'a str>>(
    raw_tag_config: &str,
    tag_sets: &BTreeMap<Uuid, BTreeSet<String>>,
    allowed_supervisor_ids: HashSet<Uuid>,
) -> BTreeSet<Uuid> {
    let tag_config = M::from(raw_tag_config);
    let mut matches = BTreeSet::default();
    for (supervisor_id, tag_set) in tag_sets {
        if !allowed_supervisor_ids.contains(supervisor_id) {
            continue;
        }
        if tag_config.matches(tag_set) {
            matches.insert(*supervisor_id);
        }
    }
    matches
}

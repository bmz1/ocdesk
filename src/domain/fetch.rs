//! Request status of a cached, re-fetchable resource (saved grants, folder
//! diffs, usage reports).
//!
//! The cached value is stored beside the status, not inside it: a refresh
//! keeps showing what was last loaded, and so does a failed refresh, with its
//! error. What the status rules out is the combination the old
//! `loading: bool` + `error: Option<String>` pairs allowed: loading *and*
//! failed at once. Starting a fetch clears the previous error; finishing it
//! sets one or none.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum FetchStatus {
    #[default]
    Idle,
    Loading,
    Failed(String),
}

impl FetchStatus {
    pub(crate) fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }

    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// A fetch started; any previous error is superseded.
    pub(crate) fn begin(&mut self) {
        *self = Self::Loading;
    }

    /// A fetch ended with `error` (or none).
    pub(crate) fn finish(&mut self, error: Option<String>) {
        *self = error.map_or(Self::Idle, Self::Failed);
    }

    /// Abandon a fetch whose completion will be discarded (server restart):
    /// loading → idle; a shown error stays.
    pub(crate) fn abandon(&mut self) {
        if self.is_loading() {
            *self = Self::Idle;
        }
    }
}

/// [`FetchStatus`] per key. Idle keys are not stored, so the map only holds
/// resources that are loading or failed.
pub(crate) struct FetchStatuses<K> {
    map: HashMap<K, FetchStatus>,
}

impl<K> Default for FetchStatuses<K> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

impl<K: Hash + Eq> FetchStatuses<K> {
    pub(crate) fn get<Q: Hash + Eq + ?Sized>(&self, key: &Q) -> &FetchStatus
    where
        K: Borrow<Q>,
    {
        const IDLE: &FetchStatus = &FetchStatus::Idle;
        self.map.get(key).unwrap_or(IDLE)
    }

    pub(crate) fn is_loading<Q: Hash + Eq + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.get(key).is_loading()
    }

    pub(crate) fn error<Q: Hash + Eq + ?Sized>(&self, key: &Q) -> Option<&str>
    where
        K: Borrow<Q>,
    {
        self.get(key).error()
    }

    /// Returns whether the status changed.
    pub(crate) fn begin(&mut self, key: K) -> bool {
        self.map.insert(key, FetchStatus::Loading) != Some(FetchStatus::Loading)
    }

    /// Returns whether the status changed.
    pub(crate) fn finish(&mut self, key: K, error: Option<String>) -> bool {
        match error {
            Some(e) => {
                let next = FetchStatus::Failed(e);
                self.map.insert(key, next.clone()) != Some(next)
            }
            None => self.map.remove(&key).is_some(),
        }
    }

    pub(crate) fn remove<Q: Hash + Eq + ?Sized>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
    {
        self.map.remove(key);
    }

    /// [`FetchStatus::abandon`] every key; returns those that were loading.
    pub(crate) fn abandon_all(&mut self) -> Vec<K>
    where
        K: Clone,
    {
        let loading: Vec<K> = self
            .map
            .iter()
            .filter(|(_, s)| s.is_loading())
            .map(|(k, _)| k.clone())
            .collect();
        for key in &loading {
            self.map.remove(key);
        }
        loading
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_and_failed_are_exclusive_and_begin_clears_the_error() {
        let mut s = FetchStatus::default();
        s.begin();
        assert!(s.is_loading() && s.error().is_none());
        s.finish(Some("boom".into()));
        assert!(!s.is_loading());
        assert_eq!(s.error(), Some("boom"));
        s.begin();
        assert!(s.is_loading() && s.error().is_none());
        s.finish(None);
        assert_eq!(s, FetchStatus::Idle);
    }

    #[test]
    fn abandon_releases_loading_but_keeps_a_shown_error() {
        let mut s = FetchStatus::Loading;
        s.abandon();
        assert_eq!(s, FetchStatus::Idle);
        let mut s = FetchStatus::Failed("x".into());
        s.abandon();
        assert_eq!(s.error(), Some("x"));
    }

    #[test]
    fn keyed_statuses_report_changes_and_isolate_keys() {
        let mut m = FetchStatuses::default();
        assert!(m.begin("a"));
        assert!(!m.begin("a"), "already loading");
        assert!(m.finish("b", Some("e".into())));
        assert!(!m.finish("b", Some("e".into())), "same error");
        assert!(m.is_loading(&"a") && m.error(&"a").is_none());
        assert_eq!(m.error(&"b"), Some("e"));
        assert_eq!(m.abandon_all(), vec!["a"]);
        assert_eq!(m.get(&"a"), &FetchStatus::Idle);
        assert_eq!(m.error(&"b"), Some("e"), "errors survive a restart");
        assert!(m.finish("b", None));
        assert!(!m.finish("b", None), "idle stays idle");
        m.finish("c", Some("gone".into()));
        m.remove("c");
        assert_eq!(
            m.get("c"),
            &FetchStatus::Idle,
            "removed resources forget errors"
        );
    }
}

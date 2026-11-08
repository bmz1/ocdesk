//! Small, domain-specific transition models. No UI or I/O dependencies:
//! regressions can be exercised as event sequences without a window/server.

use std::collections::{BTreeSet, HashMap, hash_map::Entry};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferOperation {
    Load,
    Revalidate,
    Save,
}

/// Exactly one operation owns a buffer. Disk notifications received while it
/// runs coalesce into one follow-up read, including after an operation fails.
#[derive(Default, Debug, PartialEq, Eq)]
pub enum BufferIo {
    #[default]
    Idle,
    Running {
        operation: BufferOperation,
        refresh_pending: bool,
    },
}

impl BufferIo {
    pub fn busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub fn saving(&self) -> bool {
        matches!(
            self,
            Self::Running {
                operation: BufferOperation::Save,
                ..
            }
        )
    }

    pub fn begin(&mut self, operation: BufferOperation) -> bool {
        if self.busy() {
            return false;
        }
        *self = Self::Running {
            operation,
            refresh_pending: false,
        };
        true
    }

    /// True means the caller owns a new revalidation; false means it was queued.
    pub fn refresh(&mut self) -> bool {
        match self {
            Self::Idle => self.begin(BufferOperation::Revalidate),
            Self::Running {
                refresh_pending, ..
            } => {
                *refresh_pending = true;
                false
            }
        }
    }

    /// Release the operation and return whether a disk notification needs a read.
    pub fn finish(&mut self) -> bool {
        matches!(
            std::mem::take(self),
            Self::Running {
                refresh_pending: true,
                ..
            }
        )
    }
}

#[derive(Clone, Copy)]
enum DirectoryLoad {
    Loading,
    RefreshPending,
}

/// Per-path listing ownership. Removing an entry releases it even after failure.
#[derive(Default)]
pub struct DirectoryReads(HashMap<String, DirectoryLoad>);

impl DirectoryReads {
    pub fn contains(&self, path: &str) -> bool {
        self.0.contains_key(path)
    }

    /// Start a listing, or remember that the in-flight result needs refreshing.
    pub fn begin(&mut self, path: &str) -> bool {
        match self.0.entry(path.to_string()) {
            Entry::Vacant(entry) => {
                entry.insert(DirectoryLoad::Loading);
                true
            }
            Entry::Occupied(mut entry) => {
                entry.insert(DirectoryLoad::RefreshPending);
                false
            }
        }
    }

    pub fn finish(&mut self, path: &str) -> bool {
        matches!(self.0.remove(path), Some(DirectoryLoad::RefreshPending))
    }
}

/// Debouncing delays work; it must never narrow the set of invalidated folders.
#[derive(Default, Debug, PartialEq, Eq)]
pub enum RefreshTargets {
    #[default]
    None,
    Folders(BTreeSet<PathBuf>),
    All,
}

impl RefreshTargets {
    pub fn include(&mut self, dir: Option<PathBuf>) {
        match (self, dir) {
            (targets, None) => *targets = Self::All,
            (Self::All, Some(_)) => {}
            (Self::Folders(folders), Some(dir)) => {
                folders.insert(dir);
            }
            (targets @ Self::None, Some(dir)) => {
                *targets = Self::Folders(BTreeSet::from([dir]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_events_during_each_operation_produce_one_follow_up_read() {
        for operation in [
            BufferOperation::Load,
            BufferOperation::Save,
            BufferOperation::Revalidate,
        ] {
            let mut io = BufferIo::default();
            assert!(io.begin(operation));
            assert!(
                !io.begin(BufferOperation::Load),
                "must not overlap an existing operation"
            );
            assert_eq!(io.saving(), operation == BufferOperation::Save);
            for _ in 0..20 {
                assert!(!io.refresh());
            }
            // Both success and failure paths release through finish().
            assert!(io.finish());
            assert!(io.refresh());
            assert!(
                !io.finish(),
                "the follow-up must not loop without another event"
            );
            assert!(!io.busy());
        }
    }

    #[test]
    fn idle_refresh_and_quiet_save_do_not_schedule_extra_work() {
        let mut io = BufferIo::default();
        assert!(io.refresh());
        assert!(!io.finish());
        assert!(io.begin(BufferOperation::Save));
        assert!(!io.finish());
        assert!(!io.saving());
    }

    #[test]
    fn directory_invalidation_during_listing_is_replayed_without_affecting_other_paths() {
        let mut reads = DirectoryReads::default();
        assert!(reads.begin(""));
        assert!(reads.begin("src"));
        for _ in 0..20 {
            assert!(!reads.begin(""));
        }
        assert!(
            reads.finish(""),
            "an in-flight invalidation needs another listing"
        );
        assert!(
            reads.contains("src"),
            "another directory still owns its request"
        );
        assert!(reads.begin(""));
        assert!(
            !reads.finish(""),
            "one follow-up consumes all earlier invalidations"
        );
        assert!(!reads.finish("src"));
        assert!(!reads.contains(""));
        assert!(!reads.contains("src"));
    }

    #[test]
    fn debounce_retains_every_folder_and_global_invalidation_in_both_orders() {
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        let mut targets = RefreshTargets::default();
        for dir in [&a, &b, &a] {
            targets.include(Some(dir.clone()));
        }
        assert_eq!(
            targets,
            RefreshTargets::Folders(BTreeSet::from([a.clone(), b.clone()]))
        );
        targets.include(None);
        targets.include(Some(a));
        assert_eq!(std::mem::take(&mut targets), RefreshTargets::All);
        assert_eq!(targets, RefreshTargets::None);
        targets.include(Some(b));
        assert!(matches!(targets, RefreshTargets::Folders(_)));
    }
}

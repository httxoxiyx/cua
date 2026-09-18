//! Generic locked-HashMap plumbing for per-platform element caches.
//!
//! Each platform crate stores a per-(pid, window) snapshot of the
//! actionable accessibility elements it last walked, so subsequent
//! tool calls (`click`, `type_text`, etc.) can resolve an
//! `element_index` back to a native handle without re-walking the
//! tree. Before this module the three crates each owned a
//! near-identical `Mutex<HashMap<CacheKey, CachedSnapshot>>` plus
//! the same insert / lookup / count methods — see
//! `docs/dedup-audit.md` item #3.
//!
//! What lives here: the lock + HashMap, generic over the caller's
//! key type `K` and snapshot type `S`. What stays per-platform:
//!
//! - the concrete `CacheKey` (different pid/window-id widths)
//! - the concrete `CachedSnapshot` and its `Drop` impl, which on
//!   macOS calls `CFRelease` on every cached AXUIElementRef and on
//!   Windows calls COM `Release` on every IUIAutomationElement.
//!   Those releases are load-bearing — dropping the wrong way leaks
//!   the AX / UIA handles. The generic core just stores `S` by
//!   value; when the entry is removed or replaced (via
//!   `HashMap::insert`'s replace-return semantics) Rust runs `S`'s
//!   destructor, so the platform `Drop` impl still fires exactly as
//!   it did pre-refactor.
//!
//! Each platform's `ElementCache` is now a thin wrapper around an
//! `ElementCacheCore<CacheKey, CachedSnapshot>`. Specialised
//! accessors (`get_element_retained` on macOS and Windows,
//! `get_element_center` on Windows, `get_element_key` on Linux) call
//! `with_snapshot` and project the field they care about. The macOS and
//! Windows `get_element_retained` accessors retain the element under the
//! lock (CFRetain / COM AddRef) so a concurrent `insert` (snapshot replace)
//! can't free it mid-action — see
//! `platform-macos/src/ax/cache.rs::RetainedElement` and
//! `platform-windows/src/uia/cache.rs::RetainedElement`.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

/// Generic locked-HashMap holding one snapshot of type `S` per key.
///
/// The `K: Clone` bound lets the wrapper build a key value on the
/// stack and pass `&K` in for lookups without forcing callers to
/// hand out owned keys on every read.
pub struct ElementCacheCore<K: Eq + Hash, S> {
    inner: Mutex<HashMap<K, VersionedSnapshot<S>>>,
}

struct VersionedSnapshot<S> {
    snapshot_id: Option<u32>,
    value: S,
}

impl<K: Eq + Hash, S> ElementCacheCore<K, S> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Replace the snapshot for `key`. If an entry already existed
    /// its destructor runs here — for macOS that fires `CFRelease`
    /// on every retained AXUIElementRef, for Windows it fires COM
    /// `Release`, for Linux it's a no-op (just frees the `Vec<u64>`).
    pub fn insert(&self, key: K, snapshot: S) {
        self.insert_for_snapshot(key, None, snapshot);
    }

    /// Replace the cached value and bind it to the observation snapshot that
    /// produced it. Element actions must use [`Self::with_snapshot_id`] so an
    /// already-resolved token cannot silently read a newer same-window cache.
    pub fn insert_for_snapshot(&self, key: K, snapshot_id: Option<u32>, snapshot: S) {
        let mut inner = self.inner.lock().unwrap();
        inner.insert(
            key,
            VersionedSnapshot {
                snapshot_id,
                value: snapshot,
            },
        );
    }

    /// Run `f` against the snapshot for `key` while the lock is
    /// held. Returns `None` if there is no entry.
    pub fn with_snapshot<R>(&self, key: &K, f: impl FnOnce(&S) -> R) -> Option<R> {
        let inner = self.inner.lock().unwrap();
        inner.get(key).map(|snapshot| f(&snapshot.value))
    }

    /// Run `f` only when the current cache entry was produced by exactly
    /// `snapshot_id`. The generation comparison and projection happen under
    /// the same lock as replacement, closing the resolve-then-replace race.
    pub fn with_snapshot_id<R>(
        &self,
        key: &K,
        snapshot_id: u32,
        f: impl FnOnce(&S) -> R,
    ) -> Option<R> {
        let inner = self.inner.lock().unwrap();
        let snapshot = inner.get(key)?;
        (snapshot.snapshot_id == Some(snapshot_id)).then(|| f(&snapshot.value))
    }

    /// Drop the snapshot for `key` if present.
    #[allow(dead_code)]
    pub fn remove(&self, key: &K) {
        let mut inner = self.inner.lock().unwrap();
        inner.remove(key);
    }
}

impl<K: Eq + Hash, S> Default for ElementCacheCore<K, S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct TestKey {
        pid: u32,
        window_id: u64,
    }

    struct TestSnapshot {
        elements: Vec<usize>,
    }

    #[test]
    fn insert_then_get_returns_projection() {
        let cache: ElementCacheCore<TestKey, TestSnapshot> = ElementCacheCore::new();
        let key = TestKey {
            pid: 42,
            window_id: 7,
        };
        cache.insert(
            key,
            TestSnapshot {
                elements: vec![10, 20, 30],
            },
        );

        let third = cache.with_snapshot(&key, |s| s.elements.get(2).copied());
        assert_eq!(third, Some(Some(30)));
    }

    #[test]
    fn miss_returns_none() {
        let cache: ElementCacheCore<TestKey, TestSnapshot> = ElementCacheCore::new();
        let key = TestKey {
            pid: 1,
            window_id: 1,
        };
        let v = cache.with_snapshot(&key, |s| s.elements.len());
        assert_eq!(v, None);
    }

    #[test]
    fn count_via_with_snapshot() {
        let cache: ElementCacheCore<TestKey, TestSnapshot> = ElementCacheCore::new();
        let key = TestKey {
            pid: 9,
            window_id: 99,
        };

        // Before insert: None.
        assert_eq!(cache.with_snapshot(&key, |s| s.elements.len()), None);

        cache.insert(
            key,
            TestSnapshot {
                elements: vec![1, 2, 3, 4, 5],
            },
        );
        assert_eq!(cache.with_snapshot(&key, |s| s.elements.len()), Some(5));
    }

    #[test]
    fn insert_replaces_existing_and_runs_drop() {
        // Smoke test that re-inserting under the same key replaces
        // the prior snapshot — the platform Drop fires here in real
        // code (CFRelease/COM Release). We can't exercise FFI in a
        // unit test, but we can confirm the value was replaced.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct DropCounter {
            counter: Arc<AtomicUsize>,
        }
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.counter.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let cache: ElementCacheCore<u32, DropCounter> = ElementCacheCore::new();

        cache.insert(
            1,
            DropCounter {
                counter: drops.clone(),
            },
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        cache.insert(
            1,
            DropCounter {
                counter: drops.clone(),
            },
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "replacement should drop the prior snapshot"
        );

        cache.remove(&1);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            2,
            "remove should drop the snapshot"
        );
    }

    #[test]
    fn default_impl_matches_new() {
        let _cache: ElementCacheCore<TestKey, TestSnapshot> = ElementCacheCore::default();
    }

    #[test]
    fn stale_snapshot_generation_cannot_read_replacement_at_same_index() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let cache = Arc::new(ElementCacheCore::<TestKey, TestSnapshot>::new());
        let key = TestKey {
            pid: 42,
            window_id: 7,
        };
        cache.insert_for_snapshot(key, Some(100), TestSnapshot { elements: vec![10] });

        // Model the real click path: token resolution has completed, then the
        // action pauses for async routing/modal checks while another state
        // observation replaces the same (pid, window) cache.
        let resolved = Arc::new(Barrier::new(2));
        let replaced = Arc::new(Barrier::new(2));
        let action_cache = Arc::clone(&cache);
        let action_resolved = Arc::clone(&resolved);
        let action_replaced = Arc::clone(&replaced);
        let action = thread::spawn(move || {
            let resolved_snapshot_id = 100;
            action_resolved.wait();
            action_replaced.wait();
            action_cache
                .with_snapshot_id(&key, resolved_snapshot_id, |snapshot| snapshot.elements[0])
        });

        resolved.wait();
        cache.insert_for_snapshot(key, Some(101), TestSnapshot { elements: vec![99] });
        replaced.wait();

        assert_eq!(
            cache.with_snapshot(&key, |snapshot| snapshot.elements[0]),
            Some(99),
            "the legacy generation-unbound lookup reproduces the old same-index alias"
        );

        assert_eq!(
            action.join().unwrap(),
            None,
            "an old resolved token must not alias index 0 in the replacement snapshot"
        );
        assert_eq!(
            cache.with_snapshot_id(&key, 101, |snapshot| snapshot.elements[0]),
            Some(99),
            "the replacement remains available to its own generation"
        );
    }
}

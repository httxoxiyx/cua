//! AT-SPI element cache for Linux.
//! Stores native AT-SPI object references indexed by observation generation and
//! `(pid, xid, element_index)`.
//!
//! The locked-HashMap plumbing lives in `cua_driver_core::element_cache` — see
//! `docs/dedup-audit.md` item #3. This module owns the Linux-specific
//! `CacheKey` and `CachedSnapshot` (no custom Drop is needed because the
//! retained identities are owned strings and geometry values).

use std::collections::HashMap;

use super::{AtspiElementRef, AtspiNode};
use cua_driver_core::element_cache::ElementCacheCore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub pid: u32,
    pub xid: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CachedElement {
    pub(crate) element_ref: AtspiElementRef,
    /// Screen-space bounds captured in the same observation as the element.
    pub(crate) screen_bounds: Option<(i32, i32, u32, u32)>,
}

pub(crate) struct CachedSnapshot {
    /// Application-wide element indices can be sparse in a window-scoped
    /// observation, so preserve their actual numeric keys instead of packing
    /// them into a vector.
    pub(crate) elements: HashMap<usize, CachedElement>,
}

pub struct ElementCache {
    core: ElementCacheCore<CacheKey, CachedSnapshot>,
}

impl ElementCache {
    pub fn new() -> Self {
        Self {
            core: ElementCacheCore::new(),
        }
    }

    pub fn update(
        &self,
        pid: u32,
        xid: u64,
        snapshot_id: Option<u32>,
        nodes: &[AtspiNode],
        bounds: &[(usize, i32, i32, u32, u32)],
    ) {
        let bounds_by_index: HashMap<usize, (i32, i32, u32, u32)> = bounds
            .iter()
            .map(|(index, x, y, width, height)| (*index, (*x, *y, *width, *height)))
            .collect();
        let elements = nodes
            .iter()
            .filter_map(|node| {
                Some((
                    node.element_index?,
                    CachedElement {
                        element_ref: node.element_ref.clone()?,
                        screen_bounds: bounds_by_index.get(&node.element_index?).copied(),
                    },
                ))
            })
            .collect();
        self.core.insert_for_snapshot(
            CacheKey { pid, xid },
            snapshot_id,
            CachedSnapshot { elements },
        );
    }

    pub(crate) fn get_element_for_snapshot(
        &self,
        pid: u32,
        xid: u64,
        snapshot_id: u32,
        idx: usize,
    ) -> Option<CachedElement> {
        self.core
            .with_snapshot_id(&CacheKey { pid, xid }, snapshot_id, |s| {
                s.elements.get(&idx).cloned()
            })
            .flatten()
    }

    pub fn element_count(&self, pid: u32, xid: u64) -> usize {
        self.core
            .with_snapshot(&CacheKey { pid, xid }, |s| s.elements.len())
            .unwrap_or(0)
    }
}

impl Default for ElementCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: usize, path: &str) -> AtspiNode {
        AtspiNode {
            element_index: Some(index),
            role: "button".into(),
            name: Some(path.into()),
            value: None,
            checked: None,
            enabled: Some(true),
            selected: None,
            description: None,
            actions: vec!["click".into()],
            element_key: index as u64,
            element_ref: Some(AtspiElementRef {
                destination: ":1.42".into(),
                path: path.into(),
                in_web_content: false,
            }),
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
        }
    }

    #[test]
    fn snapshot_lookup_preserves_sparse_indices_and_native_identity() {
        let cache = ElementCache::new();
        cache.update(
            42,
            7,
            Some(100),
            &[node(3, "/old/button")],
            &[(3, 10, 20, 30, 40)],
        );

        let element = cache
            .get_element_for_snapshot(42, 7, 100, 3)
            .expect("snapshot element");
        assert_eq!(element.element_ref.path, "/old/button");
        assert_eq!(element.screen_bounds, Some((10, 20, 30, 40)));
        assert!(cache.get_element_for_snapshot(42, 7, 100, 0).is_none());
    }

    #[test]
    fn replacement_snapshot_cannot_alias_an_old_index() {
        use std::sync::{Arc, Barrier};

        let cache = Arc::new(ElementCache::new());
        cache.update(42, 7, Some(100), &[node(0, "/old/button")], &[]);

        let resolved = Arc::new(Barrier::new(2));
        let replaced = Arc::new(Barrier::new(2));
        let action_cache = Arc::clone(&cache);
        let action_resolved = Arc::clone(&resolved);
        let action_replaced = Arc::clone(&replaced);
        let action = std::thread::spawn(move || {
            action_resolved.wait();
            action_replaced.wait();
            action_cache.get_element_for_snapshot(42, 7, 100, 0)
        });

        resolved.wait();
        cache.update(42, 7, Some(101), &[node(0, "/new/button")], &[]);
        replaced.wait();

        assert!(action.join().unwrap().is_none());
        assert_eq!(
            cache
                .get_element_for_snapshot(42, 7, 101, 0)
                .expect("replacement element")
                .element_ref
                .path,
            "/new/button"
        );
    }
}

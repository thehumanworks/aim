//! Bounded page selection for directory listings.
//!
//! Kernel candidate: pure, no I/O. A directory is read in arbitrary order, so a page of the
//! `limit` smallest names after a cursor needs a selection, not a sort of the whole directory.
//! [`Page`] keeps at most `limit + 1` candidates at any time (a max-heap by name), so a listing's
//! memory is bounded by its page size however large the directory is. The extra candidate tells
//! whether another page follows.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// A candidate ordered by name only.
struct ByName<T> {
    name: String,
    item: T,
}

impl<T> PartialEq for ByName<T> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl<T> Eq for ByName<T> {}

impl<T> PartialOrd for ByName<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for ByName<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

/// The `limit` smallest names strictly after a cursor, chosen from an unordered stream.
pub struct Page<'a, T> {
    after: Option<&'a str>,
    keep: usize,
    heap: BinaryHeap<ByName<T>>,
    peak: usize,
}

/// A selected page: entries in name order, and the cursor of the next page when more follow.
#[derive(Debug, PartialEq, Eq)]
pub struct Selected<T> {
    /// At most `limit` entries, sorted by name.
    pub entries: Vec<(String, T)>,
    /// The last returned name, when more entries follow it.
    pub next_page: Option<String>,
}

impl<'a, T> Page<'a, T> {
    /// An empty selection of `limit` entries (at least one) after `after`.
    #[must_use]
    pub fn new(limit: usize, after: Option<&'a str>) -> Self {
        let keep = limit.max(1).saturating_add(1);
        Self { after, keep, heap: BinaryHeap::with_capacity(keep.min(4096)), peak: 0 }
    }

    /// Whether a name could still make the page (so a caller can skip work for it).
    #[must_use]
    pub fn wants(&self, name: &str) -> bool {
        if self.after.is_some_and(|after| name <= after) {
            return false;
        }
        self.heap.len() < self.keep || self.heap.peek().is_some_and(|largest| name < largest.name.as_str())
    }

    /// Offers one entry; it is kept only while it is among the `limit + 1` smallest so far.
    pub fn offer(&mut self, name: String, item: T) {
        if !self.wants(&name) {
            return;
        }
        if self.heap.len() >= self.keep {
            self.heap.pop();
        }
        self.heap.push(ByName { name, item });
        self.peak = self.peak.max(self.heap.len());
    }

    /// The most candidates held at once (never more than `limit + 1`).
    #[must_use]
    pub fn peak(&self) -> usize {
        self.peak
    }

    /// The page, sorted by name.
    #[must_use]
    pub fn finish(self) -> Selected<T> {
        let limit = self.keep - 1;
        let mut entries: Vec<(String, T)> = self.heap.into_sorted_vec().into_iter().map(|c| (c.name, c.item)).collect();
        let next_page = if entries.len() > limit {
            entries.truncate(limit);
            entries.last().map(|(name, _)| name.clone())
        } else {
            None
        };
        Selected { entries, next_page }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn naive(names: &[String], after: Option<&str>, limit: usize) -> (Vec<String>, Option<String>) {
        let mut all: Vec<String> = names.iter().filter(|n| after.is_none_or(|a| n.as_str() > a)).cloned().collect();
        all.sort();
        all.dedup();
        let limit = limit.max(1);
        if all.len() > limit {
            all.truncate(limit);
            let next = all.last().cloned();
            (all, next)
        } else {
            (all, None)
        }
    }

    #[test]
    fn a_huge_directory_holds_only_a_page() {
        // One million names streamed in reverse order (the worst case for a naive "push all").
        let mut page = Page::new(1, None);
        for i in (0..1_000_000u32).rev() {
            page.offer(format!("{i:07}"), i);
        }
        assert!(page.peak() <= 2, "held {} entries for a page of 1", page.peak());
        let selected = page.finish();
        assert_eq!(selected.entries, vec![("0000000".to_owned(), 0)]);
        assert_eq!(selected.next_page.as_deref(), Some("0000000"));
    }

    #[test]
    fn the_cursor_resumes_after_the_last_name() {
        let names = ["c", "a", "e", "b", "d"];
        let mut page = Page::new(2, Some("b"));
        for name in names {
            page.offer(name.to_owned(), ());
        }
        let selected = page.finish();
        assert_eq!(selected.entries.iter().map(|(n, ())| n.as_str()).collect::<Vec<_>>(), ["c", "d"]);
        assert_eq!(selected.next_page.as_deref(), Some("d"));
        let mut last = Page::new(2, Some("d"));
        for name in names {
            last.offer(name.to_owned(), ());
        }
        assert_eq!(last.finish().next_page, None);
    }

    proptest! {
        /// The bounded selection equals sorting everything, and never holds more than limit + 1.
        #[test]
        fn agrees_with_a_full_sort(names in prop::collection::hash_set("[a-e]{0,3}", 0..60), limit in 0usize..8, after in prop::option::of("[a-e]{0,2}")) {
            let names: Vec<String> = names.into_iter().collect();
            let mut page = Page::new(limit, after.as_deref());
            for name in &names {
                page.offer(name.clone(), ());
            }
            prop_assert!(page.peak() <= limit.max(1) + 1);
            let selected = page.finish();
            let (expected, next) = naive(&names, after.as_deref(), limit);
            prop_assert_eq!(selected.entries.into_iter().map(|(n, ())| n).collect::<Vec<_>>(), expected);
            prop_assert_eq!(selected.next_page, next);
        }
    }
}

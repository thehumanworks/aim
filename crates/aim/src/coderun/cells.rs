//! What the parent keeps of a cell: an `exec` cell's unread output, and the session's store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use aim_coderun::budget::{Charge, EventKind, MAX_EVENTS, MAX_STORE_BYTES, OutputBudget, dropped_note, truncate_middle};
use aim_coderun::protocol::{CellOutput, ExecuteResult};
use aim_proto::error::ProtoError;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::supervisor::{Observer, OutputSink};

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Progress {
    /// Output not yet returned to the model: dropped as soon as a poll returns it (`REV13a` M3).
    unread: String,
    /// A `notify` or yield arrived: the next poll returns at once.
    surface: bool,
    /// The parent's own budget, so a compromised worker cannot flood the daemon.
    budget: OutputBudget,
    done: Option<Result<(), ProtoError>>,
    finished_at: Option<Instant>,
}

/// An `exec` cell's output, read by `exec` and `wait` polls.
pub struct CellRecord {
    progress: Mutex<Progress>,
    changed: Notify,
}

impl CellRecord {
    /// A record keeping at most `limit_bytes` of output.
    #[must_use]
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            progress: Mutex::new(Progress {
                unread: String::new(),
                surface: false,
                budget: OutputBudget::new(limit_bytes, MAX_EVENTS),
                done: None,
                finished_at: None,
            }),
            changed: Notify::new(),
        }
    }

    /// Records how the cell ended: its final output (the returned value, when it emitted no
    /// text), what its budget dropped, and any note about its store.
    pub fn finish(&self, result: Result<(ExecuteResult, Option<String>), ProtoError>) {
        let mut progress = locked(&self.progress);
        match result {
            Ok((result, store_note)) => {
                if result.returned && !result.output.is_empty() {
                    let kept = progress.budget.fit(result.output.len());
                    progress.unread.push_str(result.output.get(..result.output.floor_char_boundary(kept)).unwrap_or_default());
                    progress.unread.push('\n');
                }
                let dropped = dropped_note(
                    result.dropped_bytes.saturating_add(progress.budget.dropped_bytes()),
                    result.dropped_events.saturating_add(progress.budget.dropped_events()),
                );
                for note in [dropped, store_note].into_iter().flatten() {
                    progress.unread.push_str(&note);
                    progress.unread.push('\n');
                }
                progress.done = Some(Ok(()));
            }
            Err(error) => progress.done = Some(Err(error)),
        }
        progress.finished_at = Some(Instant::now());
        drop(progress);
        self.changed.notify_waiters();
    }

    /// Whether the cell finished before `cutoff`.
    #[must_use]
    pub fn finished_before(&self, cutoff: Instant) -> bool {
        locked(&self.progress).finished_at.is_some_and(|at| at <= cutoff)
    }

    /// Whether the cell finished.
    #[must_use]
    pub fn is_done(&self) -> bool {
        locked(&self.progress).done.is_some()
    }

    /// Waits up to `wait` for output worth returning, then returns the unread output, cut to
    /// `max_bytes` (Codex's head-and-tail truncation, `REV13a` M8), and whether the cell is done.
    ///
    /// # Errors
    /// The cell's own failure, with any output it produced first.
    pub async fn poll(&self, wait: Duration, max_bytes: usize) -> Result<(String, bool), ProtoError> {
        let deadline = Instant::now() + wait;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut progress = locked(&self.progress);
                if progress.done.is_some() || progress.surface || Instant::now() >= deadline {
                    progress.surface = false;
                    let unread = std::mem::take(&mut progress.unread);
                    let shown = truncate_middle(&unread, max_bytes);
                    return match progress.done.clone() {
                        Some(Err(mut error)) => {
                            if !shown.is_empty() {
                                error.message = format!("{}\nOutput before the failure:\n{shown}", error.message);
                            }
                            // The failure and the output before it share the response's budget.
                            error.message = truncate_middle(&error.message, max_bytes);
                            Err(error)
                        }
                        done => Ok((shown, done.is_some())),
                    };
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                // The next iteration returns whatever arrived.
            }
        }
    }

    /// Everything unread, for a terminated cell.
    #[must_use]
    pub fn take_unread(&self, max_bytes: usize) -> String {
        truncate_middle(&std::mem::take(&mut locked(&self.progress).unread), max_bytes)
    }
}

impl OutputSink for CellRecord {
    fn push(&self, output: CellOutput) {
        let kind = match (output.immediate, output.yielded) {
            (_, true) => EventKind::Yield,
            (true, false) => EventKind::Notify,
            (false, false) => EventKind::Text,
        };
        let mut progress = locked(&self.progress);
        if progress.budget.charge(kind, output.text.len()) != Charge::Keep {
            return;
        }
        if kind != EventKind::Yield {
            progress.unread.push_str(&output.text);
            progress.unread.push('\n');
        }
        progress.surface |= kind != EventKind::Text;
        drop(progress);
        self.changed.notify_waiters();
    }
}

/// A live or finished `exec` cell.
pub struct ExecCell {
    /// Its output.
    pub record: Arc<CellRecord>,
    /// The turn that observes it.
    pub observer: Arc<Observer>,
    /// Cancelled by `wait {terminate}`.
    pub cancel: CancellationToken,
    /// Its task, awaited on termination so nothing of it outlives the answer.
    pub task: Mutex<Option<JoinHandle<()>>>,
}

impl ExecCell {
    /// Takes the cell's task handle.
    pub fn take_task(&self) -> Option<JoinHandle<()>> {
        locked(&self.task).take()
    }

    /// Stores the cell's task handle.
    pub fn set_task(&self, task: JoinHandle<()>) {
        *locked(&self.task) = Some(task);
    }
}

/// Applies what a cell changed in its store to the session's store, key by key (`REV13a` L3). A
/// cell that finished later never overwrites keys another cell changed meanwhile. Returns a note
/// for the model when the merged store would exceed [`MAX_STORE_BYTES`]; the store is then left
/// as it was (`REV13a` L10).
#[must_use]
pub fn merge_store(store: &mut HashMap<String, Value>, before: &HashMap<String, Value>, after: HashMap<String, Value>) -> Option<String> {
    let mut merged = store.clone();
    for key in before.keys() {
        if !after.contains_key(key) {
            merged.remove(key);
        }
    }
    for (key, value) in after {
        if before.get(&key) != Some(&value) {
            merged.insert(key, value);
        }
    }
    let size = serde_json::to_vec(&merged).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_STORE_BYTES {
        return Some(format!(
            "[store not saved: it would hold {size} bytes of JSON, over the {MAX_STORE_BYTES}-byte limit; store(key, undefined) removes a key]"
        ));
    }
    *store = merged;
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{Value, json};

    use super::merge_store;

    fn map(entries: &[(&str, Value)]) -> HashMap<String, Value> {
        entries.iter().map(|(key, value)| ((*key).to_owned(), value.clone())).collect()
    }

    #[test]
    fn merging_applies_only_what_the_cell_changed() {
        // Cell A stored `a`; queued cell B then stored `b` from its own snapshot (REV13a L3).
        let mut store = map(&[("a", json!(1))]);
        assert_eq!(merge_store(&mut store, &HashMap::new(), map(&[("b", json!(2))])), None);
        assert_eq!(store, map(&[("a", json!(1)), ("b", json!(2))]));
        // A cell that removed `a` removes it; unchanged keys it saw stay as others left them.
        store.insert("c".into(), json!(3));
        assert_eq!(merge_store(&mut store, &map(&[("a", json!(1)), ("b", json!(0))]), map(&[("b", json!(0))])), None);
        assert_eq!(store, map(&[("b", json!(2)), ("c", json!(3))]));
    }

    #[test]
    fn an_oversized_store_is_refused_and_left_as_it_was() {
        let mut store = map(&[("a", json!(1))]);
        let huge = json!("x".repeat(aim_coderun::budget::MAX_STORE_BYTES));
        let note = merge_store(&mut store, &HashMap::new(), map(&[("big", huge)]));
        assert!(note.is_some_and(|note| note.contains("store not saved")));
        assert_eq!(store, map(&[("a", json!(1))]));
    }
}

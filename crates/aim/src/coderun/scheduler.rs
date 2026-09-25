//! Which of a session's code cells may run (ADR 0066, `REV13a` H1 and M9). Pure: no clock, no I/O.
//!
//! Spec (a kernel candidate):
//! - at most one cell runs;
//! - the queue holds at most `capacity` cells, first in first out, without duplicates, and never
//!   the running cell;
//! - [`Scheduler::admit`] mints a fresh ticket. The cell runs at once only when nothing runs and
//!   nothing waits. Otherwise it is queued while there is room; with no room it is refused;
//! - [`Scheduler::leave`] changes only the ticket it names:
//!   - a running ticket hands the slot to the head of the queue;
//!   - a queued ticket leaves the queue. Tickets are never admitted twice, so it can never run
//!     afterwards;
//!   - any other ticket changes nothing;
//! - after [`Scheduler::close`], nothing runs, nothing waits, and nothing is admitted.
//!
//! Theorems the kernel version should prove:
//! - `leave(t)` never changes whether another ticket `u != t` runs or waits, except that the
//!   queue's head starts running when `t` was running;
//! - a ticket that left while queued never runs.

use std::collections::VecDeque;

/// A cell's place in the scheduler.
pub type Ticket = u64;

/// What [`Scheduler::admit`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The cell runs now.
    Running,
    /// The cell waits behind `ahead` others (the running one included).
    Queued {
        /// Cells ahead of it.
        ahead: usize,
    },
    /// The queue is full.
    Busy,
    /// The scheduler is closed.
    Closed,
}

/// What [`Scheduler::leave`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Left {
    /// The ticket was running; `next` runs now.
    Running {
        /// The queued ticket that took the slot.
        next: Option<Ticket>,
    },
    /// The ticket was queued and never ran.
    Queued,
    /// The ticket was neither running nor queued.
    Absent,
}

/// One running slot and a bounded queue.
#[derive(Debug)]
pub struct Scheduler {
    capacity: usize,
    running: Option<Ticket>,
    queue: VecDeque<Ticket>,
    next: Ticket,
    closed: bool,
}

impl Scheduler {
    /// A scheduler whose queue holds at most `capacity` waiting cells.
    #[must_use]
    pub const fn new(capacity: usize) -> Self {
        Self { capacity, running: None, queue: VecDeque::new(), next: 0, closed: false }
    }

    /// Mints a ticket and decides whether it runs, waits, or is refused.
    pub fn admit(&mut self) -> (Ticket, Admission) {
        let ticket = self.next;
        self.next = self.next.saturating_add(1);
        let admission = if self.closed {
            Admission::Closed
        } else if self.running.is_none() && self.queue.is_empty() {
            self.running = Some(ticket);
            Admission::Running
        } else if self.queue.len() < self.capacity {
            self.queue.push_back(ticket);
            Admission::Queued { ahead: self.queue.len() }
        } else {
            Admission::Busy
        };
        (ticket, admission)
    }

    /// The ticket that runs now.
    #[must_use]
    pub const fn running(&self) -> Option<Ticket> {
        self.running
    }

    /// Whether `ticket` runs now.
    #[must_use]
    pub fn is_running(&self, ticket: Ticket) -> bool {
        self.running == Some(ticket)
    }

    /// Whether `ticket` waits in the queue.
    #[must_use]
    pub fn is_queued(&self, ticket: Ticket) -> bool {
        self.queue.contains(&ticket)
    }

    /// How many cells are ahead of a queued `ticket`, the running one included.
    #[must_use]
    pub fn ahead(&self, ticket: Ticket) -> Option<usize> {
        let position = self.queue.iter().position(|queued| *queued == ticket)?;
        Some(position.saturating_add(usize::from(self.running.is_some())))
    }

    /// How many cells wait.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Removes `ticket`, whatever its state. Only a running ticket's leaving starts another.
    pub fn leave(&mut self, ticket: Ticket) -> Left {
        if self.running == Some(ticket) {
            self.running = self.queue.pop_front();
            return Left::Running { next: self.running };
        }
        if let Some(position) = self.queue.iter().position(|queued| *queued == ticket) {
            self.queue.remove(position);
            return Left::Queued;
        }
        Left::Absent
    }

    /// Refuses every later admission and forgets every cell. Returns the ticket that was running.
    pub fn close(&mut self) -> Option<Ticket> {
        self.closed = true;
        self.queue.clear();
        self.running.take()
    }
}

#[cfg(test)]
mod tests {
    use super::{Admission, Left, Scheduler, Ticket};

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    fn check(scheduler: &Scheduler) {
        assert!(scheduler.queue.len() <= scheduler.capacity);
        if let Some(running) = scheduler.running {
            assert!(!scheduler.queue.contains(&running));
        }
        let mut sorted: Vec<Ticket> = scheduler.queue.iter().copied().collect();
        sorted.dedup();
        assert_eq!(sorted.len(), scheduler.queue.len(), "no duplicates");
    }

    #[test]
    fn terminating_a_queued_cell_affects_only_it_and_it_never_runs() {
        let mut scheduler = Scheduler::new(4);
        let (a, first) = scheduler.admit();
        let (b, second) = scheduler.admit();
        assert_eq!((first, second), (Admission::Running, Admission::Queued { ahead: 1 }));
        assert_eq!(scheduler.ahead(b), Some(1));
        assert_eq!(scheduler.ahead(a), None);
        assert_eq!(scheduler.leave(b), Left::Queued);
        assert!(scheduler.is_running(a), "the running cell is untouched (REV13a H1)");
        assert_eq!(scheduler.leave(a), Left::Running { next: None });
        assert!(!scheduler.is_running(b), "a terminated queued cell never runs");
    }

    #[test]
    fn the_queue_is_bounded_and_fifo() {
        let mut scheduler = Scheduler::new(2);
        let (a, _) = scheduler.admit();
        let (b, _) = scheduler.admit();
        let (c, _) = scheduler.admit();
        assert_eq!(scheduler.admit().1, Admission::Busy);
        assert_eq!(scheduler.leave(a), Left::Running { next: Some(b) });
        assert_eq!(scheduler.leave(b), Left::Running { next: Some(c) });
        assert_eq!(scheduler.leave(b), Left::Absent);
    }

    #[test]
    fn close_refuses_and_forgets_everything() {
        let mut scheduler = Scheduler::new(2);
        let (a, _) = scheduler.admit();
        let (b, _) = scheduler.admit();
        assert_eq!(scheduler.close(), Some(a));
        assert!(!scheduler.is_queued(b));
        assert_eq!(scheduler.admit().1, Admission::Closed);
        assert_eq!(scheduler.leave(b), Left::Absent);
    }

    #[test]
    fn leaving_never_changes_another_cell_except_promoting_the_head() {
        let mut random = Lcg(11);
        for _ in 0..200 {
            let mut scheduler = Scheduler::new(usize::try_from(random.next() % 5).unwrap());
            let mut left: Vec<Ticket> = Vec::new();
            let mut minted: Vec<Ticket> = Vec::new();
            for _ in 0..60 {
                if random.next().is_multiple_of(2) {
                    let (ticket, admission) = scheduler.admit();
                    assert!(!minted.contains(&ticket), "tickets are fresh");
                    minted.push(ticket);
                    if matches!(admission, Admission::Busy | Admission::Closed) {
                        left.push(ticket);
                    }
                } else if let Some(&target) = minted.get(usize::try_from(random.next()).unwrap() % minted.len().max(1)) {
                    let before: Vec<(Ticket, bool, bool)> =
                        minted.iter().map(|t| (*t, scheduler.is_running(*t), scheduler.is_queued(*t))).collect();
                    let head = scheduler.queue.front().copied();
                    let outcome = scheduler.leave(target);
                    for (ticket, running, queued) in before {
                        if ticket == target {
                            continue;
                        }
                        let promoted = matches!(outcome, Left::Running { .. }) && Some(ticket) == head;
                        if promoted {
                            assert!(scheduler.is_running(ticket) && !scheduler.is_queued(ticket));
                        } else {
                            assert_eq!((scheduler.is_running(ticket), scheduler.is_queued(ticket)), (running, queued));
                        }
                    }
                    if outcome != Left::Absent {
                        left.push(target);
                    }
                }
                for ticket in &left {
                    assert!(!scheduler.is_running(*ticket), "a cell that left or was refused never runs (again)");
                }
                check(&scheduler);
            }
        }
    }
}

//! Which of a session's code cells may run (ADR 0066 §3, `REV13a` H1 and M9).
//!
//! One running slot and a bounded first-in-first-out queue. The shell
//! (`aim::coderun::scheduler`) owns the clock, the worker and the cells' names; this module only
//! decides which ticket runs, waits, or is refused. The theorems say why termination by ticket is
//! safe: leaving never touches another cell except to hand the slot to the queue's head, and a
//! ticket that left (or was refused) can never run afterwards, whatever happens next.
use alloc::vec::Vec;
use vstd::prelude::*;

verus! {

/// A cell's place in the scheduler. Tickets are minted in increasing order and never reused.
pub type Ticket = u64;

/// What [`Cells::arrive`] decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arrival {
    /// The cell runs now.
    Running,
    /// The cell waits behind `ahead` others (the running one included).
    Queued {
        /// Cells ahead of it.
        ahead: usize,
    },
    /// The queue is full.
    Busy,
    /// The scheduler is closed (or has minted its last ticket).
    Closed,
}

/// What [`Cells::leave`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Departure {
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

/// The scheduler's state, as proofs see it.
pub struct CellsView {
    /// How many cells may wait.
    pub capacity: nat,
    /// The cell that runs, if any.
    pub running: Option<Ticket>,
    /// The waiting cells, oldest first.
    pub queue: Seq<Ticket>,
    /// The next ticket to mint.
    pub next: Ticket,
    /// Whether the scheduler refuses every admission.
    pub closed: bool,
}

/// DRAFT(ADR-0066): the scheduler's invariant. At most one cell runs (one slot). The queue is
/// bounded, has no duplicates and never holds the running ticket. Every live ticket was minted
/// before `next`. A cell waits only while another runs, and a closed scheduler holds nothing.
pub open spec fn cells_wf(v: CellsView) -> bool {
    &&& v.capacity <= usize::MAX as nat
    &&& v.queue.len() <= v.capacity
    &&& v.queue.no_duplicates()
    &&& (v.running is Some ==> !v.queue.contains(v.running->0) && v.running->0 < v.next)
    &&& (forall|i: int| 0 <= i < v.queue.len() ==> #[trigger] v.queue[i] < v.next)
    &&& (v.queue.len() > 0 ==> v.running is Some)
    &&& (v.closed ==> v.running is None && v.queue.len() == 0)
}

/// DRAFT(ADR-0066): a new scheduler whose queue holds at most `capacity` cells.
pub open spec fn cells_start(capacity: usize) -> CellsView {
    CellsView {
        capacity: capacity as nat,
        running: None,
        queue: Seq::empty(),
        next: 0,
        closed: false,
    }
}

/// DRAFT(ADR-0066): what admitting the ticket `v.next` decides. It runs at once only when nothing
/// runs and nothing waits; otherwise it waits while the queue has room; otherwise it is refused.
/// A closed scheduler admits nothing, and neither does one that minted its last ticket (tickets
/// are never reused).
pub open spec fn arrival_of(v: CellsView) -> Arrival {
    if v.closed || v.next == u64::MAX {
        Arrival::Closed
    } else if v.running is None && v.queue.len() == 0 {
        Arrival::Running
    } else if v.queue.len() < v.capacity {
        Arrival::Queued { ahead: (v.queue.len() + 1) as usize }
    } else {
        Arrival::Busy
    }
}

/// The ticket counter after an admission: it only grows, and stops at the last ticket.
pub open spec fn next_ticket(next: Ticket) -> Ticket {
    if next < u64::MAX {
        (next + 1) as Ticket
    } else {
        next
    }
}

/// DRAFT(ADR-0066): the scheduler after `arrive`. The fresh ticket takes the slot or joins the back
/// of the queue; a refusal changes nothing but the ticket counter.
pub open spec fn arrived(v: CellsView) -> CellsView {
    let counted = CellsView { next: next_ticket(v.next), ..v };
    match arrival_of(v) {
        Arrival::Running => CellsView { running: Some(v.next), ..counted },
        Arrival::Queued { .. } => CellsView { queue: v.queue.push(v.next), ..counted },
        _ => counted,
    }
}

/// `q` without the ticket `t` (a queue holds each ticket at most once).
pub open spec fn dequeued(q: Seq<Ticket>, t: Ticket) -> Seq<Ticket> {
    if q.contains(t) {
        q.remove(q.index_of(t))
    } else {
        q
    }
}

/// DRAFT(ADR-0066): what `leave(t)` reports.
pub open spec fn departure_of(v: CellsView, t: Ticket) -> Departure {
    if v.running == Some(t) {
        Departure::Running {
            next: if v.queue.len() > 0 {
                Some(v.queue[0])
            } else {
                None
            },
        }
    } else if v.queue.contains(t) {
        Departure::Queued
    } else {
        Departure::Absent
    }
}

/// DRAFT(ADR-0066): the scheduler after `leave(t)`. A running ticket hands the slot to the head
/// of the queue; a queued ticket leaves the queue; any other ticket changes nothing.
pub open spec fn departed(v: CellsView, t: Ticket) -> CellsView {
    if v.running == Some(t) {
        if v.queue.len() > 0 {
            CellsView { running: Some(v.queue[0]), queue: v.queue.drop_first(), ..v }
        } else {
            CellsView { running: None, ..v }
        }
    } else {
        CellsView { queue: dequeued(v.queue, t), ..v }
    }
}

/// DRAFT(ADR-0066): the scheduler after `close`: nothing runs, nothing waits, nothing is admitted.
pub open spec fn shut(v: CellsView) -> CellsView {
    CellsView { running: None, queue: Seq::empty(), closed: true, ..v }
}

/// Whether `t` runs.
pub open spec fn runs(v: CellsView, t: Ticket) -> bool {
    v.running == Some(t)
}

/// Whether `t` waits.
pub open spec fn queues(v: CellsView, t: Ticket) -> bool {
    v.queue.contains(t)
}

/// DRAFT(ADR-0066): how many cells are ahead of a queued `t`, the running one included.
pub open spec fn ahead_of(v: CellsView, t: Ticket) -> Option<usize> {
    if v.queue.contains(t) {
        Some(
            (v.queue.index_of(t) + if v.running is Some {
                1int
            } else {
                0int
            }) as usize,
        )
    } else {
        None
    }
}

/// `t` can never run again: it neither runs nor waits, and it is not the next ticket to mint.
pub open spec fn retired(v: CellsView, t: Ticket) -> bool {
    !runs(v, t) && !queues(v, t) && (t < v.next || v.next == u64::MAX)
}

/// One operation of the shell.
pub enum CellOp {
    /// [`Cells::arrive`].
    Arrive,
    /// [`Cells::leave`] of a ticket.
    Leave(Ticket),
    /// [`Cells::close`].
    Close,
}

/// The scheduler after one operation.
pub open spec fn cells_step(v: CellsView, op: CellOp) -> CellsView {
    match op {
        CellOp::Arrive => arrived(v),
        CellOp::Leave(t) => departed(v, t),
        CellOp::Close => shut(v),
    }
}

/// The scheduler after a sequence of operations.
pub open spec fn cells_replay(v: CellsView, ops: Seq<CellOp>) -> CellsView
    decreases ops.len(),
{
    if ops.len() == 0 {
        v
    } else {
        cells_replay(cells_step(v, ops[0]), ops.drop_first())
    }
}

// ---------------------------------------------------------------------------------------------
// Theorems
// ---------------------------------------------------------------------------------------------
/// The index of a queued ticket is the one position that holds it.
proof fn lemma_index_of(q: Seq<Ticket>, t: Ticket, i: int)
    requires
        q.no_duplicates(),
        0 <= i < q.len(),
        q[i] == t,
    ensures
        q.index_of(t) == i,
{
    assert(q.contains(t));
    let j = q.index_of(t);
    assert(0 <= j < q.len() && q[j] == t);
}

/// Removing a queued ticket keeps the other tickets, in order, and only them.
proof fn lemma_dequeued(q: Seq<Ticket>, t: Ticket)
    requires
        q.no_duplicates(),
    ensures
        dequeued(q, t).no_duplicates(),
        dequeued(q, t).len() <= q.len(),
        !dequeued(q, t).contains(t),
        forall|u: Ticket| u != t ==> (#[trigger] dequeued(q, t).contains(u) <==> q.contains(u)),
        forall|i: int| 0 <= i < dequeued(q, t).len() ==> q.contains(#[trigger] dequeued(q, t)[i]),
{
    if q.contains(t) {
        let k = q.index_of(t);
        let r = q.remove(k);
        assert(0 <= k < q.len() && q[k] == t);
        assert(r.len() == q.len() - 1);
        assert forall|i: int| 0 <= i < r.len() implies #[trigger] r[i] == if i < k {
            q[i]
        } else {
            q[i + 1]
        } by {}
        assert forall|i: int, j: int| 0 <= i < r.len() && 0 <= j < r.len() && i != j implies r[i]
            != r[j] by {
            let a = if i < k {
                i
            } else {
                i + 1
            };
            let b = if j < k {
                j
            } else {
                j + 1
            };
            assert(r[i] == q[a] && r[j] == q[b] && a != b);
        }
        assert forall|i: int| 0 <= i < r.len() implies q.contains(#[trigger] r[i]) by {
            let a = if i < k {
                i
            } else {
                i + 1
            };
            assert(r[i] == q[a]);
        }
        if r.contains(t) {
            let i = choose|i: int| 0 <= i < r.len() && r[i] == t;
            let a = if i < k {
                i
            } else {
                i + 1
            };
            assert(q[a] == t && a != k);
        }
        assert forall|u: Ticket| u != t implies (#[trigger] r.contains(u) <==> q.contains(u)) by {
            if q.contains(u) {
                let a = choose|a: int| 0 <= a < q.len() && q[a] == u;
                assert(a != k);
                let i = if a < k {
                    a
                } else {
                    a - 1
                };
                assert(r[i] == u);
            }
            if r.contains(u) {
                let i = choose|i: int| 0 <= i < r.len() && r[i] == u;
                assert(q.contains(r[i]));
            }
        }
    }
}

/// Dropping the head keeps the other tickets and only them.
proof fn lemma_drop_first(q: Seq<Ticket>)
    requires
        q.no_duplicates(),
        q.len() > 0,
    ensures
        q.drop_first().no_duplicates(),
        !q.drop_first().contains(q[0]),
        forall|u: Ticket| u != q[0] ==> (#[trigger] q.drop_first().contains(u) <==> q.contains(u)),
{
    let r = q.drop_first();
    assert forall|i: int| 0 <= i < r.len() implies #[trigger] r[i] == q[i + 1] by {}
    assert forall|i: int, j: int| 0 <= i < r.len() && 0 <= j < r.len() && i != j implies r[i]
        != r[j] by {
        assert(r[i] == q[i + 1] && r[j] == q[j + 1]);
    }
    if r.contains(q[0]) {
        let i = choose|i: int| 0 <= i < r.len() && r[i] == q[0];
        assert(q[i + 1] == q[0]);
    }
    assert forall|u: Ticket| u != q[0] implies (#[trigger] r.contains(u) <==> q.contains(u)) by {
        if q.contains(u) {
            let a = choose|a: int| 0 <= a < q.len() && q[a] == u;
            assert(a != 0);
            assert(r[a - 1] == u);
        }
        if r.contains(u) {
            let i = choose|i: int| 0 <= i < r.len() && r[i] == u;
            assert(q[i + 1] == u);
        }
    }
}

/// A new scheduler is well formed.
pub proof fn lemma_cells_start_wf(capacity: usize)
    ensures
        cells_wf(cells_start(capacity)),
{
}

/// Admitting keeps the invariant.
pub proof fn lemma_arrived_wf(v: CellsView)
    requires
        cells_wf(v),
    ensures
        cells_wf(arrived(v)),
{
    let w = arrived(v);
    if arrival_of(v) is Queued {
        let q = v.queue.push(v.next);
        assert(q[q.len() - 1] == v.next);
        assert forall|i: int| 0 <= i < q.len() implies #[trigger] q[i] < w.next by {
            if i < v.queue.len() {
                assert(q[i] == v.queue[i]);
            }
        }
        assert forall|i: int, j: int| 0 <= i < q.len() && 0 <= j < q.len() && i != j implies q[i]
            != q[j] by {
            if i < v.queue.len() {
                assert(q[i] == v.queue[i] && v.queue[i] < v.next);
            }
            if j < v.queue.len() {
                assert(q[j] == v.queue[j] && v.queue[j] < v.next);
            }
        }
        let r = v.running->0;
        if q.contains(r) {
            let i = choose|i: int| 0 <= i < q.len() && q[i] == r;
            if i < v.queue.len() {
                assert(v.queue[i] == r);
            }
        }
    } else if arrival_of(v) is Running {
        assert(w.queue.len() == 0);
    } else {
        assert forall|i: int| 0 <= i < w.queue.len() implies #[trigger] w.queue[i] < w.next by {
            assert(v.queue[i] < v.next);
        }
    }
}

/// Leaving keeps the invariant.
pub proof fn lemma_departed_wf(v: CellsView, t: Ticket)
    requires
        cells_wf(v),
    ensures
        cells_wf(departed(v, t)),
{
    let w = departed(v, t);
    if v.running == Some(t) {
        if v.queue.len() > 0 {
            lemma_drop_first(v.queue);
            assert(v.queue[0] < v.next);
            assert forall|i: int| 0 <= i < w.queue.len() implies #[trigger] w.queue[i] < w.next by {
                assert(w.queue[i] == v.queue[i + 1]);
            }
            assert(w.queue.len() > 0 ==> w.running is Some);
        }
    } else {
        lemma_dequeued(v.queue, t);
        assert forall|i: int| 0 <= i < w.queue.len() implies #[trigger] w.queue[i] < w.next by {
            let u = w.queue[i];
            assert(v.queue.contains(u));
            let a = choose|a: int| 0 <= a < v.queue.len() && v.queue[a] == u;
            assert(v.queue[a] < v.next);
        }
        if v.running is Some {
            let r = v.running->0;
            assert(r != t);
            assert(!w.queue.contains(r));
        }
    }
}

/// Closing keeps the invariant.
pub proof fn lemma_shut_wf(v: CellsView)
    requires
        cells_wf(v),
    ensures
        cells_wf(shut(v)),
{
}

/// Every operation keeps the invariant.
pub proof fn lemma_step_wf(v: CellsView, op: CellOp)
    requires
        cells_wf(v),
    ensures
        cells_wf(cells_step(v, op)),
{
    match op {
        CellOp::Arrive => lemma_arrived_wf(v),
        CellOp::Leave(t) => lemma_departed_wf(v, t),
        CellOp::Close => lemma_shut_wf(v),
    }
}

/// At most one cell runs, and it never also waits.
pub proof fn theorem_one_running(v: CellsView)
    requires
        cells_wf(v),
    ensures
        forall|t: Ticket| runs(v, t) ==> !queues(v, t),
        forall|t: Ticket, u: Ticket| runs(v, t) && runs(v, u) ==> t == u,
{
}

/// A cell runs at once only when nothing runs and nothing waits; it waits only while there is
/// room; a full queue refuses it; a closed scheduler admits nothing and stays empty.
pub proof fn theorem_admission(v: CellsView)
    requires
        cells_wf(v),
    ensures
        arrival_of(v) is Running ==> v.running is None && v.queue.len() == 0 && runs(
            arrived(v),
            v.next,
        ),
        arrival_of(v) is Queued ==> v.running is Some && v.queue.len() < v.capacity && queues(
            arrived(v),
            v.next,
        ),
        arrival_of(v) is Busy ==> v.queue.len() == v.capacity && arrived(v).queue == v.queue
            && arrived(v).running == v.running,
        v.closed ==> arrival_of(v) is Closed && arrived(v).running is None && arrived(v).queue.len()
            == 0 && arrived(v).closed,
{
    if arrival_of(v) is Queued {
        let q = v.queue.push(v.next);
        assert(q[q.len() - 1] == v.next);
    }
}

/// The queue is first in, first out: a new cell joins the back, and the head takes the slot.
pub proof fn theorem_fifo(v: CellsView, t: Ticket)
    requires
        cells_wf(v),
        runs(v, t),
    ensures
        arrival_of(v) is Queued ==> arrived(v).queue == v.queue.push(v.next),
        v.queue.len() > 0 ==> runs(departed(v, t), v.queue[0]) && departed(v, t).queue
            == v.queue.drop_first(),
{
}

/// `leave(t)` never changes whether another ticket runs or waits, except that the queue's head
/// starts running when `t` was running.
pub proof fn theorem_leave_changes_only_its_ticket(v: CellsView, t: Ticket, u: Ticket)
    requires
        cells_wf(v),
        u != t,
    ensures
        ({
            let w = departed(v, t);
            let promoted = runs(v, t) && v.queue.len() > 0 && u == v.queue[0];
            if promoted {
                runs(w, u) && !queues(w, u) && queues(v, u)
            } else {
                runs(w, u) == runs(v, u) && queues(w, u) == queues(v, u)
            }
        }),
{
    if v.running == Some(t) {
        if v.queue.len() > 0 {
            lemma_drop_first(v.queue);
        }
    } else {
        lemma_dequeued(v.queue, t);
    }
    if runs(v, t) && v.queue.len() > 0 && u == v.queue[0] {
        assert(v.queue.contains(u));
    }
}

/// After `leave(t)`, `t` neither runs nor waits, whatever state it was in.
pub proof fn theorem_leave_retires(v: CellsView, t: Ticket)
    requires
        cells_wf(v),
        runs(v, t) || queues(v, t),
    ensures
        retired(departed(v, t), t),
{
    if v.running == Some(t) {
        if v.queue.len() > 0 {
            lemma_drop_first(v.queue);
            assert(v.queue[0] != t);
        }
    } else {
        lemma_dequeued(v.queue, t);
        let a = choose|a: int| 0 <= a < v.queue.len() && v.queue[a] == t;
        assert(v.queue[a] < v.next);
    }
}

/// A refused ticket never enters the scheduler.
pub proof fn theorem_refused_is_retired(v: CellsView)
    requires
        cells_wf(v),
        arrival_of(v) is Busy || arrival_of(v) is Closed,
    ensures
        retired(arrived(v), v.next),
{
    let w = arrived(v);
    if w.queue.contains(v.next) {
        let a = choose|a: int| 0 <= a < w.queue.len() && w.queue[a] == v.next;
        assert(v.queue[a] < v.next);
    }
}

/// A retired ticket stays retired through any one operation.
pub proof fn lemma_retired_step(v: CellsView, op: CellOp, t: Ticket)
    requires
        cells_wf(v),
        retired(v, t),
    ensures
        retired(cells_step(v, op), t),
{
    match op {
        CellOp::Arrive => {
            let w = arrived(v);
            if arrival_of(v) is Queued {
                let q = v.queue.push(v.next);
                if q.contains(t) {
                    let a = choose|a: int| 0 <= a < q.len() && q[a] == t;
                    if a < v.queue.len() {
                        assert(v.queue[a] == t);
                    }
                }
            }
        },
        CellOp::Leave(u) => {
            if v.running == Some(u) {
                if v.queue.len() > 0 {
                    lemma_drop_first(v.queue);
                    assert(v.queue[0] != t);
                }
            } else {
                lemma_dequeued(v.queue, u);
            }
        },
        CellOp::Close => {},
    }
}

/// A retired ticket never runs again, whatever sequence of operations follows.
pub proof fn theorem_retired_never_runs(v: CellsView, ops: Seq<CellOp>, t: Ticket)
    requires
        cells_wf(v),
        retired(v, t),
    ensures
        cells_wf(cells_replay(v, ops)),
        retired(cells_replay(v, ops), t),
        !runs(cells_replay(v, ops), t),
    decreases ops.len(),
{
    if ops.len() > 0 {
        lemma_step_wf(v, ops[0]);
        lemma_retired_step(v, ops[0], t);
        theorem_retired_never_runs(cells_step(v, ops[0]), ops.drop_first(), t);
    }
}

/// A ticket that left while queued never runs (`REV13a` H1).
pub proof fn theorem_left_while_queued_never_runs(v: CellsView, t: Ticket, ops: Seq<CellOp>)
    requires
        cells_wf(v),
        queues(v, t),
    ensures
        departure_of(v, t) is Queued,
        !runs(cells_replay(departed(v, t), ops), t),
{
    theorem_one_running(v);
    theorem_leave_retires(v, t);
    lemma_departed_wf(v, t);
    theorem_retired_never_runs(departed(v, t), ops, t);
}

/// After `close`, nothing runs or waits and every later admission is refused.
pub proof fn theorem_closed_stays_closed(v: CellsView, op: CellOp)
    requires
        cells_wf(v),
        v.closed,
    ensures
        cells_step(v, op).closed,
        cells_step(v, op).running is None,
        cells_step(v, op).queue.len() == 0,
        arrival_of(v) is Closed,
{
    if let CellOp::Leave(t) = op {
        assert(dequeued(v.queue, t) == v.queue);
    }
}

// ---------------------------------------------------------------------------------------------
// Executable implementation, proven to refine the specs above.
// ---------------------------------------------------------------------------------------------
/// One running slot and a bounded queue. Fields are private; the type invariant is [`cells_wf`].
#[derive(Debug)]
pub struct Cells {
    capacity: usize,
    running: Option<Ticket>,
    queue: Vec<Ticket>,
    next: Ticket,
    closed: bool,
}

impl View for Cells {
    type V = CellsView;

    closed spec fn view(&self) -> CellsView {
        CellsView {
            capacity: self.capacity as nat,
            running: self.running,
            queue: self.queue@,
            next: self.next,
            closed: self.closed,
        }
    }
}

/// `v` with `x` appended (a fresh vector: fields of a type-invariant struct are replaced whole).
fn pushed(v: &[Ticket], x: Ticket) -> (out: Vec<Ticket>)
    ensures
        out@ == v@.push(x),
{
    let mut out: Vec<Ticket> = Vec::new();
    let n = v.len();
    for k in 0..n
        invariant
            n == v@.len(),
            out@.len() == k,
            forall|j: int| 0 <= j < k ==> #[trigger] out@[j] == v@[j],
    {
        out.push(v[k]);
    }
    out.push(x);
    proof {
        assert(out@ =~= v@.push(x));
    }
    out
}

/// `v` without position `i`.
fn removed(v: &[Ticket], i: usize) -> (out: Vec<Ticket>)
    requires
        i < v@.len(),
    ensures
        out@ == v@.remove(i as int),
{
    let mut out: Vec<Ticket> = Vec::new();
    let n = v.len();
    for k in 0..n
        invariant
            n == v@.len(),
            i < n,
            out@.len() == if k <= i {
                k as int
            } else {
                k - 1
            },
            forall|j: int|
                0 <= j < out@.len() ==> #[trigger] out@[j] == if j < i {
                    v@[j]
                } else {
                    v@[j + 1]
                },
    {
        if k != i {
            out.push(v[k]);
        }
    }
    proof {
        assert(out@ =~= v@.remove(i as int));
    }
    out
}

/// A copy of `v`.
fn copied(v: &[Ticket]) -> (out: Vec<Ticket>)
    ensures
        out@ == v@,
{
    let mut out: Vec<Ticket> = Vec::new();
    let n = v.len();
    for k in 0..n
        invariant
            n == v@.len(),
            out@.len() == k,
            forall|j: int| 0 <= j < k ==> #[trigger] out@[j] == v@[j],
    {
        out.push(v[k]);
    }
    proof {
        assert(out@ =~= v@);
    }
    out
}

/// Where `t` is in `v`.
fn position(v: &[Ticket], t: Ticket) -> (r: Option<usize>)
    ensures
        r is None ==> !v@.contains(t),
        r matches Some(i) ==> i < v@.len() && v@[i as int] == t,
{
    let n = v.len();
    for k in 0..n
        invariant
            n == v@.len(),
            forall|i: int| 0 <= i < k ==> #[trigger] v@[i] != t,
    {
        if v[k] == t {
            return Some(k);
        }
    }
    proof {
        if v@.contains(t) {
            let w = choose|w: int| 0 <= w < v@.len() && v@[w] == t;
            assert(v@[w] != t);
        }
    }
    None
}

impl Cells {
    #[verifier::type_invariant]
    spec fn inv(self) -> bool {
        cells_wf(self@)
    }

    /// A scheduler whose queue holds at most `capacity` waiting cells.
    #[must_use]
    pub fn new(capacity: usize) -> (out: Self)
        ensures
            out@ == cells_start(capacity),
    {
        let out = Self { capacity, running: None, queue: Vec::new(), next: 0, closed: false };
        proof {
            assert(out@.queue =~= Seq::<Ticket>::empty());
        }
        out
    }

    /// Mints a ticket and decides whether it runs, waits, or is refused.
    pub fn arrive(&mut self) -> (out: (Ticket, Arrival))
        ensures
            out.0 == old(self)@.next,
            out.1 == arrival_of(old(self)@),
            final(self)@ == arrived(old(self)@),
    {
        proof {
            use_type_invariant(&*self);
            lemma_arrived_wf(self@);
        }
        let ticket = self.next;
        let next = if self.next < u64::MAX {
            self.next + 1
        } else {
            self.next
        };
        let admission = if self.closed || self.next == u64::MAX {
            Arrival::Closed
        } else if self.running.is_none() && self.queue.is_empty() {
            Arrival::Running
        } else if self.queue.len() < self.capacity {
            Arrival::Queued { ahead: self.queue.len() + 1 }
        } else {
            Arrival::Busy
        };
        let (running, queue) = match admission {
            Arrival::Running => (Some(ticket), copied(&self.queue)),
            Arrival::Queued { .. } => (self.running, pushed(&self.queue, ticket)),
            _ => (self.running, copied(&self.queue)),
        };
        *self = Self { capacity: self.capacity, running, queue, next, closed: self.closed };
        (ticket, admission)
    }

    /// The ticket that runs now.
    #[must_use]
    pub const fn running(&self) -> (out: Option<Ticket>)
        ensures
            out == self@.running,
    {
        self.running
    }

    /// Whether `ticket` runs now.
    #[must_use]
    pub fn is_running(&self, ticket: Ticket) -> (out: bool)
        ensures
            out == runs(self@, ticket),
    {
        match self.running {
            Some(running) => running == ticket,
            None => false,
        }
    }

    /// Whether `ticket` waits in the queue.
    #[must_use]
    pub fn is_queued(&self, ticket: Ticket) -> (out: bool)
        ensures
            out == queues(self@, ticket),
    {
        position(&self.queue, ticket).is_some()
    }

    /// How many cells are ahead of a queued `ticket`, the running one included.
    #[must_use]
    pub fn ahead(&self, ticket: Ticket) -> (out: Option<usize>)
        ensures
            out == ahead_of(self@, ticket),
    {
        proof {
            use_type_invariant(self);
        }
        match position(&self.queue, ticket) {
            Some(i) => {
                proof {
                    lemma_index_of(self@.queue, ticket, i as int);
                }
                match self.running {
                    Some(_) => Some(i + 1),
                    None => Some(i),
                }
            },
            None => None,
        }
    }

    /// The waiting tickets, oldest first.
    #[must_use]
    pub fn waiting(&self) -> (out: Vec<Ticket>)
        ensures
            out@ == self@.queue,
    {
        copied(&self.queue)
    }

    /// How many cells wait.
    #[must_use]
    pub fn queued(&self) -> (out: usize)
        ensures
            out as nat == self@.queue.len(),
    {
        self.queue.len()
    }

    /// Removes `ticket`, whatever its state. Only a running ticket's leaving starts another.
    pub fn leave(&mut self, ticket: Ticket) -> (out: Departure)
        ensures
            out == departure_of(old(self)@, ticket),
            final(self)@ == departed(old(self)@, ticket),
    {
        proof {
            use_type_invariant(&*self);
            lemma_departed_wf(self@, ticket);
        }
        let ghost pre = self@;
        if self.is_running(ticket) {
            if !self.queue.is_empty() {
                let head = self.queue[0];
                let queue = removed(&self.queue, 0);
                proof {
                    assert(queue@ =~= pre.queue.drop_first());
                }
                *self =
                Self {
                    capacity: self.capacity,
                    running: Some(head),
                    queue,
                    next: self.next,
                    closed: self.closed,
                };
                return Departure::Running { next: Some(head) };
            }
            *self =
            Self {
                capacity: self.capacity,
                running: None,
                queue: copied(&self.queue),
                next: self.next,
                closed: self.closed,
            };
            return Departure::Running { next: None };
        }
        if let Some(i) = position(&self.queue, ticket) {
            proof {
                lemma_index_of(pre.queue, ticket, i as int);
            }
            let queue = removed(&self.queue, i);
            *self =
            Self {
                capacity: self.capacity,
                running: self.running,
                queue,
                next: self.next,
                closed: self.closed,
            };
            return Departure::Queued;
        }
        proof {
            assert(dequeued(pre.queue, ticket) == pre.queue);
        }
        Departure::Absent
    }

    /// Refuses every later admission and forgets every cell. Returns the ticket that was running.
    pub fn close(&mut self) -> (out: Option<Ticket>)
        ensures
            out == old(self)@.running,
            final(self)@ == shut(old(self)@),
    {
        proof {
            use_type_invariant(&*self);
            lemma_shut_wf(self@);
        }
        let running = self.running;
        *self =
        Self {
            capacity: self.capacity,
            running: None,
            queue: Vec::new(),
            next: self.next,
            closed: true,
        };
        proof {
            assert(self@.queue =~= Seq::<Ticket>::empty());
        }
        running
    }
}

} // verus!

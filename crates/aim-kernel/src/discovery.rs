//! A single file and byte admission budget for resource discovery (ADR 0038).
//!
//! The shell requests a batch only after reserving the worst-case per-file bytes. Every returned
//! read settles one reservation, including bytes from a file later discarded by precedence.
use vstd::prelude::*;

verus! {

/// One read's accounting result. A failed read spends its entire reservation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadCharge {
    /// Bytes the backend returned, capped to the advertised per-file limit.
    Bytes(u64),
    /// No file existed; refund the reservation.
    Missing,
    /// The attempted read failed; charge its whole reservation.
    Failed,
}

/// A caller tried to settle more reads than it admitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BudgetError {
    /// There is no pending reservation to settle.
    NoReservation,
}

/// The accounting state visible to proofs.
pub struct BudgetView {
    /// Maximum admitted files.
    pub max_files: nat,
    /// Maximum bytes charged or reserved at any instant.
    pub max_bytes: nat,
    /// Worst-case bytes charged per file.
    pub cap: nat,
    /// File slots not yet admitted.
    pub files_left: nat,
    /// Bytes not charged or reserved.
    pub bytes_left: nat,
    /// Worst-case bytes reserved by reads admitted but not yet settled.
    pub reserved_bytes: nat,
}

/// LOCKED(ADR-0050): admitted file slots and charged plus pending bytes never exceed their limits.
pub open spec fn budget_wf(v: BudgetView) -> bool {
    v.files_left <= v.max_files && v.bytes_left <= v.max_bytes && v.reserved_bytes <= v.max_bytes
        - v.bytes_left && (v.cap == 0 ==> v.reserved_bytes == 0)
}

/// LOCKED(ADR-0050): reserve only a prefix fitting the caller's batch, file slots, and worst-case
/// byte credit. A zero per-file cap admits nothing, matching discovery's previous behavior.
pub open spec fn reservable(v: BudgetView, wanted: nat, batch: nat) -> nat {
    if v.cap == 0 {
        0
    } else {
        let a = if wanted < batch {
            wanted
        } else {
            batch
        };
        let b = if a < v.files_left {
            a
        } else {
            v.files_left
        };
        let by_bytes = v.bytes_left / v.cap;
        if b < by_bytes {
            b
        } else {
            by_bytes
        }
    }
}

/// LOCKED(ADR-0050): missing reads refund their reservation, failed reads charge the cap,
/// and successful reads charge at most the cap even when later discarded by precedence.
pub open spec fn charged(cap: nat, read: ReadCharge) -> nat {
    match read {
        ReadCharge::Bytes(n) => if (n as nat) < cap {
            n as nat
        } else {
            cap
        },
        ReadCharge::Missing => 0,
        ReadCharge::Failed => cap,
    }
}

/// The budget state produced by `new` before any reads are admitted.
pub open spec fn initial_budget(max_files: nat, max_bytes: nat, cap: nat) -> BudgetView {
    BudgetView {
        max_files,
        max_bytes,
        cap,
        files_left: max_files,
        bytes_left: max_bytes,
        reserved_bytes: 0,
    }
}

/// The exact accounting transition for one reservation.
pub open spec fn reserved_budget(v: BudgetView, wanted: nat, batch: nat) -> BudgetView {
    let count = reservable(v, wanted, batch);
    BudgetView {
        max_files: v.max_files,
        max_bytes: v.max_bytes,
        cap: v.cap,
        files_left: (v.files_left - count) as nat,
        bytes_left: (v.bytes_left - count * v.cap) as nat,
        reserved_bytes: v.reserved_bytes + count * v.cap,
    }
}

/// The exact accounting transition for one successful settlement.
pub open spec fn settled_budget(v: BudgetView, read: ReadCharge) -> BudgetView {
    BudgetView {
        max_files: v.max_files,
        max_bytes: v.max_bytes,
        cap: v.cap,
        files_left: v.files_left,
        bytes_left: (v.bytes_left + v.cap - charged(v.cap, read)) as nat,
        reserved_bytes: (v.reserved_bytes - v.cap) as nat,
    }
}

/// One scope's verified admission counter. It owns no I/O or parser state.
#[derive(Debug)]
pub struct Budget {
    max_files: u64,
    max_bytes: u64,
    cap: u64,
    files_left: u64,
    bytes_left: u64,
    reserved_bytes: u64,
}

impl View for Budget {
    type V = BudgetView;

    closed spec fn view(&self) -> BudgetView {
        BudgetView {
            max_files: self.max_files as nat,
            max_bytes: self.max_bytes as nat,
            cap: self.cap as nat,
            files_left: self.files_left as nat,
            bytes_left: self.bytes_left as nat,
            reserved_bytes: self.reserved_bytes as nat,
        }
    }
}

impl Budget {
    #[verifier::type_invariant]
    spec fn inv(self) -> bool {
        budget_wf(self@)
    }

    /// Starts one scope with all of its file and byte credit available.
    #[must_use]
    pub fn new(max_files: u64, max_bytes: u64, cap: u64) -> (out: Self)
        ensures
            budget_wf(out@),
            out@ == initial_budget(max_files as nat, max_bytes as nat, cap as nat),
            out@.files_left == max_files as nat,
            out@.bytes_left == max_bytes as nat,
    {
        Self {
            max_files,
            max_bytes,
            cap,
            files_left: max_files,
            bytes_left: max_bytes,
            reserved_bytes: 0,
        }
    }

    /// Reserves the largest safe prefix for the next batch.
    #[must_use]
    pub fn reserve(&mut self, wanted: u64, batch: u64) -> (selected: u64)
        ensures
            selected as nat == reservable(old(self)@, wanted as nat, batch as nat),
            budget_wf(final(self)@),
            final(self)@ == reserved_budget(old(self)@, wanted as nat, batch as nat),
            final(self)@.files_left + selected as nat == old(self)@.files_left,
            final(self)@.reserved_bytes == old(self)@.reserved_bytes + selected as nat
                * final(self)@.cap,
            final(self)@.bytes_left + selected as nat * final(self)@.cap == old(self)@.bytes_left,
    {
        proof {
            use_type_invariant(&*self);
        }
        if self.cap == 0 {
            return 0;
        }
        let selected = wanted.min(batch).min(self.files_left).min(self.bytes_left / self.cap);
        proof {
            vstd::arithmetic::div_mod::lemma_remainder(self.bytes_left as int, self.cap as int);
            let x = self.bytes_left as int;
            let c = self.cap as int;
            let q = x / c;
            assert((selected as int) <= q);
            assert((selected as int) * c <= q * c) by (nonlinear_arith)
                requires
                    selected as int <= q,
                    0 < c,
            ;
            assert(q * c <= x);
            assert((selected as int) * c <= x);
        }
        let files_left = self.files_left - selected;
        let bytes_left = self.bytes_left - selected * self.cap;
        let reserved_bytes = self.reserved_bytes + selected * self.cap;
        *self =
        Self {
            max_files: self.max_files,
            max_bytes: self.max_bytes,
            cap: self.cap,
            files_left,
            bytes_left,
            reserved_bytes,
        };
        selected
    }

    /// Settles one admitted read. An unmatched read cannot inflate the budget.
    ///
    /// # Errors
    /// Returns `NoReservation` without a state change when there is no pending read.
    pub fn settle(&mut self, read: ReadCharge) -> (result: Result<(), BudgetError>)
        ensures
            budget_wf(final(self)@),
            (result is Err) <==> (old(self)@.cap == 0 || old(self)@.reserved_bytes < old(
                self,
            )@.cap),
            result is Err ==> final(self)@ == old(self)@,
            result is Ok ==> final(self)@ == settled_budget(old(self)@, read),
            result is Ok ==> old(self)@.reserved_bytes == final(self)@.reserved_bytes + old(
                self,
            )@.cap && final(self)@.bytes_left == old(self)@.bytes_left + old(self)@.cap - charged(
                old(self)@.cap,
                read,
            ),
    {
        proof {
            use_type_invariant(&*self);
        }
        if self.cap == 0 || self.reserved_bytes < self.cap {
            return Err(BudgetError::NoReservation);
        }
        let used = match read {
            ReadCharge::Bytes(n) => n.min(self.cap),
            ReadCharge::Missing => 0,
            ReadCharge::Failed => self.cap,
        };
        proof {
            assert(self.reserved_bytes as int <= self.max_bytes as int - self.bytes_left as int);
            assert(self.cap as int <= self.reserved_bytes as int);
            assert((self.bytes_left as int) + (self.cap as int) <= self.max_bytes as int);
        }
        let reserved_bytes = self.reserved_bytes - self.cap;
        let bytes_left = self.bytes_left + (self.cap - used);
        *self =
        Self {
            max_files: self.max_files,
            max_bytes: self.max_bytes,
            cap: self.cap,
            files_left: self.files_left,
            bytes_left,
            reserved_bytes,
        };
        Ok(())
    }

    /// File slots that have not been admitted.
    #[must_use]
    pub fn files_left(&self) -> (left: u64)
        ensures
            left as nat == self@.files_left,
    {
        self.files_left
    }

    /// Byte credit not charged or reserved.
    #[must_use]
    pub fn bytes_left(&self) -> (left: u64)
        ensures
            left as nat == self@.bytes_left,
    {
        self.bytes_left
    }
}

/// A reservation or settlement in a discovery trace.
pub enum BudgetEvent {
    /// Admit the largest prefix that fits the current credit.
    Reserve {
        /// Files requested by the caller.
        wanted: nat,
        /// Maximum batch size.
        batch: nat,
    },
    /// Settle one pending read, or leave the state unchanged when none is pending.
    Settle {
        /// Observed read outcome.
        read: ReadCharge,
    },
}

/// Accounting state for a discovery trace, including ghost totals.
pub struct BudgetTraceState {
    /// The executable budget's view.
    pub budget: BudgetView,
    /// Total file slots admitted by reservations.
    pub admitted: nat,
    /// Total settled byte charges.
    pub charged_bytes: nat,
}

/// The state before the first reservation.
pub open spec fn initial_trace(max_files: nat, max_bytes: nat, cap: nat) -> BudgetTraceState {
    BudgetTraceState {
        budget: initial_budget(max_files, max_bytes, cap),
        admitted: 0,
        charged_bytes: 0,
    }
}

/// One budget operation and its ghost accounting. A failed settle is a no-op.
pub open spec fn budget_next(pre: BudgetTraceState, event: BudgetEvent) -> BudgetTraceState {
    match event {
        BudgetEvent::Reserve { wanted, batch } => {
            let count = reservable(pre.budget, wanted, batch);
            BudgetTraceState {
                budget: reserved_budget(pre.budget, wanted, batch),
                admitted: pre.admitted + count,
                charged_bytes: pre.charged_bytes,
            }
        },
        BudgetEvent::Settle { read } => {
            if pre.budget.cap == 0 || pre.budget.reserved_bytes < pre.budget.cap {
                pre
            } else {
                BudgetTraceState {
                    budget: settled_budget(pre.budget, read),
                    admitted: pre.admitted,
                    charged_bytes: pre.charged_bytes + charged(pre.budget.cap, read),
                }
            }
        },
    }
}

/// Admitted files and settled charges exactly account for spent credit.
pub open spec fn budget_trace_wf(s: BudgetTraceState) -> bool {
    budget_wf(s.budget) && s.budget.files_left + s.admitted == s.budget.max_files
        && s.budget.bytes_left + s.budget.reserved_bytes + s.charged_bytes == s.budget.max_bytes
}

proof fn lemma_budget_step(pre: BudgetTraceState, event: BudgetEvent)
    requires
        budget_trace_wf(pre),
    ensures
        budget_trace_wf(budget_next(pre, event)),
        budget_next(pre, event).budget.max_files == pre.budget.max_files,
        budget_next(pre, event).budget.max_bytes == pre.budget.max_bytes,
        budget_next(pre, event).budget.cap == pre.budget.cap,
{
    match event {
        BudgetEvent::Reserve { wanted, batch } => {
            let v = pre.budget;
            let count = reservable(v, wanted, batch);
            if v.cap > 0 {
                assert(count <= v.bytes_left / v.cap);
                assert(count * v.cap <= v.bytes_left) by (nonlinear_arith)
                    requires
                        count <= v.bytes_left / v.cap,
                        v.cap > 0,
                ;
            }
            assert(count <= v.files_left);
        },
        BudgetEvent::Settle { read } => {
            if pre.budget.cap > 0 && pre.budget.reserved_bytes >= pre.budget.cap {
                match read {
                    ReadCharge::Bytes(n) => {},
                    ReadCharge::Missing => {},
                    ReadCharge::Failed => {},
                }
                assert(charged(pre.budget.cap, read) <= pre.budget.cap);
            }
        },
    }
}

/// Traces start at `Budget::new` and follow exact reserve and settle transitions.
pub open spec fn budget_valid_trace(
    states: Seq<BudgetTraceState>,
    events: Seq<BudgetEvent>,
    max_files: nat,
    max_bytes: nat,
    cap: nat,
) -> bool {
    states.len() == events.len() + 1 && states[0] == initial_trace(max_files, max_bytes, cap) && (
    forall|i: int| 0 <= i < events.len() ==> states[i + 1] == budget_next(states[i], events[i]))
}

proof fn lemma_budget_prefix(
    states: Seq<BudgetTraceState>,
    events: Seq<BudgetEvent>,
    max_files: nat,
    max_bytes: nat,
    cap: nat,
    n: nat,
)
    requires
        budget_valid_trace(states, events, max_files, max_bytes, cap),
        n <= events.len(),
    ensures
        budget_trace_wf(states[n as int]),
        states[n as int].budget.max_files == max_files,
        states[n as int].budget.max_bytes == max_bytes,
        states[n as int].budget.cap == cap,
    decreases n,
{
    if n > 0 {
        lemma_budget_prefix(states, events, max_files, max_bytes, cap, (n - 1) as nat);
        lemma_budget_step(states[(n - 1) as int], events[(n - 1) as int]);
    }
}

/// Across any trace from `new`, admitted files stay within the file limit, and settled charges
/// plus outstanding reservations stay within the byte limit at every prefix.
pub proof fn theorem_budget_bounds(
    states: Seq<BudgetTraceState>,
    events: Seq<BudgetEvent>,
    max_files: nat,
    max_bytes: nat,
    cap: nat,
)
    requires
        budget_valid_trace(states, events, max_files, max_bytes, cap),
    ensures
        forall|i: int|
            0 <= i < states.len() ==> states[i].admitted <= max_files && states[i].charged_bytes
                + states[i].budget.reserved_bytes <= max_bytes && budget_trace_wf(states[i]),
{
    assert forall|i: int| 0 <= i < states.len() implies states[i].admitted <= max_files
        && states[i].charged_bytes + states[i].budget.reserved_bytes <= max_bytes
        && budget_trace_wf(states[i]) by {
        lemma_budget_prefix(states, events, max_files, max_bytes, cap, i as nat);
    }
}

} // verus!

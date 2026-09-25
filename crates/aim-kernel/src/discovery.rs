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
            result is Err ==> final(self)@ == old(self)@,
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

/// Reserving and settling any number of files cannot overspend either bound.
pub proof fn theorem_budget_bounds(v: BudgetView)
    requires
        budget_wf(v),
    ensures
        v.max_files - v.files_left <= v.max_files,
        v.max_bytes - v.bytes_left - v.reserved_bytes <= v.max_bytes,
{
}

} // verus!

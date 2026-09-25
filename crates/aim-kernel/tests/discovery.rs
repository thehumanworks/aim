//! Boundary examples for the verified reservation counter.
use aim_kernel::discovery::{Budget, BudgetError, ReadCharge};

#[test]
fn fixed_discarded_and_failed_reads_share_one_budget() {
    let mut budget = Budget::new(3, 12, 4);
    assert_eq!(budget.reserve(3, 8), 3);
    assert_eq!((budget.files_left(), budget.bytes_left()), (0, 0));
    assert_eq!(budget.settle(ReadCharge::Bytes(2)), Ok(())); // parsed or discarded: still charged
    assert_eq!(budget.settle(ReadCharge::Failed), Ok(()));
    assert_eq!(budget.settle(ReadCharge::Missing), Ok(()));
    assert_eq!((budget.files_left(), budget.bytes_left()), (0, 6));
    assert_eq!(budget.settle(ReadCharge::Missing), Err(BudgetError::NoReservation));
    assert_eq!(budget.bytes_left(), 6);
    assert_eq!(budget.reserve(1, 1), 0);
}

#[test]
fn zero_cap_and_maximum_values_do_not_overflow() {
    assert_eq!(Budget::new(10, 10, 0).reserve(10, 10), 0);
    let mut budget = Budget::new(u64::MAX, u64::MAX, u64::MAX);
    assert_eq!(budget.reserve(2, 2), 1);
    assert_eq!(budget.reserve(1, 1), 0);
    assert_eq!(budget.settle(ReadCharge::Bytes(u64::MAX)), Ok(()));
    assert_eq!(budget.bytes_left(), 0);
}

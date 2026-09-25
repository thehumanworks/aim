//! Plain `cargo test` over the verified API: proofs cover all inputs, tests pin concrete behaviour
//! and guard the erased build (ghost code removed, exec code unchanged).
use aim_kernel::compaction::{Item, ItemKind, PlanError, kept_tokens};
use aim_kernel::job::{Event, Job, JobState, LifecycleError};

#[test]
fn claimant_is_exclusive_and_terminal_absorbs() {
    let mut job = Job::new(0);
    assert_eq!(job.apply(Event::Claim { worker: 1 }), Ok(()));
    assert_eq!(job.apply(Event::Claim { worker: 2 }), Err(LifecycleError::WrongState));
    assert_eq!(job.apply(Event::Start { worker: 1 }), Ok(()));
    assert_eq!(job.apply(Event::Complete { worker: 2 }), Err(LifecycleError::NotHolder));
    assert_eq!(job.apply(Event::Complete { worker: 1 }), Ok(()));
    assert_eq!(job.state(), JobState::Done { worker: 1 });
    for ev in [Event::Cancel, Event::Expire, Event::Claim { worker: 1 }] {
        assert_eq!(job.apply(ev), Err(LifecycleError::Terminal));
    }
}

#[test]
fn retry_counter_counts_expiries() {
    let mut job = Job::new(u32::MAX);
    for _ in 0..3 {
        assert_eq!(job.apply(Event::Claim { worker: 9 }), Ok(()));
        assert_eq!(job.apply(Event::Expire), Ok(()));
    }
    assert_eq!(job.retries(), 3);
}

#[test]
fn token_sum_reports_overflow_instead_of_wrapping() {
    let big = Item { kind: ItemKind::Message, pinned: false, tokens: u32::MAX };
    let items = vec![big; 3];
    assert_eq!(kept_tokens(&items, &[true; 3]), Ok(3 * u64::from(u32::MAX)));
    assert_eq!(kept_tokens(&items, &[true; 2]), Err(PlanError::LengthMismatch));
}

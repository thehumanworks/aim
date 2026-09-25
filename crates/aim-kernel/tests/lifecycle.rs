//! Plain `cargo test` pins concrete behaviour of the verified kernel's executable boundary.
use aim_kernel::board::{BoardError, check_admission, claim_admitted};
use aim_kernel::compaction::{Item, ItemKind, PlanError, kept_tokens};
use aim_kernel::job::{Claim, CleanupState, Event, Job, JobState, LifecycleError, ReviewState};

fn claim(job: &mut Job, worker: u64, claim_id: u64, lease: u64, now: u64) -> Result<(), BoardError> {
    claim_admitted(job, &[], &[], worker, 1, claim_id, lease, now)
}

#[test]
fn claimant_is_fenced_and_acceptance_is_separate() {
    let mut job = Job::new(0);
    assert_eq!(claim(&mut job, 1, 7, 10, 0), Ok(()));
    assert_eq!(job.transition(Event::Claim { worker: 2, claim_id: 8, lease_until: 10 }, 0), Err(LifecycleError::WrongState));
    assert_eq!(claim(&mut job, 2, 8, 10, 0), Err(BoardError::LifecycleRejected));
    assert_eq!(job.transition(Event::Start { generation: 0, claim_id: 7 }, 1), Ok(()));
    assert_eq!(job.transition(Event::Complete { generation: 0, claim_id: 8 }, 2), Err(LifecycleError::StaleClaim));
    assert_eq!(job.transition(Event::Complete { generation: 0, claim_id: 7 }, 2), Ok(()));
    assert_eq!(job.state(), JobState::Succeeded);
    assert_eq!(job.cleanup(), CleanupState::Pending);
    assert!(job.holds_capacity(1));
    assert_eq!(job.review(), ReviewState::Pending);
    assert!(!job.is_accepted());
    assert_eq!(job.transition(Event::Review { accepted: true, evidence_present: true }, 3), Err(LifecycleError::WrongState));
    assert_eq!(job.transition(Event::ConfirmCleanup, 3), Ok(()));
    assert_eq!(job.transition(Event::Review { accepted: true, evidence_present: false }, 3), Err(LifecycleError::MissingEvidence));
    assert_eq!(job.transition(Event::Review { accepted: true, evidence_present: true }, 3), Ok(()));
    assert!(job.is_accepted());
    assert!(!job.authorizes_attempt(0, 7, 3));
}

#[test]
fn expiry_retains_capacity_until_cleanup_then_fences_old_generation() {
    let mut job = Job::new(1);
    assert_eq!(claim(&mut job, 9, 1, 10, 0), Ok(()));
    assert_eq!(job.transition(Event::Expire { generation: 0 }, 10), Ok(()));
    assert_eq!(job.cleanup(), CleanupState::Pending);
    assert!(job.holds_capacity(9));
    assert_eq!(job.transition(Event::Retry, 10), Err(LifecycleError::CleanupPending));
    assert_eq!(job.transition(Event::ConfirmCleanup, 11), Ok(()));
    assert_eq!(job.transition(Event::Retry, 11), Ok(()));
    assert_eq!(job.generation(), 1);
    assert_eq!(claim(&mut job, 9, 2, 20, 12), Ok(()));
    assert_eq!(job.transition(Event::Start { generation: 0, claim_id: 1 }, 13), Err(LifecycleError::StaleClaim));
    assert_eq!(job.transition(Event::Start { generation: 1, claim_id: 2 }, 13), Ok(()));
}

#[test]
fn admission_requires_accepted_evidence_and_a_free_slot() {
    let pending = Job::new(0);
    assert_eq!(check_admission(&[pending], &[], 1, 2), Err(BoardError::DependencyNotAccepted));
    let mut held = Job::new(0);
    assert_eq!(claim(&mut held, 1, 1, 10, 0), Ok(()));
    let holders = [held];
    assert_eq!(check_admission(&[], &holders, 1, 1), Err(BoardError::CapacityExceeded));
    assert_eq!(check_admission(&[], &holders, 1, 2), Ok(()));
}

#[test]
fn retry_budget_and_snapshot_validation() {
    let mut job = Job::new(1);
    assert_eq!(claim(&mut job, 1, 5, 10, 0), Ok(()));
    assert_eq!(job.transition(Event::Start { generation: 0, claim_id: 5 }, 1), Ok(()));
    assert_eq!(job.transition(Event::Fail { generation: 0, claim_id: 5, cleanup_confirmed: true }, 2), Ok(()));
    assert_eq!(job.transition(Event::Retry, 3), Ok(()));
    assert_eq!(job.transition(Event::Retry, 3), Err(LifecycleError::WrongState));
    assert_eq!(
        Job::restore(JobState::Posted, ReviewState::Accepted, 1, None, CleanupState::Confirmed, 1, true).err(),
        Some(LifecycleError::InvalidSnapshot)
    );
    assert_eq!(
        Job::restore(
            JobState::Failed,
            ReviewState::Pending,
            0,
            Some(Claim { worker: 1, claim_id: 1, lease_until: 10 }),
            CleanupState::Active,
            1,
            false
        )
        .err(),
        Some(LifecycleError::InvalidSnapshot)
    );
}

#[test]
fn token_sum_reports_overflow_instead_of_wrapping() {
    let big = Item { kind: ItemKind::Message, pinned: false, tokens: u32::MAX };
    let items = vec![big; 3];
    assert_eq!(kept_tokens(&items, &[true; 3]), Ok(3 * u64::from(u32::MAX)));
    assert_eq!(kept_tokens(&items, &[true; 2]), Err(PlanError::LengthMismatch));
}

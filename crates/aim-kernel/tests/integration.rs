//! Integration reconciliation boundary cases.

mod tests {
    use aim_kernel::integration::{Decision, Failure, GitObservation, Intent, Phase, decide_cleanup, decide_next, decide_reconcile};

    fn intent(phase: Phase, result: Option<&[u8]>) -> Intent {
        Intent {
            phase,
            target: b"target-oid".to_vec(),
            source: b"source-oid".to_vec(),
            scratch: b"/owned/scratch".to_vec(),
            result: result.map(<[u8]>::to_vec),
            rescue_expected: result.is_some(),
        }
    }

    fn observed() -> GitObservation {
        GitObservation {
            target: Some(b"target-oid".to_vec()),
            source: Some(b"source-oid".to_vec()),
            scratch: Some(b"/owned/scratch".to_vec()),
            scratch_owned: true,
            target_checkout_owned: false,
            checkout_attempt_failed: false,
            rescue: None,
            failure: Failure::None,
            apply_requested: false,
        }
    }

    #[test]
    fn result_needs_rescue_then_owned_checkout_or_explicit_apply() {
        let pending = intent(Phase::Integrating, Some(b"result-oid"));
        let mut git = observed();
        assert_eq!(decide_reconcile(&pending, &git), Decision::EnsureRescue);
        assert!(!decide_cleanup(&pending, &git));

        git.rescue = Some(b"result-oid".to_vec());
        assert_eq!(decide_reconcile(&pending, &git), Decision::AcquireOwnedCheckout);
        assert!(decide_cleanup(&pending, &git));

        git.checkout_attempt_failed = true;
        assert_eq!(decide_reconcile(&pending, &git), Decision::FinalizeFailed);
        assert_eq!(decide_next(&pending, &git, Decision::FinalizeFailed), Some(Phase::Failed));

        let failed = intent(Phase::Failed, Some(b"result-oid"));
        git.target = Some(b"result-oid".to_vec());
        assert_eq!(decide_reconcile(&failed, &git), Decision::None);
        git.apply_requested = true;
        assert_eq!(decide_reconcile(&failed, &git), Decision::MarkApplied);
        assert_eq!(decide_next(&failed, &git, Decision::MarkApplied), Some(Phase::Integrated));
    }

    #[test]
    fn moved_target_records_outcome_and_cannot_retry() {
        let pending = intent(Phase::Integrating, None);
        let mut git = observed();
        git.scratch = None;
        assert_eq!(decide_reconcile(&pending, &git), Decision::RetryQueued);
        git.target = Some(b"unrelated-oid".to_vec());
        assert_eq!(decide_reconcile(&pending, &git), Decision::FinalizeFailed);
        assert_eq!(decide_next(&pending, &git, Decision::RetryQueued), None);
    }

    #[test]
    fn source_drift_blocks_advance_but_target_result_finalizes() {
        let pending = intent(Phase::Integrating, Some(b"result-oid"));
        let mut git = observed();
        git.rescue = Some(b"result-oid".to_vec());
        git.source = Some(b"unreviewed-oid".to_vec());
        git.target_checkout_owned = true;
        assert_eq!(decide_reconcile(&pending, &git), Decision::FinalizeFailed);
        git.target = Some(b"result-oid".to_vec());
        assert_eq!(decide_reconcile(&pending, &git), Decision::FinalizeIntegrated);
    }
}

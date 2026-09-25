//! Concrete scenarios over the erased build of the verified turn machine (the proofs cover every
//! input; these pin behaviour and guard the plain compilation).
use aim_kernel::turn::{Event, Phase, Turn, TurnError};

#[test]
fn a_turn_with_tools_then_a_final_answer() {
    let mut t = Turn::new();
    assert_eq!(t.apply(Event::CallComplete { id: 1 }), Ok(()));
    assert_eq!(t.apply(Event::CallComplete { id: 2 }), Ok(()));
    assert_eq!(t.apply(Event::CallComplete { id: 2 }), Err(TurnError::DuplicateCall));
    assert_eq!(t.apply(Event::Result { id: 1 }), Ok(()), "results may arrive while streaming");
    assert_eq!(t.apply(Event::ResponseDone), Ok(()));
    assert_eq!(t.phase(), Phase::AwaitingResults);
    assert_eq!(t.apply(Event::NextRequest), Err(TurnError::WrongPhase), "never request with outstanding calls");
    assert_eq!(t.outstanding(), vec![2]);
    assert_eq!(t.apply(Event::Result { id: 1 }), Err(TurnError::UnknownOrAnswered));
    assert_eq!(t.apply(Event::Result { id: 2 }), Ok(()));
    assert_eq!(t.phase(), Phase::Ready);
    assert_eq!(t.apply(Event::NextRequest), Ok(()));
    assert_eq!(t.apply(Event::ResponseDone), Ok(()), "no new calls: the final answer");
    assert_eq!(t.phase(), Phase::Settled);
    assert_eq!(t.apply(Event::Cancel), Err(TurnError::Settled));
}

#[test]
fn cancel_settles_and_lists_what_must_be_answered() {
    let mut t = Turn::new();
    t.apply(Event::CallComplete { id: 7 }).unwrap();
    t.apply(Event::CallComplete { id: 8 }).unwrap();
    t.apply(Event::Result { id: 8 }).unwrap();
    assert_eq!(t.outstanding(), vec![7], "the loop answers these with cancelled results");
    assert_eq!(t.apply(Event::Cancel), Ok(()));
    assert_eq!(t.phase(), Phase::Settled);
    assert!(t.outstanding().is_empty());
}

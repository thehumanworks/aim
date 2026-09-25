//! Concrete scenarios over the erased build of the verified turn machine (the proofs cover every
//! input; these pin behaviour and guard the plain compilation).
use aim_kernel::turn::{Event, Phase, Turn, TurnError};

const DONE: Event = Event::ResponseDone { may_continue: true };

#[test]
fn a_turn_with_tools_then_a_final_answer() {
    let mut t = Turn::new();
    assert_eq!(t.apply(Event::CallComplete { id: 1 }), Ok(()));
    assert_eq!(t.apply(Event::CallComplete { id: 2 }), Ok(()));
    assert_eq!(t.apply(Event::CallComplete { id: 2 }), Err(TurnError::DuplicateCall));
    assert_eq!(t.apply(Event::Result { id: 1 }), Ok(()), "results may arrive while streaming");
    assert_eq!(t.apply(DONE), Ok(()));
    assert_eq!(t.phase(), Phase::AwaitingResults);
    assert_eq!(t.apply(Event::NextRequest), Err(TurnError::WrongPhase), "never request with outstanding calls");
    assert_eq!(t.outstanding(), vec![2]);
    assert_eq!(t.apply(Event::Result { id: 1 }), Err(TurnError::UnknownOrAnswered));
    assert_eq!(t.apply(Event::Result { id: 2 }), Ok(()));
    assert_eq!(t.phase(), Phase::Ready);
    assert_eq!(t.apply(Event::NextRequest), Ok(()));
    assert_eq!(t.apply(DONE), Ok(()), "no new calls: the final answer");
    assert_eq!(t.phase(), Phase::Settled);
    assert_eq!(t.apply(Event::Cancel), Err(TurnError::WrongPhase));
}

#[test]
fn cancel_winds_down_until_every_call_has_a_result() {
    let mut t = Turn::new();
    t.apply(Event::CallComplete { id: 7 }).unwrap();
    t.apply(Event::CallComplete { id: 8 }).unwrap();
    t.apply(Event::Result { id: 8 }).unwrap();
    assert_eq!(t.apply(Event::Cancel), Ok(()));
    assert_eq!(t.phase(), Phase::Cancelling, "cancel answers nothing by itself");
    assert_eq!(t.outstanding(), vec![7], "the loop owes these cancelled results");
    assert_eq!(t.apply(Event::Steer), Err(TurnError::WrongPhase), "no steering while winding down");
    assert_eq!(t.apply(Event::Result { id: 7 }), Ok(()));
    assert_eq!(t.phase(), Phase::Settled);
}

#[test]
fn steers_continue_a_turn_and_are_never_lost() {
    let mut t = Turn::new();
    t.apply(Event::Steer).unwrap();
    assert_eq!(t.apply(DONE), Ok(()), "final answer, but a steer is queued");
    assert_eq!(t.phase(), Phase::Ready, "the queued steer continues the turn");
    assert_eq!(t.apply(Event::NextRequest), Ok(()));
    assert_eq!(t.queued_steers(), 0, "delivered with the request");
    t.apply(Event::Steer).unwrap();
    t.apply(Event::CallComplete { id: 1 }).unwrap();
    assert_eq!(t.apply(Event::Cancel), Ok(()));
    assert_eq!(t.returned_steers(), 1, "a steer that was never sent goes back to the caller");
    t.apply(Event::Result { id: 1 }).unwrap();
    assert_eq!(t.phase(), Phase::Settled);
}

#[test]
fn a_response_the_model_may_not_continue_winds_down() {
    let mut t = Turn::new();
    t.apply(Event::CallComplete { id: 3 }).unwrap();
    assert_eq!(t.apply(Event::ResponseDone { may_continue: false }), Ok(()));
    assert_eq!(t.phase(), Phase::Cancelling);
    t.apply(Event::Result { id: 3 }).unwrap();
    assert_eq!(t.phase(), Phase::Settled);
}

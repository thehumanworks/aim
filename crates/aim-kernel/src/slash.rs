//! Runtime slash-command output routing and deadlines (ADR 0078).
use vstd::prelude::*;

verus! {

/// Where a completed command may deliver output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Its invoking session/command is no longer current.
    Discard,
    /// A local TUI notice, never a prompt or history entry.
    User,
    /// A normal prompt to the still-current invoking session.
    Agent,
}

/// DRAFT(ADR-0078): stale results are discarded; failures and user-only output never reach a model.
pub open spec fn delivery_spec(agent_visible: bool, current: bool, succeeded: bool) -> Delivery {
    if !current {
        Delivery::Discard
    } else if agent_visible && succeeded {
        Delivery::Agent
    } else {
        Delivery::User
    }
}

/// Routes one runtime result, using identity/freshness facts supplied by the I/O shell.
pub fn delivery(agent_visible: bool, current: bool, succeeded: bool) -> (out: Delivery)
    ensures
        out == delivery_spec(agent_visible, current, succeeded),
{
    if !current {
        Delivery::Discard
    } else if agent_visible && succeeded {
        Delivery::Agent
    } else {
        Delivery::User
    }
}

/// User-only, failed and stale results cannot become agent input.
pub proof fn no_unintended_agent_output(agent_visible: bool, current: bool, succeeded: bool)
    ensures
        !agent_visible || !current || !succeeded ==> delivery_spec(
            agent_visible,
            current,
            succeeded,
        ) != Delivery::Agent,
        !current ==> delivery_spec(agent_visible, current, succeeded) == Delivery::Discard,
{
}

/// Default runtime deadline, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Maximum runtime deadline, in milliseconds (including workspace connection and tool I/O).
pub const MAX_TIMEOUT_MS: u64 = 600_000;

/// DRAFT(ADR-0078): only finite, positive, bounded deadlines are accepted.
pub open spec fn valid_timeout(ms: u64) -> bool {
    0 < ms <= MAX_TIMEOUT_MS
}

/// Parses a runtime deadline without silently changing its meaning.
pub fn timeout(ms: u64) -> (out: Option<u64>)
    ensures
        out == if valid_timeout(ms) {
            Some(ms)
        } else {
            None
        },
{
    if ms > 0 && ms <= MAX_TIMEOUT_MS {
        Some(ms)
    } else {
        None
    }
}

} // verus!

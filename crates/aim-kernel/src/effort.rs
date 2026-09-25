//! Bounded integer effort decisions (docs/adr/0013).
//!
//! The external score is quantized to basis points before it reaches this module. An explicit
//! override is still constrained by the catalog and the configured bounds; it intentionally
//! bypasses the one-step and hysteresis rules.
use vstd::prelude::*;

verus! {

/// Inputs to one effort decision. Indices and bounds may be malformed; [`next`] is total.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Input {
    /// Number of ordered efforts in the selected model's catalog.
    pub ladder_len: u32,
    /// Minimum permitted index.
    pub lo: u32,
    /// Maximum permitted index.
    pub hi: u32,
    /// Effort currently in force.
    pub current: u32,
    /// Jev's proposed position on the full ladder, in basis points.
    pub proposed_bp: u32,
    /// Completed decisions since the last change.
    pub since_change: u32,
    /// Minimum number of decisions between changes.
    pub hysteresis: u32,
    /// A user-set index, which wins over Jev and hysteresis.
    pub override_index: Option<u32>,
}

/// Clamp an index to an inclusive interval.
pub open spec fn clamp(value: u32, lo: u32, hi: u32) -> u32 {
    if value < lo {
        lo
    } else if value > hi {
        hi
    } else {
        value
    }
}

/// The legal lower bound; inverted bounds collapse to this index.
pub open spec fn lower(i: Input) -> u32 {
    if i.ladder_len == 0 {
        0
    } else if i.lo >= i.ladder_len {
        (i.ladder_len as int - 1) as u32
    } else {
        i.lo
    }
}

/// The legal upper bound after clipping both configured bounds to the catalog.
pub open spec fn upper(i: Input) -> u32 {
    if i.ladder_len == 0 {
        0
    } else {
        let clipped = if i.hi >= i.ladder_len {
            (i.ladder_len as int - 1) as u32
        } else {
            i.hi
        };
        if clipped < lower(i) {
            lower(i)
        } else {
            clipped
        }
    }
}

/// Map 0..=10000 basis points to the nearest ladder index; malformed values saturate.
pub open spec fn target(i: Input) -> u32 {
    if i.ladder_len <= 1 {
        0
    } else {
        let bp = if i.proposed_bp > 10_000 {
            10_000
        } else {
            i.proposed_bp
        };
        (((bp as int) * (i.ladder_len as int - 1) + 5_000) / 10_000) as u32
    }
}

/// LOCKED(ADR-0013): the next bounded effort, or `None` for an empty ladder. An explicit override
/// wins but stays within the ladder and the user/agent bounds.
pub open spec fn next_spec(i: Input) -> Option<u32> {
    if i.ladder_len == 0 {
        None
    } else {
        let lo = lower(i);
        let hi = upper(i);
        let current = clamp(i.current, lo, hi);
        match i.override_index {
            Some(override_index) => Some(clamp(override_index, lo, hi)),
            None => {
                let proposed = clamp(target(i), lo, hi);
                if i.since_change < i.hysteresis {
                    Some(current)
                } else if proposed > current {
                    Some((current as int + 1) as u32)
                } else if proposed < current {
                    Some((current as int - 1) as u32)
                } else {
                    Some(current)
                }
            },
        }
    }
}

/// A nonempty ladder always yields an index in the catalog and in the normalized bounds.
pub proof fn bounded(i: Input)
    ensures
        i.ladder_len == 0 ==> next_spec(i) is None,
        i.ladder_len > 0 ==> match next_spec(i) {
            Some(n) => lower(i) <= n && n <= upper(i) && n < i.ladder_len,
            None => false,
        },
{
}

/// Without an override, the decision moves at most one index from the normalized current.
pub proof fn one_step(i: Input)
    ensures
        i.ladder_len > 0 && i.override_index is None ==> match next_spec(i) {
            Some(n) => n as int <= clamp(i.current, lower(i), upper(i)) as int + 1 && clamp(
                i.current,
                lower(i),
                upper(i),
            ) as int <= n as int + 1,
            None => false,
        },
{
}

/// Without an override, a decision within the hysteresis window preserves current effort.
pub proof fn hysteresis(i: Input)
    ensures
        i.ladder_len > 0 && i.override_index is None && i.since_change < i.hysteresis ==> next_spec(
            i,
        ) == Some(clamp(i.current, lower(i), upper(i))),
{
}

/// An explicit user index wins, subject to the catalog and configured bounds.
pub proof fn override_wins(i: Input)
    ensures
        i.ladder_len > 0 ==> match i.override_index {
            Some(o) => next_spec(i) == Some(clamp(o, lower(i), upper(i))),
            None => true,
        },
{
}

/// Compute the next effort without preconditions.
#[must_use]
#[expect(clippy::cast_lossless, clippy::cast_possible_truncation, clippy::comparison_chain, reason = "Verus proves the integer bounds and direct comparisons")]
pub fn next(i: Input) -> (result: Option<u32>)
    ensures
        result == next_spec(i),
{
    if i.ladder_len == 0 {
        return None;
    }
    let lo = if i.lo >= i.ladder_len {
        i.ladder_len - 1
    } else {
        i.lo
    };
    let clipped_hi = if i.hi >= i.ladder_len {
        i.ladder_len - 1
    } else {
        i.hi
    };
    let hi = if clipped_hi < lo {
        lo
    } else {
        clipped_hi
    };
    let current = if i.current < lo {
        lo
    } else if i.current > hi {
        hi
    } else {
        i.current
    };
    if let Some(override_index) = i.override_index {
        return Some(
            if override_index < lo {
                lo
            } else if override_index > hi {
                hi
            } else {
                override_index
            },
        );
    }
    if i.since_change < i.hysteresis {
        return Some(current);
    }
    let bp = if i.proposed_bp > 10_000 {
        10_000
    } else {
        i.proposed_bp
    };
    let target = if i.ladder_len <= 1 {
        0
    } else {
        assert(bp <= 10_000);
        assert(i.ladder_len - 1 <= u32::MAX);
        assert((bp as int) * (i.ladder_len as int - 1) + 5_000 <= u64::MAX) by (nonlinear_arith);
        let width = i.ladder_len - 1;
        let scaled = (bp as u64) * (width as u64) + 5_000;
        assert(scaled as int == (bp as int) * (width as int) + 5_000);
        assert(width <= u32::MAX - 1);
        assert((bp as int) * (width as int) <= 10_000 * (width as int)) by (nonlinear_arith)
            requires
                bp <= 10_000,
                width <= u32::MAX - 1,
        ;
        assert(10_000 * (width as int) + 5_000 < ((u32::MAX as int) * 10_000));
        assert((scaled as int) < ((u32::MAX as int) * 10_000));
        assert(scaled / 10_000 <= u32::MAX);
        (scaled / 10_000) as u32
    };
    let proposed = if target < lo {
        lo
    } else if target > hi {
        hi
    } else {
        target
    };
    if proposed > current {
        Some(current + 1)
    } else if proposed < current {
        Some(current - 1)
    } else {
        Some(current)
    }
}

} // verus!
#[cfg(test)]
mod tests {
    use super as effort;

    #[test]
    fn edge_cases() {
        let input = effort::Input {
            ladder_len: 0,
            lo: 0,
            hi: 4,
            current: 0,
            proposed_bp: 10_000,
            since_change: 5,
            hysteresis: 2,
            override_index: None,
        };
        assert_eq!(effort::next(input), None);
        let input = effort::Input { ladder_len: 5, ..input };
        assert_eq!(effort::next(input), Some(1));
        assert_eq!(effort::next(effort::Input { since_change: 0, ..input }), Some(0));
        assert_eq!(effort::next(effort::Input { override_index: Some(4), ..input }), Some(4));
        assert_eq!(effort::next(effort::Input { lo: 3, hi: 1, override_index: None, ..input }), Some(3));
        assert_eq!(effort::next(effort::Input { lo: 0, hi: 4, current: 4, proposed_bp: 0, ..input }), Some(3));
        assert_eq!(effort::next(effort::Input { proposed_bp: u32::MAX, ..input }), Some(1));
    }
}

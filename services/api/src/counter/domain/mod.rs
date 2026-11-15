//! The counter's domain types, and their conversions to the contract.
//!
//! This is the shape `cargo xtask new-feature` writes, and the example it was written
//! from: the types this feature's own code speaks in, the conversions to and from the
//! generated messages beside them, and a handler (`service/v1.rs`) that reads as the
//! sequence of steps it is rather than as message-building.
//!
//! What the conversions buy is not brevity — it is that a field added to the contract
//! fails to compile in **one** place. Spread across four handlers, the same change is four
//! edits, and the one that gets missed still compiles: the field simply arrives empty.
//!
//! # Where this deviates from the generated shape, and why
//!
//! A generated slice also has `From<Model> for <Domain>` in its `models.rs`. This one does
//! not, because the counter never materialises a `Model`. Its reads and writes are single
//! statements (`service/v1.rs`) so that two concurrent increments cannot lose one — the
//! database does the arithmetic under its own row lock — and a statement that returns
//! columns is not a row sea-orm has mapped into an entity. A feature doing ordinary CRUD
//! gets that conversion; this one would have nothing to put in it.

use buffa::{Inline, MessageField};
use proto::r#gen::buffa::counter::v1::{
    CounterState, DecrementRequest, DecrementResponse, GetResponse, IncrementRequest,
    IncrementResponse, ResetResponse,
};

/// The counter, as this service's own code speaks of it.
///
/// A named pair rather than the `(i64, i64)` this used to be: the two fields are both
/// `i64` and mean entirely different things, so a tuple makes transposing them a silent
/// bug that type-checks. It travels together because the two are read in one statement and
/// must describe the same moment — a count from one read and a stamp from another is a lie
/// the wire cannot detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counter {
    /// The current value.
    pub count: i64,
    /// When `count` was last written, as epoch milliseconds.
    ///
    /// Zero means *never*, which is the contract's shared spelling for "no moment": the
    /// shells blank it rather than rendering it, so a counter nobody has touched does not
    /// display as an instant in 1970 (ADR 0020). The schema cannot produce it — the column
    /// is `NOT NULL DEFAULT CURRENT_TIMESTAMP` — so the only source is a read that found no
    /// row at all.
    pub updated_at_unix_ms: i64,
}

/// How much one call changes the count.
///
/// A type rather than an `i64` parameter, because the rule below has to be applied exactly
/// once per request and a bare integer gives nothing to hang it on. Constructing one from
/// the request is the only way to get a `Step`, so the rule cannot be skipped by a handler
/// that reads `request.by` directly — it would not type-check against [`apply`].
///
/// [`apply`]: crate::counter::service::v1::Counter
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step(i64);

impl Step {
    /// The delta to apply.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }

    /// The same step in the other direction.
    ///
    /// What makes decrement the same code path as increment, rather than a second copy of
    /// it with one sign changed.
    #[must_use]
    pub const fn negated(self) -> Self {
        Self(-self.0)
    }

    /// `by` of zero means one.
    ///
    /// Proto3 cannot distinguish an unset scalar from zero, so an omitted `by` arrives as
    /// 0. Treating that as "no change" would make the obvious call — increment with an
    /// empty body — silently do nothing.
    const fn from_by(by: i64) -> Self {
        Self(if by == 0 { 1 } else { by })
    }
}

impl From<IncrementRequest> for Step {
    fn from(v: IncrementRequest) -> Self {
        Self::from_by(v.by)
    }
}

impl From<DecrementRequest> for Step {
    /// The magnitude, not the signed delta: the direction belongs to the handler, which
    /// calls [`Step::negated`]. Folding the sign in here would make a `Step` mean different
    /// things depending on which request it came from, and nothing downstream could tell.
    fn from(v: DecrementRequest) -> Self {
        Self::from_by(v.by)
    }
}

impl From<Counter> for CounterState {
    fn from(v: Counter) -> Self {
        Self {
            count: v.count,
            updated_at_unix_ms: v.updated_at_unix_ms,
            // The generated message carries an unknown-fields store, so it cannot be built
            // field by field alone.
            ..Default::default()
        }
    }
}

impl From<Counter> for MessageField<CounterState, Inline<CounterState>> {
    fn from(v: Counter) -> Self {
        CounterState::from(v).into()
    }
}

impl From<Counter> for GetResponse {
    fn from(v: Counter) -> Self {
        Self {
            state: v.into(),
            ..Default::default()
        }
    }
}

impl From<Counter> for IncrementResponse {
    fn from(v: Counter) -> Self {
        Self {
            state: v.into(),
            ..Default::default()
        }
    }
}

impl From<Counter> for DecrementResponse {
    fn from(v: Counter) -> Self {
        Self {
            state: v.into(),
            ..Default::default()
        }
    }
}

impl From<Counter> for ResetResponse {
    fn from(v: Counter) -> Self {
        Self {
            state: v.into(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_omitted_step_means_one() {
        let request = IncrementRequest {
            by: 0,
            ..Default::default()
        };
        assert_eq!(
            Step::from(request).get(),
            1,
            "proto3 cannot tell unset from zero"
        );
    }

    #[test]
    fn an_explicit_step_is_respected() {
        let step = |by| {
            Step::from(IncrementRequest {
                by,
                ..Default::default()
            })
        };
        assert_eq!(step(5).get(), 5);
        assert_eq!(step(-3).get(), -3);
        assert_eq!(step(5).negated().get(), -5);
    }

    #[test]
    fn a_decrement_carries_its_magnitude_and_not_its_sign() {
        // Both requests mean "by one"; only the handler knows which way. A `From` that
        // negated here would make the two disagree about what a `Step` is.
        let increment = Step::from(IncrementRequest {
            by: 7,
            ..Default::default()
        });
        let decrement = Step::from(DecrementRequest {
            by: 7,
            ..Default::default()
        });
        assert_eq!(increment, decrement);
    }
}

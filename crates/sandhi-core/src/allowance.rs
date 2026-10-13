//! Opt-in weighted allowance arithmetic (TD-0032), separate from neutral metering.
//!
//! This module does not enforce budgets, select policies or establish provider authenticity.
//! Callers must supply qualified server-side observations. In particular, legacy parsers can
//! collapse absent and explicit-zero reasoning counts; this calculator never repairs that loss.
//! Live enforcement requires an admission-bound policy snapshot and durable fixed-point units.

use serde::{Deserialize, Serialize};

use crate::{CacheReadSource, CacheReadStatus, UsageBasis, UsageCompleteness, UsageV2};

/// One allowance token is 10,000 integer units. Persist units without per-call rounding.
pub const UNITS_PER_TOKEN: u64 = 10_000;

/// Integer basis-point weight: 10,000 is 1x, 1,000 is 0.1x, and 0 is explicitly free.
/// The bounded range (0..=100x) keeps operator mistakes and arithmetic costs predictable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Weight(u32);

impl Weight {
    pub fn new(basis_points: u32) -> Result<Self, AllowanceError> {
        if basis_points > 1_000_000 {
            return Err(AllowanceError::InvalidWeight);
        }
        Ok(Self(basis_points))
    }

    #[must_use]
    pub const fn basis_points(self) -> u32 {
        self.0
    }
}

impl Default for Weight {
    fn default() -> Self {
        Self(10_000)
    }
}

impl TryFrom<u32> for Weight {
    type Error = AllowanceError;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Weight> for u32 {
    fn from(value: Weight) -> Self {
        value.0
    }
}

/// Category weights, independent of provider prices or commercial subscription tiers.
/// Omitted fields retain 1x; omitted reasoning inherits output. Unknown fields are errors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllowanceWeights {
    pub fresh_input: Weight,
    pub cache_creation: Weight,
    pub cache_read: Weight,
    pub output: Weight,
    pub reasoning: Option<Weight>,
}

/// A version-one quote in fixed-point units, never a settlement receipt or a currency amount.
/// Output and reasoning are disjoint whenever the origin reports their split. If an included
/// reasoning count is unavailable and shares output's weight, output retains the unsplit total
/// and reasoning_units is None (unknown, not a measured zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllowanceQuoteV1 {
    pub fresh_input_units: u64,
    pub cache_creation_units: u64,
    pub cache_read_units: u64,
    pub output_units: u64,
    pub reasoning_units: Option<u64>,
    pub total_units: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowanceError {
    InvalidWeight,
    UsageNotFinal,
    UsageEstimated,
    CacheEvidenceUnavailable,
    ReasoningCountUnavailable,
    ReasoningConventionUnavailable,
    InconsistentReasoning,
    Overflow,
}

impl std::fmt::Display for AllowanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidWeight => "weight must be integer basis points in 0..=1000000",
            Self::UsageNotFinal => "final usage is required for an allowance quote",
            Self::UsageEstimated => "estimated usage cannot authorize a final allowance quote",
            Self::CacheEvidenceUnavailable => "cache weighting requires qualified cache evidence",
            Self::ReasoningCountUnavailable => "reasoning count unavailable for this weighting",
            Self::ReasoningConventionUnavailable => "reasoning inclusion convention unavailable",
            Self::InconsistentReasoning => "included reasoning exceeds total output",
            Self::Overflow => "allowance exceeds signed 64-bit storage range",
        })
    }
}

impl std::error::Error for AllowanceError {}

impl AllowanceWeights {
    /// Quote final reported usage with checked arithmetic; never mutate the measurement.
    /// Failure is an unresolved quote, not zero spend or permission to release a reservation.
    pub fn quote(&self, usage: &UsageV2) -> Result<AllowanceQuoteV1, AllowanceError> {
        if usage.completeness != UsageCompleteness::Final {
            return Err(AllowanceError::UsageNotFinal);
        }
        if usage.basis != UsageBasis::ProviderReported {
            return Err(AllowanceError::UsageEstimated);
        }
        if self.cache_read != self.fresh_input {
            let qualified = usage.cache_read_observation.and_then(|o| o.validated());
            match qualified.map(|o| (o.status, o.source)) {
                Some((CacheReadStatus::Reported, CacheReadSource::OriginUsage)) => {}
                Some((CacheReadStatus::Unsupported, CacheReadSource::ExplicitCapability))
                    if usage.cache_read_tokens == 0 => {}
                _ => return Err(AllowanceError::CacheEvidenceUnavailable),
            }
        }
        let reasoning_weight = self.reasoning.unwrap_or(self.output);
        let differential_reasoning = reasoning_weight != self.output;
        let reasoning = usage.reasoning_tokens;
        if reasoning.is_none()
            && (differential_reasoning || usage.reasoning_included == Some(false))
        {
            return Err(AllowanceError::ReasoningCountUnavailable);
        }
        if usage.reasoning_included.is_none()
            && (differential_reasoning || reasoning.is_none() || reasoning.unwrap_or(0) > 0)
        {
            return Err(AllowanceError::ReasoningConventionUnavailable);
        }
        let output = if usage.reasoning_included == Some(true) {
            usage
                .tokens_out
                .checked_sub(reasoning.unwrap_or(0))
                .ok_or(AllowanceError::InconsistentReasoning)?
        } else {
            usage.tokens_out
        };
        let mut quote = AllowanceQuoteV1 {
            fresh_input_units: units(usage.tokens_in, self.fresh_input)?,
            cache_creation_units: units(usage.cache_creation_tokens, self.cache_creation)?,
            cache_read_units: units(usage.cache_read_tokens, self.cache_read)?,
            output_units: units(output, self.output)?,
            reasoning_units: reasoning.map(|n| units(n, reasoning_weight)).transpose()?,
            total_units: 0,
        };
        let total = u128::from(quote.fresh_input_units)
            + u128::from(quote.cache_creation_units)
            + u128::from(quote.cache_read_units)
            + u128::from(quote.output_units)
            + u128::from(quote.reasoning_units.unwrap_or(0));
        quote.total_units = storage_units(total)?;
        Ok(quote)
    }
}

fn units(tokens: u64, weight: Weight) -> Result<u64, AllowanceError> {
    storage_units(u128::from(tokens) * u128::from(weight.basis_points()))
}

fn storage_units(value: u128) -> Result<u64, AllowanceError> {
    if value > i64::MAX as u128 {
        return Err(AllowanceError::Overflow);
    }
    Ok(value as u64)
}

//! Per-model calibration for the admission-time input-token estimate (TD-0027 S3).
//!
//! This is deliberately statistics over authoritative usage events, not another tokenizer. The
//! caller supplies the same UTF-8 wire length used by the historical `len / 4` heuristic and, once
//! the call finishes, the corresponding neutral [`UsageEvent`]. Only final, provider-reported
//! observations train the model.

use std::collections::HashMap;

use sandhi_core::{UsageBasis, UsageCompleteness, UsageEvent};

/// Samples required before a model-specific estimate may replace the historical `/4` heuristic.
pub const CALIBRATION_MIN_SAMPLES: u64 = 32;

const MIN_UNITS_PER_TOKEN: f64 = 1.5;
const MAX_UNITS_PER_TOKEN: f64 = 8.0;
const EWMA_ALPHA: f64 = 0.125;

#[derive(Debug, Clone, Copy)]
struct Calibration {
    samples: u64,
    units_per_token: f64,
}

/// In-process per-model input-token estimator.
///
/// Keys are provider slug plus model because compatible endpoints can tokenize the same bytes
/// differently. The map is intentionally process-local: usage events remain the durable truth,
/// while a restart safely returns every model to the conservative, established cold-start rule.
#[derive(Debug, Default)]
pub struct TokenEstimateCalibrator {
    providers: HashMap<String, HashMap<String, Calibration>>,
}

impl TokenEstimateCalibrator {
    /// Estimate input tokens from the request's UTF-8 wire length.
    ///
    /// Before 32 authoritative samples this is exactly the historical `(len + 3) / 4`. Once warm,
    /// the EWMA is bounded to 1.5..=8.0 units/token, and the result cannot fall below 75% of the
    /// historical estimate.
    #[must_use]
    pub fn estimate(&self, provider: &str, model: &str, input_len: usize) -> u64 {
        let baseline = baseline_estimate(input_len);
        let Some(calibration) = self
            .providers
            .get(provider)
            .and_then(|models| models.get(model))
        else {
            return baseline;
        };
        if calibration.samples < CALIBRATION_MIN_SAMPLES {
            return baseline;
        }

        let calibrated = ((input_len as f64) / calibration.units_per_token).ceil() as u64;
        let floor = baseline.saturating_mul(3).saturating_add(3) / 4;
        calibrated.max(floor)
    }

    /// Learn from one neutral event paired with the request length retained by the proxy.
    ///
    /// Cache-creation and cache-read counts are input categories too, so the denominator is the
    /// complete measured input rather than only the fresh-input portion. Estimated or incomplete
    /// events never influence future admission.
    pub fn observe(&mut self, input_len: usize, event: &UsageEvent) {
        if event.usage_completeness != UsageCompleteness::Final
            || event.usage_basis != UsageBasis::ProviderReported
        {
            return;
        }
        let input_tokens = event
            .tokens_in
            .saturating_add(event.cache_creation_tokens)
            .saturating_add(event.cache_read_tokens);
        if input_len == 0 || input_tokens == 0 {
            return;
        }

        let observed = ((input_len as f64) / (input_tokens as f64))
            .clamp(MIN_UNITS_PER_TOKEN, MAX_UNITS_PER_TOKEN);
        let entry = self
            .providers
            .entry(event.provider.clone())
            .or_default()
            .entry(event.model.clone())
            .or_insert(Calibration {
                samples: 0,
                units_per_token: observed,
            });
        if entry.samples > 0 {
            entry.units_per_token =
                EWMA_ALPHA.mul_add(observed, (1.0 - EWMA_ALPHA) * entry.units_per_token);
        }
        entry.samples = entry.samples.saturating_add(1);
    }
}

#[must_use]
pub const fn baseline_estimate(input_len: usize) -> u64 {
    (input_len as u64).saturating_add(3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::Backend;

    fn event(provider: &str, model: &str, input_tokens: u64) -> UsageEvent {
        UsageEvent::new(
            "r",
            "2026-09-16T00:00:00Z",
            provider,
            model,
            Backend::External,
        )
        .with_tokens(input_tokens, 0)
        .with_measurement(UsageCompleteness::Final, 1, Some("success".into()), None)
    }

    #[test]
    fn cold_start_is_byte_for_byte_the_existing_estimate() {
        let mut estimator = TokenEstimateCalibrator::default();
        for _ in 0..CALIBRATION_MIN_SAMPLES - 1 {
            estimator.observe(300, &event("inferflux", "qwen", 100));
        }
        assert_eq!(estimator.estimate("inferflux", "qwen", 303), 76);
    }

    #[test]
    fn calibration_is_per_model_and_counts_cached_input() {
        let mut estimator = TokenEstimateCalibrator::default();
        let cached = event("inferflux", "qwen", 40).with_cache(20, 40);
        for _ in 0..CALIBRATION_MIN_SAMPLES {
            estimator.observe(300, &cached);
        }

        assert_eq!(estimator.estimate("inferflux", "qwen", 300), 100);
        assert_eq!(estimator.estimate("inferflux", "llama", 300), 75);
        assert_eq!(estimator.estimate("openai", "qwen", 300), 75);
    }

    #[test]
    fn estimated_partial_and_zero_input_events_do_not_train() {
        let mut estimator = TokenEstimateCalibrator::default();
        let estimated = event("inferflux", "qwen", 100)
            .with_measurement(UsageCompleteness::Partial, 1, None, None)
            .with_basis(UsageBasis::Estimated);
        for _ in 0..CALIBRATION_MIN_SAMPLES + 10 {
            estimator.observe(300, &estimated);
            estimator.observe(300, &event("inferflux", "qwen", 0));
        }
        assert_eq!(estimator.estimate("inferflux", "qwen", 300), 75);
    }

    #[test]
    fn bounds_and_seventy_five_percent_floor_limit_outliers() {
        let mut low_ratio = TokenEstimateCalibrator::default();
        let mut high_ratio = TokenEstimateCalibrator::default();
        for _ in 0..CALIBRATION_MIN_SAMPLES {
            low_ratio.observe(100, &event("p", "low", 1_000));
            high_ratio.observe(1_000, &event("p", "high", 1));
        }
        assert_eq!(low_ratio.estimate("p", "low", 300), 200); // 300 / 1.5
        assert_eq!(high_ratio.estimate("p", "high", 300), 57); // ceil(75 * .75)
    }

    #[test]
    fn synthetic_replay_preserves_coverage_and_reduces_low_ratio_overshoot() {
        // Synthetic observations exercise the estimator arithmetic; these are not captured
        // request/tokenizer pairs and do not establish production or language-corpus coverage.
        let corpus = [
            ("openai", "gpt", 408usize, 102u64, 16u64),
            ("anthropic", "claude", 520, 130, 20),
            ("gemini", "gemini", 360, 90, 12),
            ("cohere", "command", 440, 110, 18),
            ("inferflux", "qwen", 300, 100, 10),
            ("inferflux", "qwen", 330, 110, 12),
        ];
        let mut estimator = TokenEstimateCalibrator::default();
        for _ in 0..CALIBRATION_MIN_SAMPLES {
            for (provider, model, input_len, input_tokens, _) in corpus {
                estimator.observe(input_len, &event(provider, model, input_tokens));
            }
        }

        let mut baseline_covered = 0;
        let mut calibrated_covered = 0;
        let mut baseline_overshoot = 0;
        let mut calibrated_overshoot = 0;
        for (provider, model, input_len, input_tokens, output_tokens) in corpus {
            let actual = input_tokens + output_tokens;
            // The fixture's observed output is used as the bounded-output term so this directly
            // models reservation-versus-settlement coverage without conflating output policy.
            let baseline = baseline_estimate(input_len) + output_tokens;
            let calibrated = estimator.estimate(provider, model, input_len) + output_tokens;
            baseline_covered += usize::from(baseline >= actual);
            calibrated_covered += usize::from(calibrated >= actual);
            if provider == "inferflux" {
                baseline_overshoot += actual.saturating_sub(baseline);
                calibrated_overshoot += actual.saturating_sub(calibrated);
            }
        }

        assert!(calibrated_covered >= baseline_covered);
        assert!(calibrated_overshoot < baseline_overshoot);
    }
}

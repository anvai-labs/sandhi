use sandhi_core::allowance::{AllowanceError, AllowanceWeights, Weight, UNITS_PER_TOKEN};
use sandhi_core::{
    billable, CacheReadObservation, CacheReadStatus, UsageBasis, UsageCompleteness, UsageV2,
};

fn usage() -> UsageV2 {
    UsageV2 {
        tokens_in: 100,
        cache_creation_tokens: 20,
        cache_read_tokens: 80,
        tokens_out: 50,
        reasoning_tokens: Some(30),
        reasoning_included: Some(true),
        cache_read_observation: Some(CacheReadObservation::origin(CacheReadStatus::Reported)),
        completeness: UsageCompleteness::Final,
        basis: UsageBasis::ProviderReported,
        ..Default::default()
    }
}

fn discounted() -> AllowanceWeights {
    AllowanceWeights {
        cache_read: Weight::new(1_000).unwrap(),
        ..Default::default()
    }
}

#[test]
fn unit_weights_preserve_neutral_accounting_without_mutating_measurements() {
    for included in [true, false] {
        let mut u = usage();
        u.reasoning_included = Some(included);
        let before = u.clone();
        let quote = AllowanceWeights::default().quote(&u).unwrap();
        assert_eq!(quote.total_units, billable(&u) * UNITS_PER_TOKEN);
        assert_eq!(u, before);
    }
}

#[test]
fn cached_input_is_discounted_or_explicitly_free() {
    for (weight, expected) in [(1_000, 178 * UNITS_PER_TOKEN), (0, 170 * UNITS_PER_TOKEN)] {
        let policy = AllowanceWeights {
            cache_read: Weight::new(weight).unwrap(),
            ..Default::default()
        };
        let quote = policy.quote(&usage()).unwrap();
        assert_eq!(quote.total_units, expected);
        assert_eq!(quote.cache_read_units, 80 * u64::from(weight));
    }
}

#[test]
fn separate_reasoning_weight_never_counts_included_reasoning_twice() {
    let policy = AllowanceWeights {
        output: Weight::new(20_000).unwrap(),
        reasoning: Some(Weight::new(40_000).unwrap()),
        ..discounted()
    };
    let mut u = usage();
    for (included, output_units, total) in
        [(true, 400_000, 2_880_000), (false, 1_000_000, 3_480_000)]
    {
        u.reasoning_included = Some(included);
        let quote = policy.quote(&u).unwrap();
        assert_eq!(quote.output_units, output_units);
        assert_eq!(quote.reasoning_units, Some(1_200_000));
        assert_eq!(quote.total_units, total);
    }
}

#[test]
fn missing_reasoning_evidence_is_not_verified_zero() {
    let policy = AllowanceWeights {
        reasoning: Some(Weight::new(20_000).unwrap()),
        ..Default::default()
    };
    let mut u = usage();
    u.reasoning_tokens = None;
    assert_eq!(
        policy.quote(&u),
        Err(AllowanceError::ReasoningCountUnavailable)
    );
    u.reasoning_tokens = Some(0);
    assert!(policy.quote(&u).is_ok());
    u.reasoning_included = None;
    assert_eq!(
        policy.quote(&u),
        Err(AllowanceError::ReasoningConventionUnavailable)
    );
}

#[test]
fn unknown_positive_reasoning_convention_is_not_guessed_from_magnitude() {
    let mut u = usage();
    u.reasoning_included = None;
    for count in [1, 80] {
        u.reasoning_tokens = Some(count);
        assert_eq!(
            AllowanceWeights::default().quote(&u),
            Err(AllowanceError::ReasoningConventionUnavailable)
        );
    }
}

#[test]
fn included_reasoning_cannot_exceed_output() {
    let mut u = usage();
    u.reasoning_tokens = Some(51);
    assert_eq!(
        discounted().quote(&u),
        Err(AllowanceError::InconsistentReasoning)
    );
}

#[test]
fn cache_discount_requires_origin_reporting_or_explicit_unsupported_zero() {
    let mut u = usage();
    for observation in [
        None,
        Some(CacheReadObservation::origin(CacheReadStatus::Absent)),
        Some(CacheReadObservation::origin(CacheReadStatus::Malformed)),
    ] {
        u.cache_read_observation = observation;
        assert_eq!(
            discounted().quote(&u),
            Err(AllowanceError::CacheEvidenceUnavailable)
        );
    }
    u.cache_read_observation = Some(CacheReadObservation {
        status: CacheReadStatus::Reported,
        source: sandhi_core::CacheReadSource::CallerSupplied,
    });
    assert_eq!(
        discounted().quote(&u),
        Err(AllowanceError::CacheEvidenceUnavailable)
    );
    u.cache_read_observation = Some(CacheReadObservation {
        status: CacheReadStatus::Unsupported,
        source: sandhi_core::CacheReadSource::ExplicitCapability,
    });
    assert_eq!(
        discounted().quote(&u),
        Err(AllowanceError::CacheEvidenceUnavailable)
    );
    u.cache_read_tokens = 0;
    assert!(discounted().quote(&u).is_ok());
}

#[test]
fn final_measured_usage_is_required() {
    let mut u = usage();
    for completeness in [UsageCompleteness::Unavailable, UsageCompleteness::Partial] {
        u.completeness = completeness;
        assert_eq!(discounted().quote(&u), Err(AllowanceError::UsageNotFinal));
    }
    u.completeness = UsageCompleteness::Final;
    u.basis = UsageBasis::Estimated;
    assert_eq!(discounted().quote(&u), Err(AllowanceError::UsageEstimated));
}

#[test]
fn fractional_units_do_not_round_each_request_to_a_whole_token() {
    let u = UsageV2 {
        tokens_in: 0,
        tokens_out: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 1,
        reasoning_tokens: Some(0),
        ..usage()
    };
    assert_eq!(
        discounted().quote(&u).unwrap().total_units * 10,
        UNITS_PER_TOKEN
    );
}

#[test]
fn weights_reject_out_of_range_and_non_integer_config() {
    assert!(Weight::new(1_000_001).is_err());
    for raw in ["-1", "0.1", "1000001", "null", "\"1000\""] {
        assert!(serde_json::from_str::<Weight>(raw).is_err(), "{raw}");
    }
    assert_eq!(
        serde_json::from_str::<Weight>("1000").unwrap(),
        Weight::new(1000).unwrap()
    );
    assert_eq!(serde_json::to_string(&Weight::default()).unwrap(), "10000");
    assert!(serde_json::from_str::<AllowanceWeights>(r#"{"cache_reed":1000}"#).is_err());
}

#[test]
fn overflow_is_explicit_instead_of_wrapping_or_clamping() {
    let mut u = usage();
    u.tokens_in = u64::MAX;
    assert_eq!(discounted().quote(&u), Err(AllowanceError::Overflow));
    let policy = AllowanceWeights {
        fresh_input: Weight::new(0).unwrap(),
        ..discounted()
    };
    assert!(policy.quote(&u).is_ok());
}

#[test]
fn unreported_included_reasoning_can_inherit_output_without_claiming_zero() {
    let mut u = usage();
    u.reasoning_tokens = None;
    let quote = discounted().quote(&u).unwrap();
    assert_eq!(quote.reasoning_units, None);
    assert_eq!(quote.output_units, 50 * UNITS_PER_TOKEN);
    assert_eq!(quote.total_units, 178 * UNITS_PER_TOKEN);
    u.reasoning_included = None;
    assert_eq!(
        discounted().quote(&u),
        Err(AllowanceError::ReasoningConventionUnavailable)
    );
    u.reasoning_included = Some(false);
    assert_eq!(
        discounted().quote(&u),
        Err(AllowanceError::ReasoningCountUnavailable)
    );
}

#[test]
fn equivalent_disjoint_output_and_reasoning_have_equal_weighted_allowance() {
    let policy = AllowanceWeights {
        reasoning: Some(Weight::new(30_000).unwrap()),
        ..discounted()
    };
    let folded = usage();
    let mut separate = folded.clone();
    separate.reasoning_included = Some(false);
    separate.tokens_out -= separate.reasoning_tokens.unwrap();
    assert_eq!(policy.quote(&folded), policy.quote(&separate));
}

#[test]
fn sum_overflow_is_rejected_even_when_each_category_fits_sqlite() {
    let u = UsageV2 {
        tokens_in: (i64::MAX as u64) / UNITS_PER_TOKEN,
        ..usage()
    };
    assert_eq!(discounted().quote(&u), Err(AllowanceError::Overflow));
}

#[test]
fn omitted_policy_fields_are_unit_weight_and_reasoning_inherits_output() {
    let policy: AllowanceWeights = serde_json::from_str("{}").unwrap();
    assert_eq!(policy, AllowanceWeights::default());
    assert_eq!(
        policy.quote(&usage()).unwrap().total_units,
        billable(&usage()) * UNITS_PER_TOKEN
    );
    let serialized = serde_json::to_string(&discounted()).unwrap();
    assert_eq!(
        serde_json::from_str::<AllowanceWeights>(&serialized).unwrap(),
        discounted()
    );
}

use super::*;
use serde_json::json;

#[test]
fn plaintext_qualification_keeps_input_only_and_generation_evidence_distinct() {
    let embedding = json!({"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1]}],"usage":{"prompt_tokens":9,"total_tokens":9,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2}}});
    let usage = qualify_openai_plaintext_usage(&embedding, true).unwrap();
    assert_eq!(
        (
            usage.tokens_in,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            usage.tokens_out
        ),
        (4, 3, 2, 0)
    );
    assert_eq!(usage.reasoning_tokens, None);
    assert_eq!(usage.reasoning_included, None);
    assert_eq!(usage.completeness, crate::UsageCompleteness::Final);
    assert!(qualify_openai_plaintext_usage(&embedding, false).is_err());
    for counters in [
        json!({}),
        json!({"prompt_tokens":null}),
        json!({"prompt_tokens":9,"total_tokens":10}),
        json!({"prompt_tokens":9,"completion_tokens":1}),
        json!({"prompt_tokens":9,"completion_tokens_details":{"reasoning_tokens":0}}),
    ] {
        let mut invalid = embedding.clone();
        invalid["usage"] = counters;
        assert!(qualify_openai_plaintext_usage(&invalid, true).is_err());
    }
    let completion = json!({"object":"text_completion","choices":[{"index":0,"text":"ok","finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11}});
    assert_eq!(
        qualify_openai_plaintext_usage(&completion, false)
            .unwrap()
            .tokens_out,
        2
    );
    for counters in [
        json!({"prompt_tokens":9}),
        json!({"completion_tokens":2}),
        json!(null),
        json!({"prompt_tokens":9,"completion_tokens":0.5}),
    ] {
        let mut invalid = completion.clone();
        invalid["usage"] = counters;
        assert!(qualify_openai_plaintext_usage(&invalid, false).is_err());
    }
    let mut zero = embedding;
    zero["usage"] = json!({"prompt_tokens":0,"total_tokens":0});
    assert_eq!(
        qualify_openai_plaintext_usage(&zero, true)
            .unwrap()
            .tokens_in,
        0
    );
}

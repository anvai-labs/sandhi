//! Startup-compiled, identity-scoped pre-dispatch policy. No transport or storage.
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub issuer: Option<String>,
    pub subject: Option<String>,
    pub groups: Vec<String>,
    pub roles: Vec<String>,
    /// False for durable subject/legacy keys: absence is not proof of no membership.
    pub directory_known: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyDocumentV1 {
    pub schema_version: String,
    pub revision: u64,
    pub deadline_ms: u64,
    pub max_body_bytes: usize,
    pub rules: Vec<Rule>,
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub when: Selector,
    pub evaluator: EvaluatorSpec,
    pub effect: Effect,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    pub issuer: Option<String>,
    #[serde(default)]
    pub subjects: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub upstreams: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvaluatorSpec {
    /// Only a factory explicitly registered by trusted embedding code can satisfy this name.
    Registered {
        name: String,
        configuration: Value,
    },
    Regex {
        pattern: String,
    },
    Threshold {
        metric: Metric,
        above: u64,
    },
    /// Jaccard over lowercase alphanumeric words. Not an embedding model.
    LexicalSimilarity {
        reference: String,
        at_least: f64,
    },
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    BodyBytes,
    MaxOutputTokens,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Audit,
    Quarantine,
    Block,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Forward,
    Quarantine,
    Block,
    Unavailable,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Decision {
    pub revision: u64,
    pub disposition: Disposition,
    pub matched_rules: Vec<String>,
    pub reason: String,
    pub elapsed_us: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct EvaluationError;

/// Trusted compiled extensions return findings only. Implementations must bound CPU/memory
/// and cooperate with the deadline. Untrusted dynamic/native plugins are not supported.
pub trait Evaluator: Send + Sync {
    /// Trusted adapters declare remote inspection so local denials run before text egress.
    fn requires_text_egress(&self) -> bool {
        false
    }
    fn evaluate(&self, input: &Inspection, deadline: Instant) -> Result<bool, EvaluationError>;
}
/// Backend-neutral normalized score. Embedded models and supervised/remote adapters
/// share the same input and deadline; the policy engine alone assigns effects.
pub trait ScoreBackend: Send + Sync {
    fn requires_text_egress(&self) -> bool {
        false
    }
    fn score(&self, input: &Inspection, deadline: Instant) -> Result<f64, EvaluationError>;
}
pub struct ThresholdedScore {
    backend: std::sync::Arc<dyn ScoreBackend>,
    at_least: f64,
}
impl ThresholdedScore {
    pub fn new(
        backend: std::sync::Arc<dyn ScoreBackend>,
        at_least: f64,
    ) -> Result<Self, EvaluationError> {
        if !at_least.is_finite() || !(0.0..=1.0).contains(&at_least) {
            return Err(EvaluationError);
        }
        Ok(Self { backend, at_least })
    }
}
impl Evaluator for ThresholdedScore {
    fn requires_text_egress(&self) -> bool {
        self.backend.requires_text_egress()
    }
    fn evaluate(&self, input: &Inspection, deadline: Instant) -> Result<bool, EvaluationError> {
        check_time(deadline)?;
        let score = self.backend.score(input, deadline)?;
        check_time(deadline)?;
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err(EvaluationError);
        }
        Ok(score >= self.at_least)
    }
}
pub struct Inspection {
    pub text: String,
    pub joined: String,
    pub body_bytes: usize,
    pub max_output_tokens: Option<u64>,
}
struct RegexEvaluator(regex::Regex);
impl Evaluator for RegexEvaluator {
    fn evaluate(&self, input: &Inspection, deadline: Instant) -> Result<bool, EvaluationError> {
        check_time(deadline)?;
        let matched = self.0.is_match(&input.text) || self.0.is_match(&input.joined);
        check_time(deadline)?;
        Ok(matched)
    }
}
struct ThresholdEvaluator(Metric, u64);
impl Evaluator for ThresholdEvaluator {
    fn evaluate(&self, input: &Inspection, deadline: Instant) -> Result<bool, EvaluationError> {
        check_time(deadline)?;
        let value = match self.0 {
            Metric::BodyBytes => input.body_bytes as u64,
            Metric::MaxOutputTokens => input.max_output_tokens.ok_or(EvaluationError)?,
        };
        Ok(value > self.1)
    }
}
struct LexicalEvaluator(BTreeSet<String>, f64);
impl Evaluator for LexicalEvaluator {
    fn evaluate(&self, input: &Inspection, deadline: Instant) -> Result<bool, EvaluationError> {
        check_time(deadline)?;
        let words = words(&input.text)?;
        let union = self.0.union(&words).count();
        let score = self.0.intersection(&words).count() as f64 / union.max(1) as f64;
        check_time(deadline)?;
        Ok(score >= self.1)
    }
}
fn words(value: &str) -> Result<BTreeSet<String>, EvaluationError> {
    let mut out = BTreeSet::new();
    for (i, word) in value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        if i >= 8192 {
            return Err(EvaluationError);
        }
        out.insert(word.to_lowercase());
    }
    Ok(out)
}
fn check_time(deadline: Instant) -> Result<(), EvaluationError> {
    if Instant::now() >= deadline {
        Err(EvaluationError)
    } else {
        Ok(())
    }
}

pub type EvaluatorFactory =
    std::sync::Arc<dyn Fn(&Value) -> Result<Box<dyn Evaluator>, EvaluationError> + Send + Sync>;
#[derive(Default)]
pub struct Registry(BTreeMap<String, EvaluatorFactory>);
impl Registry {
    pub fn register<F>(&mut self, name: &str, factory: F) -> Result<(), EvaluationError>
    where
        F: Fn(&Value) -> Result<Box<dyn Evaluator>, EvaluationError> + Send + Sync + 'static,
    {
        if name.is_empty()
            || name.len() > 64
            || self.0.contains_key(name)
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(EvaluationError);
        }
        self.0.insert(name.into(), std::sync::Arc::new(factory));
        Ok(())
    }
}

pub struct Engine {
    document: PolicyDocumentV1,
    evaluators: Vec<Box<dyn Evaluator>>,
}
impl Engine {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, EvaluationError> {
        Self::from_slice_with_registry(bytes, &Registry::default())
    }
    pub fn from_slice_with_registry(
        bytes: &[u8],
        registry: &Registry,
    ) -> Result<Self, EvaluationError> {
        if bytes.len() > 131072 {
            return Err(EvaluationError);
        }
        let value = strict_json(bytes)?;
        let document: PolicyDocumentV1 =
            serde_json::from_value(value).map_err(|_| EvaluationError)?;
        if document.schema_version != "1"
            || document.revision == 0
            || !(1..=2000).contains(&document.deadline_ms)
            || !(1..=262144).contains(&document.max_body_bytes)
            || document.rules.is_empty()
            || document.rules.len() > 32
        {
            return Err(EvaluationError);
        }
        let mut ids = HashSet::new();
        let mut evaluators: Vec<Box<dyn Evaluator>> = Vec::new();
        for rule in &document.rules {
            if rule.id.is_empty()
                || rule.id.len() > 64
                || !rule
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                || !ids.insert(&rule.id)
            {
                return Err(EvaluationError);
            }
            for values in [
                &rule.when.subjects,
                &rule.when.groups,
                &rule.when.roles,
                &rule.when.upstreams,
                &rule.when.models,
            ] {
                if values.len() > 64 || values.iter().any(|s| s.is_empty() || s.len() > 256) {
                    return Err(EvaluationError);
                }
            }
            if rule
                .when
                .issuer
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 2048)
            {
                return Err(EvaluationError);
            }
            let evaluator: Box<dyn Evaluator> = match &rule.evaluator {
                EvaluatorSpec::Registered {
                    name,
                    configuration,
                } => registry.0.get(name).ok_or(EvaluationError)?(configuration)?,
                EvaluatorSpec::Regex { pattern } => {
                    if pattern.is_empty() || pattern.len() > 4096 {
                        return Err(EvaluationError);
                    }
                    Box::new(RegexEvaluator(
                        RegexBuilder::new(pattern)
                            .size_limit(262144)
                            .dfa_size_limit(262144)
                            .build()
                            .map_err(|_| EvaluationError)?,
                    ))
                }
                EvaluatorSpec::Threshold { metric, above } => {
                    Box::new(ThresholdEvaluator(*metric, *above))
                }
                EvaluatorSpec::LexicalSimilarity {
                    reference,
                    at_least,
                } => {
                    if reference.len() > 4096
                        || !at_least.is_finite()
                        || !(0.0..=1.0).contains(at_least)
                    {
                        return Err(EvaluationError);
                    }
                    let reference = words(reference)?;
                    if reference.is_empty() {
                        return Err(EvaluationError);
                    }
                    Box::new(LexicalEvaluator(reference, *at_least))
                }
            };
            evaluators.push(evaluator);
        }
        Ok(Self {
            document,
            evaluators,
        })
    }
    pub fn revision(&self) -> u64 {
        self.document.revision
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.document.deadline_ms)
    }
    pub fn evaluate(
        &self,
        body: &[u8],
        who: &Identity,
        upstream: &str,
        model: &str,
        deadline: Instant,
    ) -> Decision {
        let start = Instant::now();
        let mut decision = Decision {
            revision: self.document.revision,
            disposition: Disposition::Forward,
            matched_rules: Vec::new(),
            reason: "evaluated".into(),
            elapsed_us: 0,
        };
        let result = (|| -> Result<(), EvaluationError> {
            check_time(deadline)?;
            if body.len() > self.document.max_body_bytes {
                return Err(EvaluationError);
            }
            let input = inspect(body)?;
            // Local checks run first regardless of document ordering. A known denial
            // or incomplete local check must never disclose text to a remote evaluator.
            for remote_phase in [false, true] {
                if remote_phase && decision.disposition != Disposition::Forward {
                    break;
                }
                for (rule, evaluator) in self.document.rules.iter().zip(&self.evaluators) {
                    if evaluator.requires_text_egress() != remote_phase {
                        continue;
                    }
                    if remote_phase && decision.disposition != Disposition::Forward {
                        break;
                    }
                    check_time(deadline)?;
                    if !rule.when.matches(who, upstream, model)? {
                        continue;
                    }
                    if evaluator.evaluate(&input, deadline)? {
                        decision.matched_rules.push(rule.id.clone());
                        let disposition = match rule.effect {
                            Effect::Audit => Disposition::Forward,
                            Effect::Quarantine => Disposition::Quarantine,
                            Effect::Block => Disposition::Block,
                        };
                        decision.disposition = decision.disposition.max(disposition);
                    }
                }
            }
            check_time(deadline)
        })();
        if result.is_err() {
            // Preserve known denial, but an incomplete check never turns into admission.
            if decision.disposition == Disposition::Forward {
                decision.disposition = Disposition::Unavailable;
            }
            decision.reason = "incomplete_evaluation".into();
        }
        decision.matched_rules.sort();
        decision.elapsed_us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
        decision
    }
}
impl Selector {
    fn matches(
        &self,
        who: &Identity,
        upstream: &str,
        model: &str,
    ) -> Result<bool, EvaluationError> {
        let contains =
            |values: &[String], v: &str| values.is_empty() || values.iter().any(|x| x == v);
        if self
            .issuer
            .as_ref()
            .is_some_and(|s| Some(s) != who.issuer.as_ref())
            || !contains(&self.subjects, who.subject.as_deref().unwrap_or(""))
            || !contains(&self.upstreams, upstream)
            || !contains(&self.models, model)
        {
            return Ok(false);
        }
        if (!self.groups.is_empty() || !self.roles.is_empty()) && !who.directory_known {
            return Err(EvaluationError);
        }
        Ok(
            (self.groups.is_empty() || who.groups.iter().any(|g| self.groups.contains(g)))
                && (self.roles.is_empty() || who.roles.iter().any(|r| self.roles.contains(r))),
        )
    }
}

/// Strict duplicate-key rejection prevents inspecting one interpretation and forwarding another.
pub fn strict_json(bytes: &[u8]) -> Result<Value, EvaluationError> {
    struct Unique(Value);
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Unique;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("unambiguous JSON")
                }
                fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Unique, E> {
                    serde_json::Number::from_f64(v)
                        .map(|v| Unique(Value::Number(v)))
                        .ok_or_else(|| E::custom("finite number required"))
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Unique, E> {
                    Ok(Unique(v.into()))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                    Ok(Unique(Value::Null))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut a: A,
                ) -> Result<Unique, A::Error> {
                    let mut out = Vec::new();
                    while let Some(Unique(v)) = a.next_element()? {
                        out.push(v);
                    }
                    Ok(Unique(Value::Array(out)))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut a: A,
                ) -> Result<Unique, A::Error> {
                    let mut out = serde_json::Map::new();
                    while let Some((k, Unique(v))) = a.next_entry::<String, Unique>()? {
                        if out.insert(k, v).is_some() {
                            return Err(serde::de::Error::custom("duplicate key"));
                        }
                    }
                    Ok(Unique(Value::Object(out)))
                }
            }
            d.deserialize_any(Visitor)
        }
    }
    serde_json::from_slice::<Unique>(bytes)
        .map(|v| v.0)
        .map_err(|_| EvaluationError)
}

fn inspect(body: &[u8]) -> Result<Inspection, EvaluationError> {
    let value = strict_json(body)?;
    let object = value.as_object().ok_or(EvaluationError)?;
    const FIELDS: &[&str] = &[
        "model",
        "messages",
        "stream",
        "stream_options",
        "max_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "response_format",
        "stop",
        "seed",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "user",
        "n",
        "service_tier",
        "reasoning_effort",
    ];
    if object.keys().any(|k| !FIELDS.contains(&k.as_str())) {
        return Err(EvaluationError);
    }
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(EvaluationError)?;
    let mut parts = Vec::new();
    for message in messages {
        let m = message.as_object().ok_or(EvaluationError)?;
        if m.keys().any(|k| {
            ![
                "role",
                "content",
                "name",
                "tool_calls",
                "tool_call_id",
                "function_call",
                "refusal",
            ]
            .contains(&k.as_str())
        }) {
            return Err(EvaluationError);
        }
        match m.get("content") {
            Some(Value::String(s)) => parts.push(s.clone()),
            Some(Value::Null) | None => (),
            Some(Value::Array(items)) => {
                for item in items {
                    let item = item.as_object().ok_or(EvaluationError)?;
                    if item.len() != 2 || item.get("type").and_then(Value::as_str) != Some("text") {
                        return Err(EvaluationError);
                    }
                    parts.push(
                        item.get("text")
                            .and_then(Value::as_str)
                            .ok_or(EvaluationError)?
                            .into(),
                    );
                }
            }
            _ => return Err(EvaluationError),
        }
        if let Some(calls) = m.get("tool_calls") {
            for call in calls.as_array().ok_or(EvaluationError)? {
                if call.get("type").and_then(Value::as_str) != Some("function") {
                    return Err(EvaluationError);
                }
                inspect_arguments(call.get("function").ok_or(EvaluationError)?, &mut parts)?;
            }
        }
        if let Some(call) = m.get("function_call") {
            inspect_arguments(call, &mut parts)?;
        }
        for key in [
            "name",
            "tool_calls",
            "tool_call_id",
            "function_call",
            "refusal",
        ] {
            if let Some(v) = m.get(key) {
                collect(v, &mut parts, 0)?;
            }
        }
    }
    // Include string-bearing extras and tool schemas; no hidden uninspected text fields.
    for (key, value) in object {
        if key != "messages" && key != "model" {
            collect(value, &mut parts, 0)?;
        }
    }
    let max_output_tokens = match (
        object.get("max_tokens"),
        object.get("max_completion_tokens"),
    ) {
        (Some(_), Some(_)) => return Err(EvaluationError),
        (Some(v), None) | (None, Some(v)) => Some(v.as_u64().ok_or(EvaluationError)?),
        _ => None,
    };
    Ok(Inspection {
        text: parts.join("\n"),
        joined: parts.concat(),
        body_bytes: body.len(),
        max_output_tokens,
    })
}
fn inspect_arguments(value: &Value, parts: &mut Vec<String>) -> Result<(), EvaluationError> {
    let args = value
        .get("arguments")
        .and_then(Value::as_str)
        .ok_or(EvaluationError)?;
    let decoded = strict_json(args.as_bytes())?;
    collect(&decoded, parts, 0)
}

fn collect(value: &Value, out: &mut Vec<String>, depth: usize) -> Result<(), EvaluationError> {
    if depth > 32 || out.len() > 4096 {
        return Err(EvaluationError);
    }
    match value {
        Value::String(v) => out.push(v.clone()),
        Value::Array(values) => {
            for v in values {
                collect(v, out, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for (key, v) in values {
                out.push(key.clone());
                collect(v, out, depth + 1)?;
            }
        }
        _ => (),
    }
    Ok(())
}

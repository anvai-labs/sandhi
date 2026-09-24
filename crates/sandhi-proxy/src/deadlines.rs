//! Startup-only route deadlines. Lease renewal is deliberately unsupported.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

/// Reserved opportunity for settlement, not a guarantee against ledger contention.
pub const SETTLEMENT_HEADROOM_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "PolicyFile", into = "PolicyFile")]
pub struct BufferedDeadlines(PolicyFile);

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    ceiling_ms: u64,
    #[serde(default)]
    default_ms: Option<u64>,
    #[serde(default, deserialize_with = "unique_map")]
    endpoints: BTreeMap<String, Endpoint>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    #[serde(default)]
    default_ms: Option<u64>,
    #[serde(default, deserialize_with = "unique_map")]
    models: BTreeMap<String, u64>,
}

fn unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a map with unique keys")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut input: A,
        ) -> Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = input.next_entry::<String, T>()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate deadline override"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Visitor(std::marker::PhantomData))
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    BuiltIn,
    Global,
    Endpoint,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EffectiveDeadline {
    pub milliseconds: u64,
    pub source: Source,
}

fn default_ms() -> u64 {
    sandhi_providers::TimeoutConfig::default()
        .complete
        .as_millis() as u64
}

impl TryFrom<PolicyFile> for BufferedDeadlines {
    type Error = String;
    fn try_from(file: PolicyFile) -> Result<Self, Self::Error> {
        let maximum = (crate::ledger::RESERVATION_TTL_SECS as u64) * 1000 - SETTLEMENT_HEADROOM_MS;
        if file.ceiling_ms == 0 || file.ceiling_ms > maximum {
            return Err(format!(
                "buffered ceiling_ms must be between 1 and {maximum}"
            ));
        }
        let check = |value: &u64| {
            if *value == 0 || *value > file.ceiling_ms {
                Err("buffered deadline must be positive and no greater than ceiling_ms".to_string())
            } else {
                Ok(())
            }
        };
        check(&file.default_ms.unwrap_or_else(default_ms))?;
        validate_routes(
            file.endpoints.iter().map(|(reference, endpoint)| {
                (reference, endpoint.default_ms.as_ref(), &endpoint.models)
            }),
            check,
        )?;
        Ok(Self(file))
    }
}

impl From<BufferedDeadlines> for PolicyFile {
    fn from(policy: BufferedDeadlines) -> Self {
        policy.0
    }
}

impl BufferedDeadlines {
    pub fn validate_endpoints(&self, known: impl Fn(&str) -> bool) -> Result<(), String> {
        for reference in self.0.endpoints.keys() {
            if !known(reference) {
                return Err(format!(
                    "buffered deadline endpoint {reference:?} is not registered"
                ));
            }
        }
        Ok(())
    }

    pub fn resolve(&self, reference: &str, model: &str) -> EffectiveDeadline {
        let (milliseconds, source) = resolve_route(
            default_ms(),
            self.0.default_ms,
            self.0
                .endpoints
                .get(reference)
                .map(|e| (e.default_ms, &e.models)),
            model,
        );
        EffectiveDeadline {
            milliseconds,
            source,
        }
    }

    /// Operator-only snapshot includes each declared override and inherited endpoint default.
    pub fn report(&self) -> serde_json::Value {
        let endpoints: BTreeMap<_, _> = self
            .0
            .endpoints
            .iter()
            .map(|(reference, endpoint)| {
                let models: BTreeMap<_, _> = endpoint
                    .models
                    .keys()
                    .map(|model| (model, self.resolve(reference, model)))
                    .collect();
                (
                    reference,
                    serde_json::json!({"default": self.resolve(reference, ""), "models": models}),
                )
            })
            .collect();
        serde_json::json!({"ceiling_ms": self.0.ceiling_ms, "default": self.resolve("", ""), "endpoints": endpoints})
    }
}

impl EffectiveDeadline {
    pub fn duration(self) -> Duration {
        Duration::from_millis(self.milliseconds)
    }

    /// Refuse, never clamp, if the actual lease cannot cover dispatch plus headroom.
    /// SQLite persists expiry in seconds, so discard fractional precision here too.
    pub fn fits_lease(self, expires_at: time::OffsetDateTime, now: time::OffsetDateTime) -> bool {
        fits_lease(self.duration(), expires_at, now)
    }
}

/// Shared actual-lease check for buffered and streaming body policies.
pub(crate) fn fits_lease(
    duration: Duration,
    expires_at: time::OffsetDateTime,
    now: time::OffsetDateTime,
) -> bool {
    let expires_at = time::OffsetDateTime::from_unix_timestamp(expires_at.unix_timestamp())
        .expect("valid reservation timestamp");
    (expires_at - now).whole_nanoseconds()
        >= (duration + Duration::from_millis(SETTLEMENT_HEADROOM_MS)).as_nanos() as i128
}

/// Only project startup policy: unrelated desired-state sections retain their existing lifecycle.
pub fn from_json(text: &str) -> Result<Option<BufferedDeadlines>, serde_json::Error> {
    #[derive(Deserialize)]
    struct Projection {
        #[serde(default)]
        buffered_deadlines: Option<BufferedDeadlines>,
    }
    serde_json::from_str::<Projection>(text).map(|file| file.buffered_deadlines)
}

// One exact-route resolver and route-key validator for both policy surfaces.
fn resolve_route<T: Copy>(
    fallback: T,
    global: Option<T>,
    endpoint: Option<(Option<T>, &BTreeMap<String, T>)>,
    model: &str,
) -> (T, Source) {
    if let Some((default, models)) = endpoint {
        if let Some(value) = models.get(model) {
            return (*value, Source::Model);
        }
        if let Some(value) = default {
            return (value, Source::Endpoint);
        }
    }
    global.map_or((fallback, Source::BuiltIn), |value| (value, Source::Global))
}

fn validate_routes<'a, T: 'a>(
    routes: impl Iterator<Item = (&'a String, Option<&'a T>, &'a BTreeMap<String, T>)>,
    check: impl Fn(&T) -> Result<(), String>,
) -> Result<(), String> {
    for (reference, default, models) in routes {
        if !reference.split_once(':').is_some_and(|(provider, label)| {
            !provider.trim().is_empty() && !label.trim().is_empty()
        }) {
            return Err("deadline endpoint must be a credential reference (provider:label)".into());
        }
        if let Some(value) = default {
            check(value)?;
        }
        for (model, value) in models {
            if model.trim().is_empty() || model.contains('*') {
                return Err("deadline model overrides require nonempty exact model IDs".into());
            }
            check(value)?;
        }
    }
    Ok(())
}

/// Complete limits; partial inheritance cannot accidentally leave a phase unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "RawStreamLimits")]
pub struct StreamLimits {
    setup_ms: u64,
    idle_ms: u64,
    body_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStreamLimits {
    setup_ms: u64,
    idle_ms: u64,
    body_ms: u64,
}
impl TryFrom<RawStreamLimits> for StreamLimits {
    type Error = String;
    fn try_from(raw: RawStreamLimits) -> Result<Self, String> {
        let limits = Self {
            setup_ms: raw.setup_ms,
            idle_ms: raw.idle_ms,
            body_ms: raw.body_ms,
        };
        limits.validate(
            (crate::ledger::RESERVATION_TTL_SECS as u64) * 1000 - SETTLEMENT_HEADROOM_MS,
        )?;
        Ok(limits)
    }
}

impl StreamLimits {
    fn validate(self, ceiling_ms: u64) -> Result<(), String> {
        if self.setup_ms == 0
            || self.idle_ms == 0
            || self.body_ms == 0
            || self.idle_ms > ceiling_ms
            || !self
                .setup_ms
                .checked_add(self.body_ms)
                .is_some_and(|total| total <= ceiling_ms)
        {
            Err("streaming limits must be positive; idle and setup+body must fit ceiling_ms".into())
        } else {
            Ok(())
        }
    }
    pub fn dispatch_duration(self) -> Duration {
        Duration::from_millis(self.setup_ms + self.body_ms)
    }
    pub fn body_lifetime(self) -> crate::streaming::StreamBodyLifetime {
        crate::streaming::StreamBodyLifetime::new(Duration::from_millis(self.body_ms))
            .expect("validated streaming policy")
    }
    pub fn transport(self) -> sandhi_providers::StreamingDeadline {
        sandhi_providers::StreamingDeadline::new(
            Duration::from_millis(self.setup_ms),
            Duration::from_millis(self.idle_ms),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "StreamingFile", into = "StreamingFile")]
pub struct StreamingDeadlines(StreamingFile);
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StreamingFile {
    ceiling_ms: u64,
    default: StreamLimits,
    #[serde(default, deserialize_with = "unique_map")]
    endpoints: BTreeMap<String, StreamingEndpoint>,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StreamingEndpoint {
    #[serde(default)]
    default: Option<StreamLimits>,
    #[serde(default, deserialize_with = "unique_map")]
    models: BTreeMap<String, StreamLimits>,
}
impl TryFrom<StreamingFile> for StreamingDeadlines {
    type Error = String;
    fn try_from(file: StreamingFile) -> Result<Self, String> {
        let maximum = (crate::ledger::RESERVATION_TTL_SECS as u64) * 1000 - SETTLEMENT_HEADROOM_MS;
        if file.ceiling_ms == 0 || file.ceiling_ms > maximum {
            return Err(format!(
                "streaming ceiling_ms must be between 1 and {maximum}"
            ));
        }
        let check = |value: &StreamLimits| value.validate(file.ceiling_ms);
        check(&file.default)?;
        validate_routes(
            file.endpoints.iter().map(|(reference, endpoint)| {
                (reference, endpoint.default.as_ref(), &endpoint.models)
            }),
            check,
        )?;
        Ok(Self(file))
    }
}
impl From<StreamingDeadlines> for StreamingFile {
    fn from(policy: StreamingDeadlines) -> Self {
        policy.0
    }
}
impl StreamingDeadlines {
    pub fn validate_endpoints(&self, known: impl Fn(&str) -> bool) -> Result<(), String> {
        for reference in self.0.endpoints.keys() {
            if !known(reference) {
                return Err(format!(
                    "streaming deadline endpoint {reference:?} is not registered"
                ));
            }
        }
        Ok(())
    }
    pub fn resolve(&self, reference: &str, model: &str) -> (StreamLimits, Source) {
        resolve_route(
            self.0.default,
            Some(self.0.default),
            self.0
                .endpoints
                .get(reference)
                .map(|e| (e.default, &e.models)),
            model,
        )
    }
    pub fn report(&self) -> serde_json::Value {
        let resolved = |reference: &str, model: &str| {
            let (limits, source) = self.resolve(reference, model);
            serde_json::json!({"limits":limits,"source":source})
        };
        let endpoints: BTreeMap<_, _> = self
            .0
            .endpoints
            .iter()
            .map(|(reference, endpoint)| {
                let models: BTreeMap<_, _> = endpoint
                    .models
                    .keys()
                    .map(|model| (model, resolved(reference, model)))
                    .collect();
                (
                    reference,
                    serde_json::json!({"default":resolved(reference,""),"models":models}),
                )
            })
            .collect();
        serde_json::json!({"ceiling_ms":self.0.ceiling_ms,"default":resolved("",""),"endpoints":endpoints})
    }
}

pub fn streaming_from_json(text: &str) -> Result<Option<StreamingDeadlines>, serde_json::Error> {
    #[derive(Deserialize)]
    struct Projection {
        #[serde(default)]
        streaming_deadlines: Option<StreamingDeadlines>,
    }
    serde_json::from_str::<Projection>(text).map(|file| file.streaming_deadlines)
}

//! Startup-only buffered policy. Streaming lifetime/lease renewal is a separate contract.
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
                    return Err(serde::de::Error::custom(
                        "duplicate buffered deadline override",
                    ));
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
        let check = |value| {
            if value == 0 || value > file.ceiling_ms {
                Err("buffered deadline must be positive and no greater than ceiling_ms".to_string())
            } else {
                Ok(())
            }
        };
        check(file.default_ms.unwrap_or_else(default_ms))?;
        for (reference, endpoint) in &file.endpoints {
            if !reference.split_once(':').is_some_and(|(provider, label)| {
                !provider.trim().is_empty() && !label.trim().is_empty()
            }) {
                return Err(
                    "buffered endpoint must be a credential reference (provider:label)".into(),
                );
            }
            if let Some(value) = endpoint.default_ms {
                check(value)?;
            }
            for (model, value) in &endpoint.models {
                if model.trim().is_empty() || model.contains('*') {
                    return Err("buffered model overrides require nonempty exact model IDs".into());
                }
                check(*value)?;
            }
        }
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
        if let Some(endpoint) = self.0.endpoints.get(reference) {
            if let Some(value) = endpoint.models.get(model) {
                return EffectiveDeadline {
                    milliseconds: *value,
                    source: Source::Model,
                };
            }
            if let Some(value) = endpoint.default_ms {
                return EffectiveDeadline {
                    milliseconds: value,
                    source: Source::Endpoint,
                };
            }
        }
        EffectiveDeadline {
            milliseconds: self.0.default_ms.unwrap_or_else(default_ms),
            source: if self.0.default_ms.is_some() {
                Source::Global
            } else {
                Source::BuiltIn
            },
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
        let expires_at = time::OffsetDateTime::from_unix_timestamp(expires_at.unix_timestamp())
            .expect("valid reservation timestamp");
        (expires_at - now).whole_milliseconds()
            >= i128::from(self.milliseconds + SETTLEMENT_HEADROOM_MS)
    }
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

//! Bounded trusted built-in policy evaluation + mandatory durable admission receipt.
use sandhi_core::policy::{Decision, Disposition, Engine, Identity, InputFormat};
use sandhi_store::policy::PolicyAuditStore;
use std::{sync::Arc, time::Instant};
use tokio::sync::Semaphore;

pub struct PolicyGate {
    engine: Arc<Engine>,
    audit: Arc<PolicyAuditStore>,
    slots: Arc<Semaphore>,
}
pub struct Admission {
    pub decision: Decision,
    pub receipt: String,
}
#[derive(Debug)]
pub struct Unavailable;
impl PolicyGate {
    pub fn new(engine: Engine, audit: Arc<PolicyAuditStore>) -> Self {
        Self {
            engine: Arc::new(engine),
            audit,
            slots: Arc::new(Semaphore::new(4)),
        }
    }
    pub fn status(&self) -> Result<serde_json::Value, Unavailable> {
        use sandhi_core::policy::EvaluatorSpec;
        let document = self.engine.configuration();
        let rules: Vec<_> = document
            .rules
            .iter()
            .map(|rule| {
                let evaluator = match &rule.evaluator {
                    EvaluatorSpec::Registered { name, .. } => name.as_str(),
                    EvaluatorSpec::Regex { .. } => "regex",
                    EvaluatorSpec::Threshold { .. } => "threshold",
                    EvaluatorSpec::LexicalSimilarity { .. } => "lexical_similarity",
                };
                serde_json::json!({"id": rule.id, "effect": rule.effect, "evaluator": evaluator})
            })
            .collect();
        Ok(
            serde_json::json!({"enabled":true, "revision":document.revision,
            "deadline_ms":document.deadline_ms, "max_body_bytes":document.max_body_bytes,
            "rules":rules, "receipts":self.audit.summary().map_err(|_| Unavailable)?,
            "supported_input":"OpenAI chat, completions and embeddings text", "on_unavailable":"deny"}),
        )
    }
    pub async fn check(
        &self,
        body: bytes::Bytes,
        identity: Identity,
        key: String,
        upstream: String,
        model: String,
        supported_dialect: bool,
    ) -> Result<Admission, Unavailable> {
        self.check_input(
            body,
            identity,
            key,
            upstream,
            model,
            supported_dialect.then_some(InputFormat::Chat),
        )
        .await
    }
    pub async fn check_input(
        &self,
        body: bytes::Bytes,
        identity: Identity,
        key: String,
        upstream: String,
        model: String,
        format: Option<InputFormat>,
    ) -> Result<Admission, Unavailable> {
        let start = Instant::now();
        let deadline = start + self.engine.timeout();
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Unavailable)?;
        let engine = self.engine.clone();
        let audit = self.audit.clone();
        // Only bounded built-ins run here. A timeout denies dispatch, but cannot kill native
        // work; the slot stays owned by the worker until cleanup, even after cancellation.
        let work = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let decision = if let Some(format) = format {
                engine.evaluate_with_format(&body, &identity, &upstream, &model, format, deadline)
            } else {
                Decision {
                    revision: engine.revision(),
                    disposition: Disposition::Unavailable,
                    matched_rules: vec![],
                    reason: "unsupported_dialect".into(),
                    elapsed_us: 0,
                }
            };
            if Instant::now() >= deadline {
                return Err(Unavailable);
            }
            let receipt = audit
                .record(&identity, &key, &decision)
                .map_err(|_| Unavailable)?;
            if Instant::now() >= deadline {
                return Err(Unavailable);
            }
            Ok(Admission { decision, receipt })
        });
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), work)
            .await
            .map_err(|_| Unavailable)?
            .map_err(|_| Unavailable)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::policy::{EvaluationError, Evaluator, Inspection, Registry};
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn saturated_gate_and_late_extension_cannot_admit() {
        struct Late;
        impl Evaluator for Late {
            fn evaluate(&self, _: &Inspection, _: Instant) -> Result<bool, EvaluationError> {
                std::thread::sleep(Duration::from_millis(30));
                Ok(false)
            }
        }
        let mut registry = Registry::default();
        registry
            .register("test.late.v1", |_| Ok(Box::new(Late)))
            .unwrap();
        let bytes = serde_json::to_vec(&json!({"schema_version":"1","revision":1,"deadline_ms":1,
            "max_body_bytes":65536,"rules":[{"id":"slow","effect":"block",
                "evaluator":{"kind":"registered","name":"test.late.v1","configuration":{}}}]}))
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let gate = PolicyGate::new(
            Engine::from_slice_with_registry(&bytes, &registry).unwrap(),
            Arc::new(PolicyAuditStore::open(&temp.path().join("audit.db"), 10).unwrap()),
        );
        let body = bytes::Bytes::from_static(br#"{"model":"test","messages":[]}"#);
        let all_slots = gate.slots.clone().try_acquire_many_owned(4).unwrap();
        assert!(gate
            .check(
                body.clone(),
                Identity::default(),
                "key".into(),
                "openai".into(),
                "test".into(),
                true
            )
            .await
            .is_err());
        drop(all_slots);
        assert!(gate
            .check(
                body,
                Identity::default(),
                "key".into(),
                "openai".into(),
                "test".into(),
                true
            )
            .await
            .is_err());
        // Timeout may precede worker start on a busy machine. Cleanup remains bounded
        // by this trusted fixture; an eventual result cannot reopen admission.
        for _ in 0..100 {
            if gate.slots.available_permits() == 4 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed-out worker did not release its slot");
    }
}

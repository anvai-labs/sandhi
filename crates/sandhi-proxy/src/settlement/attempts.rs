//! Bounded transport observer owned by the request until immutable settlement capture.
use sandhi_providers::{AttemptContext, AttemptOutcome, AttemptPhase, AttemptReceiver};
use sandhi_store::ledger::evidence::{AttemptEvidence, TransportOutcome, TransportTerminal};

pub(crate) struct Observer {
    pub context: AttemptContext,
    receiver: AttemptReceiver,
    execution_id: String,
    attempt_id: String,
}

impl Observer {
    pub fn new(execution: &super::AuthorizedExecution) -> Result<Self, &'static str> {
        let attempt_id = execution
            .attempt_id()
            .ok_or("missing persisted attempt identity")?
            .to_owned();
        let execution_id = execution.intent().execution_id.clone();
        let (context, receiver) = AttemptContext::single_attempt(&execution_id, &attempt_id)
            .map_err(|_| "attempt observer unavailable")?;
        Ok(Self {
            context,
            receiver,
            execution_id,
            attempt_id,
        })
    }

    /// Consume once, after dropping the transport source. Missing/dropped/mismatched
    /// events stay unresolved; diagnostic labels and parsed usage are not authority.
    pub fn finish(self) -> AttemptEvidence {
        let mut evidence = AttemptEvidence {
            version: 1,
            attempt_id: self.attempt_id,
            attempt_ordinal: 1,
            dispatch_observed_at_ms: None,
            terminal: None,
            dropped_observations: self.context.dropped_observations().min(2),
            invalid: false,
        };
        for event in self.receiver.drain() {
            if event.version != 1
                || event.execution_id != self.execution_id
                || event.attempt_id != evidence.attempt_id
                || event.attempt_ordinal != 1
            {
                evidence.invalid = true;
                continue;
            }
            match event.phase {
                AttemptPhase::Dispatch => {
                    if evidence.dispatch_observed_at_ms.is_some() || evidence.terminal.is_some() {
                        evidence.invalid = true;
                    } else {
                        evidence.dispatch_observed_at_ms = Some(event.observed_at_unix_ms);
                    }
                }
                AttemptPhase::Terminal {
                    outcome,
                    provider_status,
                    provider_request_id,
                    ..
                } => {
                    if evidence.terminal.is_some() || evidence.dispatch_observed_at_ms.is_none() {
                        evidence.invalid = true;
                    } else {
                        evidence.terminal = Some(TransportTerminal {
                            observed_at_ms: event.observed_at_unix_ms,
                            outcome: match outcome {
                                AttemptOutcome::Success => TransportOutcome::Success,
                                AttemptOutcome::ProviderRejected => {
                                    TransportOutcome::ProviderRejected
                                }
                                AttemptOutcome::TransportError => TransportOutcome::TransportError,
                                AttemptOutcome::Timeout => TransportOutcome::Timeout,
                                AttemptOutcome::Cancelled => TransportOutcome::Cancelled,
                                AttemptOutcome::IncompleteStream => {
                                    TransportOutcome::IncompleteStream
                                }
                            },
                            provider_status,
                            provider_request_id,
                        });
                    }
                }
            }
        }
        evidence
    }
}

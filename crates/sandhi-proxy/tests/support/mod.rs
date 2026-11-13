//! Real canonical fixture storage. The file exists for the entire state/sink
//! lifetime; no accounting-off path, memory-to-durable data copy or leaked dir.
#![allow(dead_code)] // Separate integration crates use different fixture subsets.
use sandhi_core::{KeyStore, Sink, UsageEvent};
use sandhi_providers::ProviderHandle;
use sandhi_proxy::{ProxyLedger, ProxyState};
use sandhi_store::SqliteStore;
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
};
pub struct Ledger {
    inner: ProxyLedger,
    directory: Option<tempfile::TempDir>,
}
impl Ledger {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let inner = ProxyLedger::durable(directory.path().join("original.db").to_str().unwrap(), 1)
            .unwrap();
        Self {
            inner,
            directory: Some(directory),
        }
    }
}
impl From<ProxyLedger> for Ledger {
    fn from(inner: ProxyLedger) -> Self {
        Self {
            inner,
            directory: None,
        }
    }
}
impl Deref for Ledger {
    type Target = ProxyLedger;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl DerefMut for Ledger {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
struct FixtureSink {
    inner: Arc<dyn Sink>,
    _directory: tempfile::TempDir,
}
impl Sink for FixtureSink {
    fn emit(&self, event: &UsageEvent) {
        self.inner.emit(event)
    }
}
pub fn state(
    keys: KeyStore,
    ledger: impl Into<Ledger>,
    sink: Arc<dyn Sink>,
    providers: HashMap<String, ProviderHandle>,
    store: Option<Arc<SqliteStore>>,
) -> ProxyState {
    let Ledger { inner, directory } = ledger.into();
    let sink: Arc<dyn Sink> = match directory {
        Some(directory) => Arc::new(FixtureSink {
            inner: sink,
            _directory: directory,
        }),
        None => sink,
    };
    ProxyState::new(keys, inner, sink, providers, store)
}

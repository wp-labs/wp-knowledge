use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use crate::loader::ProviderKind;
use crate::runtime::QueryModeTag;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheLayer {
    Local,
    Result,
    Metadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    Hit,
    Miss,
}

#[derive(Debug, Clone)]
pub struct CacheTelemetryEvent {
    pub layer: CacheLayer,
    pub outcome: CacheOutcome,
    pub provider_kind: Option<ProviderKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOutcome {
    Success,
    Failure,
}

#[derive(Debug, Clone)]
pub struct ReloadTelemetryEvent {
    pub outcome: ReloadOutcome,
    pub provider_kind: ProviderKind,
}

#[derive(Debug, Clone)]
pub struct QueryTelemetryEvent {
    pub provider_kind: ProviderKind,
    pub mode: QueryModeTag,
    pub success: bool,
    pub elapsed: Duration,
}

pub trait KnowledgeTelemetry: Send + Sync {
    fn on_cache(&self, _event: &CacheTelemetryEvent) {}
    fn on_reload(&self, _event: &ReloadTelemetryEvent) {}
    fn on_query(&self, _event: &QueryTelemetryEvent) {}
}

#[derive(Debug, Default)]
pub struct NoopTelemetry;

impl KnowledgeTelemetry for NoopTelemetry {}

fn telemetry_slot() -> &'static RwLock<Arc<dyn KnowledgeTelemetry>> {
    static SLOT: OnceLock<RwLock<Arc<dyn KnowledgeTelemetry>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(Arc::new(NoopTelemetry)))
}

pub fn telemetry() -> Arc<dyn KnowledgeTelemetry> {
    telemetry_slot()
        .read()
        .expect("knowledge telemetry lock poisoned")
        .clone()
}

pub fn install_telemetry(telemetry: Arc<dyn KnowledgeTelemetry>) -> Arc<dyn KnowledgeTelemetry> {
    let mut guard = telemetry_slot()
        .write()
        .expect("knowledge telemetry lock poisoned");
    std::mem::replace(&mut *guard, telemetry)
}

pub fn reset_telemetry() -> Arc<dyn KnowledgeTelemetry> {
    install_telemetry(Arc::new(NoopTelemetry))
}

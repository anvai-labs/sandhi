//! Sandhi Python binding (PyO3) — published to PyPI as `sandhi-gateway`, imported as
//! `sandhi_gateway`. Skeleton: exposes the wire-contract version so consumers can pin against
//! it. The in-process metering middleware + virtual-key API are the first binding milestones.

use pyo3::prelude::*;

/// The usage-event wire-contract major version this build targets.
#[pyfunction]
fn wire_contract_version() -> &'static str {
    sandhi_core::UsageEvent::SCHEMA_VERSION
}

#[pymodule]
fn sandhi_gateway(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__doc__", "Sandhi — the metering layer for AI agents (Python binding).")?;
    m.add_function(wrap_pyfunction!(wire_contract_version, m)?)?;
    Ok(())
}

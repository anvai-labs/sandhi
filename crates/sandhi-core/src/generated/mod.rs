//! Generated typify narrow models (ADR-0003 §2/§4 — TD-0001 W3 pilot).
//!
//! The submodule files here are **generated** from the byte-pinned schemas in
//! `crates/sandhi-core/schemas/` by `scripts/gen-provider-models.sh` (which runs `cargo typify`).
//! **Never hand-edit them** — edit the schema and regenerate; CI drift-checks that the committed
//! output matches (`.github/workflows/ci.yml` → `codegen-drift`). This `mod.rs` is the
//! hand-written overlay index (the adjacent layer, per ADR-0003 §4) — safe to edit.
//!
//! typify emits a full builder/error surface per schema; the parser overlay
//! (`crate::usage::parse_anthropic_usage`) uses only the plain `Deserialize` struct, so the rest
//! is `#[allow]`-ed dead code by design.

#[allow(dead_code, unused, clippy::all, clippy::pedantic)]
pub mod anthropic_usage;

//! `sandhi-proxy` — the in-path (inline) reverse-proxy egress gate.
//!
//! The same core (`sandhi-core` + `sandhi-providers`) fronted by an HTTP/streaming listener.
//! It terminates the client connection, resolves a **virtual key** → subject/group + the real
//! upstream key (held server-side, never exposed), forwards prefix-exact, streams the response
//! back O(1)-memory, and emits one neutral [`sandhi_core::UsageEvent`] after the stream closes.
//! In-path, never a redirect (ADR-0047 D8); session/cache affinity preserved (D9).
//!
//! Skeleton only — the HTTP/streaming server (hyper/axum + tokio) is the first proxy milestone.

fn main() {
    eprintln!(
        "sandhi-proxy {} — not yet implemented (skeleton). \
         See docs/adr/0001-sandhi-architecture-and-wire-contract.md.",
        env!("CARGO_PKG_VERSION")
    );
    // Prove the core links: emit the wire-contract version this build targets.
    eprintln!("usage-event wire contract: v{}", sandhi_core::UsageEvent::SCHEMA_VERSION);
    std::process::exit(1);
}

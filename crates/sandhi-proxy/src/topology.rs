//! Optional deployment-topology file (`SANDHI_TOPOLOGY_FILE`).
//!
//! A small, secret-free projection of the operator's infrastructure inventory
//! (hosts, endpoints, ports) served read-only via `GET /admin/topology` and
//! the dashboard, so operators and the control plane can see *where* this
//! gateway sits without granting anything beyond `Permission::Read`.
//!
//! Contract notes:
//! - The file is **generated** by the operator's topology tooling (e.g.
//!   anvaiops `anvai_topology.py show --sandhi-topology`) and is never
//!   hand-edited; this module only reads and projects it.
//! - Masking is by construction: no struct field may ever carry a credential,
//!   token, or secret — keep it that way (the check lives in the tests).
//! - Absent env var = feature off (404 from the endpoints); malformed file =
//!   fail-closed at startup, exactly like the policy loader.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

/// Maximum accepted file size, mirroring the policy loader's bound.
const MAX_TOPOLOGY_BYTES: u64 = 128 * 1024;

/// Top-level file schema. Every field is `#[serde(default)]`: an empty file
/// parses, and forward-compatible additions by the generator never break the
/// gateway at startup.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TopologyFile {
    #[serde(default)]
    pub deployment: DeploymentTopology,
    #[serde(default)]
    pub peers: Vec<PeerTopology>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeploymentTopology {
    /// Host name the gateway runs on (e.g. "ds3").
    #[serde(default)]
    pub gateway_host: String,
    /// Public DNS name clients use (e.g. "dataserver3.singh.local").
    #[serde(default)]
    pub public_name: Option<String>,
    #[serde(default)]
    pub tls_port: Option<u16>,
    #[serde(default)]
    pub http_port: Option<u16>,
    #[serde(default)]
    pub lan_ip: Option<String>,
    /// Free-form deployment label (e.g. "home-primary").
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerTopology {
    /// Service name (e.g. "inferflux", "anvaiops-api").
    #[serde(default)]
    pub name: String,
    /// How this gateway reaches the peer: "tunnel" | "lan" | "cloud" | …
    #[serde(default)]
    pub kind: Option<String>,
    /// Peer base endpoint (scheme://host:port). Never carries credentials.
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// Read + parse the topology file. Any failure is a startup-fatal error string
/// (the caller exits before bind); absence of the env var keeps the feature
/// off entirely.
pub fn load_topology_file(path: &Path) -> Result<TopologyFile, String> {
    let meta = std::fs::metadata(path)
        .map_err(|error| format!("reading topology file {}: {error}", path.display()))?;
    if meta.len() > MAX_TOPOLOGY_BYTES {
        return Err(format!(
            "topology file {} exceeds the {}-byte cap",
            path.display(),
            MAX_TOPOLOGY_BYTES
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("reading topology file {}: {error}", path.display()))?;
    serde_json::from_slice::<TopologyFile>(&bytes)
        .map_err(|error| format!("parsing topology file {}: {error}", path.display()))
}

/// Project the file into the response body. Deliberately a subset copy, not a
/// serde pass-through of internal types, so future internal fields can never
/// leak by accident.
pub(crate) fn topology_response(file: &TopologyFile) -> Value {
    serde_json::json!({
        "deployment": {
            "gateway_host": file.deployment.gateway_host,
            "public_name": file.deployment.public_name,
            "tls_port": file.deployment.tls_port,
            "http_port": file.deployment.http_port,
            "lan_ip": file.deployment.lan_ip,
            "label": file.deployment.label,
        },
        "peers": file.peers.iter()
            .map(|peer| serde_json::json!({
                "name": peer.name,
                "kind": peer.kind,
                "endpoint": peer.endpoint,
            }))
            .collect::<Vec<Value>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "deployment": {
            "gateway_host": "ds3",
            "public_name": "dataserver3.singh.local",
            "tls_port": 18788,
            "http_port": 18789,
            "lan_ip": "192.168.1.89",
            "label": "home-primary"
        },
        "peers": [
            {"name": "inferflux", "kind": "tunnel", "endpoint": "http://127.0.0.1:8190"},
            {"name": "anvaiops-api", "kind": "lan", "endpoint": "http://192.168.1.89:8000"}
        ]
    }"#;

    #[test]
    fn parses_the_generated_shape() {
        let file: TopologyFile = serde_json::from_str(SAMPLE).expect("parse sample");
        assert_eq!(file.deployment.gateway_host, "ds3");
        assert_eq!(file.deployment.tls_port, Some(18788));
        assert_eq!(file.peers.len(), 2);
        assert_eq!(file.peers[0].name, "inferflux");
        assert_eq!(
            file.peers[0].endpoint.as_deref(),
            Some("http://127.0.0.1:8190")
        );
    }

    #[test]
    fn empty_file_parses_via_defaults() {
        let file: TopologyFile = serde_json::from_str("{}").expect("empty parses");
        assert_eq!(file, TopologyFile::default());
        assert!(file.peers.is_empty());
    }

    #[test]
    fn malformed_file_is_an_error() {
        assert!(serde_json::from_str::<TopologyFile>("{ not json").is_err());
        assert!(serde_json::from_str::<TopologyFile>(r#"{"peers": "not-a-list"}"#).is_err());
    }

    #[test]
    fn loader_rejects_oversized_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path: PathBuf = tmp.path().join("big.json");
        std::fs::write(&path, vec![b' '; (MAX_TOPOLOGY_BYTES + 1) as usize]).expect("write");
        let error = load_topology_file(&path).expect_err("oversized must fail");
        assert!(error.contains("exceeds"), "unexpected: {error}");
    }

    #[test]
    fn loader_reports_missing_files_as_errors() {
        let error =
            load_topology_file(Path::new("/nonexistent/topology.json")).expect_err("missing");
        assert!(
            error.contains("reading topology file"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn projection_carries_no_secret_shaped_keys() {
        let file: TopologyFile = serde_json::from_str(SAMPLE).expect("parse sample");
        let body = topology_response(&file).to_string().to_lowercase();
        for forbidden in ["token", "secret", "api_key", "password", "credential"] {
            assert!(!body.contains(forbidden), "projection leaked {forbidden}");
        }
    }

    #[test]
    fn projection_preserves_peer_fields() {
        let file: TopologyFile = serde_json::from_str(SAMPLE).expect("parse sample");
        let body = topology_response(&file);
        let peers = body["peers"].as_array().expect("peers array");
        assert_eq!(peers[1]["name"], "anvaiops-api");
        assert_eq!(peers[1]["endpoint"], "http://192.168.1.89:8000");
        assert_eq!(body["deployment"]["public_name"], "dataserver3.singh.local");
    }
}

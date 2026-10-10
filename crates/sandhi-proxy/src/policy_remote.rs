//! Operator-owned HTTP evaluator routing. No end-user credential or destination crosses
//! this boundary. Each replica has one bounded transport slot; failed text is never replayed.
use reqwest::{
    blocking::Client,
    header::{HeaderValue, AUTHORIZATION},
    Url,
};
use sandhi_core::policy::{
    strict_json, EvaluationError, Inspection, Registry, ScoreBackend, ThresholdedScore,
};
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    evaluators: Vec<Deployment>,
}
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Deployment {
    name: String,
    artifact_sha256: String,
    code_sha256: String,
    timeout_ms: u64,
    replicas: Vec<Endpoint>,
}
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    url: String,
    auth_file: PathBuf,
    #[serde(default)]
    ca_file: Option<PathBuf>,
    #[serde(default)]
    identity_file: Option<PathBuf>,
}

pub fn load_remotes(registry: &mut Registry, path: Option<&Path>) -> Result<(), EvaluationError> {
    if let Some(path) = path {
        register_remotes(registry, &crate::policy_workers::read_bounded(path, 65536)?)?;
    }
    Ok(())
}
pub fn register_remotes(registry: &mut Registry, bytes: &[u8]) -> Result<(), EvaluationError> {
    if bytes.len() > 65536 {
        return Err(EvaluationError);
    }
    let m: Manifest = serde_json::from_value(strict_json(bytes)?).map_err(|_| EvaluationError)?;
    if m.version != 1
        || m.evaluators.is_empty()
        || m.evaluators.len() > 4
        || m.evaluators.iter().map(|d| d.replicas.len()).sum::<usize>() > 4
    {
        return Err(EvaluationError);
    }
    for d in m.evaluators {
        let name = d.name.clone();
        let backend = Arc::new(Remote::start(d)?);
        registry.register(&name, move |value| {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Config {
                at_least: f64,
            }
            let c: Config = serde_json::from_value(value.clone()).map_err(|_| EvaluationError)?;
            Ok(Box::new(ThresholdedScore::new(
                backend.clone(),
                c.at_least,
            )?))
        })?;
    }
    Ok(())
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn private_file(path: &Path, limit: usize) -> Result<Vec<u8>, EvaluationError> {
    if !path.is_absolute() {
        return Err(EvaluationError);
    }
    let before = std::fs::symlink_metadata(path).map_err(|_| EvaluationError)?;
    if !before.is_file() {
        return Err(EvaluationError);
    }
    let mut f = std::fs::File::open(path).map_err(|_| EvaluationError)?;
    let after = f.metadata().map_err(|_| EvaluationError)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if after.mode() & 0o077 != 0 || before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(EvaluationError);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = after;
        return Err(EvaluationError);
    }
    let mut bytes = Vec::new();
    f.by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| EvaluationError)?;
    if bytes.len() > limit {
        return Err(EvaluationError);
    }
    Ok(bytes)
}
struct Prepared {
    url: Url,
    auth: HeaderValue,
    ca: Option<reqwest::Certificate>,
    identity: Option<reqwest::Identity>,
}
impl Prepared {
    fn new(e: &Endpoint, name: &str) -> Result<Self, EvaluationError> {
        let mut url = Url::parse(&e.url).map_err(|_| EvaluationError)?;
        let loopback = url
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || url.host().is_none()
        {
            return Err(EvaluationError);
        }
        if url.scheme() != "https" && (e.ca_file.is_some() || e.identity_file.is_some()) {
            return Err(EvaluationError);
        }
        url.set_path(&format!("/v1/evaluators/{name}/evaluate"));
        let bytes = private_file(&e.auth_file, 256)?;
        let token = std::str::from_utf8(&bytes)
            .map_err(|_| EvaluationError)?
            .trim();
        if !(32..=128).contains(&token.len())
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(EvaluationError);
        }
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| EvaluationError)?;
        auth.set_sensitive(true);
        let ca = e
            .ca_file
            .as_ref()
            .map(|p| {
                if !p.is_absolute() {
                    return Err(EvaluationError);
                }
                let mut certificates = reqwest::Certificate::from_pem_bundle(
                    &crate::policy_workers::read_bounded(p, 65536)?,
                )
                .map_err(|_| EvaluationError)?;
                if certificates.len() != 1 {
                    return Err(EvaluationError);
                }
                certificates.pop().ok_or(EvaluationError)
            })
            .transpose()?;
        let identity = e
            .identity_file
            .as_ref()
            .map(|p| {
                reqwest::Identity::from_pem(&private_file(p, 65536)?).map_err(|_| EvaluationError)
            })
            .transpose()?;
        Ok(Self {
            url,
            auth,
            ca,
            identity,
        })
    }
    fn client(&self) -> Result<Client, EvaluationError> {
        let mut b = Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(2))
            .pool_max_idle_per_host(1)
            .pool_idle_timeout(Duration::from_secs(30));
        if let Some(ca) = &self.ca {
            b = b.add_root_certificate(ca.clone());
        }
        if let Some(identity) = &self.identity {
            b = b.identity(identity.clone());
        }
        b.build().map_err(|_| EvaluationError)
    }
}
struct Job {
    id: String,
    text: String,
    joined: String,
    deadline: Instant,
    reply: SyncSender<Result<f64, EvaluationError>>,
}
struct Slot {
    sender: SyncSender<Job>,
    busy: Arc<AtomicBool>,
    cooldown: Arc<Mutex<Option<Instant>>>,
}
struct Remote {
    slots: Vec<Slot>,
    threads: Vec<std::thread::JoinHandle<()>>,
    sequence: AtomicU64,
    cursor: AtomicUsize,
    timeout: Duration,
}
impl Remote {
    fn start(d: Deployment) -> Result<Self, EvaluationError> {
        if d.name.is_empty()
            || d.name.len() > 64
            || !d
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !digest(&d.artifact_sha256)
            || !digest(&d.code_sha256)
            || !(1..=2000).contains(&d.timeout_ms)
            || d.replicas.is_empty()
            || d.replicas.len() > 4
        {
            return Err(EvaluationError);
        }
        let mut prepared = Vec::new();
        let mut urls = std::collections::HashSet::new();
        for e in &d.replicas {
            let p = Prepared::new(e, &d.name)?;
            if !urls.insert(p.url.clone()) {
                return Err(EvaluationError);
            }
            prepared.push(p);
        }
        let mut result = Self {
            slots: vec![],
            threads: vec![],
            sequence: AtomicU64::new(1),
            cursor: AtomicUsize::new(0),
            timeout: Duration::from_millis(d.timeout_ms),
        };
        for endpoint in prepared {
            let (tx, rx) = mpsc::sync_channel::<Job>(1);
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let busy = Arc::new(AtomicBool::new(false));
            let owned = busy.clone();
            let cooldown = Arc::new(Mutex::new(None));
            let circuit = cooldown.clone();
            let config = d.clone();
            let thread = std::thread::Builder::new()
                .name("policy-remote".into())
                .spawn(move || {
                    // Blocking reqwest runtime is created and dropped on this owned thread,
                    // never inside the gateway's Tokio runtime.
                    let client = match endpoint.client() {
                        Ok(c) => c,
                        Err(_) => {
                            let _ = ready_tx.send(false);
                            return;
                        }
                    };
                    let _ = ready_tx.send(true);
                    while let Ok(job) = rx.recv() {
                        let answer = request(&client, &endpoint, &config, &job);
                        if answer.is_err() {
                            if let Ok(mut until) = circuit.lock() {
                                *until = Some(Instant::now() + Duration::from_secs(1));
                            }
                        }
                        owned.store(false, Ordering::Release);
                        let _ = job.reply.send(answer);
                    }
                })
                .map_err(|_| EvaluationError)?;
            result.threads.push(thread);
            if ready_rx.recv() != Ok(true) {
                return Err(EvaluationError);
            }
            result.slots.push(Slot {
                sender: tx,
                busy,
                cooldown,
            });
        }
        Ok(result)
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        self.slots.clear();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
fn remaining(deadline: Instant) -> Result<Duration, EvaluationError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(EvaluationError)
}
fn request(
    client: &Client,
    endpoint: &Prepared,
    d: &Deployment,
    job: &Job,
) -> Result<f64, EvaluationError> {
    let timeout = remaining(job.deadline)?;
    let ms = timeout.as_millis() as u64;
    if ms == 0 {
        return Err(EvaluationError);
    }
    let body = serde_json::json!({"version":1,"id":job.id,"text":job.text,"joined":job.joined,"timeout_ms":ms.min(d.timeout_ms),"evaluator":d.name,"artifact_sha256":d.artifact_sha256,"code_sha256":d.code_sha256});
    let bytes = serde_json::to_vec(&body).map_err(|_| EvaluationError)?;
    if bytes.len() > 2097152 {
        return Err(EvaluationError);
    }
    let response = client
        .post(endpoint.url.clone())
        .header(AUTHORIZATION, endpoint.auth.clone())
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .timeout(remaining(job.deadline)?)
        .body(bytes)
        .send()
        .map_err(|_| EvaluationError)?;
    if response.status() != reqwest::StatusCode::OK
        || response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.split(';').next())
            .map(str::trim)
            != Some("application/json")
        || response.content_length().is_some_and(|n| n > 4096)
    {
        return Err(EvaluationError);
    }
    let mut bytes = Vec::new();
    response
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| EvaluationError)?;
    remaining(job.deadline)?;
    if bytes.len() > 4096 {
        return Err(EvaluationError);
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reply {
        version: u32,
        id: String,
        evaluator: String,
        artifact_sha256: String,
        code_sha256: String,
        score: f64,
    }
    let r: Reply = serde_json::from_value(strict_json(&bytes)?).map_err(|_| EvaluationError)?;
    if r.version != 1
        || r.id != job.id
        || r.evaluator != d.name
        || r.artifact_sha256 != d.artifact_sha256
        || r.code_sha256 != d.code_sha256
        || !r.score.is_finite()
        || !(0.0..=1.0).contains(&r.score)
    {
        return Err(EvaluationError);
    }
    Ok(r.score)
}
impl ScoreBackend for Remote {
    fn requires_text_egress(&self) -> bool {
        true
    }
    fn score(&self, input: &Inspection, deadline: Instant) -> Result<f64, EvaluationError> {
        remaining(deadline)?;
        if input.text.len() > 262144 || input.joined.len() > 262144 {
            return Err(EvaluationError);
        }
        let deadline = deadline.min(Instant::now() + self.timeout);
        let id = self
            .sequence
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .map_err(|_| EvaluationError)?
            .to_string();
        let first = self.cursor.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        for offset in 0..self.slots.len() {
            let slot = &self.slots[(first + offset) % self.slots.len()];
            if slot
                .cooldown
                .lock()
                .map_err(|_| EvaluationError)?
                .is_some_and(|until| Instant::now() < until)
                || slot
                    .busy
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                continue;
            }
            // Recheck after owning the slot; a concurrent failure may have opened it.
            if slot
                .cooldown
                .lock()
                .map_err(|_| EvaluationError)?
                .is_some_and(|until| Instant::now() < until)
            {
                slot.busy.store(false, Ordering::Release);
                continue;
            }
            let (reply, rx) = mpsc::sync_channel(1);
            let job = Job {
                id,
                text: input.text.clone(),
                joined: input.joined.clone(),
                deadline,
                reply,
            };
            if slot.sender.try_send(job).is_err() {
                slot.busy.store(false, Ordering::Release);
                return Err(EvaluationError);
            }
            let answer = rx
                .recv_timeout(remaining(deadline)?)
                .map_err(|_| EvaluationError)??;
            remaining(deadline)?;
            return Ok(answer);
        }
        Err(EvaluationError)
    }
}

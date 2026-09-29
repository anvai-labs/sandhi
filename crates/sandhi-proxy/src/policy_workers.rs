//! Proxy-owned local process adapter. No model/runtime dependency enters sandhi-core.
//! POSIX only. Trusted workers, not an adversarial-code sandbox.
use sandhi_core::policy::{strict_json, EvaluationError, Registry};
#[cfg(unix)]
use sandhi_core::policy::{Inspection, ScoreBackend, ThresholdedScore};
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use {
    serde::Serialize,
    sha2::{Digest, Sha256},
    std::{
        io::Write,
        os::{fd::OwnedFd, unix::net::UnixStream},
        process::{Child, Command, Stdio},
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc::{self, SyncSender, TrySendError},
            Arc,
        },
        time::{Duration, Instant},
    },
};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    workers: Vec<Deployment>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Deployment {
    name: String,
    python: PathBuf,
    script: PathBuf,
    script_sha256: String,
    artifact: PathBuf,
    artifact_sha256: String,
    pool_size: usize,
    startup_timeout_ms: u64,
}
pub fn load_registry(path: Option<&Path>) -> Result<Registry, EvaluationError> {
    let mut registry = Registry::default();
    if let Some(path) = path {
        register_workers(&mut registry, &read_bounded(path, 65536)?)?;
    }
    Ok(registry)
}
pub(crate) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, EvaluationError> {
    let mut data = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| EvaluationError)?
        .take((limit + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|_| EvaluationError)?;
    if data.len() > limit {
        return Err(EvaluationError);
    }
    Ok(data)
}
pub fn register_workers(registry: &mut Registry, bytes: &[u8]) -> Result<(), EvaluationError> {
    if bytes.len() > 65536 {
        return Err(EvaluationError);
    }
    let manifest: Manifest =
        serde_json::from_value(strict_json(bytes)?).map_err(|_| EvaluationError)?;
    if manifest.version != 1
        || manifest.workers.is_empty()
        || manifest.workers.len() > 4
        || manifest
            .workers
            .iter()
            .any(|d| !(1..=4).contains(&d.pool_size))
        || manifest.workers.iter().map(|d| d.pool_size).sum::<usize>() > 4
    {
        return Err(EvaluationError);
    }
    for deployment in manifest.workers {
        if !(1..=4).contains(&deployment.pool_size)
            || !(1..=30000).contains(&deployment.startup_timeout_ms)
            || !deployment.python.is_absolute()
            || !deployment.script.is_absolute()
            || !deployment.artifact.is_absolute()
        {
            return Err(EvaluationError);
        }
        #[cfg(unix)]
        {
            let name = deployment.name.clone();
            let backend = Arc::new(Pool::start(deployment)?);
            registry.register(&name, move |value| {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Config {
                    at_least: f64,
                }
                let c: Config =
                    serde_json::from_value(value.clone()).map_err(|_| EvaluationError)?;
                Ok(Box::new(ThresholdedScore::new(
                    backend.clone(),
                    c.at_least,
                )?))
            })?;
        }
        #[cfg(not(unix))]
        {
            let _ = (registry, deployment);
            return Err(EvaluationError);
        }
    }
    Ok(())
}
#[cfg(unix)]
fn verify(path: &Path, expected: &str, limit: usize) -> Result<(), EvaluationError> {
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(EvaluationError);
    }
    if format!("{:x}", Sha256::digest(read_bounded(path, limit)?)) != expected {
        return Err(EvaluationError);
    }
    Ok(())
}
#[cfg(unix)]
struct Worker {
    child: Child,
    socket: UnixStream,
}
#[cfg(unix)]
impl Drop for Worker {
    fn drop(&mut self) {
        // Trusted workers do not spawn descendants. Deployment must enforce that for
        // third-party models. Reap the owned process before its slot is replaced.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[cfg(unix)]
impl Worker {
    fn start(d: &Deployment) -> Result<Self, EvaluationError> {
        verify(&d.script, &d.script_sha256, 262144)?;
        verify(&d.artifact, &d.artifact_sha256, 1048576)?;
        let (socket, child_socket) = UnixStream::pair().map_err(|_| EvaluationError)?;
        let input: OwnedFd = child_socket
            .try_clone()
            .map_err(|_| EvaluationError)?
            .into();
        let output: OwnedFd = child_socket.into();
        let child = Command::new(&d.python)
            .args(["-I", "-B", "-u"])
            .arg(&d.script)
            .arg(&d.artifact)
            .arg(&d.artifact_sha256)
            .current_dir("/")
            .env_clear()
            .env("OMP_NUM_THREADS", "1")
            .env("OPENBLAS_NUM_THREADS", "1")
            .env("MKL_NUM_THREADS", "1")
            .env("VECLIB_MAXIMUM_THREADS", "1")
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| EvaluationError)?;
        let mut worker = Self { child, socket };
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Ready {
            version: u32,
            kind: String,
            artifact_sha256: String,
        }
        let ready: Ready = serde_json::from_value(
            worker.read(Instant::now() + Duration::from_millis(d.startup_timeout_ms))?,
        )
        .map_err(|_| EvaluationError)?;
        if ready.version != 1 || ready.kind != "ready" || ready.artifact_sha256 != d.artifact_sha256
        {
            return Err(EvaluationError);
        }
        Ok(worker)
    }
    fn read(&mut self, deadline: Instant) -> Result<serde_json::Value, EvaluationError> {
        // Small frames, no unbounded reader thread or buffer. Absolute deadline is
        // re-applied after each partial read, defeating byte-dribble extensions.
        let mut data = Vec::new();
        let mut buf = [0; 512];
        loop {
            self.socket
                .set_read_timeout(Some(remaining(deadline)?))
                .map_err(|_| EvaluationError)?;
            let n = self.socket.read(&mut buf).map_err(|_| EvaluationError)?;
            if n == 0 || data.len() + n > 4096 {
                return Err(EvaluationError);
            }
            data.extend_from_slice(&buf[..n]);
            if let Some(i) = data.iter().position(|b| *b == b'\n') {
                if i + 1 != data.len() {
                    return Err(EvaluationError);
                }
                return strict_json(&data[..i]);
            }
        }
    }
    fn score(&mut self, job: &Job) -> Result<f64, EvaluationError> {
        #[derive(Serialize)]
        struct Request<'a> {
            version: u32,
            id: &'a str,
            text: &'a str,
            joined: &'a str,
        }
        let mut bytes = serde_json::to_vec(&Request {
            version: 1,
            id: &job.id,
            text: &job.text,
            joined: &job.joined,
        })
        .map_err(|_| EvaluationError)?;
        if bytes.len() > 2097152 {
            return Err(EvaluationError);
        }
        bytes.push(b'\n');
        let mut rest = bytes.as_slice();
        while !rest.is_empty() {
            self.socket
                .set_write_timeout(Some(remaining(job.deadline)?))
                .map_err(|_| EvaluationError)?;
            let n = self.socket.write(rest).map_err(|_| EvaluationError)?;
            if n == 0 {
                return Err(EvaluationError);
            }
            rest = &rest[n..];
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            version: u32,
            id: String,
            score: f64,
        }
        let result: Reply =
            serde_json::from_value(self.read(job.deadline)?).map_err(|_| EvaluationError)?;
        if result.version != 1
            || result.id != job.id
            || !result.score.is_finite()
            || !(0.0..=1.0).contains(&result.score)
        {
            return Err(EvaluationError);
        }
        remaining(job.deadline)?;
        Ok(result.score)
    }
}
#[cfg(unix)]
fn remaining(deadline: Instant) -> Result<Duration, EvaluationError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(EvaluationError)
}
#[cfg(unix)]
struct Job {
    id: String,
    text: String,
    joined: String,
    deadline: Instant,
    reply: SyncSender<Result<f64, EvaluationError>>,
}
#[cfg(unix)]
struct Slot {
    sender: SyncSender<Job>,
    busy: Arc<AtomicBool>,
}
#[cfg(unix)]
struct Pool {
    senders: Vec<Slot>,
    threads: Vec<std::thread::JoinHandle<()>>,
    sequence: AtomicU64,
}
#[cfg(unix)]
impl Pool {
    fn start(d: Deployment) -> Result<Self, EvaluationError> {
        let mut pool = Self {
            senders: Vec::new(),
            threads: Vec::new(),
            sequence: AtomicU64::new(1),
        };
        for _ in 0..d.pool_size {
            let mut worker = Worker::start(&d)?;
            let (tx, rx) = mpsc::sync_channel::<Job>(1);
            let busy = Arc::new(AtomicBool::new(false));
            let worker_busy = busy.clone();
            let (ready_tx, ready_rx) = mpsc::sync_channel(0);
            let deployment = d.clone();
            let handle = std::thread::Builder::new()
                .name("policy-worker".into())
                .spawn(move || {
                    let _ = ready_tx.send(());
                    let mut replacements = 0;
                    while let Ok(job) = rx.recv() {
                        // A caller that timed out before scheduling cannot send stale text.
                        if remaining(job.deadline).is_err() {
                            worker_busy.store(false, Ordering::Release);
                            let _ = job.reply.send(Err(EvaluationError));
                            continue;
                        }
                        let result = worker.score(&job);
                        let failed = result.is_err();
                        if !failed {
                            worker_busy.store(false, Ordering::Release);
                        }
                        let _ = job.reply.send(result);
                        if failed {
                            drop(worker);
                            replacements += 1;
                            if replacements > 3 {
                                return;
                            }
                            // Replacement is background work; it never delays a caller or
                            // accepts a request before the next readiness handshake.
                            match Worker::start(&deployment) {
                                Ok(next) => worker = next,
                                Err(_) => return,
                            }
                            worker_busy.store(false, Ordering::Release);
                        }
                    }
                })
                .map_err(|_| EvaluationError)?;
            ready_rx.recv().map_err(|_| EvaluationError)?;
            pool.senders.push(Slot { sender: tx, busy });
            pool.threads.push(handle);
        }
        Ok(pool)
    }
}
#[cfg(unix)]
impl Drop for Pool {
    fn drop(&mut self) {
        self.senders.clear();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}
#[cfg(unix)]
impl ScoreBackend for Pool {
    fn score(&self, input: &Inspection, deadline: Instant) -> Result<f64, EvaluationError> {
        remaining(deadline)?;
        if input.text.len() > 262144 || input.joined.len() > 262144 {
            return Err(EvaluationError);
        }
        let (reply, rx) = mpsc::sync_channel(1);
        let seq = self
            .sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .map_err(|_| EvaluationError)?;
        let mut job = Job {
            id: seq.to_string(),
            text: input.text.clone(),
            joined: input.joined.clone(),
            deadline,
            reply,
        };
        for slot in &self.senders {
            if slot
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            match slot.sender.try_send(job) {
                Ok(()) => {
                    return rx
                        .recv_timeout(remaining(deadline)?)
                        .map_err(|_| EvaluationError)?
                }
                Err(TrySendError::Full(returned) | TrySendError::Disconnected(returned)) => {
                    slot.busy.store(false, Ordering::Release);
                    job = returned
                }
            }
        }
        Err(EvaluationError)
    }
}

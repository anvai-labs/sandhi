//! Optional embedded CPU ONNX score adapter. Operator-pinned artifacts only.
use sandhi_core::policy::{EvaluationError, Registry};
use std::path::Path;
pub fn load_models(registry: &mut Registry, path: Option<&Path>) -> Result<(), EvaluationError> {
    if let Some(path) = path {
        let bytes = crate::policy_workers::read_bounded(path, 65536)?;
        register_models(registry, &bytes)?;
    }
    Ok(())
}
#[cfg(not(feature = "policy-onnx"))]
pub fn register_models(_: &mut Registry, _: &[u8]) -> Result<(), EvaluationError> {
    Err(EvaluationError)
}
#[cfg(feature = "policy-onnx")]
pub use embedded::{load_backend, register_models};

#[cfg(feature = "policy-onnx")]
mod embedded {
    use super::*;
    use ort::{
        session::{RunOptions, Session},
        value::{Tensor, TensorElementType, ValueType},
    };
    use sandhi_core::policy::{strict_json, Inspection, ScoreBackend, ThresholdedScore};
    use serde::Deserialize;
    use sha2::{Digest, Sha256};
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{mpsc, Arc, Mutex},
        time::{Duration, Instant},
    };

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Manifest {
        version: u32,
        runtime_library: PathBuf,
        runtime_sha256: String,
        models: Vec<Deployment>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Deployment {
        name: String,
        model: PathBuf,
        model_sha256: String,
        preprocessing: PathBuf,
        preprocessing_sha256: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Preprocessing {
        version: u32,
        profile: String,
        vocabulary: Vec<String>,
    }
    static RUNTIME: Mutex<Option<(PathBuf, String)>> = Mutex::new(None);
    fn bytes(path: &Path, hash: &str, max: usize) -> Result<Vec<u8>, EvaluationError> {
        if !path.is_absolute()
            || hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(EvaluationError);
        }
        let bytes = crate::policy_workers::read_bounded(path, max)?;
        if format!("{:x}", Sha256::digest(&bytes)) != hash {
            return Err(EvaluationError);
        }
        Ok(bytes)
    }
    fn manifest(data: &[u8]) -> Result<Manifest, EvaluationError> {
        if data.len() > 65536 {
            return Err(EvaluationError);
        }
        let m: Manifest =
            serde_json::from_value(strict_json(data)?).map_err(|_| EvaluationError)?;
        if m.version != 1 || m.models.is_empty() || m.models.len() > 4 {
            return Err(EvaluationError);
        }
        Ok(m)
    }
    fn runtime(m: &Manifest) -> Result<(), EvaluationError> {
        let mut loaded = RUNTIME.lock().map_err(|_| EvaluationError)?;
        let path = m
            .runtime_library
            .canonicalize()
            .map_err(|_| EvaluationError)?;
        if let Some((current, hash)) = loaded.as_ref() {
            if current != &path || hash != &m.runtime_sha256 {
                return Err(EvaluationError);
            }
            return Ok(());
        }
        bytes(&m.runtime_library, &m.runtime_sha256, 268435456)?;
        let committed = ort::init_from(&path)
            .map_err(|_| EvaluationError)?
            .with_logger(Arc::new(|_, _, _, _, _| {}))
            .commit();
        if !committed {
            return Err(EvaluationError);
        }
        *loaded = Some((path, m.runtime_sha256.clone()));
        Ok(())
    }
    pub fn register_models(registry: &mut Registry, data: &[u8]) -> Result<(), EvaluationError> {
        let m = manifest(data)?;
        runtime(&m)?;
        for d in m.models {
            let backend = Arc::new(Backend::load(&d)?);
            registry.register(&d.name, move |v| {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Config {
                    at_least: f64,
                }
                let c: Config = serde_json::from_value(v.clone()).map_err(|_| EvaluationError)?;
                Ok(Box::new(ThresholdedScore::new(
                    backend.clone(),
                    c.at_least,
                )?))
            })?;
        }
        Ok(())
    }
    /// Load a single approved deployment for validation or trusted embedding use.
    pub fn load_backend(data: &[u8]) -> Result<Arc<dyn ScoreBackend>, EvaluationError> {
        let m = manifest(data)?;
        if m.models.len() != 1 {
            return Err(EvaluationError);
        }
        runtime(&m)?;
        Ok(Arc::new(Backend::load(&m.models[0])?))
    }
    struct Backend {
        session: Mutex<Session>,
        vocabulary: HashMap<String, usize>,
    }
    impl Backend {
        fn load(d: &Deployment) -> Result<Self, EvaluationError> {
            let data = bytes(&d.preprocessing, &d.preprocessing_sha256, 262144)?;
            let pre: Preprocessing =
                serde_json::from_value(strict_json(&data)?).map_err(|_| EvaluationError)?;
            let vocabulary = validate_preprocessing(pre)?;
            let model = bytes(&d.model, &d.model_sha256, 8388608)?;
            let session = Session::builder()
                .map_err(|_| EvaluationError)?
                .with_intra_threads(1)
                .map_err(|_| EvaluationError)?
                .with_inter_threads(1)
                .map_err(|_| EvaluationError)?
                .with_parallel_execution(false)
                .map_err(|_| EvaluationError)?
                .with_memory_pattern(false)
                .map_err(|_| EvaluationError)?
                .with_execution_providers([ort::ep::CPU::default()
                    .with_arena_allocator(false)
                    .build()
                    .error_on_failure()])
                .map_err(|_| EvaluationError)?
                .with_logger(Arc::new(|_, _, _, _, _| {}))
                .map_err(|_| EvaluationError)?
                .commit_from_memory(&model)
                .map_err(|_| EvaluationError)?;
            if session.inputs().len() != 1
                || session.outputs().len() != 1
                || session.inputs()[0].name() != "counts"
                || session.outputs()[0].name() != "score"
                || !shape(session.inputs()[0].dtype(), &[2, vocabulary.len() as i64])
                || !shape(session.outputs()[0].dtype(), &[1])
            {
                return Err(EvaluationError);
            }
            let backend = Self {
                session: Mutex::new(session),
                vocabulary,
            };
            // Readiness runs actual inference. It is not a model-quality assertion.
            backend.score(
                &Inspection {
                    text: String::new(),
                    joined: String::new(),
                    body_bytes: 0,
                    max_output_tokens: None,
                },
                Instant::now() + Duration::from_secs(2),
            )?;
            Ok(backend)
        }
    }
    fn shape(value: &ValueType, expected: &[i64]) -> bool {
        matches!(value,ValueType::Tensor{ty:TensorElementType::Float32,shape,..} if shape.as_ref()==expected)
    }
    fn validate_preprocessing(
        pre: Preprocessing,
    ) -> Result<HashMap<String, usize>, EvaluationError> {
        if pre.version != 1
            || pre.profile != "ascii_word_counts_v1"
            || pre.vocabulary.is_empty()
            || pre.vocabulary.len() > 4096
        {
            return Err(EvaluationError);
        }
        let mut words = HashMap::new();
        for (i, word) in pre.vocabulary.into_iter().enumerate() {
            if word.len() < 2
                || word.len() > 256
                || !word
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
                || words.insert(word, i).is_some()
            {
                return Err(EvaluationError);
            }
        }
        Ok(words)
    }
    fn remaining(deadline: Instant) -> Result<Duration, EvaluationError> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(EvaluationError)
    }
    fn counts(
        input: &Inspection,
        words: &HashMap<String, usize>,
        deadline: Instant,
    ) -> Result<Vec<f32>, EvaluationError> {
        remaining(deadline)?;
        let mut values = vec![0.; 2 * words.len()];
        for (row, text) in [&input.text, &input.joined].iter().enumerate() {
            if text.len() > 262144 || !text.is_ascii() {
                return Err(EvaluationError);
            }
            for word in text
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|w| w.len() >= 2)
            {
                if let Some(index) = words.get(&word.to_ascii_lowercase()) {
                    values[row * words.len() + index] += 1.;
                }
            }
        }
        remaining(deadline)?;
        Ok(values)
    }
    impl ScoreBackend for Backend {
        fn score(&self, input: &Inspection, deadline: Instant) -> Result<f64, EvaluationError> {
            remaining(deadline)?;
            let mut session = self.session.try_lock().map_err(|_| EvaluationError)?;
            let values = counts(input, &self.vocabulary, deadline)?;
            let tensor = Tensor::from_array(([2, self.vocabulary.len()], values))
                .map_err(|_| EvaluationError)?;
            run(&mut session, tensor, deadline)
        }
    }
    fn run(
        session: &mut Session,
        tensor: Tensor<f32>,
        deadline: Instant,
    ) -> Result<f64, EvaluationError> {
        let options = Arc::new(RunOptions::new().map_err(|_| EvaluationError)?);
        remaining(deadline)?;
        let cancel = options.clone();
        let (tx, rx) = mpsc::sync_channel::<()>(1);
        let watcher = std::thread::Builder::new()
            .name("policy-onnx-deadline".into())
            .spawn(move || {
                if deadline
                    .checked_duration_since(Instant::now())
                    .map_or(true, |timeout| {
                        matches!(
                            rx.recv_timeout(timeout),
                            Err(mpsc::RecvTimeoutError::Timeout)
                        )
                    })
                {
                    let _ = cancel.terminate();
                }
            })
            .map_err(|_| EvaluationError)?;
        let result = session.run_with_options(ort::inputs![tensor], &options);
        let _ = tx.send(());
        let _ = watcher.join();
        remaining(deadline)?;
        let outputs = result.map_err(|_| EvaluationError)?;
        let (shape, values) = outputs["score"]
            .try_extract_tensor::<f32>()
            .map_err(|_| EvaluationError)?;
        if shape.as_ref() != [1]
            || values.len() != 1
            || !values[0].is_finite()
            || !(0.0..=1.0).contains(&values[0])
        {
            return Err(EvaluationError);
        }
        Ok(values[0] as f64)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn onnx_ascii_preprocessing_is_bounded_and_has_exact_word_semantics() {
            let vocabulary = validate_preprocessing(Preprocessing {
                version: 1,
                profile: "ascii_word_counts_v1".into(),
                vocabulary: vec!["private".into(), "customer".into(), "_private_".into()],
            })
            .unwrap();
            let mut input = Inspection {
                text: "PRIVATE private! _private_ customer_42 customer".into(),
                joined: "privatecustomer".into(),
                body_bytes: 0,
                max_output_tokens: None,
            };
            assert_eq!(
                counts(&input, &vocabulary, Instant::now() + Duration::from_secs(1)).unwrap(),
                vec![2., 1., 1., 0., 0., 0.]
            );
            input.text = "privaté".into();
            assert!(counts(&input, &vocabulary, Instant::now() + Duration::from_secs(1)).is_err());
            input.text = "x".repeat(262145);
            assert!(counts(&input, &vocabulary, Instant::now() + Duration::from_secs(1)).is_err());
            assert!(counts(&input, &vocabulary, Instant::now()).is_err());
        }
        #[test]
        fn onnx_invalid_profiles_and_manifests_fail_before_runtime_load() {
            for vocabulary in [
                vec![],
                vec!["x".into()],
                vec!["aB".into()],
                vec!["ab".into(), "ab".into()],
            ] {
                assert!(validate_preprocessing(Preprocessing {
                    version: 1,
                    profile: "ascii_word_counts_v1".into(),
                    vocabulary
                })
                .is_err());
            }
            assert!(manifest(br#"{"version":1,"version":1}"#).is_err());
            assert!(manifest(&vec![b'x'; 65537]).is_err());
        }
    }
}
#[cfg(all(test, not(feature = "policy-onnx")))]
mod tests {
    use super::*;
    #[test]
    fn onnx_without_compiled_feature_fails_closed() {
        assert!(register_models(&mut Registry::default(), b"{}").is_err());
        assert!(load_models(&mut Registry::default(), None).is_ok());
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The real embedding model: EmbeddingGemma 2 at q4 through ONNX Runtime, its pinned files,
//! and their download and checks.

use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use ort::environment::Environment;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{RunOptions, Session};
use ort::value::Tensor;
use tokenizers::{Tokenizer, TruncationParams};

use knoggin_core::embed::{EmbedError, EmbeddingProvider, ModelIdentity};

pub mod model;

/// Width of the unused image, video and audio inputs of this export.
const MODALITY_WIDTH: usize = 512;
const DIM: usize = model::DIMENSIONS as usize;

/// ONNX Runtime allows one environment per process.
static ENVIRONMENT: OnceLock<Environment> = OnceLock::new();

/// Loading the ONNX Runtime library, once per process.
pub mod runtime {
    use std::path::Path;

    use knoggin_core::embed::EmbedError;

    /// Loads the ONNX Runtime library from `library` and makes the one environment of this process.
    /// Call it once at start, before any `OnnxProvider::load`. It sets no environment variable.
    pub fn init(library: &Path) -> Result<(), EmbedError> {
        let environment = ort::init_from(library)
            .map_err(EmbedError::load)?
            .with_telemetry(false)
            .build()
            .map_err(EmbedError::load)?;
        // The `tracing` feature starts the environment at the verbose level; every verbose
        // message would be formatted and handed over only to be dropped.
        environment.set_log_level(ort::logging::LogLevel::Warning);
        super::ENVIRONMENT
            .set(environment)
            .map_err(|_| EmbedError::already_initialized())
    }
}

/// Which side of the search a provider embeds for. It sets only threads and memory use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Query texts: a recall waits for these.
    Query,
    /// Document texts: saves and the queue.
    Document,
}

/// One ONNX Runtime session and one tokenizer for one role.
pub struct OnnxProvider {
    // Each provider is used by one thread; the run options sit beside the session they serve.
    session: Mutex<(Session, Option<RunOptions>)>,
    tokenizer: Tokenizer,
    identity: ModelIdentity,
    role: Role,
}

impl std::fmt::Debug for OnnxProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnnxProvider")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl OnnxProvider {
    /// Loads the model from `dir`, the folder that holds the three checked files.
    pub fn load(dir: &Path, role: Role) -> Result<Self, EmbedError> {
        let environment = ENVIRONMENT.get().ok_or_else(EmbedError::not_initialized)?;
        let mut tokenizer =
            Tokenizer::from_file(dir.join(model::TOKENIZER.name)).map_err(EmbedError::load)?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: model::MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(EmbedError::load)?;
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let threads = match role {
            Role::Query => cores.min(4),
            Role::Document => (cores / 2).max(1),
        };
        let mut builder = Session::builder(environment)
            .map_err(EmbedError::load)?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(EmbedError::load)?
            .with_intra_threads(threads)
            .map_err(EmbedError::load)?
            // Idle threads block instead of spinning. Spinning held a whole core while idle.
            .with_intra_op_spinning(false)
            .map_err(EmbedError::load)?
            .with_inter_op_spinning(false)
            .map_err(EmbedError::load)?;
        let session = builder
            .commit_from_file(dir.join(model::GRAPH.name))
            .map_err(EmbedError::load)?;
        let options = match role {
            Role::Query => None,
            Role::Document => {
                // A long document must not pin its peak arena memory after the run.
                let mut options = RunOptions::new().map_err(EmbedError::load)?;
                options
                    .set("memory.enable_memory_arena_shrinkage", "cpu:0")
                    .map_err(EmbedError::load)?;
                Some(options)
            }
        };
        Ok(Self {
            session: Mutex::new((session, options)),
            tokenizer,
            identity: model::identity(),
            role,
        })
    }
}

impl EmbeddingProvider for OnnxProvider {
    fn identity(&self) -> &ModelIdentity {
        &self.identity
    }

    /// `text` is already in its recipe. One text per run.
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let encoding = self.tokenizer.encode(text, true).map_err(EmbedError::run)?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&i| i64::from(i)).collect();
        let len = ids.len();
        // The mask comes from the tokenizer, so a padding rule in tokenizer.json cannot give pad tokens weight.
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| i64::from(m))
            .collect();
        let ints = |shape: [usize; 2], data: Vec<i64>| {
            Tensor::from_array((shape, data)).map_err(EmbedError::run)
        };
        let empty = || {
            Tensor::from_array(([0usize, MODALITY_WIDTH], Vec::<f32>::new()))
                .map_err(EmbedError::run)
        };
        let inputs = ort::inputs![
            "input_ids" => ints([1, len], ids)?,
            "attention_mask" => ints([1, len], mask)?,
            "image_features" => empty()?,
            "video_features" => empty()?,
            "audio_features" => empty()?,
        ];
        let mut guard = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let (session, options) = &mut *guard;
        let outputs = match options {
            Some(options) => session.run_with_options(inputs, options),
            None => session.run(inputs),
        }
        .map_err(EmbedError::run)?;
        let output = outputs
            .get("sentence_embedding")
            .ok_or_else(EmbedError::bad_output)?;
        let (_, data) = output
            .try_extract_tensor::<f32>()
            .map_err(EmbedError::run)?;
        let Some(head) = data.get(..DIM) else {
            return Err(EmbedError::bad_output());
        };
        let mut vector = head.to_vec();
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if !norm.is_finite() || norm == 0.0 {
            return Err(EmbedError::bad_output());
        }
        vector.iter_mut().for_each(|v| *v /= norm);
        Ok(vector)
    }
}

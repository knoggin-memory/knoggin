// SPDX-License-Identifier: AGPL-3.0-only
//! `knoggin-bench`: the LoCoMo benchmark harness, `embed-compare` (the faster compute path
//! measurement) and `gen-export` (large stores for the server measurements). A separate
//! binary, not part of any release.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line tool that reports on the terminal"
)]

mod compare;
mod data;
mod gen_export;
mod llm;
mod locomo;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use knoggin_core::time::Timestamp;
use knoggin_embed::{OnnxProvider, Role};

#[derive(Debug, Parser)]
#[command(version, about = "Knoggin's benchmark and measurement tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the LoCoMo benchmark. Calls the model server at --llm-url;
    /// set BENCH_LLM_API_KEY for its bearer token and BENCH_JUDGE_STYLE=mem0 for Mem0's judge.
    Locomo(LocomoArgs),
    /// Embed every LoCoMo turn and question with two graphs and print the mean overlap of
    /// the top 10 vector results per question.
    EmbedCompare {
        /// The LoCoMo data file, locomo10.json.
        #[arg(long)]
        data: PathBuf,
        /// A folder holding the model's three files with the first graph.
        #[arg(long)]
        a: PathBuf,
        /// A folder holding the model's three files with the second graph. ONNX Runtime
        /// does not follow a symbolic link to the weights file; use a hard link.
        #[arg(long)]
        b: PathBuf,
        #[command(flatten)]
        ort: OrtArg,
    },
    /// Write an export-format file of synthetic memories for the server measurements.
    GenExport {
        /// How many memories.
        #[arg(long)]
        count: usize,
        /// The generator's seed; the same seed gives the same memories.
        #[arg(long)]
        seed: u64,
        /// The file to write.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Debug, clap::Args)]
struct OrtArg {
    /// The ONNX Runtime library.
    #[arg(long, env = "ORT_DYLIB_PATH")]
    ort_lib: PathBuf,
}

#[derive(Debug, clap::Args)]
struct LocomoArgs {
    /// The LoCoMo data file, locomo10.json.
    #[arg(long)]
    data: PathBuf,
    /// The OpenAI-compatible model server, without /v1/chat/completions.
    #[arg(long)]
    llm_url: String,
    /// The model that answers and writes the search queries.
    #[arg(long, default_value = "qwen3-235b")]
    model: String,
    /// The model that turns conversation turns into memories.
    #[arg(long, default_value = "qwen3-235b")]
    ingest_model: String,
    /// The model that judges the answers.
    #[arg(long, default_value = "deepseek-v3.2")]
    judge_model: String,
    /// Memories per recall and in the final list.
    #[arg(long, default_value_t = 25)]
    top_k: usize,
    /// How many of the final list carry their full text, as Knoggin's default recall.
    #[arg(long, default_value_t = 5)]
    full_text_count: usize,
    /// Leave out the adversarial questions (category 5).
    #[arg(long)]
    skip_adversarial: bool,
    /// Conversations run at once.
    #[arg(long, default_value_t = 2)]
    parallel: usize,
    /// Questions run at once within a conversation.
    #[arg(long, default_value_t = 4)]
    qa_parallel: usize,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
    /// Where bench_results.json and bench_debug.log go.
    #[arg(long, default_value = ".")]
    out_dir: PathBuf,
    /// The folder holding the embedding model's three files; by default the one that
    /// `knoggin model fetch` fills, `~/.knoggin/models/<model identity>`.
    #[arg(long)]
    model_dir: Option<PathBuf>,
    #[command(flatten)]
    ort: OrtArg,
}

fn load(dir: &Path, role: Role) -> anyhow::Result<OnnxProvider> {
    OnnxProvider::load(dir, role)
        .map_err(|e| anyhow::anyhow!("could not load the model in {}: {e}", dir.display()))
}

fn init_ort(ort: &OrtArg) -> anyhow::Result<()> {
    knoggin_embed::runtime::init(&ort.ort_lib)
        .map_err(|e| anyhow::anyhow!("could not load {}: {e}", ort.ort_lib.display()))
}

fn now() -> Timestamp {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    Timestamp::from_millis(i64::try_from(nanos / 1_000_000).unwrap_or(i64::MAX))
}

async fn locomo(args: LocomoArgs) -> anyhow::Result<()> {
    let judge_style = llm::JudgeStyle::parse(std::env::var("BENCH_JUDGE_STYLE").ok().as_deref())?;
    let key = std::env::var("BENCH_LLM_API_KEY").ok();
    let client = |model: &str| llm::LlmClient::new(model, &args.llm_url, key.clone(), judge_style);
    let models = locomo::Models {
        answer: client(&args.model)?,
        ingest: client(&args.ingest_model)?,
        judge: client(&args.judge_model)?,
    };
    let model_dir = match args.model_dir {
        Some(dir) => dir,
        None => {
            let home = std::env::var_os("HOME").context("HOME is not set; pass --model-dir")?;
            knoggin_embed::model::model_dir(&PathBuf::from(home).join(".knoggin/models"))
        }
    };
    init_ort(&args.ort)?;
    let embedders = locomo::Embedders {
        document: Arc::new(load(&model_dir, Role::Document)?),
        query: Arc::new(load(&model_dir, Role::Query)?),
    };
    let config = locomo::RunConfig {
        data: args.data,
        out_dir: args.out_dir,
        top_k: args.top_k,
        full_text_count: args.full_text_count,
        skip_adversarial: args.skip_adversarial,
        parallel: args.parallel,
        qa_parallel: args.qa_parallel,
        json: args.json,
    };
    locomo::run(config, models, embedders).await
}

fn embed_compare(data: &Path, a: &Path, b: &Path, ort: &OrtArg) -> anyhow::Result<()> {
    init_ort(ort)?;
    let convs = data::load(data, now().millis())?;
    eprintln!("Graph A: {}", a.display());
    let va = compare::embed_all(&convs, &load(a, Role::Query)?)?;
    eprintln!("Graph B: {}", b.display());
    let vb = compare::embed_all(&convs, &load(b, Role::Query)?)?;
    let c = compare::compare(&va, &vb);
    let turns: usize = convs.iter().map(|c| c.turns.len()).sum();
    println!("turns: {turns}, questions: {}", c.questions);
    println!("mean top-10 overlap: {:.4}", c.mean_overlap);
    println!("min top-10 overlap: {:.4}", c.min_overlap);
    match c.mean_cosine {
        Some(cos) => println!("mean cosine A to B: {cos:.5}"),
        None => println!("mean cosine A to B: not comparable (different widths)"),
    }
    Ok(())
}

fn gen_export(count: usize, seed: u64, out: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::create(out)
        .with_context(|| format!("could not create {}", out.display()))?;
    gen_export::generate(count, seed, now(), &mut std::io::BufWriter::new(file))?;
    eprintln!("Wrote {count} memories to {}", out.display());
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Locomo(args) => {
            rustls::crypto::aws_lc_rs::default_provider()
                .install_default()
                .map_err(|_| anyhow::anyhow!("a TLS provider is installed already"))?;
            tokio::runtime::Runtime::new()?.block_on(locomo(args))
        }
        Command::EmbedCompare { data, a, b, ort } => embed_compare(&data, &a, &b, &ort),
        Command::GenExport { count, seed, out } => gen_export(count, seed, &out),
    }
}

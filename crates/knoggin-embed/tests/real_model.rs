// SPDX-License-Identifier: AGPL-3.0-only
//! The real model. Needs `KNOGGIN_MODEL__DIR` (the models root) and `ORT_DYLIB_PATH`:
//! `cargo nextest run -p knoggin-embed --run-ignored only -E 'binary(real_model)'`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::path::PathBuf;
use std::sync::OnceLock;

use knoggin_core::embed::{EmbeddingProvider, query_text};
use knoggin_embed::{OnnxProvider, Role, model, runtime};

struct Model {
    dir: PathBuf,
    query: OnnxProvider,
    document: OnnxProvider,
}

/// Checks the files, loads the library once, and loads both providers in this process.
fn model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(|| {
        let root =
            PathBuf::from(std::env::var_os("KNOGGIN_MODEL__DIR").expect("KNOGGIN_MODEL__DIR"));
        let library = PathBuf::from(std::env::var_os("ORT_DYLIB_PATH").expect("ORT_DYLIB_PATH"));
        let dir = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(model::ensure(&root, false, |_, _| {}))
            .unwrap();
        runtime::init(&library).unwrap();
        let query = OnnxProvider::load(&dir, Role::Query).unwrap();
        let document = OnnxProvider::load(&dir, Role::Document).unwrap();
        Model {
            dir,
            query,
            document,
        }
    })
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn check_unit(v: &[f32]) {
    assert_eq!(v.len(), 768);
    let norm = dot(v, v).sqrt();
    assert!((norm - 1.0).abs() < 1e-4, "norm {norm}");
}

const DOCS: [&str; 3] = [
    "title: none \u{b7} 2026-11-23 | text: Mars, known for its reddish appearance, is often referred to as the Red Planet.",
    "title: none \u{b7} 2026-11-23 | text: Venus is often called Earth's twin because of its similar size and proximity.",
    "title: Maya \u{b7} 2026-11-23 | text: Maya prefers window seats travel entity/maya",
];

#[test]
#[ignore = "needs model"]
fn five_texts_give_unit_vectors_and_the_same_vector_twice() {
    let m = model();
    assert_eq!(m.query.identity(), &model::identity());
    let queries = [
        query_text("Which planet is known as the Red Planet?"),
        query_text("Where does Maya like to sit on a plane?"),
    ];
    for text in DOCS {
        let a = m.document.embed(text).unwrap();
        check_unit(&a);
        let b = m.document.embed(text).unwrap();
        let diff = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max);
        assert!(diff < 1e-6, "document vectors differ by {diff}");
    }
    for text in &queries {
        let a = m.query.embed(text).unwrap();
        check_unit(&a);
        let b = m.query.embed(text).unwrap();
        let diff = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max);
        assert!(diff < 1e-6, "query vectors differ by {diff}");
    }
}

#[test]
#[ignore = "needs model"]
fn a_query_scores_its_matching_document_higher() {
    let m = model();
    let docs: Vec<Vec<f32>> = DOCS.iter().map(|t| m.document.embed(t).unwrap()).collect();
    let planet = m
        .query
        .embed(&query_text("Which planet is known as the Red Planet?"))
        .unwrap();
    let seat = m
        .query
        .embed(&query_text("Where does Maya like to sit on a plane?"))
        .unwrap();
    let p: Vec<f32> = docs.iter().map(|d| dot(&planet, d)).collect();
    let s: Vec<f32> = docs.iter().map(|d| dot(&seat, d)).collect();
    println!("planet query: {p:?}; seat query: {s:?}");
    assert!(p[0] > p[1] && p[0] > p[2]);
    assert!(s[2] > s[0] && s[2] > s[1]);
}

#[test]
#[ignore = "needs model"]
fn a_5000_word_text_is_accepted() {
    let m = model();
    let words = [
        "memory", "window", "planet", "seat", "travel", "Maya", "red",
    ];
    let text: Vec<&str> = (0..5000).map(|i| words[i % words.len()]).collect();
    let text = format!("title: none \u{b7} 2026-11-23 | text: {}", text.join(" "));
    check_unit(&m.document.embed(&text).unwrap());
    check_unit(&m.query.embed(&query_text(&text)).unwrap());
}

#[test]
#[ignore = "needs model"]
fn token_ids_match_the_reference_tokenizer() {
    let m = model();
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/tokenizer/ids.json")).unwrap();
    assert_eq!(fixture["tokenizer_sha256"], model::TOKENIZER.sha256);
    let tokenizer = tokenizers::Tokenizer::from_file(m.dir.join("tokenizer.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 20);
    for case in cases {
        let text = case["text"].as_str().unwrap();
        let want: Vec<u32> = case["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| u32::try_from(v.as_u64().unwrap()).unwrap())
            .collect();
        let got = tokenizer.encode(text, true).unwrap();
        assert_eq!(got.get_ids(), want.as_slice(), "text {text:?}");
    }
}

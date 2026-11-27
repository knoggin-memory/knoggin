// SPDX-License-Identifier: AGPL-3.0-only
//! `embed-compare` (the faster compute path measurement): embeds every LoCoMo turn and
//! question with two graphs and reports how much the top 10 vector results per question
//! agree. A turn is embedded as a memory in recipe `doc-v1` (summary `Speaker: text`, the
//! turn's time, no labels); a question in recipe `query-v1`; a question is searched among
//! its own conversation's turns.

use std::time::Instant;

use knoggin_core::embed::{EmbeddingProvider, document_text_from_parts, query_text};
use knoggin_core::time::Timestamp;

use crate::data::Conversation;

const TOP: usize = 10;

/// The vectors of one conversation under one graph.
#[derive(Debug)]
pub(crate) struct Vectors {
    turns: Vec<Vec<f32>>,
    questions: Vec<Vec<f32>>,
}

/// Embeds every turn and question of `convs` with `provider`.
pub(crate) fn embed_all(
    convs: &[Conversation],
    provider: &dyn EmbeddingProvider,
) -> anyhow::Result<Vec<Vectors>> {
    let total: usize = convs.iter().map(|c| c.turns.len() + c.qa_pairs.len()).sum();
    let (start, mut done) = (Instant::now(), 0_usize);
    let mut embed = |text: String| -> anyhow::Result<Vec<f32>> {
        done += 1;
        if done % 500 == 0 || done == total {
            eprintln!(
                "  {done}/{total} texts, {:.0} s",
                start.elapsed().as_secs_f64()
            );
        }
        provider
            .embed(&text)
            .map_err(|e| anyhow::anyhow!("embedding failed: {e}"))
    };
    convs
        .iter()
        .map(|c| {
            let turns = c
                .turns
                .iter()
                .map(|t| {
                    let summary = format!("{}: {}", t.speaker, t.text);
                    let at = Timestamp::from_millis(t.timestamp_ms);
                    embed(document_text_from_parts(&[], at, &summary, None, &[]))
                })
                .collect::<anyhow::Result<_>>()?;
            let questions = c
                .qa_pairs
                .iter()
                .map(|q| embed(query_text(&q.question)))
                .collect::<anyhow::Result<_>>()?;
            Ok(Vectors { turns, questions })
        })
        .collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Indices of the `TOP` turns nearest to `query`, best first (ties by index).
fn top(query: &[f32], turns: &[Vec<f32>]) -> Vec<usize> {
    let mut scored: Vec<(f32, usize)> = turns
        .iter()
        .enumerate()
        .map(|(i, t)| (dot(query, t), i))
        .collect();
    scored.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
    scored.into_iter().take(TOP).map(|(_, i)| i).collect()
}

/// What `embed-compare` prints.
#[derive(Debug, PartialEq)]
pub(crate) struct Comparison {
    pub(crate) questions: usize,
    pub(crate) mean_overlap: f64,
    pub(crate) min_overlap: f64,
    /// Mean cosine of the two vectors of each text; `None` when the widths differ.
    pub(crate) mean_cosine: Option<f64>,
}

/// Compares the two embeddings of the same conversations.
pub(crate) fn compare(a: &[Vectors], b: &[Vectors]) -> Comparison {
    let (mut overlaps, mut cosines) = (Vec::new(), Vec::new());
    for (va, vb) in a.iter().zip(b) {
        for (qa, qb) in va.questions.iter().zip(&vb.questions) {
            let (ta, tb) = (top(qa, &va.turns), top(qb, &vb.turns));
            let shared = ta.iter().filter(|i| tb.contains(i)).count();
            if !ta.is_empty() {
                overlaps.push(shared as f64 / ta.len() as f64);
            }
        }
        let texts = va.turns.iter().chain(&va.questions);
        for (x, y) in texts.zip(vb.turns.iter().chain(&vb.questions)) {
            if x.len() == y.len() {
                cosines.push(f64::from(dot(x, y)));
            }
        }
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let all_texts: usize = a.iter().map(|v| v.turns.len() + v.questions.len()).sum();
    Comparison {
        questions: overlaps.len(),
        mean_overlap: mean(&overlaps),
        min_overlap: overlaps.iter().copied().fold(1.0, f64::min),
        mean_cosine: (cosines.len() == all_texts).then(|| mean(&cosines)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use knoggin_core::testing::FakeProvider;

    const DATA: &str = r#"[{"sample_id": "c1",
        "conversation": {"session_1_date_time": "1:56 pm on 8 May, 2023",
            "session_1": [
                {"speaker": "Ann", "text": "I adopted a dog named Rex"},
                {"speaker": "Bo", "text": "I started painting landscapes"},
                {"speaker": "Ann", "text": "Rex loves the beach"}]},
        "qa": [{"question": "What is the dog called?", "answer": "Rex", "category": 4},
               {"question": "What does Bo paint?", "answer": "landscapes", "category": 4}]}]"#;

    #[test]
    fn same_graph_agrees_fully_and_another_does_not_crash() {
        let convs = crate::data::parse(DATA, 0).unwrap();
        let fake = FakeProvider::with_dim(768);
        let a = embed_all(&convs, &fake).unwrap();
        let b = embed_all(&convs, &fake).unwrap();
        let same = compare(&a, &b);
        assert_eq!(same.questions, 2);
        assert!((same.mean_overlap - 1.0).abs() < 1e-9);
        assert!((same.mean_cosine.unwrap() - 1.0).abs() < 1e-5);

        // A narrower model: the overlap is still defined, the cosine is not.
        let other = embed_all(&convs, &FakeProvider::new()).unwrap();
        let cross = compare(&a, &other);
        assert_eq!(cross.mean_cosine, None);
        assert!((0.0..=1.0).contains(&cross.min_overlap));
    }

    #[test]
    fn top_is_best_first_with_ties_by_index() {
        let turns = vec![vec![0.0, 1.0], vec![1.0, 0.0], vec![1.0, 0.0]];
        assert_eq!(top(&[1.0, 0.0], &turns), [1, 2, 0]);
    }
}

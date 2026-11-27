// SPDX-License-Identifier: AGPL-3.0-only
//! The LoCoMo dataset loader, ported from recalld's `parse_dataset` (`src/bench/locomo.rs`).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

/// The question category's name, as in the published report.
pub(crate) fn category_name(id: u32) -> &'static str {
    match id {
        1 => "multi-hop",
        2 => "temporal",
        3 => "open-domain",
        4 => "single-hop",
        5 => "adversarial",
        _ => "unknown",
    }
}

#[derive(Deserialize)]
struct RawConversation {
    #[serde(default)]
    sample_id: Option<String>,
    #[serde(default)]
    conversation: HashMap<String, serde_json::Value>,
    qa: Vec<RawQa>,
}

#[derive(Deserialize)]
struct RawQa {
    question: String,
    #[serde(default, deserialize_with = "value_to_opt_string")]
    answer: Option<String>,
    #[serde(default, deserialize_with = "value_to_opt_string")]
    adversarial_answer: Option<String>,
    category: u32,
}

/// A few gold answers are numbers; they are compared as text.
fn value_to_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(v.map(|val| match val {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }))
}

/// One turn of a conversation.
pub(crate) struct Turn {
    pub(crate) speaker: String,
    pub(crate) text: String,
    pub(crate) blip_caption: Option<String>,
    pub(crate) image_query: Option<String>,
    /// The session's time plus one second per turn, so turns are distinct and ordered.
    pub(crate) timestamp_ms: i64,
}

/// One conversation with its questions.
pub(crate) struct Conversation {
    pub(crate) id: String,
    pub(crate) turns: Vec<Turn>,
    pub(crate) qa_pairs: Vec<QaPair>,
}

/// One question.
pub(crate) struct QaPair {
    pub(crate) question: String,
    /// For an adversarial question: the wrong answer a tricked system would give.
    pub(crate) gold_answer: String,
    pub(crate) category: u32,
    pub(crate) is_adversarial: bool,
}

/// Parses a LoCoMo session date such as `1:56 pm on 8 May, 2023` into Unix milliseconds (UTC).
pub(crate) fn parse_session_date(s: &str) -> Option<i64> {
    let (time_part, date_part) = s.trim().split_once(" on ")?;
    let (clock, is_pm) = if let Some(t) = time_part.strip_suffix(" pm") {
        (t.trim(), true)
    } else {
        (time_part.strip_suffix(" am")?.trim(), false)
    };
    let (hour, minute) = clock.split_once(':')?;
    let mut hour: u8 = hour.parse().ok()?;
    let minute: u8 = minute.parse().ok()?;
    if is_pm && hour != 12 {
        hour += 12;
    } else if !is_pm && hour == 12 {
        hour = 0;
    }
    let mut parts = date_part.trim().splitn(3, ' ');
    let day: u8 = parts.next()?.parse().ok()?;
    let month: time::Month = parts.next()?.trim_end_matches(',').parse().ok()?;
    let year: i32 = parts.next()?.parse().ok()?;
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let at = date.with_hms(hour, minute, 0).ok()?.assume_utc();
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()
}

/// Reads `locomo10.json`. `now_ms` stands in for a session without a date.
pub(crate) fn load(path: &Path, now_ms: i64) -> anyhow::Result<Vec<Conversation>> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    parse(&data, now_ms)
}

/// Parses the dataset's JSON.
pub(crate) fn parse(data: &str, now_ms: i64) -> anyhow::Result<Vec<Conversation>> {
    let raw: Vec<RawConversation> =
        serde_json::from_str(data).context("not a LoCoMo dataset file")?;
    let mut conversations = Vec::with_capacity(raw.len());
    for (idx, raw_conv) in raw.into_iter().enumerate() {
        let id = raw_conv.sample_id.unwrap_or_else(|| format!("conv_{idx}"));

        let mut session_dates: HashMap<u32, i64> = HashMap::new();
        let mut sessions: Vec<(u32, &Vec<serde_json::Value>)> = Vec::new();
        for (key, value) in &raw_conv.conversation {
            let Some(rest) = key.strip_prefix("session_") else {
                continue;
            };
            if let Some(num) = rest.strip_suffix("_date_time") {
                if let (Ok(num), Some(ts)) = (
                    num.parse::<u32>(),
                    value.as_str().and_then(parse_session_date),
                ) {
                    session_dates.insert(num, ts);
                }
            } else if let (Ok(num), Some(list)) = (rest.parse::<u32>(), value.as_array()) {
                sessions.push((num, list));
            }
        }
        sessions.sort_by_key(|(num, _)| *num);

        let mut turns = Vec::new();
        for (num, list) in sessions {
            let base_ts = session_dates.get(&num).copied().unwrap_or(now_ms);
            for (turn_idx, turn) in list.iter().enumerate() {
                let Some(obj) = turn.as_object() else {
                    continue;
                };
                let field = |name: &str| obj.get(name).and_then(|v| v.as_str()).map(str::to_owned);
                let text = field("text").unwrap_or_default();
                if text.is_empty() && obj.get("blip_caption").is_none() {
                    continue;
                }
                let offset = i64::try_from(turn_idx).unwrap_or(i64::MAX / 2_000) * 1000;
                turns.push(Turn {
                    speaker: field("speaker").unwrap_or_default(),
                    text,
                    blip_caption: field("blip_caption"),
                    image_query: field("query"),
                    timestamp_ms: base_ts.saturating_add(offset),
                });
            }
        }

        let qa_pairs = raw_conv
            .qa
            .into_iter()
            .filter_map(|qa| {
                let is_adversarial = qa.category == 5;
                let gold_answer = if is_adversarial {
                    qa.adversarial_answer?
                } else {
                    qa.answer?
                };
                Some(QaPair {
                    question: qa.question,
                    gold_answer,
                    category: qa.category,
                    is_adversarial,
                })
            })
            .collect();

        conversations.push(Conversation {
            id,
            turns,
            qa_pairs,
        });
    }
    Ok(conversations)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY: &str = r#"[
      {
        "sample_id": "conv-x",
        "conversation": {
          "speaker_a": "Ann", "speaker_b": "Bo",
          "session_2_date_time": "12:05 am on 9 May, 2023",
          "session_2": [{"speaker": "Bo", "dia_id": "D2:1", "text": "later"}],
          "session_1_date_time": "1:56 pm on 8 May, 2023",
          "session_1": [
            {"speaker": "Ann", "dia_id": "D1:1", "text": "hello"},
            {"speaker": "Bo", "dia_id": "D1:2", "text": "", "blip_caption": "a dog", "query": "dog"},
            {"speaker": "Ann", "dia_id": "D1:3", "text": ""}
          ],
          "session_3": [{"speaker": "Ann", "dia_id": "D3:1", "text": "undated"}]
        },
        "observation": {}, "session_summary": {}, "event_summary": {},
        "qa": [
          {"question": "q1", "answer": "a1", "evidence": [], "category": 4},
          {"question": "q2", "answer": 2022, "evidence": [], "category": 2},
          {"question": "q3", "adversarial_answer": "wrong", "evidence": [], "category": 5},
          {"question": "q4", "evidence": [], "category": 1}
        ]
      },
      {"conversation": {}, "qa": []}
    ]"#;

    #[test]
    fn session_dates_parse_as_utc() {
        // 2023-05-08 13:56 UTC
        assert_eq!(
            parse_session_date("1:56 pm on 8 May, 2023"),
            Some(1_683_554_160_000)
        );
        assert_eq!(
            parse_session_date("12:05 am on 9 May, 2023"),
            Some(1_683_590_700_000)
        );
        assert_eq!(
            parse_session_date("12:30 pm on 1 January, 2020"),
            Some(1_577_881_800_000)
        );
        assert_eq!(parse_session_date("13:00 on 1 May, 2023"), None);
        assert_eq!(parse_session_date("1:00 pm on 1 Mai, 2023"), None);
    }

    #[test]
    fn tiny_file_loads_in_session_order() {
        let convs = parse(TINY, 7).unwrap();
        assert_eq!(convs.len(), 2);
        let c = &convs[0];
        assert_eq!(c.id, "conv-x");
        let texts: Vec<_> = c.turns.iter().map(|t| t.text.as_str()).collect();
        // The empty turn without a caption is dropped; sessions sort by number.
        assert_eq!(texts, ["hello", "", "later", "undated"]);
        assert_eq!(c.turns[0].timestamp_ms, 1_683_554_160_000);
        assert_eq!(c.turns[1].timestamp_ms, 1_683_554_161_000);
        assert_eq!(c.turns[1].blip_caption.as_deref(), Some("a dog"));
        assert_eq!(c.turns[1].image_query.as_deref(), Some("dog"));
        assert_eq!(c.turns[2].timestamp_ms, 1_683_590_700_000);
        assert_eq!(c.turns[3].timestamp_ms, 7, "an undated session takes now");
        let golds: Vec<_> = c.qa_pairs.iter().map(|q| q.gold_answer.as_str()).collect();
        assert_eq!(
            golds,
            ["a1", "2022", "wrong"],
            "a question without a gold answer is dropped"
        );
        assert!(c.qa_pairs[2].is_adversarial);
        assert_eq!(category_name(c.qa_pairs[1].category), "temporal");
        assert_eq!(convs[1].id, "conv_1");
    }

    /// With `LOCOMO_DATA` set to the real `locomo10.json`, its shape matches the published run.
    #[test]
    fn real_dataset_structure() {
        let Ok(path) = std::env::var("LOCOMO_DATA") else {
            return;
        };
        let convs = load(Path::new(&path), 0).unwrap();
        assert_eq!(convs.len(), 10);
        assert_eq!(convs.iter().map(|c| c.turns.len()).sum::<usize>(), 5_882);
        assert_eq!(convs.iter().map(|c| c.qa_pairs.len()).sum::<usize>(), 1_986);
        let adversarial = convs
            .iter()
            .flat_map(|c| &c.qa_pairs)
            .filter(|q| q.is_adversarial)
            .count();
        assert_eq!(adversarial, 446);
        assert!(
            convs
                .iter()
                .flat_map(|c| &c.turns)
                .all(|t| t.timestamp_ms > 0)
        );
    }
}

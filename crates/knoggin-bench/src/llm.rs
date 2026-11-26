// SPDX-License-Identifier: AGPL-3.0-only
//! The model client and the prompts, ported from recalld's `src/bench/claude.rs`: only
//! its OpenAI-compatible client, on `reqwest`. The prompts are in `prompts/`, unchanged.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::locomo::MemoryContext;

/// How the judge grades questions of categories 1 to 4 (`BENCH_JUDGE_STYLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JudgeStyle {
    /// recalld's own lenient prompt (unset).
    Recalld,
    /// Mem0's 2025 `ACCURACY_PROMPT` (`mem0`), as the published run.
    Mem0,
    /// Mem0's 2026 judge (`mem0-v3`).
    Mem0V3,
}

impl JudgeStyle {
    /// Reads `BENCH_JUDGE_STYLE`'s value; an unknown value is refused.
    pub(crate) fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value.unwrap_or("") {
            "" => Ok(Self::Recalld),
            "mem0" => Ok(Self::Mem0),
            "mem0-v3" => Ok(Self::Mem0V3),
            other => anyhow::bail!("BENCH_JUDGE_STYLE must be mem0 or mem0-v3, not {other:?}"),
        }
    }
}

/// One model behind an OpenAI-compatible address.
#[derive(Clone)]
pub(crate) struct LlmClient {
    client: reqwest::Client,
    model: String,
    base_url: String,
    api_key: Option<String>,
    judge_style: JudgeStyle,
    /// First wait before a retry; doubled each time.
    backoff: Duration,
}

impl std::fmt::Debug for LlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmClient")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) struct JudgeResult {
    pub(crate) correct: bool,
}

/// One memory the ingestion model asked to save.
#[derive(Debug, Deserialize)]
pub(crate) struct MemoryToStore {
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) full_text: Option<String>,
    #[serde(default)]
    pub(crate) entities: Vec<String>,
    #[serde(default)]
    pub(crate) topics: Vec<String>,
    #[serde(default)]
    pub(crate) emotions: Vec<String>,
    #[serde(default)]
    pub(crate) supersedes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchQuery {
    pub(crate) query: String,
    /// Written by the model but unused: Knoggin's recall has no separate keyword query.
    pub(crate) fts_query: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchParams {
    pub(crate) queries: Vec<SearchQuery>,
    pub(crate) entities: Vec<String>,
    pub(crate) topics: Vec<String>,
    pub(crate) emotions: Vec<String>,
    pub(crate) depth: u32,
    pub(crate) time_range_start: Option<i64>,
    pub(crate) time_range_end: Option<i64>,
}

#[derive(Deserialize)]
struct SearchParamsOutput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    queries: Option<Vec<SearchQueryOutput>>,
    #[serde(default)]
    fts_query: Option<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    emotions: Vec<String>,
    #[serde(default)]
    depth: Option<u32>,
    #[serde(default)]
    time_range_start: Option<i64>,
    #[serde(default)]
    time_range_end: Option<i64>,
}

#[derive(Deserialize)]
struct SearchQueryOutput {
    query: String,
    #[serde(default)]
    fts_query: Option<String>,
}

#[derive(Deserialize)]
struct FollowupOutput {
    #[serde(default)]
    sufficient: bool,
    #[serde(default)]
    queries: Option<Vec<SearchQueryOutput>>,
    #[serde(default)]
    entities: Vec<String>,
}

#[derive(Deserialize)]
struct ProcessTurnOutput {
    #[serde(default)]
    store_memory: Vec<MemoryToStore>,
}

#[derive(Serialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessageContent,
}

#[derive(Deserialize)]
struct OpenAiMessageContent {
    content: String,
}

#[derive(Deserialize)]
struct JudgeOutput {
    #[serde(default)]
    label: String,
}

/// A model's answer without a Markdown code fence around it.
fn strip_fence(response: &str) -> &str {
    response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

/// The first `max` bytes of `text`, cut at a character boundary.
fn head(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl LlmClient {
    /// `base_url` is the server's address without `/v1/chat/completions`; `api_key` is
    /// sent as a bearer token.
    pub(crate) fn new(
        model: &str,
        base_url: &str,
        api_key: Option<String>,
        judge_style: JudgeStyle,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            client,
            model: model.to_owned(),
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            judge_style,
            backoff: Duration::from_secs(2),
        })
    }

    #[cfg(test)]
    pub(crate) fn without_backoff(mut self) -> Self {
        self.backoff = Duration::ZERO;
        self
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn judge_style(&self) -> JudgeStyle {
        self.judge_style
    }

    /// The answering call. The prompt still describes graph neighbours; Knoggin's
    /// default recall gives none, so none are ever shown.
    pub(crate) async fn generate_answer(
        &self,
        question: &str,
        memories: &[MemoryContext],
    ) -> anyhow::Result<String> {
        let user = fill(
            prompts::ANSWER_USER,
            &[
                // LoCoMo questions carry no date of their own.
                ("date_context", ""),
                (
                    "memory_context",
                    &crate::locomo::render_memory_context(memories),
                ),
                ("question", question),
                ("reasoning_hint", prompts::ANSWER_REASONING_HINT),
            ],
        );
        self.call(prompts::ANSWER_SYSTEM, &user, 4096).await
    }

    /// The judging call: recalld's own rule, or Mem0's judge for categories 1 to 4.
    pub(crate) async fn judge_answer(
        &self,
        question: &str,
        gold_answer: &str,
        generated_answer: &str,
        is_adversarial: bool,
    ) -> anyhow::Result<JudgeResult> {
        // BENCH_JUDGE_STYLE=mem0 grades non-adversarial questions with Mem0's
        // published LoCoMo judge prompt (the de-facto shared protocol) for a
        // comparable number. Adversarial questions keep recalld's rule.
        // BENCH_JUDGE_STYLE=mem0-v3 uses Mem0's current, looser judge instead.
        let mem0_prompt = match self.judge_style {
            JudgeStyle::Mem0 => Some((prompts::JUDGE_MEM0_SYSTEM, prompts::JUDGE_MEM0)),
            JudgeStyle::Mem0V3 => Some((prompts::JUDGE_MEM0_V3_SYSTEM, prompts::JUDGE_MEM0_V3)),
            JudgeStyle::Recalld => None,
        };
        if let (false, Some((system, template))) = (is_adversarial, mem0_prompt) {
            let user = fill(
                template,
                &[
                    ("question", question),
                    ("gold_answer", gold_answer),
                    ("generated_answer", generated_answer),
                ],
            );
            let response = self.call(system, &user, 1024).await?;
            return Ok(JudgeResult {
                correct: mem0_label(&response).eq_ignore_ascii_case("CORRECT"),
            });
        }

        let template = if is_adversarial {
            prompts::JUDGE_ADVERSARIAL
        } else {
            prompts::JUDGE_RECALLD
        };
        let user = fill(
            template,
            &[
                ("question", question),
                ("gold_answer", gold_answer),
                ("generated_answer", generated_answer),
            ],
        );
        let response = self.call(prompts::JUDGE_SYSTEM, &user, 1024).await?;
        Ok(JudgeResult {
            correct: recalld_verdict(&response),
        })
    }

    /// The ingestion call: which memories to save for the latest turn. A reply that is not
    /// the expected JSON saves nothing.
    pub(crate) async fn process_turn(
        &self,
        recent_turns: &[String],
        current_turn: &str,
        recent_memories: &[(String, String)],
        turn_timestamp_ms: i64,
    ) -> anyhow::Result<Vec<MemoryToStore>> {
        let mut user_msg = String::new();
        user_msg.push_str(&format!(
            "Conversation timestamp: {}\n\n",
            crate::locomo::format_timestamp(turn_timestamp_ms)
        ));

        if !recent_memories.is_empty() {
            user_msg.push_str(prompts::INGEST_RECENT_MEMORIES);
            for (id, summary) in recent_memories {
                user_msg.push_str(&format!("  [{}] {}\n", id, summary));
            }
            user_msg.push('\n');
        }

        if !recent_turns.is_empty() {
            user_msg.push_str("Recent context:\n");
            for t in recent_turns {
                user_msg.push_str(t);
                user_msg.push('\n');
            }
            user_msg.push_str("\nLatest turn:\n");
        }
        user_msg.push_str(current_turn);

        let response = self.call(prompts::INGEST_SYSTEM, &user_msg, 2048).await?;
        Ok(
            serde_json::from_str::<ProcessTurnOutput>(strip_fence(&response))
                .map(|parsed| parsed.store_memory)
                .unwrap_or_default(),
        )
    }

    /// The search step's first call: one to three queries and their filters.
    pub(crate) async fn construct_search_query(
        &self,
        question: &str,
    ) -> anyhow::Result<SearchParams> {
        let user_msg = format!("Question: {question}");
        let response = self.call(prompts::SEARCH_SYSTEM, &user_msg, 1024).await?;
        Ok(parse_search_params(question, &response))
    }

    /// The search step's follow-up call: more queries, or `None` when the model says the
    /// memories found suffice.
    pub(crate) async fn construct_followup_query(
        &self,
        question: &str,
        retrieved_summaries: &str,
    ) -> anyhow::Result<Option<SearchParams>> {
        let user_msg = fill(
            prompts::FOLLOWUP_USER,
            &[
                ("question", question),
                ("retrieved_summaries", retrieved_summaries),
            ],
        );
        let response = self.call(prompts::FOLLOWUP_SYSTEM, &user_msg, 512).await?;
        Ok(parse_followup(&response))
    }

    /// One chat completion: the system prompt, one user message, temperature 0. Three
    /// tries; a 429, a 5xx, a broken body or a failed request is tried again.
    async fn call(&self, system: &str, user: &str, max_tokens: u32) -> anyhow::Result<String> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let body = OpenAiRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_owned(),
                    content: system.to_owned(),
                },
                ChatMessage {
                    role: "user".to_owned(),
                    content: user.to_owned(),
                },
            ],
            max_tokens,
            temperature: 0.0,
        };
        let mut last_err = String::new();
        for attempt in 0..3_u32 {
            if attempt > 0 {
                tokio::time::sleep(self.backoff * 2_u32.pow(attempt - 1)).await;
            }
            let mut req = self.client.post(&url).json(&body);
            if let Some(key) = &self.api_key {
                req = req.bearer_auth(key);
            }
            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    last_err = format!("request error: {e}");
                    continue;
                }
            };
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                match serde_json::from_str::<OpenAiResponse>(&text) {
                    Ok(r) => {
                        return Ok(r
                            .choices
                            .into_iter()
                            .map(|c| c.message.content)
                            .collect::<String>());
                    }
                    Err(e) => last_err = format!("parse error: {e}, body: {}", head(&text, 200)),
                }
            } else if status.as_u16() == 429 || status.is_server_error() {
                last_err = format!("HTTP {status}: {}", head(&text, 200));
            } else {
                anyhow::bail!("HTTP {status}: {}", head(&text, 500));
            }
        }
        anyhow::bail!("failed after 3 attempts: {last_err}")
    }
}

/// Reads the search parameters; a reply that is not the expected JSON searches for the
/// question itself.
fn parse_search_params(question: &str, response: &str) -> SearchParams {
    let fallback = || SearchParams {
        queries: vec![SearchQuery {
            query: question.to_owned(),
            fts_query: None,
        }],
        entities: Vec::new(),
        topics: Vec::new(),
        emotions: Vec::new(),
        depth: 2,
        time_range_start: None,
        time_range_end: None,
    };
    let Ok(parsed) = serde_json::from_str::<SearchParamsOutput>(strip_fence(response)) else {
        return fallback();
    };
    let queries = if let Some(qs) = parsed.queries {
        qs.into_iter()
            .map(|q| SearchQuery {
                query: q.query,
                fts_query: q.fts_query,
            })
            .collect()
    } else if let Some(q) = parsed.query {
        vec![SearchQuery {
            query: q,
            fts_query: parsed.fts_query,
        }]
    } else {
        fallback().queries
    };
    SearchParams {
        queries,
        entities: parsed.entities,
        topics: parsed.topics,
        emotions: parsed.emotions,
        depth: parsed.depth.unwrap_or(2).min(2),
        time_range_start: parsed.time_range_start,
        time_range_end: parsed.time_range_end,
    }
}

/// Reads the follow-up reply: `None` when the model says the memories suffice or the
/// reply is not the expected JSON.
fn parse_followup(response: &str) -> Option<SearchParams> {
    let parsed = serde_json::from_str::<FollowupOutput>(strip_fence(response)).ok()?;
    if parsed.sufficient {
        return None;
    }
    let queries = parsed
        .queries?
        .into_iter()
        .map(|q| SearchQuery {
            query: q.query,
            fts_query: q.fts_query,
        })
        .collect();
    Some(SearchParams {
        queries,
        entities: parsed.entities,
        topics: Vec::new(),
        emotions: Vec::new(),
        depth: 2,
        time_range_start: None,
        time_range_end: None,
    })
}

/// recalld's own judge reply: the JSON `label`, or, when the reply is not JSON, a quoted
/// `"CORRECT"` without a quoted `"WRONG"`.
fn recalld_verdict(response: &str) -> bool {
    let cleaned = strip_fence(response);
    match serde_json::from_str::<JudgeOutput>(cleaned) {
        Ok(parsed) => parsed.label.eq_ignore_ascii_case("CORRECT"),
        Err(_) => {
            let upper = cleaned.to_uppercase();
            upper.contains("\"CORRECT\"") && !upper.contains("\"WRONG\"")
        }
    }
}

/// Like Mem0's `extract_json`: the `label` of the last JSON object in the reply.
fn mem0_label(response: &str) -> String {
    response
        .rfind('{')
        .and_then(|start| {
            let end = response[start..].find('}')? + start;
            serde_json::from_str::<serde_json::Value>(&response[start..=end]).ok()
        })
        .and_then(|v| v.get("label").and_then(|l| l.as_str()).map(str::to_owned))
        .unwrap_or_default()
}

/// The prompts, copied unchanged from recalld's `src/bench/claude.rs` at the benchmark
/// commit (Mem0's two judges are verbatim copies there too). Placeholders are `{name}`.
pub(crate) mod prompts {
    pub(crate) const ANSWER_SYSTEM: &str = include_str!("../prompts/answer_system.txt");
    pub(crate) const ANSWER_USER: &str = include_str!("../prompts/answer_user.txt");
    pub(crate) const ANSWER_REASONING_HINT: &str =
        include_str!("../prompts/answer_reasoning_hint.txt");
    pub(crate) const INGEST_SYSTEM: &str = include_str!("../prompts/ingest_system.txt");
    pub(crate) const INGEST_RECENT_MEMORIES: &str =
        include_str!("../prompts/ingest_recent_memories.txt");
    pub(crate) const SEARCH_SYSTEM: &str = include_str!("../prompts/search_system.txt");
    pub(crate) const FOLLOWUP_SYSTEM: &str = include_str!("../prompts/followup_system.txt");
    pub(crate) const FOLLOWUP_USER: &str = include_str!("../prompts/followup_user.txt");
    pub(crate) const JUDGE_SYSTEM: &str = include_str!("../prompts/judge_system.txt");
    pub(crate) const JUDGE_RECALLD: &str = include_str!("../prompts/judge_recalld.txt");
    pub(crate) const JUDGE_ADVERSARIAL: &str = include_str!("../prompts/judge_adversarial.txt");
    /// Mem0's 2025 `ACCURACY_PROMPT` (mem0ai/mem0@aae5989e78) and its system line.
    pub(crate) const JUDGE_MEM0_SYSTEM: &str = include_str!("../prompts/judge_mem0_system.txt");
    pub(crate) const JUDGE_MEM0: &str = include_str!("../prompts/judge_mem0.txt");
    /// Mem0's 2026 judge (mem0ai/memory-benchmarks@edcd6f1d42, without evidence).
    pub(crate) const JUDGE_MEM0_V3_SYSTEM: &str =
        include_str!("../prompts/judge_mem0_v3_system.txt");
    pub(crate) const JUDGE_MEM0_V3: &str = include_str!("../prompts/judge_mem0_v3.txt");
}

/// `template` with each `{name}` of `values` replaced, in one pass, so text put in is
/// never filled again. Any other brace stays as it is.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let hit = values.iter().find(|(name, _)| {
            after
                .strip_prefix(name)
                .is_some_and(|tail| tail.starts_with('}'))
        });
        if let Some((name, value)) = hit {
            out.push_str(value);
            rest = &after[name.len() + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn reply(content: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "x", "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
    }

    fn client(server: &MockServer, key: Option<&str>, style: JudgeStyle) -> LlmClient {
        LlmClient::new(
            "qwen3-235b",
            &format!("{}/", server.uri()),
            key.map(str::to_owned),
            style,
        )
        .unwrap()
        .without_backoff()
    }

    #[tokio::test]
    async fn sends_bearer_key_and_openai_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_partial_json(serde_json::json!({
                "model": "qwen3-235b",
                "max_tokens": 4096,
                "temperature": 0.0,
            })))
            .respond_with(reply("Paris."))
            .expect(1)
            .mount(&server)
            .await;
        let llm = client(&server, Some("sk-test"), JudgeStyle::Recalld);
        let answer = llm.generate_answer("Where?", &[]).await.unwrap();
        assert_eq!(answer, "Paris.");

        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        let user = messages[1]["content"].as_str().unwrap();
        assert!(user.starts_with("Retrieved memories:\n"));
        assert!(user.contains("Question: Where?"));
        assert!(user.ends_with("Answer:"));
    }

    #[tokio::test]
    async fn no_key_sends_no_authorization() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply("{\"store_memory\": [{\"summary\": \"Ann adopted a dog\", \"entities\": [\"Ann\"], \"supersedes\": \"x\"}]}"))
            .mount(&server)
            .await;
        let llm = client(&server, None, JudgeStyle::Recalld);
        let memories = llm
            .process_turn(&[], "Ann: I adopted a dog", &[], 1_683_554_160_000)
            .await
            .unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].summary, "Ann adopted a dog");
        assert_eq!(memories[0].supersedes.as_deref(), Some("x"));
        let requests = server.received_requests().await.unwrap();
        assert!(!requests[0].headers.contains_key("authorization"));
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["max_tokens"], 2048);
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.starts_with("Conversation timestamp: 2023-05-08 13:56:00 UTC\n\n"));
    }

    #[tokio::test]
    async fn retries_a_server_error_but_not_a_client_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(reply("{\"label\": \"CORRECT\"}"))
            .mount(&server)
            .await;
        let llm = client(&server, None, JudgeStyle::Mem0);
        let verdict = llm.judge_answer("q", "gold", "gen", false).await.unwrap();
        assert!(verdict.correct);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad model"))
            .expect(1)
            .mount(&server)
            .await;
        let err = client(&server, None, JudgeStyle::Recalld)
            .judge_answer("q", "gold", "gen", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn mem0_style_uses_mem0_prompt_except_for_adversarial() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply("{\"label\": \"WRONG\"}"))
            .mount(&server)
            .await;
        let llm = client(&server, None, JudgeStyle::Mem0);
        llm.judge_answer("Q1", "G1", "A1", false).await.unwrap();
        llm.judge_answer("Q2", "G2", "A2", true).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let system = |i: usize| {
            let body: serde_json::Value = serde_json::from_slice(&requests[i].body).unwrap();
            (
                body["messages"][0]["content"].as_str().unwrap().to_owned(),
                body["messages"][1]["content"].as_str().unwrap().to_owned(),
            )
        };
        let (s0, u0) = system(0);
        assert_eq!(s0, "Output ONLY the JSON object.");
        assert!(u0.contains("Question: Q1") && u0.contains("Gold answer: G1"));
        assert!(u0.contains("Generated answer: A1"));
        let (s1, u1) = system(1);
        assert!(s1.starts_with("You are evaluating conversational AI memory recall."));
        assert!(u1.starts_with("This is an ADVERSARIAL question"));
    }

    #[test]
    fn judge_style_parses() {
        assert_eq!(JudgeStyle::parse(None).unwrap(), JudgeStyle::Recalld);
        assert_eq!(JudgeStyle::parse(Some("mem0")).unwrap(), JudgeStyle::Mem0);
        assert_eq!(
            JudgeStyle::parse(Some("mem0-v3")).unwrap(),
            JudgeStyle::Mem0V3
        );
        assert!(JudgeStyle::parse(Some("MEM0")).is_err());
    }

    #[test]
    fn search_params_parse() {
        let p = parse_search_params(
            "q?",
            "```json\n{\"queries\": [{\"query\": \"a\", \"fts_query\": \"k\"}, {\"query\": \"b\"}], \
             \"entities\": [\"Ann\"], \"topics\": [\"pets\"], \"depth\": 3, \
             \"time_range_start\": 1, \"time_range_end\": 2}\n```",
        );
        assert_eq!(p.queries.len(), 2);
        assert_eq!(p.queries[0].fts_query.as_deref(), Some("k"));
        assert_eq!(p.entities, ["Ann"]);
        assert_eq!(p.topics, ["pets"]);
        assert_eq!(p.depth, 2, "depth is capped at 2");
        assert_eq!((p.time_range_start, p.time_range_end), (Some(1), Some(2)));

        let single = parse_search_params("q?", "{\"query\": \"one\", \"fts_query\": \"k\"}");
        assert_eq!(
            single.queries,
            [SearchQuery {
                query: "one".into(),
                fts_query: Some("k".into())
            }]
        );
        assert_eq!(single.depth, 2);

        let empty = parse_search_params("q?", "{}");
        assert_eq!(empty.queries[0].query, "q?");

        let garbage = parse_search_params("q?", "I think you should search for dogs");
        assert_eq!(
            garbage.queries,
            [SearchQuery {
                query: "q?".into(),
                fts_query: None
            }]
        );
        assert!(garbage.entities.is_empty() && garbage.time_range_start.is_none());
    }

    #[test]
    fn followup_parse() {
        assert_eq!(parse_followup("{\"sufficient\": true}"), None);
        assert_eq!(parse_followup("{\"sufficient\": false}"), None);
        assert_eq!(parse_followup("nonsense"), None);
        let p = parse_followup(
            "{\"sufficient\": false, \"queries\": [{\"query\": \"x\"}], \"entities\": [\"Bo\"]}",
        )
        .unwrap();
        assert_eq!(p.queries[0].query, "x");
        assert_eq!(p.entities, ["Bo"]);
        assert_eq!(p.depth, 2);
    }

    #[test]
    fn verdicts() {
        assert!(recalld_verdict(
            "{\"label\": \"correct\", \"reasoning\": \"ok\"}"
        ));
        assert!(!recalld_verdict("```json\n{\"label\": \"WRONG\"}\n```"));
        assert!(recalld_verdict("label: \"CORRECT\" because"));
        assert!(!recalld_verdict("\"CORRECT\" or \"WRONG\""));
        assert_eq!(
            mem0_label("thinking {x} then {\"label\": \"CORRECT\"}"),
            "CORRECT"
        );
        assert_eq!(mem0_label("no json"), "");
    }

    #[test]
    fn fill_replaces_named_placeholders_once() {
        let filled = fill(
            "Q: {question} | G: {gold_answer} | {\"label\": \"x\"} {other}",
            &[("question", "{gold_answer}"), ("gold_answer", "g")],
        );
        assert_eq!(
            filled,
            "Q: {gold_answer} | G: g | {\"label\": \"x\"} {other}"
        );
    }

    /// Each template names exactly the values the code fills in, and nothing is left over.
    #[test]
    fn prompt_templates_have_the_expected_placeholders() {
        let judge = ["question", "gold_answer", "generated_answer"];
        let cases: [(&str, &[&str]); 6] = [
            (
                prompts::ANSWER_USER,
                &[
                    "date_context",
                    "memory_context",
                    "question",
                    "reasoning_hint",
                ],
            ),
            (prompts::FOLLOWUP_USER, &["question", "retrieved_summaries"]),
            (prompts::JUDGE_RECALLD, &judge),
            (prompts::JUDGE_ADVERSARIAL, &judge),
            (prompts::JUDGE_MEM0, &judge),
            (prompts::JUDGE_MEM0_V3, &judge),
        ];
        for (template, names) in cases {
            let values: Vec<(&str, &str)> = names.iter().map(|n| (*n, "\u{1}")).collect();
            let filled = fill(template, &values);
            for name in names {
                assert!(template.contains(&format!("{{{name}}}")), "{name} missing");
                assert!(!filled.contains(&format!("{{{name}}}")));
            }
        }
        for system in [
            prompts::ANSWER_SYSTEM,
            prompts::INGEST_SYSTEM,
            prompts::SEARCH_SYSTEM,
            prompts::FOLLOWUP_SYSTEM,
            prompts::JUDGE_SYSTEM,
        ] {
            assert!(system.len() > 400, "a system prompt is loaded");
        }
    }
}

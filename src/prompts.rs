//! The benchmarking question pool. Three pieces:
//!
//! * a built-in pool of 18 curated questions across five categories (reasoning, math, coding,
//!   knowledge, instruction), three of which are the dashboard's default questions;
//! * `fetch_all`, which samples a handful of questions from each configured benchmark source
//!   (`cfg.prompts_sources`) and reports per-source failures instead of aborting;
//! * parsers for the three wire shapes the real sources use — GSM8K's newline-delimited JSON,
//!   HumanEval's gzipped JSONL, TruthfulQA's JSON array — plus a free-form
//!   `[{"category", "text"}]` array so anyone can point `LOMORA_PROMPTS_URLS` at their own set.

use serde_json::Value;
use std::io::Read;

/// One benchmark question: what it exercises and what to send to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub category: String,
    pub text: String,
}

/// `(category, text)`, ordered so the three defaults are first of their category: the reasoning
/// default opens the pool, the coding default sits at 7, the knowledge default at 11.
/// `DEFAULT_IDX` below must point at questions that genuinely belong in a default smoke test —
/// short, unambiguous, and hard to answer by accident.
pub const POOL: &[(&str, &str)] = &[
    // reasoning (0-3): trick questions a parroting model gets wrong
    ("reasoning", "A farmer has 17 sheep and all but 9 run away. How many sheep does the farmer have left?"),
    ("reasoning", "You are running a race and you pass the person in second place. What place are you in?"),
    ("reasoning", "Which weighs more, a pound of feathers or a pound of steel?"),
    ("reasoning", "Sally has 3 brothers. Each brother has 2 sisters. How many sisters does Sally have?"),
    // math (4-6): arithmetic with a trap or two steps
    ("math", "A pen and a pencil cost $1.10 together. The pen costs $1.00 more than the pencil. How much does the pencil cost?"),
    ("math", "A train travels 60 km in 45 minutes. What is its average speed in km per hour?"),
    ("math", "Natalia sold clips to 48 of her friends in April, and then she sold half as many clips in May. How many clips did Natalia sell altogether in April and May?"),
    // coding (7-10): single-function asks, easy to eyeball for correctness
    ("coding", "Write a Python function is_palindrome(s) that returns True if the string s reads the same forwards and backwards, ignoring case and non-alphanumeric characters."),
    ("coding", "Write a SQL query that returns the second-highest distinct salary from an employees table with a salary column."),
    ("coding", "Write a JavaScript function flatten(arr) that flattens a nested array of arbitrary depth into a single-level array."),
    ("coding", "Write a program that prints the numbers from 1 to 20, but for multiples of three print 'Fizz', for multiples of five print 'Buzz', and for multiples of both print 'FizzBuzz'."),
    // knowledge (11-14): one networking fact, one myth, two school facts
    ("knowledge", "What is the difference between TCP and UDP, and when would you choose one over the other?"),
    ("knowledge", "If you swallow a watermelon seed, will a watermelon grow in your stomach?"),
    ("knowledge", "Name the four largest planets in the solar system, in order of size."),
    ("knowledge", "Why does the sky appear blue during the day?"),
    // instruction (15-17): follow-the-ask tasks, no single right answer but clear failure modes
    ("instruction", "Summarize the plot of Romeo and Juliet in three sentences."),
    ("instruction", "Rewrite this sentence to be polite while keeping the meaning: 'Send me the report now.'"),
    ("instruction", "Give five tips for writing a clear git commit message."),
];

/// The pool indices the three default questions come from: one reasoning, one coding, one
/// knowledge — the same spread the fetch sources cover.
const DEFAULT_IDX: [usize; 3] = [0, 7, 11];

/// How many questions one source contributes per fetch (a source with fewer answers all of it).
pub const SAMPLE_PER_SOURCE: usize = 8;
/// The cache ceiling: 200 fetched questions is plenty and keeps the SQLite table small.
pub const MAX_FETCHED: usize = 200;

/// The built-in pool, in `POOL` order.
pub fn built_in() -> Vec<Question> {
    POOL.iter()
        .map(|(c, t)| Question { category: (*c).to_string(), text: (*t).to_string() })
        .collect()
}

/// The three default questions, in the order the dashboard's run-all button fires them.
pub fn defaults() -> Vec<String> {
    DEFAULT_IDX.iter().map(|i| POOL[*i].1.to_string()).collect()
}

/// The pool the dashboard sees: built-in first (stable order for the picker), then fetched
/// questions whose text is not already there. Deduping by text — not by (category, text) —
/// because a question the user already has, re-categorised by a different source, is still the
/// same prompt to send.
pub fn merge(builtin: Vec<Question>, fetched: Vec<Question>) -> Vec<Question> {
    let mut out = builtin;
    let mut seen: std::collections::HashSet<String> =
        out.iter().map(|q| q.text.clone()).collect();
    for q in fetched {
        if seen.insert(q.text.clone()) { out.push(q); }
    }
    out
}

/// Fetch every source, sample each one down to `SAMPLE_PER_SOURCE`, and collect per-source
/// failures instead of failing the whole run — one dead mirror must not block the healthy
/// sources. `(questions, failed (category, error))`.
pub async fn fetch_all(http: &reqwest::Client, sources: &[(String, String)])
    -> (Vec<Question>, Vec<(String, String)>)
{
    let mut got = Vec::new();
    let mut failed = Vec::new();
    for (category, url) in sources {
        match fetch_one(http, category, url).await {
            Ok(qs) => got.extend(sample(qs, SAMPLE_PER_SOURCE)),
            Err(e) => failed.push((category.clone(), e)),
        }
    }
    (got, failed)
}

/// One source: GET (30 s, matching `list_models`' per-request style), then sniff the body.
/// Every failure mode — connect error, HTTP error status, unreadable body, zero questions —
/// becomes a string so `fetch_all` can keep going.
async fn fetch_one(http: &reqwest::Client, category: &str, url: &str)
    -> Result<Vec<Question>, String>
{
    let resp = http.get(url).timeout(std::time::Duration::from_secs(30)).send().await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() { return Err(format!("HTTP {status}")); }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    parse_auto(&bytes, category)
}

/// Detect the shape from the bytes themselves: gzip magic (`1f 8b`) first, then a leading `[`
/// for a JSON array, otherwise newline-delimited JSON. HumanEval ships `.gz` under a URL that
/// never says so, so trusting the file extension or Content-Type was never an option.
pub fn parse_auto(bytes: &[u8], fallback_category: &str) -> Result<Vec<Question>, String> {
    let decompressed: std::borrow::Cow<[u8]> = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes).read_to_end(&mut out).map_err(|e| e.to_string())?;
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(bytes)
    };
    let text = String::from_utf8_lossy(&decompressed);
    let trimmed = text.trim();
    let qs = if trimmed.starts_with('[') {
        parse_array(trimmed, fallback_category)?
    } else {
        parse_jsonl(&text, fallback_category)?
    };
    if qs.is_empty() {
        return Err("no questions found in the response".to_string());
    }
    Ok(qs)
}

/// Newline-delimited JSON: one question object per line. A broken or blank line is skipped, not
/// fatal — real dataset files carry occasional duds — but a file with no readable line at all is
/// an error, because an empty import the user cannot distinguish from a successful one is worse.
fn parse_jsonl(text: &str, fallback_category: &str) -> Result<Vec<Question>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(q) = extract_text(&v, fallback_category) { out.push(q); }
    }
    Ok(out)
}

fn parse_array(text: &str, fallback_category: &str) -> Result<Vec<Question>, String> {
    let arr: Vec<Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(arr.iter().filter_map(|v| extract_text(v, fallback_category)).collect())
}

/// Pull the prompt text out of one dataset object. The key chain follows the real sources:
/// GSM8K and TruthfulQA say `question`, HumanEval says `prompt`, free-form sets say `text`.
/// A per-item `category` overrides the source's category (an object without one, or with a
/// blank one, falls back), so a mixed file can tag its own entries.
fn extract_text(v: &Value, fallback_category: &str) -> Option<Question> {
    let text = ["question", "prompt", "text"].iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|t| !t.is_empty())?;
    let category = v.get("category").and_then(Value::as_str)
        .map(str::trim).filter(|c| !c.is_empty())
        .unwrap_or(fallback_category)
        .to_string();
    Some(Question { category, text: text.to_string() })
}

/// A tiny linear congruential generator — enough randomness for "which questions did we grab",
/// with no dependency. Only the high 31 bits are consumed: the low bits of an LCG are the weak
/// ones (`next() % n` with low bits would skew short periods onto small `n`).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Seed from the clock and the pid: two fetches in the same nanosecond are impossible in
/// practice, and the pid keeps restarts from replaying the same sequence.
fn lcg_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    nanos ^ ((std::process::id() as u64) << 32)
}

/// Partial Fisher-Yates: pick `keep` items uniformly from `items`. A source with `keep` or
/// fewer items comes back unchanged (the mock in the tests relies on this for determinism).
fn sample(items: Vec<Question>, keep: usize) -> Vec<Question> {
    let n = items.len();
    if n <= keep { return items; }
    let mut items = items;
    let mut rng = Lcg(lcg_seed());
    for i in 0..keep {
        let j = i + (rng.next() as usize) % (n - i);
        items.swap(i, j);
    }
    items.truncate(keep);
    items
}

#[cfg(test)]
mod tests {
    use super::{MAX_FETCHED, POOL, Question, extract_text, merge, parse_auto, sample,
                SAMPLE_PER_SOURCE};

    fn q(category: &str, text: &str) -> Question {
        Question { category: category.to_string(), text: text.to_string() }
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    // The pool is data the UI renders blindly: every entry must be usable, the defaults must
    // point at real entries, and the category set must stay the five the picker groups by.
    #[test]
    fn the_pool_is_complete_and_the_defaults_point_at_it() {
        assert_eq!(POOL.len(), 18);
        assert!(POOL.iter().all(|(c, t)| !c.is_empty() && !t.is_empty()));
        let categories: std::collections::HashSet<&str> =
            POOL.iter().map(|(c, _)| *c).collect();
        assert_eq!(categories, ["reasoning", "math", "coding", "knowledge", "instruction"].into());
        let defaults = super::defaults();
        assert_eq!(defaults.len(), 3);
        for d in &defaults {
            assert!(POOL.iter().any(|(_, t)| t == d), "default must be in the pool: {d}");
        }
        // The defaults lead their categories (0 reasoning, 7 coding, 11 knowledge).
        assert_eq!(POOL[0].0, "reasoning");
        assert_eq!(POOL[7].0, "coding");
        assert_eq!(POOL[11].0, "knowledge");
        // Compile-time sanity: the ceiling must never drop below what one source contributes.
        const { assert!(MAX_FETCHED >= SAMPLE_PER_SOURCE); }
    }

    // One parser, three real wire shapes, exercised with the exact payloads the sources ship:
    // JSONL (GSM8K, including a broken and a blank line), a JSON array (TruthfulQA), and
    // gzipped JSONL (HumanEval).
    #[test]
    fn parse_auto_reads_jsonl_array_and_gzip() {
        let jsonl = concat!(
            r#"{"question": "m1", "answer": "a"}"#, "\n",
            r#"{ not json }"#, "\n",
            "\n",
            r#"{"question": "m2", "answer": "a"}"#, "\n",
        );
        let qs = parse_auto(jsonl.as_bytes(), "math").unwrap();
        assert_eq!(qs, vec![q("math", "m1"), q("math", "m2")]);

        let arr = r#"[{"question": "k1"}, {"question": "k2"}]"#;
        assert_eq!(parse_auto(arr.as_bytes(), "knowledge").unwrap(),
                   vec![q("knowledge", "k1"), q("knowledge", "k2")]);

        let gzipped = gz(concat!(r#"{"task_id": "HumanEval/0", "prompt": "c1"}"#, "\n",
                                 r#"{"task_id": "HumanEval/1", "prompt": "c2"}"#, "\n").as_bytes());
        assert_eq!(parse_auto(&gzipped, "coding").unwrap(),
                   vec![q("coding", "c1"), q("coding", "c2")]);
    }

    // Failure modes of the parser, each reachable from a misbehaving source:
    #[test]
    fn parse_auto_rejects_empty_or_unreadable_bodies() {
        assert_eq!(parse_auto(b"", "x").unwrap_err(), "no questions found in the response");
        assert_eq!(parse_auto(b"\n\n", "x").unwrap_err(), "no questions found in the response");
        assert_eq!(parse_auto(b"{ not json }", "x").unwrap_err(),
                   "no questions found in the response");
        assert_eq!(parse_auto(b"[]", "x").unwrap_err(), "no questions found in the response");
        // A JSON array whose elements carry none of the known text keys imports nothing.
        assert_eq!(parse_auto(br#"[{"answer": "a"}]"#, "x").unwrap_err(),
                   "no questions found in the response");
        // Truncated gzip is a decompression error, not an empty import.
        assert!(parse_auto(&[0x1f, 0x8b, 0x00], "x").is_err());
        // Malformed JSON in the array shape names the parse error, not "no questions".
        assert!(parse_auto(b"[broken", "x").unwrap_err().contains("expected"));
    }

    // The key chain: question wins over prompt wins over text.
    #[test]
    fn extract_text_prefers_question_then_prompt_then_text() {
        let all = serde_json::json!({"question": "Q", "prompt": "P", "text": "T"});
        assert_eq!(extract_text(&all, "x").unwrap().text, "Q");
        let two = serde_json::json!({"prompt": "P", "text": "T"});
        assert_eq!(extract_text(&two, "x").unwrap().text, "P");
        let one = serde_json::json!({"text": "T"});
        assert_eq!(extract_text(&one, "x").unwrap().text, "T");
    }

    // A per-item category overrides the source's; a blank one falls back to the source's.
    #[test]
    fn item_category_overrides_and_blank_falls_back() {
        let tagged = serde_json::json!({"category": "trivia", "text": "t1"});
        assert_eq!(extract_text(&tagged, "custom").unwrap().category, "trivia");
        let blank = serde_json::json!({"category": "", "text": "t2"});
        assert_eq!(extract_text(&blank, "custom").unwrap().category, "custom");
        let untagged = serde_json::json!({"text": "t3"});
        assert_eq!(extract_text(&untagged, "custom").unwrap().category, "custom");
        // Whitespace-only text is as good as absent.
        let ws = serde_json::json!({"question": "   "});
        assert!(extract_text(&ws, "x").is_none());
    }

    // The sampler: short inputs pass through untouched (the endpoint tests rely on that for
    // determinism); long inputs come back exactly `keep` strong, all distinct, all original.
    #[test]
    fn sample_passes_short_input_through_and_draws_exactly_keep_when_long() {
        let short = vec![q("m", "1"), q("m", "2"), q("m", "3")];
        assert_eq!(sample(short.clone(), SAMPLE_PER_SOURCE), short);
        let long: Vec<Question> = (0..100).map(|i| q("m", &format!("q{i}"))).collect();
        let picked = sample(long.clone(), SAMPLE_PER_SOURCE);
        assert_eq!(picked.len(), SAMPLE_PER_SOURCE);
        let texts: std::collections::HashSet<&str> =
            picked.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts.len(), SAMPLE_PER_SOURCE, "no duplicates");
        for t in &texts {
            assert!(long.iter().any(|x| x.text == *t), "every pick comes from the source");
        }
        // keep = 0 truncates to nothing rather than panicking on the loop.
        assert!(sample(vec![q("m", "1")], 0).is_empty());
    }

    // merge: fetched questions join the pool; a text the pool already has does not come twice,
    // even under a different category label.
    #[test]
    fn merge_dedupes_by_text_keeping_the_builtin_first() {
        let builtin = vec![q("math", "m1"), q("math", "m2")];
        let fetched = vec![q("weird", "m1"), q("math", "m3")];
        let merged = merge(builtin, fetched);
        assert_eq!(merged, vec![q("math", "m1"), q("math", "m2"), q("math", "m3")]);
    }
}

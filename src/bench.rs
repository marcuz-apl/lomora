/// Python's round() is round-half-to-even; f64::round is half-away-from-zero and the two
/// disagree on exact ties. Rust's float formatting is correctly rounded, so format-then-
/// parse reproduces CPython. Spec 5.1.
pub fn round_half_even(x: f64, digits: u32) -> f64 {
    match format!("{:.*}", digits as usize, x).parse::<f64>() {
        Ok(v) => v,
        Err(_) => x,
    }
}

/// Incremental SSE line scanner. reqwest hands us arbitrary byte chunks; a Python
/// `aiter_lines()` hides that a "data: " line can straddle two of them. Buffers to
/// newlines and reproduces the Python predicate exactly
/// (main.py:101 `line.startswith("data: ") and "[DONE]" not in line`).
/// `push` answers `true` only on the chunk that completes the first accepted line; every
/// later chunk answers `false`, so the route timestamps exactly one line.
#[derive(Default)]
pub struct SseScanner { buf: Vec<u8>, fired: bool }

impl SseScanner {
    pub fn push(&mut self, chunk: &[u8]) -> bool {
        if self.fired { return false; }
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.starts_with("data: ") && !line.contains("[DONE]") {
                self.fired = true;
                return true;
            }
        }
        false
    }
    pub fn finished(&self) -> bool { self.fired }
}

/// The numbers `POST /api/stress` reports, built from the finished fan-out. Kept free of Axum
/// and HTTP so every rounding rule from main.py:236-252 is pinned by unit tests.
#[derive(Clone, Debug)]
pub struct StressAggregate {
    pub success: usize, pub errors: usize, pub error_samples: Vec<String>,
    pub wall_s: f64, pub avg_latency_s: Option<f64>, pub p95_latency_s: Option<f64>,
    pub total_tokens: i64, pub agg_tok_per_s: f64, pub req_per_s: f64,
}

/// Python's aggregate block: `sum(tokens_list)`, `round(sum/len, 3)` for the average,
/// `pct(0.95)` for p95, `round(total/wall, 1)`, `round(success/wall, 2)`, and the first five
/// error messages. An empty latency list yields `None` (Python's `None`), never 0.0.
pub fn from_samples(latencies: &[f64], tokens: &[i64], errors: &[String],
                    success: usize, wall: f64) -> StressAggregate {
    let total_tokens: i64 = tokens.iter().sum();
    let mut lat = latencies.to_vec();
    let avg = if lat.is_empty() {
        None
    } else {
        Some(round_half_even(lat.iter().sum::<f64>() / lat.len() as f64, 3))
    };
    let p95 = percentile_at(&mut lat, 0.95).map(|v| round_half_even(v, 3));
    let agg = if wall > 0.0 { round_half_even(total_tokens as f64 / wall, 1) } else { 0.0 };
    let rps = if wall > 0.0 { round_half_even(success as f64 / wall, 2) } else { 0.0 };
    StressAggregate {
        success, errors: errors.len(), error_samples: errors.iter().take(5).cloned().collect(),
        wall_s: round_half_even(wall, 2), avg_latency_s: avg, p95_latency_s: p95,
        total_tokens, agg_tok_per_s: agg, req_per_s: rps,
    }
}

/// Python: s = sorted(l); idx = min(len-1, int(round(p*(len-1)))); returns None if empty.
/// The inner round is half-to-even, so this replicates the index rule rather than
/// delegating to a percentile crate. Spec 5.1.
pub fn percentile_at(sorted: &mut [f64], p: f64) -> Option<f64> {
    if sorted.is_empty() { return None; }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((sorted.len() as f64 - 1.0) * p).round_ties_even() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

#[cfg(test)]
mod tests {
    use super::round_half_even;

    // Expected values are CPython's answers, measured against the reference interpreter rather
    // than reasoned out: round(0.125, 2)=0.12, round(0.135, 2)=0.14, round(1.5)=2, round(2.5)=2,
    // round(0.5)=0, round(3.5)=4, round(1.25, 1)=1.2, round(10155.5)=10156. The implementation
    // reproduces all eight. Note 1.5 goes UP to 2 — the even neighbour of a tie is not always
    // the lower one, and `half-away-from-zero` would give the same 2, so 2.5 and 0.5 are the
    // assertions that actually separate the two rules.
    #[test]
    fn rounds_half_to_even_not_away() {
        assert_eq!(round_half_even(0.125, 2), 0.12);
        assert_eq!(round_half_even(0.135, 2), 0.14);
        assert_eq!(round_half_even(1.5, 0), 2.0);
        assert_eq!(round_half_even(2.5, 0), 2.0);
        assert_eq!(round_half_even(0.5, 0), 0.0);
        assert_eq!(round_half_even(3.5, 0), 4.0);
        assert_eq!(round_half_even(1.25, 1), 1.2);
    }

    #[test]
    fn rounds_mebibyte_ties_like_pynvml() {
        assert_eq!(round_half_even(10155.5, 0), 10156.0);
    }

    #[test]
    fn leaves_clean_values_alone() {
        assert_eq!(round_half_even(23.5, 1), 23.5);
        assert_eq!(round_half_even(1200.0, 1), 1200.0);
    }
}

#[cfg(test)]
mod sse_tests {
    use super::SseScanner;

    #[test]
    fn a_complete_first_frame_fires_on_that_chunk() {
        let mut s = SseScanner::default();
        assert!(s.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n"));
        assert!(s.finished());
    }

    #[test]
    fn frame_split_mid_payload_fires_when_the_newline_arrives() {
        let mut s = SseScanner::default();
        assert!(!s.push(b"data: {\"choices\""));
        assert!(!s.push(b": [{\"delta\": "));
        assert!(s.push(b"{\"content\":\"x\"}}]}\n"));
    }

    #[test]
    fn prefix_split_across_chunks_still_counts_as_one_line() {
        let mut s = SseScanner::default();
        assert!(!s.push(b"dat"));
        assert!(s.push(b"a: hello\n"));
        assert!(!s.push(b"data: again\n")); // already fired, stays fired-once
    }

    #[test]
    fn done_line_is_skipped_and_scanning_continues() {
        // Python: `if line.startswith("data: ") and "[DONE]" not in line` then break.
        // A [DONE] line does NOT stop the loop, it is simply not accepted.
        let mut s = SseScanner::default();
        assert!(!s.push(b"data: [DONE]\n\n"));
        assert!(!s.finished());
        assert!(s.push(b"data: late\n\n"));
    }

    #[test]
    fn non_data_lines_are_ignored() {
        let mut s = SseScanner::default();
        assert!(!s.push(b"event: message\nid: 1\n\n"));
        assert!(!s.push(b"data:ok\n\n")); // no space after colon: Python requires "data: "
        assert!(s.push(b"data: now\n\n"));
    }

    #[test]
    fn several_frames_in_one_chunk_fire_only_once() {
        let mut s = SseScanner::default();
        assert!(s.push(b"data: one\n\ndata: two\n\n"));
        assert!(!s.push(b"data: three\n\n"));
    }

    // The TTFT loop must not stop on a [DONE]-only stream: `finished()` staying false is what
    // makes the route fall through to `None` -> `"ttft_ms": null`, Python's behavior when no
    // accepted data line ever arrives (`if line.startswith("data: ") ...` never fires).
    #[test]
    fn a_stream_of_only_non_data_lines_never_fires() {
        let mut s = SseScanner::default();
        assert!(!s.push(b"data: [DONE]\n\n"));
        assert!(!s.push(b": keep-alive comment\n\n"));
        assert!(!s.push(b"\n"));
        assert!(!s.finished());
    }

    // CRLF transport (LM Studio / uvicorn split on \r\n): the line terminator must not make
    // Python's `startswith("data: ")` fail after aiter_lines() has already stripped it.
    #[test]
    fn crlf_lines_are_scanned_like_lf_lines() {
        let mut s = SseScanner::default();
        assert!(!s.push(b"data: x\r"));
        assert!(s.push(b"\n\r\n"));
    }
}

#[cfg(test)]
mod agg_tests {
    use super::from_samples;

    #[test]
    fn empty_latencies_yield_none_not_zero() {
        let a = from_samples(&[], &[], &["x".into()], 0, 1.5);
        assert!(a.avg_latency_s.is_none() && a.p95_latency_s.is_none());
        assert_eq!(a.agg_tok_per_s, 0.0);
        assert_eq!(a.req_per_s, 0.0);
        assert_eq!(a.errors, 1);
        assert_eq!(a.error_samples, ["x"]);
    }

    #[test]
    fn single_sample_is_its_own_average_and_p95() {
        let a = from_samples(&[0.4], &[7], &[], 1, 0.8);
        assert_eq!(a.avg_latency_s, Some(0.4));
        assert_eq!(a.p95_latency_s, Some(0.4));
        assert_eq!(a.total_tokens, 7);
        // 7 / 0.8 = 8.75 -> round(x, 1) is half-to-even -> 8.8; 1 / 0.8 = 1.25 at 2 digits.
        assert_eq!(a.agg_tok_per_s, 8.8);
        assert_eq!(a.req_per_s, 1.25);
    }

    #[test]
    fn p95_index_matches_the_python_rule_on_a_small_set() {
        // len 4: idx = min(3, round_ties_even(0.95*3 = 2.85...)) -> sorted[3].
        let lat = [0.1, 0.2, 0.3, 0.9];
        let a = from_samples(&lat, &[1, 1, 1, 1], &[], 4, 1.0);
        assert_eq!(a.p95_latency_s, Some(0.9));
        // 1.5000000000000002 / 4 = 0.37500000000000006, rounded (not truncated) at 3 digits.
        assert_eq!(a.avg_latency_s, Some(0.375));
    }

    #[test]
    fn error_samples_are_capped_at_five() {
        let errs: Vec<String> = (0..9).map(|i| format!("HTTP {i}")).collect();
        let a = from_samples(&[], &[], &errs, 0, 1.0);
        assert_eq!(a.errors, 9);
        assert_eq!(a.error_samples.len(), 5);
        assert_eq!(a.error_samples[0], "HTTP 0");
    }

    #[test]
    fn zero_wall_does_not_divide_by_zero() {
        let a = from_samples(&[0.1], &[10], &[], 1, 0.0);
        assert_eq!(a.agg_tok_per_s, 0.0);
        assert_eq!(a.req_per_s, 0.0);
    }

    // Python sums `tokens_list` and divides by the wall clock it measured around the gather;
    // the aggregate reports exactly the numbers it was given, rounding wall (2), avg/p95 (3),
    // throughput (1) and request rate (2) as main.py does.
    #[test]
    fn rounding_matches_the_python_rounds_per_field() {
        let a = from_samples(&[1.0, 2.0, 3.0], &[5, 5, 5], &[], 3, 2.345);
        assert_eq!(a.total_tokens, 15);
        assert_eq!(a.avg_latency_s, Some(2.0));
        assert_eq!(a.p95_latency_s, Some(3.0));
        assert_eq!(a.wall_s, 2.35);
        assert_eq!(a.agg_tok_per_s, 6.4); // 15 / 2.345 = 6.39... -> 6.4
        assert_eq!(a.req_per_s, 1.28);    // 3 / 2.345 = 1.279... -> 1.28
    }
}

#[cfg(test)]
mod percentile_tests {
    use super::percentile_at;

    // Index rule from main.py's stress path: `idx = min(len-1, int(round(p*(len-1))))`, with
    // CPython's round() being half-to-even — verified against `python` on this box, not derived.
    #[test]
    fn empty_input_has_no_percentile() {
        assert_eq!(percentile_at(&mut [], 0.5), None);
    }

    #[test]
    fn picks_the_rounded_index_of_the_sorted_values() {
        assert_eq!(percentile_at(&mut [1.0, 2.0, 3.0, 4.0, 5.0], 0.5), Some(3.0));
        assert_eq!(percentile_at(&mut [5.0, 1.0, 3.0], 0.5), Some(3.0), "sorts first");
        assert_eq!(percentile_at(&mut [1.0, 2.0], 0.5), Some(1.0),
                   "0.5*(2-1)=0.5 rounds ties to even -> index 0");
        assert_eq!(percentile_at(&mut [1.0, 2.0, 3.0, 4.0], 0.95), Some(4.0),
                   "0.95*3=2.85 rounds to 3, clamped by min(len-1, .)");
    }
}

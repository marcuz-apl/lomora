use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{FieldKind, FieldSpec};

fn default_model() -> String { "local-model".into() }
fn default_prompt() -> String { "Count from 1 to 20, one number per line.".into() }
fn default_maxtok_test() -> i64 { 100 }
fn default_maxtok_stress() -> i64 { 64 }
fn default_concurrency() -> i64 { 4 }
fn default_requests() -> i64 { 20 }

#[derive(Deserialize, Clone, Debug)]
pub struct TestReq {
    #[serde(default = "default_model")]   pub model: String,
    #[serde(default = "default_prompt")]  pub prompt: String,
    #[serde(default = "default_maxtok_test")] pub max_tokens: i64,
    #[serde(default)]                     pub base_url: Option<String>,
}

/// Response of `POST /api/test` — the dict literal at main.py:131. Declaration order IS the
/// wire order (serde_json `preserve_order` keeps struct field order), and it must stay
/// `timestamp`, `model`, `max_tokens`, `ttft_ms`, `total_s`, `tokens`, `tok_per_s`.
/// `ttft_ms` is `Option` but NOT skip-serialized: when the probe catches no accepted data
/// line Python still emits the key with value `null` (`"ttft_ms": round(...) if ttft_ms else
/// None`), and a probe that raises never produces a `TestOut` at all — the `{"error": ...}`
/// envelope owns that case.
#[derive(Serialize)]
pub struct TestOut {
    pub timestamp: String, pub model: String, pub max_tokens: i64,
    pub ttft_ms: Option<f64>, pub total_s: f64, pub tokens: i64, pub tok_per_s: f64,
}

impl TestReq {
    /// The field table `ValidationErrors::parse_json_body` type-checks before deserializing;
    /// order = pydantic's declaration order (main.py:43-47), which is also the order multiple
    /// `detail` entries come back in, constraints included.
    ///
    /// `max_tokens` carries `Field(100, ge=1, le=2048)`. The two bounds are separate pydantic
    /// rules, each captured live against `:8501`: over the top is `less_than_equal` with
    /// `ctx {"le": 2048}`, under the bottom is `greater_than_equal` with `ctx {"ge": 1}`; both
    /// echo the raw offending input. The bounds ride in the spec so they are checked in the same
    /// pass as the types — pydantic answers ONE 422 carrying every offending field, and a
    /// separate post-deserialization `validate()` could only report the first one.
    pub const FIELDS: &'static [FieldSpec] = &[
        FieldSpec::plain("model", FieldKind::Str),
        FieldSpec::plain("prompt", FieldKind::Str),
        FieldSpec::ranged("max_tokens", 1, 2048),
        FieldSpec::plain("base_url", FieldKind::OptStr),
    ];
}

/// Python's `StressReq` (main.py:55-61): the same four fields as `TestReq`, with different
/// `max_tokens` default and two bounds of its own. `Clone` because every fan-out task owns a
/// copy, and `Debug` so a rejected body can be printed by a test.
#[derive(Deserialize, Clone, Debug)]
pub struct StressReq {
    #[serde(default = "default_model")]      pub model: String,
    #[serde(default = "default_prompt")]     pub prompt: String,
    #[serde(default = "default_maxtok_stress")] pub max_tokens: i64,
    #[serde(default)]                        pub base_url: Option<String>,
    #[serde(default = "default_concurrency")] pub concurrency: i64,
    #[serde(default = "default_requests")]    pub requests: i64,
}

impl StressReq {
    /// Declaration order = pydantic's (main.py:55-61) = the order multiple `detail` entries
    /// come back in. Three fields carry inclusive bounds; all three are checked in the same
    /// pass as the types (see `FieldSpec`), so a 422 can carry every offending field at once.
    pub const FIELDS: &'static [FieldSpec] = &[
        FieldSpec::plain("model", FieldKind::Str),
        FieldSpec::plain("prompt", FieldKind::Str),
        FieldSpec::ranged("max_tokens", 1, 2048),
        FieldSpec::plain("base_url", FieldKind::OptStr),
        FieldSpec::ranged("concurrency", 1, 32),
        FieldSpec::ranged("requests", 1, 500),
    ];
}

/// Response of `POST /api/stress` — the dict literal at main.py:243-255. Declaration order IS
/// the wire order. `success` counts completed calls, `errors` the rest; the samples are capped
/// at five by `bench::from_samples`; the latency keys are `None` (serialized `null`) when not
/// one request succeeded.
#[derive(Serialize)]
pub struct StressOut {
    pub model: String, pub base_url: String, pub concurrency: i64, pub requests: i64,
    pub success: usize, pub errors: usize, pub error_samples: Vec<String>,
    pub wall_s: f64, pub avg_latency_s: Option<f64>, pub p95_latency_s: Option<f64>,
    pub total_tokens: i64, pub agg_tok_per_s: f64, pub req_per_s: f64,
}

/// Wire shape for `GET /api/profile`. Built with `json!` exactly like `hw::Snapshot::to_json`,
/// so the key order is one reviewed list, unit-testable without a database or hardware, and out
/// of a handler that must not `unwrap`. The literal order IS the JSON key order (serde_json's
/// `preserve_order` keeps insertion order), matching Python's dict at main.py:163: `name`,
/// `notes`, `cpu`, `cpu_cores_physical`, `cpu_cores_logical`, `ram_total_gb`, `os`, then the GPU
/// keys appended by `prof.update(...)`.
/// The `gpu*` keys are emitted only when they carry a value — absent, never `null`: Python adds
/// them only when pynvml initialized, and Task 6 wires the `Gpu` trait's `name`/`vram_gb`/
/// `driver`/`error` into them; until then they stay out of the response.
/// `cpu_cores_physical` is the exception: Python serializes `psutil.cpu_count(logical=False)`
/// even when it is `None`, so a null there is parity, not a violation.
#[derive(Clone, Debug)]
pub struct ProfileOut {
    pub name: String, pub notes: String, pub cpu: String,
    pub cpu_cores_physical: Option<usize>, pub cpu_cores_logical: usize,
    pub ram_total_gb: f64, pub os: String,
    pub gpu: Option<String>, pub gpu_vram_gb: Option<f64>,
    pub gpu_driver: Option<String>, pub gpu_error: Option<String>,
}

impl ProfileOut {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "name": self.name,
            "notes": self.notes,
            "cpu": self.cpu,
            "cpu_cores_physical": self.cpu_cores_physical,
            "cpu_cores_logical": self.cpu_cores_logical,
            "ram_total_gb": self.ram_total_gb,
            "os": self.os,
        });
        if let Some(gpu) = &self.gpu { v["gpu"] = json!(gpu); }
        if let Some(gib) = self.gpu_vram_gb { v["gpu_vram_gb"] = json!(gib); }
        if let Some(driver) = &self.gpu_driver { v["gpu_driver"] = json!(driver); }
        if let Some(err) = &self.gpu_error { v["gpu_error"] = json!(err); }
        v
    }
}

/// Python's `ProfileReq` (main.py:185): `name`/`notes` both default to `None`, and
/// `set_profile` only overwrites a stored field when the incoming value is not `None`.
/// serde maps both "key absent" and "key present but null" to `None`, which is exactly
/// Python's behavior: either one leaves the stored value unchanged.
#[derive(Deserialize, Default)]
pub struct ProfileReq { pub name: Option<String>, pub notes: Option<String> }

impl ProfileReq {
    /// Both fields are `Optional[str]` (main.py:186-187): a non-string, non-null value is a
    /// pydantic `string_type` error — captured live (`{"name":5}` on `:8501` answers
    /// `{"type":"string_type","loc":["body","name"],"msg":"Input should be a valid
    /// string","input":5}`), while `null` stays legal and means "leave unchanged".
    pub const FIELDS: &'static [FieldSpec] = &[
        FieldSpec::plain("name", FieldKind::OptStr),
        FieldSpec::plain("notes", FieldKind::OptStr),
    ];
}

#[cfg(test)]
mod tests {
    use super::{ProfileOut, TestOut, TestReq};
    use serde_json::json;

    fn profile_with_gpu(gpu: Option<&str>, vram: Option<f64>, driver: Option<&str>,
                        err: Option<&str>) -> ProfileOut {
        ProfileOut { name: "My workstation".into(), notes: String::new(),
                     cpu: "Xeon".into(), cpu_cores_physical: Some(24), cpu_cores_logical: 48,
                     ram_total_gb: 223.66, os: "windows 11".into(),
                     gpu: gpu.map(str::to_string), gpu_vram_gb: vram,
                     gpu_driver: driver.map(str::to_string), gpu_error: err.map(str::to_string) }
    }

    fn keys(v: &serde_json::Value) -> Vec<&str> {
        v.as_object().expect("profile must be a JSON object")
            .keys().map(String::as_str).collect()
    }

    // `to_json` is a hand-written literal list now, so the order the integration test checks
    // end-to-end is checked here too, without a database, a sampler or a server.
    #[test]
    fn profile_keys_are_pythons_dict_order() {
        assert_eq!(keys(&profile_with_gpu(None, None, None, None).to_json()),
                   vec!["name", "notes", "cpu", "cpu_cores_physical", "cpu_cores_logical",
                        "ram_total_gb", "os"]);
    }

    // The binding constraint: keys Python omits are omitted, never `null`. Task 6 fills these
    // from the `Gpu` trait, and the four keys land after `os` in Python's `prof.update` order.
    #[test]
    fn gpu_keys_append_in_python_order_and_absent_ones_stay_out() {
        let v = profile_with_gpu(Some("Quadro P5000"), Some(16.0), Some("582.67"), None).to_json();
        assert_eq!(keys(&v), vec!["name", "notes", "cpu", "cpu_cores_physical",
                                  "cpu_cores_logical", "ram_total_gb", "os",
                                  "gpu", "gpu_vram_gb", "gpu_driver"]);
        let only_error = profile_with_gpu(None, None, None, Some("nvml down")).to_json();
        assert_eq!(keys(&only_error).last(), Some(&"gpu_error"));
        assert!(only_error.get("gpu").is_none(), "absent, not null");
    }

    // `psutil.cpu_count(logical=False)` is serialized even when it is `None`, so a null here is
    // parity rather than the violation the gpu keys would be.
    #[test]
    fn unknown_physical_core_count_is_null_not_dropped() {
        let mut p = profile_with_gpu(None, None, None, None);
        p.cpu_cores_physical = None;
        let v = p.to_json();
        assert!(v.get("cpu_cores_physical").is_some());
        assert!(v["cpu_cores_physical"].is_null());
    }

    // The /api/test result order is load-bearing (main.py:131-139 and the binding constraint);
    // the derive emits declaration order, so pin the declaration here without a server.
    #[test]
    fn test_out_keys_are_pythons_dict_order() {
        let v = serde_json::to_value(TestOut {
            timestamp: "09:41:03".into(), model: "m".into(), max_tokens: 100,
            ttft_ms: Some(41.5), total_s: 1.25, tokens: 20, tok_per_s: 16.0,
        }).unwrap();
        assert_eq!(keys(&v), vec!["timestamp", "model", "max_tokens", "ttft_ms",
                                  "total_s", "tokens", "tok_per_s"]);
    }

    // A probe that raised never reaches TestOut (the {"error": ...} envelope owns that case),
    // but a probe that finished without an accepted data line lands as `ttft_ms: null` — the
    // key stays present, exactly like Python's `"ttft_ms": round(x, 1) if x else None`.
    #[test]
    fn absent_ttft_is_a_null_key_not_a_dropped_one() {
        let v = serde_json::to_value(TestOut {
            timestamp: "09:41:03".into(), model: "m".into(), max_tokens: 100,
            ttft_ms: None, total_s: 1.25, tokens: 20, tok_per_s: 16.0,
        }).unwrap();
        assert!(v.get("ttft_ms").is_some(), "Python emits the key with value null");
        assert!(v["ttft_ms"].is_null());
        assert_eq!(keys(&v).len(), 7);
    }

    // The Field(ge=1, le=2048) split, shaped exactly like the live bodies captured from
    // `:8501`: separate rule, message and ctx per bound, raw input echoed — and checked in the
    // same pass as the types, so one 422 can carry both kinds of entry.
    #[test]
    fn max_tokens_range_validation_matches_pydantic() {
        use crate::error::ValidationErrors;
        let parse = |body: &str| {
            ValidationErrors::parse_json_body::<TestReq>(body.as_bytes(), TestReq::FIELDS)
        };
        assert!(parse(r#"{"max_tokens":1}"#).is_ok());
        assert!(parse(r#"{"max_tokens":2048}"#).is_ok());
        let err = parse(r#"{"max_tokens":5000}"#).unwrap_err();
        assert_eq!(err.0["detail"][0], json!({"type": "less_than_equal",
            "loc": ["body", "max_tokens"],
            "msg": "Input should be less than or equal to 2048", "input": 5000,
            "ctx": {"le": 2048}}));
        let err = parse(r#"{"max_tokens":0}"#).unwrap_err();
        assert_eq!(err.0["detail"][0], json!({"type": "greater_than_equal",
            "loc": ["body", "max_tokens"],
            "msg": "Input should be greater than or equal to 1",
            "input": 0, "ctx": {"ge": 1}}));
        // Type error on `model` and constraint error on `max_tokens` arrive together, in
        // declaration order — measured live on `:8501`, and impossible for a validate() that
        // runs only after the struct parsed.
        let err = parse(r#"{"model":5,"max_tokens":5000}"#).unwrap_err();
        assert_eq!(err.0["detail"], json!([
            {"type": "string_type", "loc": ["body", "model"],
             "msg": "Input should be a valid string", "input": 5},
            {"type": "less_than_equal", "loc": ["body", "max_tokens"],
             "msg": "Input should be less than or equal to 2048", "input": 5000,
             "ctx": {"le": 2048}}]));
    }
}

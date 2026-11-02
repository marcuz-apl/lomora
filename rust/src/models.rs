use serde::{Deserialize, Serialize};

fn default_model() -> String { "local-model".into() }
fn default_prompt() -> String { "Count from 1 to 20, one number per line.".into() }
fn default_maxtok_test() -> i64 { 100 }

#[derive(Deserialize, Clone)]
pub struct TestReq {
    #[serde(default = "default_model")]   pub model: String,
    #[serde(default = "default_prompt")]  pub prompt: String,
    #[serde(default = "default_maxtok_test")] pub max_tokens: i64,
    #[serde(default)]                     pub base_url: Option<String>,
}

/// Wire shape for `GET /api/profile`. Declaration order IS the JSON key order (serde derives
/// field order, serde_json's `preserve_order` keeps it), matching Python's dict at
/// main.py:163: `name`, `notes`, `cpu`, `cpu_cores_physical`, `cpu_cores_logical`,
/// `ram_total_gb`, `os`, then the GPU keys appended by `prof.update(...)`.
/// The `gpu*` keys are `skip_serializing_if` absent, not `null`: Python only emits them when
/// pynvml initialized, and Task 6 wires the `Gpu` trait's `name`/`vram_gb`/`driver` into
/// them — until then they must stay out of the response: omitting the keys is correct, a
/// `null` placeholder would violate the binding constraint above.
#[derive(Serialize)]
pub struct ProfileOut {
    pub name: String, pub notes: String, pub cpu: String,
    pub cpu_cores_physical: Option<usize>, pub cpu_cores_logical: usize,
    pub ram_total_gb: f64, pub os: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub gpu: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub gpu_vram_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub gpu_driver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub gpu_error: Option<String>,
}

/// Python's `ProfileReq` (main.py:185): `name`/`notes` both default to `None`, and
/// `set_profile` only overwrites a stored field when the incoming value is not `None`.
/// serde maps both "key absent" and "key present but null" to `None`, which is exactly
/// Python's behavior: either one leaves the stored value unchanged.
#[derive(Deserialize, Default)]
pub struct ProfileReq { pub name: Option<String>, pub notes: Option<String> }

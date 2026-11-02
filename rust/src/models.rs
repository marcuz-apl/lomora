use serde::Deserialize;
use serde_json::{Value, json};

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

#[cfg(test)]
mod tests {
    use super::ProfileOut;

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
}

mod support;
use support::spawn_app;
use serde_json::Value;

#[tokio::test]
async fn hardware_reports_the_four_base_keys_first() {
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/hardware")).await.unwrap().json().await.unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert!(keys.starts_with(&["cpu_percent", "ram_percent", "ram_used_gb", "ram_total_gb"]), "{keys:?}");
    assert_eq!(keys.last(), Some(&"gpu_status"), "the status key closes the object: {keys:?}");
    assert!(v["ram_total_gb"].as_f64().unwrap() > 0.5, "ram_total_gb looks wrong: {}", v["ram_total_gb"]);
    assert!(v["ram_used_gb"].as_f64().unwrap() <= v["ram_total_gb"].as_f64().unwrap());
    assert!(v["cpu_percent"].as_f64().unwrap() <= 100.0);
}

// The dashboard's driver hint reads this, so it must always answer a known value and agree
// with whether the counters are present. The probe itself depends on the host, so tests only
// pin the vocabulary and the agreement.
#[tokio::test]
async fn gpu_status_agrees_with_the_absent_counters() {
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/hardware")).await.unwrap().json().await.unwrap();
    let status = v["gpu_status"].as_str().expect("gpu_status must be a string");
    assert!(["ok", "driver_missing", "no_nvidia", "unsupported", "unknown"].contains(&status),
            "unexpected gpu_status: {status}");
    assert_eq!(status == "ok", v.get("gpu_util").is_some(),
               "gpu_status and the GPU counters must agree: {v}");
    if cfg!(target_os = "macos") {
        assert_eq!(status, "unsupported");
    }
}

#[tokio::test]
async fn gpu_keys_are_present_or_absent_never_null() {
    let (base, _h) = spawn_app().await;
    let v: Value = reqwest::get(format!("{base}/api/hardware")).await.unwrap().json().await.unwrap();
    match v.get("gpu_util") {
        Some(u) => {
            assert!(u.as_u64().unwrap() <= 100);
            assert!(v["gpu_mem_tot"].as_u64().unwrap() >= 1024, "VRAM total in MB looks wrong");
            assert!(v["gpu_mem_mb"].as_u64().unwrap() <= v["gpu_mem_tot"].as_u64().unwrap());
        }
        None => {
            assert!(v.get("gpu_mem_mb").is_none() && v.get("gpu_temp_c").is_none());
        }
    }
}

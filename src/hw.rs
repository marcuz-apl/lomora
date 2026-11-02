//! Background hardware sampler. The only module in the crate that reads system counters:
//! `routes.rs` serves the last published snapshot instead of touching `sysinfo`/NVML itself,
//! which is what replaces Python's blocking `psutil.cpu_percent(interval=0.1)` inside its
//! async handler.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::RwLock;

/// Instantaneous GPU counters, already converted to the units Python reports (MiB, degrees C).
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuSnapshot {
    pub util: u32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub temp_c: i32,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub cpu_percent: f64,
    pub ram_percent: f64,
    pub ram_used_gb: f64,
    pub ram_total_gb: f64,
    pub gpu: Option<GpuSnapshot>,
}

impl Snapshot {
    /// Wire shape for `GET /api/hardware`, kept here so the key order is one reviewed list and
    /// unit-testable without real hardware. Key order is Python's dict insertion order: the
    /// base four first, then the GPU four appended by `d.update(...)` — and when there is no
    /// GPU those keys are *absent*, never emitted as `null`.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "cpu_percent": self.cpu_percent,
            "ram_percent": self.ram_percent,
            "ram_used_gb":   self.ram_used_gb,
            "ram_total_gb":  self.ram_total_gb,
        });
        if let Some(g) = self.gpu {
            v["gpu_util"] = json!(g.util);
            v["gpu_mem_mb"] = json!(g.mem_used_mb);
            v["gpu_mem_tot"] = json!(g.mem_total_mb);
            v["gpu_temp_c"] = json!(g.temp_c);
        }
        v
    }
}

pub trait Gpu: Send + Sync {
    /// Python's `HAS_GPU`: whether NVML was initialised at startup at all. It separates the
    /// profile endpoint's two branches — `available()` false answers `gpu: "None detected"`,
    /// true answers the card's own fields or, if any query fails, `gpu_error`
    /// (main.py:178-186).
    fn available(&self) -> bool { false }
    fn sample(&self) -> Option<GpuSnapshot>;
    fn name(&self) -> Option<String>;
    fn vram_gb(&self) -> Option<f64>;
    fn driver(&self) -> Option<String>;
    fn error(&self) -> Option<String> {
        None
    }
}

/// Used when NVML is unavailable: no NVIDIA driver, no card, or a CI box with neither.
pub struct NoGpu;
impl Gpu for NoGpu {
    fn sample(&self) -> Option<GpuSnapshot> {
        None
    }
    fn name(&self) -> Option<String> {
        None
    }
    fn vram_gb(&self) -> Option<f64> {
        None
    }
    fn driver(&self) -> Option<String> {
        None
    }
}

/// NVIDIA via NVML. Python reads index 0 only (`nvmlDeviceGetHandleByIndex(0)`), so this does
/// too even though the development machine has two cards.
pub struct NvmlGpu {
    nvml: nvml_wrapper::Nvml,
    index: u32,
}

impl NvmlGpu {
    /// `None` rather than an error: `/api/hardware` has no failure path, it just omits the GPU
    /// keys, exactly like Python's `HAS_GPU` flag.
    pub fn open(index: u32) -> Option<NvmlGpu> {
        match nvml_wrapper::Nvml::init() {
            Ok(nvml) if nvml.device_by_index(index).is_ok() => Some(NvmlGpu { nvml, index }),
            _ => None,
        }
    }

    /// Bytes -> MiB with Python's `round(x / 1024**2)` (half-to-even), not integer truncation.
    fn mib(bytes: u64) -> u64 {
        crate::bench::round_half_even(bytes as f64 / 1_048_576.0, 0) as u64
    }
}

impl Gpu for NvmlGpu {
    /// `NvmlGpu` only exists after a successful `Nvml::init()` + device lookup, which is
    /// exactly Python's `HAS_GPU = True` (main.py:18-23).
    fn available(&self) -> bool { true }

    fn sample(&self) -> Option<GpuSnapshot> {
        let dev = self.nvml.device_by_index(self.index).ok()?;
        // nvml-wrapper 0.13 renamed the accessors the plan named: `memory_usage()` is now
        // `memory_info()`, and `temperature()` now takes a sensor.
        let util = dev.utilization_rates().ok()?;
        let mem = dev.memory_info().ok()?;
        let temp = dev.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu).ok()?;
        Some(GpuSnapshot {
            util: util.gpu,
            // `memory_info()` wraps the *v2* entry point, whose `used` excludes the driver's
            // `reserved` block; pynvml's `nvmlDeviceGetMemoryInfo` is v1, which folds reserved
            // into `used`. Measured here: v2 used 10_492_792_832 + reserved 156_237_824 == v1
            // used 10_649_030_656, i.e. Python's 10156 MiB against 10007 MiB for v2 alone.
            // Adding reserved back keeps `gpu_mem_mb` byte-identical to Python. `total` is the
            // same in both versions, so `gpu_mem_tot` needs no such correction.
            mem_used_mb: Self::mib(mem.used + mem.reserved),
            mem_total_mb: Self::mib(mem.total),
            temp_c: temp as i32,
        })
    }

    fn name(&self) -> Option<String> {
        self.nvml.device_by_index(self.index).ok()?.name().ok()
    }

    fn vram_gb(&self) -> Option<f64> {
        let m = self.nvml.device_by_index(self.index).ok()?.memory_info().ok()?;
        Some(crate::bench::round_half_even(m.total as f64 / 1024f64.powi(3), 2))
    }

    fn driver(&self) -> Option<String> {
        self.nvml.sys_driver_version().ok()
    }
}

/// The published snapshot plus the task that republishes it. Cheap to clone; every request
/// handler reads it instead of sampling.
#[derive(Clone)]
pub struct Sampler(Arc<RwLock<Snapshot>>);

/// Immutable machine facts for `GET /api/profile`, captured once at startup (ruling R4).
/// Python rebuilds these with `platform`/`psutil` on every poll of the profile endpoint —
/// a per-request cost the port treats as a defect, not a behavior: parity is on values, not
/// on when they are measured. `ram_total_gb` is deliberately not here: it already lives in
/// the `Sampler`'s cached snapshot, which refreshes on its own cadence.
#[derive(Clone, Debug)]
pub struct ProfileStatic { pub cpu: String, pub cores_physical: Option<usize>,
                           pub cores_logical: usize, pub os: String }

/// Read the startup-time profile facts. `routes.rs` must never call this per request — nor
/// build a `System::new_all()` in a handler, whose `refresh_all()` walks the process table
/// and is the same class of mistake as the blocking `psutil` call this port exists to remove.
pub fn read_profile_static() -> ProfileStatic {
    let cores_logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    // Python emits `f"{platform.system()} {platform.release()}"`; shape match only,
    // spec deviation 3.
    let os = format!("{} {}", std::env::consts::OS,
                      sysinfo::System::os_version().unwrap_or_default())
        .trim().to_string();
    // sysinfo 0.40.1: `new_all()`, `physical_core_count()` and `os_version()` all return
    // `Result<_, sysinfo::Error>` (the brief's `Option<usize>` direct-bind and bare
    // `let mut sys = new_all()` forms do not compile against this version). A failed
    // `new_all()` means there are no CPU strings to read, so fall back the way Python's
    // `platform.processor() or "Unknown CPU"` would.
    let Ok(mut sys) = sysinfo::System::new_all() else {
        return ProfileStatic { cpu: "Unknown CPU".to_string(), cores_physical: None,
                               cores_logical, os };
    };
    sys.refresh_all();
    let cpu = sys.cpus().first().map(|c| c.brand().trim().to_string())
        .filter(|b| !b.is_empty()).unwrap_or_else(|| "Unknown CPU".to_string());
    let cores_physical: Option<usize> = sysinfo::System::physical_core_count().ok();
    ProfileStatic { cpu, cores_physical, cores_logical, os }
}

/// sysinfo 0.40 reports memory in bytes (measured: total_memory()=240_148_307_968 on this
/// machine, which divided by 1024^3 is 223.66 — exactly Python's `ram_total_gb`).
const BYTES_PER_GIB: f64 = 1_073_741_824.0;

impl Sampler {
    pub fn new(first: Snapshot) -> Sampler {
        Sampler(Arc::new(RwLock::new(first)))
    }

    pub async fn snapshot(&self) -> Snapshot {
        self.0.read().await.clone()
    }

    /// Background loop. sysinfo needs two refreshes to compute a usage percentage, so the
    /// very first tick is discarded rather than published.
    ///
    /// The seed snapshot still carries real memory (and GPU) counters: `new_all()` refreshes
    /// them immediately, so a request that lands before the second tick reports the machine's
    /// RAM instead of zeros. Only `cpu_percent` waits for its delta window.
    pub async fn spawn(gpu: Arc<dyn Gpu>, interval: Duration) -> Sampler {
        // 0.40's constructors return Result. If a System handle cannot be built there is
        // nothing to sample, and the sampler keeps serving its GPU-only seed.
        let Ok(mut sys) = sysinfo::System::new_all() else {
            let first = Snapshot { gpu: gpu.sample(), ..Default::default() };
            return Sampler::new(first);
        };
        // new_all() has only opened the first CPU window, so the seed's percentage is 0 by
        // construction; force it rather than inherit whatever sysinfo happens to report.
        let mut first = snap(&sys, &*gpu);
        first.cpu_percent = 0.0;
        let sampler = Sampler::new(first);
        let shared = sampler.0.clone();
        tokio::spawn(async move {
            let mut first_tick = true;
            loop {
                tokio::time::sleep(interval).await;
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                if first_tick {
                    // No usage delta exists yet for this window; publish the next one instead.
                    first_tick = false;
                    continue;
                }
                *shared.write().await = snap(&sys, &*gpu);
            }
        });
        sampler
    }
}

/// One reading of the machine. Both call sites (seed and loop) go through here so the
/// rounding matches Python's `round(x, n)` exactly, digit for digit.
fn snap(sys: &sysinfo::System, gpu: &dyn Gpu) -> Snapshot {
    // `as f64` on byte counts: 240 GB is far below 2^53, so the conversion is exact.
    let total = sys.total_memory() as f64;
    let used = sys.used_memory() as f64;
    let (gb_used, gb_total) = (used / BYTES_PER_GIB, total / BYTES_PER_GIB);
    Snapshot {
        cpu_percent: crate::bench::round_half_even(sys.global_cpu_usage() as f64, 1),
        ram_percent: if total > 0.0 {
            crate::bench::round_half_even(used / total * 100.0, 1)
        } else {
            0.0
        },
        ram_used_gb: crate::bench::round_half_even(gb_used, 2),
        ram_total_gb: crate::bench::round_half_even(gb_total, 2),
        gpu: gpu.sample(),
    }
}

#[cfg(test)]
mod tests {
    use super::{GpuSnapshot, NoGpu, Sampler, Snapshot};
    use std::sync::Arc;
    use std::time::Duration;

    // routes.rs only ever forwards to this, so the two Python-response shapes are pinned here:
    // no hardware means the GPU keys are absent, hardware means they follow the base four.
    #[test]
    fn gpu_keys_are_appended_after_the_base_four_in_python_order() {
        let snap = Snapshot {
            cpu_percent: 4.7,
            ram_percent: 27.6,
            ram_used_gb: 61.84,
            ram_total_gb: 223.66,
            gpu: Some(GpuSnapshot { util: 0, mem_used_mb: 10156, mem_total_mb: 16384, temp_c: 35 }),
        };
        let v = snap.to_json();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "cpu_percent",
                "ram_percent",
                "ram_used_gb",
                "ram_total_gb",
                "gpu_util",
                "gpu_mem_mb",
                "gpu_mem_tot",
                "gpu_temp_c"
            ]
        );
        assert_eq!(v["gpu_mem_tot"].as_u64(), Some(16384));
    }

    #[test]
    fn no_gpu_means_absent_keys_and_never_null() {
        let v = Snapshot { ram_total_gb: 223.66, ..Default::default() }.to_json();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 4);
        for k in ["gpu_util", "gpu_mem_mb", "gpu_mem_tot", "gpu_temp_c"] {
            assert!(obj.get(k).is_none(), "{k} must be absent without a GPU");
        }
    }

    // `gpu_mem_tot` = 16384 on the P5000 comes from this divisor, not from NVML itself, so pin
    // it here where no driver is required. Python rounds (`round(x / 1024**2)`), it does not
    // truncate, so the tie case has to round up to the even integer like CPython does.
    #[test]
    fn bytes_convert_to_mebibytes_the_way_pynvml_rounds() {
        use super::NvmlGpu;
        assert_eq!(NvmlGpu::mib(17_179_869_184), 16384);
        assert_eq!(NvmlGpu::mib(10_648_289_280), 10155);
        assert_eq!(NvmlGpu::mib(10_648_813_568), 10156); // 10155.5 MiB, a tie
    }

    // Byte counts measured live off the P5000 (driver 582.67). pynvml's v1 `used` equals
    // nvml-wrapper's v2 `used + reserved`, and both land on the 10156 MiB Python reports; v2
    // `used` alone would have reported 10007.
    #[test]
    fn nvml_v2_used_plus_reserved_reproduces_pynvml_v1_used() {
        use super::NvmlGpu;
        assert_eq!(NvmlGpu::mib(10_492_792_832 + 156_237_824), 10156);
        assert_eq!(NvmlGpu::mib(10_492_792_832), 10007);
    }

    #[tokio::test]
    async fn spawn_seeds_ram_before_the_first_tick_is_published() {
        // The 60ms interval plus the discarded first tick means a handler that answers
        // immediately reads the seed, and the seed must not claim 0 GB of RAM.
        let sampler = Sampler::spawn(Arc::new(NoGpu), Duration::from_millis(60)).await;
        let snap = sampler.snapshot().await;
        assert!(snap.ram_total_gb > 0.5, "seed ram_total_gb is wrong: {}", snap.ram_total_gb);
        assert!(snap.ram_used_gb <= snap.ram_total_gb);
    }
}

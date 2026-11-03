//! Background hardware sampler. The only module in the crate that reads system counters:
//! `routes.rs` serves the last published snapshot instead of touching `sysinfo`/NVML itself,
//! so request handlers never block on a system scan.

use std::sync::{Arc, Mutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::RwLock;

/// Instantaneous GPU counters, already converted to the wire units (MiB, degrees C).
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
    /// unit-testable without real hardware. The key order is fixed: the base four first, the
    /// GPU four appended while a card is live, and `gpu_status` last. A missing GPU omits its
    /// four keys (never `null`), but `gpu_status` is always present so the dashboard can
    /// explain *why* the counters are missing.
    pub fn to_json(&self, status: &str) -> Value {
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
        v["gpu_status"] = json!(status);
        v
    }
}

pub trait Gpu: Send + Sync {
    /// Whether NVML was initialised at startup at all. It separates the profile endpoint's two
    /// branches — `available()` false answers `gpu: "None detected"`, true answers the card's
    /// own fields or, if any query fails, `gpu_error`.
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

/// NVIDIA via NVML. Only device index 0 is read, even though the development machine has two
/// cards.
pub struct NvmlGpu {
    nvml: nvml_wrapper::Nvml,
    index: u32,
}

impl NvmlGpu {
    /// `None` rather than an error: `/api/hardware` has no failure path, it just omits the GPU
    /// keys, the same as an unavailable GPU.
    pub fn open(index: u32) -> Option<NvmlGpu> {
        match nvml_wrapper::Nvml::init() {
            Ok(nvml) if nvml.device_by_index(index).is_ok() => Some(NvmlGpu { nvml, index }),
            _ => None,
        }
    }

    /// Bytes -> MiB with half-to-even rounding, not integer truncation.
    fn mib(bytes: u64) -> u64 {
        crate::bench::round_half_even(bytes as f64 / 1_048_576.0, 0) as u64
    }
}

impl Gpu for NvmlGpu {
    /// `NvmlGpu` only exists after a successful `Nvml::init()` + device lookup, so availability
    /// is simply true.
    fn available(&self) -> bool { true }

    fn sample(&self) -> Option<GpuSnapshot> {
        let dev = self.nvml.device_by_index(self.index).ok()?;
        // nvml-wrapper 0.13 accessors: `memory_usage()` is now `memory_info()`, and
        // `temperature()` now takes a sensor.
        let util = dev.utilization_rates().ok()?;
        let mem = dev.memory_info().ok()?;
        let temp = dev.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu).ok()?;
        Some(GpuSnapshot {
            util: util.gpu,
            // `memory_info()` wraps the *v2* entry point, whose `used` excludes the driver's
            // `reserved` block; NVML's v1 counter folds reserved into `used`. On this machine
            // v2 used 10_492_792_832 + reserved 156_237_824 == v1 used 10_649_030_656, i.e.
            // 10156 MiB against 10007 MiB for v2 alone. Adding reserved back keeps
            // `gpu_mem_mb` consistent with the v1 reading. `total` is the same in both
            // versions, so `gpu_mem_tot` needs no such correction.
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

/// How often a missing NVML is retried. A failed `dlopen` is cheap, but ten seconds keeps the
/// sampler loop from hammering the loader while a user is installing the driver.
const GPU_RECHECK_EVERY: Duration = Duration::from_secs(10);

/// The process's GPU slot, shared by the sampler and the profile endpoint. A driver installed
/// while the dashboard is open is picked up by the next `refresh` — no process restart, which
/// is what makes the dashboard's "install the driver, then reload" hint true.
#[derive(Clone)]
pub struct GpuHandle {
    slot: Arc<StdRwLock<Arc<dyn Gpu>>>,
    recheck: bool,
    last_attempt: Arc<Mutex<Option<Instant>>>,
}

impl GpuHandle {
    /// A slot that never re-detects. Tests use this so a developer machine with a card cannot
    /// turn its `NoGpu` into real counters mid-run.
    pub fn fixed(gpu: Arc<dyn Gpu>) -> GpuHandle {
        GpuHandle { slot: Arc::new(StdRwLock::new(gpu)), recheck: false,
                    last_attempt: Arc::new(Mutex::new(None)) }
    }

    /// The production slot: while no card is available, `refresh` retries detection at most
    /// every ten seconds. A successful open replaces the `NoGpu` handle, so the very next
    /// sample and profile read see the card.
    pub fn rechecking(gpu: Arc<dyn Gpu>) -> GpuHandle {
        GpuHandle { recheck: true, ..GpuHandle::fixed(gpu) }
    }

    /// The current handle, cloned out of the slot so callers never hold the lock across an
    /// NVML query.
    pub fn current(&self) -> Arc<dyn Gpu> {
        self.slot.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Throttled re-detection for the sampler loop: a no-op for `fixed` slots and while a card
    /// is already available.
    pub fn refresh(&self) {
        if !self.recheck || self.current().available() { return; }
        {
            let mut last = self.last_attempt.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < GPU_RECHECK_EVERY) { return; }
            *last = Some(Instant::now());
        }
        if let Some(g) = NvmlGpu::open(0) {
            *self.slot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(g);
        }
    }
}

/// The dashboard's answer to "should I install a driver?". NVML is the only path to GPU
/// counters, so when it is unavailable the remaining question is whether an NVIDIA display
/// device exists at all — that is what the probe adds. The closure keeps the probe lazy: an
/// available GPU returns `"ok"` without paying for it. Values are pinned by the dashboard's
/// hint text: `ok`, `driver_missing`, `no_nvidia`, `unsupported`, `unknown`.
pub fn gpu_status(available: bool, probe: impl FnOnce() -> Option<bool>) -> &'static str {
    if available {
        return "ok";
    }
    // NVIDIA stopped shipping macOS drivers after High Sierra, so there is nothing to install
    // even on a machine with an old card; the dashboard says that instead of linking a
    // download that cannot work.
    if cfg!(target_os = "macos") {
        return "unsupported";
    }
    match probe() {
        Some(true) => "driver_missing",
        Some(false) => "no_nvidia",
        None => "unknown",
    }
}

/// Cached vendor-probe result: `/api/hardware` is polled every few seconds, and on Windows
/// the probe spawns a process, so the answer is computed once per process. A stale "no card"
/// cannot outlive a driver install: once NVML opens the card, `available` answers `"ok"` and
/// the probe is never consulted again.
static NVIDIA_PRESENT: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();

/// Best-effort "is an NVIDIA display device installed?", independent of its driver. Only the
/// driverless case needs it: with NVML available the card's own name is the better answer.
pub fn nvidia_present() -> Option<bool> {
    *NVIDIA_PRESENT.get_or_init(probe_nvidia)
}

#[cfg(target_os = "linux")]
fn probe_nvidia() -> Option<bool> {
    nvidia_in_sysfs(std::path::Path::new("/sys/bus/pci/devices"))
}

/// PCI display controllers are class `0x03xxxx`. The class filter matters: an NVIDIA GPU also
/// exposes a class `0x0403` audio function, which must not make a machine with no display
/// device look like it has a card. `None` means sysfs could not answer, not "no card".
#[cfg(target_os = "linux")]
fn nvidia_in_sysfs(root: &std::path::Path) -> Option<bool> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut saw_display = false;
    for entry in entries.flatten() {
        let dev = entry.path();
        let Ok(class) = std::fs::read_to_string(dev.join("class")) else { continue };
        if !class.trim().starts_with("0x03") { continue; }
        saw_display = true;
        let Ok(vendor) = std::fs::read_to_string(dev.join("vendor")) else { continue };
        if vendor.trim().eq_ignore_ascii_case("0x10de") { return Some(true); }
    }
    saw_display.then_some(false)
}

/// Windows keeps the PnP hardware id even while the card is driven by the generic Basic
/// Display Adapter, so `VEN_10DE` separates "driver missing" from "no NVIDIA card". One lazy
/// PowerShell run, cached above; `-NoProfile` keeps it from loading a user profile.
#[cfg(windows)]
fn probe_nvidia() -> Option<bool> {
    use std::os::windows::process::CommandExt;
    // Keep the console from flashing on a dashboard poll.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command",
               "Get-CimInstance -ClassName Win32_VideoController | Select-Object -ExpandProperty PNPDeviceID"])
        .creation_flags(CREATE_NO_WINDOW)
        .output().ok()?;
    if !out.status.success() { return None; }
    let text = String::from_utf8_lossy(&out.stdout);
    // Empty output means the query produced nothing (CIM failure, stripped-down image), not
    // that the machine has no video controller.
    if text.trim().is_empty() { return None; }
    Some(contains_nvidia_hwid(&text))
}

#[cfg(windows)]
fn contains_nvidia_hwid(text: &str) -> bool {
    text.lines().any(|l| l.to_ascii_uppercase().contains("VEN_10DE"))
}

/// Everything except Linux and Windows: current macOS has no NVIDIA driver path at all, and
/// `gpu_status` already answers "unsupported" before consulting this.
#[cfg(not(any(target_os = "linux", windows)))]
fn probe_nvidia() -> Option<bool> {
    Some(false)
}

/// The published snapshot plus the task that republishes it. Cheap to clone; every request
/// handler reads it instead of sampling.
#[derive(Clone)]
pub struct Sampler(Arc<RwLock<Snapshot>>);

/// Immutable machine facts for `GET /api/profile`, captured once at startup. Rebuilding them
/// on every poll would add a per-request system scan to an async handler — a cost this design
/// avoids; the values are constants for the life of the process anyway. `ram_total_gb` is
/// deliberately not here: it already lives in the `Sampler`'s cached snapshot, which refreshes
/// on its own cadence.
#[derive(Clone, Debug)]
pub struct ProfileStatic { pub cpu: String, pub cores_physical: Option<usize>,
                           pub cores_logical: usize, pub os: String }

/// Read the startup-time profile facts. `routes.rs` must never call this per request — nor
/// build a `System::new_all()` in a handler, whose `refresh_all()` walks the process table
/// and blocks the async runtime.
pub fn read_profile_static() -> ProfileStatic {
    let cores_logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    // `"<os> <release>"`, e.g. `windows 10.0.19045`.
    let os = format!("{} {}", std::env::consts::OS,
                      sysinfo::System::os_version().unwrap_or_default())
        .trim().to_string();
    // sysinfo 0.40.1: `new_all()`, `physical_core_count()` and `os_version()` all return
    // `Result<_, sysinfo::Error>`. A failed `new_all()` means there are no CPU strings to
    // read, so fall back to `"Unknown CPU"`.
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
/// machine, which divided by 1024^3 is 223.66 GiB).
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
    pub async fn spawn(gpu: GpuHandle, interval: Duration) -> Sampler {
        // 0.40's constructors return Result. If a System handle cannot be built there is
        // nothing to sample, and the sampler keeps serving its GPU-only seed.
        let Ok(mut sys) = sysinfo::System::new_all() else {
            gpu.refresh();
            let first = Snapshot { gpu: gpu.current().sample(), ..Default::default() };
            return Sampler::new(first);
        };
        // new_all() has only opened the first CPU window, so the seed's percentage is 0 by
        // construction; force it rather than inherit whatever sysinfo happens to report.
        gpu.refresh();
        let mut first = snap(&sys, &*gpu.current());
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
                // The slot is re-checked on the sampler's own cadence, so a newly installed
                // driver appears without touching the process.
                gpu.refresh();
                *shared.write().await = snap(&sys, &*gpu.current());
            }
        });
        sampler
    }
}

/// One reading of the machine. Both call sites (seed and loop) go through here so the
/// rounding is consistent, digit for digit.
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
    use super::{GpuHandle, GpuSnapshot, NoGpu, Sampler, Snapshot};
    use std::sync::Arc;
    use std::time::Duration;

    // routes.rs only ever forwards to this, so both wire shapes are pinned here:
    // no hardware means the GPU keys are absent, hardware means they follow the base four.
    #[test]
    fn gpu_keys_are_appended_after_the_base_four() {
        let snap = Snapshot {
            cpu_percent: 4.7,
            ram_percent: 27.6,
            ram_used_gb: 61.84,
            ram_total_gb: 223.66,
            gpu: Some(GpuSnapshot { util: 0, mem_used_mb: 10156, mem_total_mb: 16384, temp_c: 35 }),
        };
        let v = snap.to_json("ok");
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
                "gpu_temp_c",
                "gpu_status"
            ]
        );
        assert_eq!(v["gpu_mem_tot"].as_u64(), Some(16384));
        assert_eq!(v["gpu_status"], "ok");
    }

    #[test]
    fn no_gpu_means_absent_keys_and_never_null() {
        let v = Snapshot { ram_total_gb: 223.66, ..Default::default() }.to_json("driver_missing");
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 5);
        for k in ["gpu_util", "gpu_mem_mb", "gpu_mem_tot", "gpu_temp_c"] {
            assert!(obj.get(k).is_none(), "{k} must be absent without a GPU");
        }
        // The status is always present so the dashboard can explain the absence.
        assert_eq!(v["gpu_status"], "driver_missing");
    }

    // The hint under the cards depends on this status, so pin every branch. The probe is a
    // closure so "available" can be shown not to pay for it.
    #[test]
    fn status_is_ok_without_probing_when_nvml_is_available() {
        use super::gpu_status;
        let probed = std::cell::Cell::new(false);
        assert_eq!(gpu_status(true, || { probed.set(true); None }), "ok");
        assert!(!probed.get(), "an available GPU must not pay for the PCI probe");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn status_separates_a_missing_driver_from_a_missing_card() {
        use super::gpu_status;
        assert_eq!(gpu_status(false, || Some(true)), "driver_missing");
        assert_eq!(gpu_status(false, || Some(false)), "no_nvidia");
        assert_eq!(gpu_status(false, || None), "unknown");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn status_is_unsupported_on_macos() {
        use super::gpu_status;
        // Even a genuine NVIDIA card has no driver to install on current macOS.
        assert_eq!(gpu_status(false, || Some(true)), "unsupported");
    }

    // `/sys/bus/pci/devices` exists on every Linux box but its contents are machine-specific,
    // so the classifier runs against a fixture tree instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn sysfs_scan_counts_only_nvidia_display_controllers() {
        use super::nvidia_in_sysfs;
        let root = std::env::temp_dir()
            .join(format!("lomora-sysfs-{}", std::process::id()));
        let dev = |name: &str, class: &str, vendor: &str| {
            let p = root.join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("class"), class).unwrap();
            std::fs::write(p.join("vendor"), vendor).unwrap();
        };
        let _ = std::fs::remove_dir_all(&root);
        dev("0000:01:00.0", "0x030000\n", "0x10de\n");
        assert_eq!(nvidia_in_sysfs(&root), Some(true));

        // An NVIDIA GPU also exposes a class 0x0403 audio function; that is not a display
        // device and must not make a driverless machine look like it has a card.
        std::fs::remove_dir_all(&root).unwrap();
        dev("0000:01:00.1", "0x040300\n", "0x10de\n");
        assert_eq!(nvidia_in_sysfs(&root), None);

        std::fs::remove_dir_all(&root).unwrap();
        dev("0000:03:00.0", "0x030000\n", "0x1002\n");
        assert_eq!(nvidia_in_sysfs(&root), Some(false));

        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(nvidia_in_sysfs(&root), None);
    }

    #[cfg(windows)]
    #[test]
    fn hwid_parser_recognises_the_nvidia_vendor() {
        use super::contains_nvidia_hwid;
        assert!(contains_nvidia_hwid("PCI\\VEN_10DE&DEV_1B02"));
        assert!(contains_nvidia_hwid("pci\\ven_10de&dev_1b02"));
        assert!(!contains_nvidia_hwid("PCI\\VEN_1002&DEV_67DF"));
        assert!(!contains_nvidia_hwid(""));
    }

    // `gpu_mem_tot` = 16384 on the P5000 comes from this divisor, not from NVML itself, so pin
    // it here where no driver is required. The conversion rounds half-to-even rather than
    // truncating, so the tie case goes to the even integer.
    #[test]
    fn bytes_convert_to_mebibytes_with_half_to_even_rounding() {
        use super::NvmlGpu;
        assert_eq!(NvmlGpu::mib(17_179_869_184), 16384);
        assert_eq!(NvmlGpu::mib(10_648_289_280), 10155);
        assert_eq!(NvmlGpu::mib(10_648_813_568), 10156); // 10155.5 MiB, a tie
    }

    // Byte counts measured live off the P5000 (driver 582.67). NVML's v1 `used` equals
    // nvml-wrapper's v2 `used + reserved`, and both land on 10156 MiB; v2 `used` alone would
    // report 10007.
    #[test]
    fn nvml_v2_used_plus_reserved_matches_the_v1_counter() {
        use super::NvmlGpu;
        assert_eq!(NvmlGpu::mib(10_492_792_832 + 156_237_824), 10156);
        assert_eq!(NvmlGpu::mib(10_492_792_832), 10007);
    }

    #[tokio::test]
    async fn spawn_seeds_ram_before_the_first_tick_is_published() {
        // The 60ms interval plus the discarded first tick means a handler that answers
        // immediately reads the seed, and the seed must not claim 0 GB of RAM.
        let sampler = Sampler::spawn(GpuHandle::fixed(Arc::new(NoGpu)),
                                     Duration::from_millis(60)).await;
        let snap = sampler.snapshot().await;
        assert!(snap.ram_total_gb > 0.5, "seed ram_total_gb is wrong: {}", snap.ram_total_gb);
        assert!(snap.ram_used_gb <= snap.ram_total_gb);
    }
}

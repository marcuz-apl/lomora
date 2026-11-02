// Scaffolding only — deleted in Task 9. Measures what sysinfo 0.40.1 actually returns for
// memory (bytes vs KB) so the sampler's GB divisor is derived from evidence, not docs, and
// what Rust's `{:.N}` formatting does on exact ties (bench::round_half_even depends on it).
use sysinfo::System;

fn main() {
    // 0.40.1: both constructors return Result, contrary to the brief's snippet.
    let Ok(mut s) = System::new_all() else {
        println!("System::new_all failed");
        return;
    };
    s.refresh_all();
    std::thread::sleep(std::time::Duration::from_millis(400));
    s.refresh_cpu_usage();
    println!(
        "used={} total={} cpu={} phys={:?}",
        s.used_memory(),
        s.total_memory(),
        s.global_cpu_usage(),
        System::physical_core_count(),
    );
    println!(
        "total/1024^3={:.6} -> {:.2} | used/1024^3={:.6} -> {:.2}",
        s.total_memory() as f64 / 1024f64.powi(3),
        s.total_memory() as f64 / 1024f64.powi(3),
        s.used_memory() as f64 / 1024f64.powi(3),
        s.used_memory() as f64 / 1024f64.powi(3),
    );

    for (x, d) in [
        (0.125f64, 2u32),
        (0.135, 2),
        (1.5, 0),
        (2.5, 0),
        (0.5, 0),
        (3.5, 0),
        (1.25, 1),
        (10155.5, 0),
    ] {
        println!("rust round_half_even({x:?}, {d}) = {:?}", lomora::bench::round_half_even(x, d));
    }

    // Second API-drift risk: nvml-wrapper 0.13 calls nvmlDeviceGetMemoryInfo_v2, pynvml calls
    // the v1 entry point. Print the raw bytes so the MiB math can be matched to Python's
    // reported gpu_mem_mb rather than guessed from headers.
    if let Ok(nvml) = nvml_wrapper::Nvml::init()
        && let Ok(dev) = nvml.device_by_index(0)
        && let Ok(m) = dev.memory_info()
    {
        println!(
            "nvml v2 bytes: used={} free={} reserved={} total={} | used+reserved={}",
            m.used, m.free, m.reserved, m.total, m.used + m.reserved,
        );
        println!("python v1 used bytes (measured) = 10649030656");
        println!("driver={:?}", dev.name().ok());
    }
}

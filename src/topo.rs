//! Logical CPU topology from sysfs, for grouping the per-CPU view:
//! SMT siblings (one physical core), the L3 domain (AMD CCD/CCX, Intel ring
//! bus) and the socket. Missing files (non-Linux, odd VMs) degrade to "every
//! CPU is its own core, one L3 domain, one socket".

use std::fs;

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct CpuTopo {
    /// Logical CPU number (`cpuN`).
    pub cpu: u32,
    /// Lowest CPU number in this CPU's physical core.
    pub core: u32,
    /// Lowest CPU number sharing this CPU's L3 cache.
    pub l3: Option<u32>,
    /// Socket (`physical_package_id`).
    pub pkg: u32,
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// First CPU of a sysfs cpu list such as `0,6` or `0-11`.
fn first_cpu(list: &str) -> Option<u32> {
    list.split([',', '-']).next()?.trim().parse().ok()
}

fn l3_of(cpu: u32) -> Option<u32> {
    (0..8).find_map(|i| {
        let base = format!("/sys/devices/system/cpu/cpu{cpu}/cache/index{i}");
        if read(&format!("{base}/level"))? != "3" {
            return None;
        }
        first_cpu(&read(&format!("{base}/shared_cpu_list"))?)
    })
}

/// `cpus` are logical CPU numbers in the order sysinfo reports them.
pub fn detect(cpus: impl IntoIterator<Item = u32>) -> Vec<CpuTopo> {
    cpus.into_iter()
        .map(|cpu| {
            let t = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
            let core = read(&format!("{t}/core_cpus_list"))
                .or_else(|| read(&format!("{t}/thread_siblings_list")))
                .and_then(|l| first_cpu(&l))
                .unwrap_or(cpu);
            let pkg = read(&format!("{t}/physical_package_id"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            CpuTopo { cpu, core, l3: l3_of(cpu), pkg }
        })
        .collect()
}

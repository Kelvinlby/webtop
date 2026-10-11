//! NVIDIA GPU sampling via NVML (loaded dynamically, so the binary still runs
//! on machines without an NVIDIA driver).
//!
//! SM and Tensor activity come from GPM (GPU Performance Monitoring, Hopper+
//! / Blackwell), which reports real per-pipe activity between two samples.
//! When GPM is unavailable we fall back to the classic NVML "GPU utilization"
//! counter for SM (the fraction of time *any* kernel was running) and report
//! Tensor as unavailable.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::enums::gpm::GpmMetricId;
use nvml_wrapper::{GpmSample, Nvml, gpm::gpm_metrics_get};
use serde::Serialize;

#[derive(Serialize, Clone, Default)]
pub struct GpuStats {
    pub index: u32,
    pub name: String,
    /// SM activity in percent.
    pub sm: Option<f64>,
    /// Where `sm` came from: "gpm" (true SM activity) or "nvml" (kernel-busy time).
    pub sm_source: &'static str,
    /// Tensor-core pipe activity in percent (GPM only).
    pub tensor: Option<f64>,
    pub mem_bw: Option<f64>,
    pub vram_used: u64,
    pub vram_total: u64,
    pub temp_c: Option<u32>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub sm_clock_mhz: Option<u32>,
}

/// Per-process GPU usage, keyed by PID.
#[derive(Default, Clone, Copy)]
pub struct GpuProc {
    pub vram: u64,
    pub sm: Option<u32>,
}

pub struct GpuSampler {
    nvml: Nvml,
}

impl GpuSampler {
    pub fn new() -> Option<Self> {
        match Nvml::init() {
            Ok(nvml) => Some(Self { nvml }),
            Err(e) => {
                eprintln!("webtop: NVML unavailable ({e}); GPU metrics disabled");
                None
            }
        }
    }

    /// Sampling state; its lifetime is tied to the NVML handle because GPM
    /// samples borrow it.
    pub fn state(&self) -> GpuState<'_> {
        let count = self.nvml.device_count().unwrap_or(0);
        let mut gpm_ok = Vec::new();
        for i in 0..count {
            let ok = self
                .nvml
                .device_by_index(i)
                .and_then(|d| d.gpm_support())
                .unwrap_or(false);
            gpm_ok.push(ok);
        }
        GpuState {
            nvml: &self.nvml,
            count,
            gpm_ok,
            prev: (0..count).map(|_| None).collect(),
            last_proc_ts: vec![0; count as usize],
        }
    }
}

pub struct GpuState<'n> {
    nvml: &'n Nvml,
    count: u32,
    gpm_ok: Vec<bool>,
    prev: Vec<Option<GpmSample<'n>>>,
    last_proc_ts: Vec<u64>,
}

impl<'n> GpuState<'n> {
    /// Drops stored GPM samples so the next reading isn't averaged over an idle gap.
    pub fn reset(&mut self) {
        self.prev.iter_mut().for_each(|p| *p = None);
    }

    pub fn sample(&mut self) -> (Vec<GpuStats>, HashMap<u32, GpuProc>) {
        let mut out = Vec::with_capacity(self.count as usize);
        let mut procs: HashMap<u32, GpuProc> = HashMap::new();

        for i in 0..self.count {
            let idx = i as usize;
            let Ok(dev) = self.nvml.device_by_index(i) else { continue };
            let mut s = GpuStats {
                index: i,
                name: dev.name().unwrap_or_else(|_| format!("GPU {i}")),
                ..Default::default()
            };

            if let Ok(m) = dev.memory_info() {
                s.vram_used = m.used;
                s.vram_total = m.total;
            }
            s.temp_c = dev.temperature(TemperatureSensor::Gpu).ok();
            s.power_w = dev.power_usage().ok().map(|mw| mw as f64 / 1000.0);
            s.power_limit_w = dev.enforced_power_limit().ok().map(|mw| mw as f64 / 1000.0);
            s.sm_clock_mhz = dev.clock_info(Clock::SM).ok();

            // GPM: diff against the previous sample.
            if self.gpm_ok[idx] {
                match dev.gpm_sample() {
                    Ok(cur) => {
                        if let Some(prev) = &self.prev[idx] {
                            let ids = [
                                GpmMetricId::SmUtil,
                                GpmMetricId::AnyTensorUtil,
                                GpmMetricId::DramBwUtil,
                            ];
                            if let Ok(res) = gpm_metrics_get(self.nvml, prev, &cur, &ids) {
                                let v = |k: usize| res.get(k).and_then(|r| r.as_ref().ok()).map(|r| r.value);
                                s.sm = v(0);
                                s.tensor = v(1);
                                s.mem_bw = v(2);
                                if s.sm.is_some() {
                                    s.sm_source = "gpm";
                                }
                            }
                        }
                        self.prev[idx] = Some(cur);
                    }
                    Err(_) => self.gpm_ok[idx] = false,
                }
            }
            if s.sm.is_none() {
                if let Ok(u) = dev.utilization_rates() {
                    s.sm = Some(u.gpu as f64);
                    s.sm_source = "nvml";
                    if s.mem_bw.is_none() {
                        s.mem_bw = Some(u.memory as f64);
                    }
                }
            }

            // Per-process VRAM.
            let lists = [dev.running_compute_processes(), dev.running_graphics_processes()];
            for list in lists.into_iter().flatten() {
                for p in list {
                    let e = procs.entry(p.pid).or_default();
                    if let UsedGpuMemory::Used(b) = p.used_gpu_memory {
                        e.vram = e.vram.max(b);
                    }
                }
            }
            // Per-process SM utilization (samples since the last call).
            let since = self.last_proc_ts[idx];
            if let Ok(samples) = dev.process_utilization_stats(if since == 0 { None } else { Some(since) }) {
                let mut agg: HashMap<u32, u32> = HashMap::new();
                for smp in samples {
                    self.last_proc_ts[idx] = self.last_proc_ts[idx].max(smp.timestamp);
                    let a = agg.entry(smp.pid).or_default();
                    *a = (*a).max(smp.sm_util);
                }
                for (pid, sm) in agg {
                    let e = procs.entry(pid).or_default();
                    e.sm = Some(e.sm.unwrap_or(0) + sm);
                }
            } else if since == 0 {
                // Avoid hammering with None every tick if unsupported.
                self.last_proc_ts[idx] = now_us();
            }

            out.push(s);
        }
        (out, procs)
    }
}

fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

//! Background sampling thread. It only works while someone is watching: if no
//! client has fetched stats recently, it idles instead of walking /proc.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
    UpdateKind, Users,
};

use crate::gpu::{GpuSampler, GpuStats};

const IDLE_AFTER: Duration = Duration::from_secs(10);
pub const MIN_INTERVAL_MS: u64 = 250;
pub const MAX_INTERVAL_MS: u64 = 10_000;

#[derive(Serialize)]
struct Snapshot<'a> {
    ts: u64,
    /// Effective sampling interval in ms.
    interval: u64,
    hostname: &'a str,
    uptime: u64,
    load: [f64; 3],
    cpu: CpuStats,
    mem: MemStats,
    gpus: Vec<GpuStats>,
    procs: Vec<Proc>,
}

#[derive(Serialize)]
struct CpuStats {
    total: f32,
    cores: Vec<f32>,
    brand: String,
}

#[derive(Serialize)]
struct MemStats {
    used: u64,
    total: u64,
    available: u64,
    swap_used: u64,
    swap_total: u64,
}

#[derive(Serialize)]
struct Proc {
    pid: u32,
    ppid: Option<u32>,
    name: String,
    user: String,
    cpu: f32,
    mem: u64,
    vram: Option<u64>,
    gpu: Option<u32>,
    status: String,
    cmd: String,
}

pub struct Shared {
    /// Latest snapshot, pre-serialized to JSON.
    pub latest: RwLock<Arc<String>>,
    /// Millis since `epoch` of the last client request.
    last_req: AtomicU64,
    /// Current sampling interval, as requested by viewers.
    interval_ms: AtomicU64,
    /// Millis since `epoch` when some viewer last confirmed `interval_ms`.
    interval_seen: AtomicU64,
    epoch: Instant,
}

impl Shared {
    pub fn new(interval_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            latest: RwLock::new(Arc::new("{}".into())),
            last_req: AtomicU64::new(0),
            interval_ms: AtomicU64::new(interval_ms),
            interval_seen: AtomicU64::new(0),
            epoch: Instant::now(),
        })
    }

    pub fn touch(&self) {
        self.last_req.store(self.epoch.elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// A viewer asks for samples every `ms`. The fastest active viewer wins;
    /// once it stops asking, the next slower request takes over.
    pub fn request_interval(&self, ms: u64) {
        let ms = ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);
        let now = self.now_ms();
        let cur = self.interval_ms.load(Ordering::Relaxed);
        let seen = self.interval_seen.load(Ordering::Relaxed);
        let stale = now.saturating_sub(seen) > cur * 3 + 2000;
        if ms <= cur || stale {
            self.interval_ms.store(ms, Ordering::Relaxed);
            self.interval_seen.store(now, Ordering::Relaxed);
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.load(Ordering::Relaxed))
    }

    fn active(&self) -> bool {
        let last = self.last_req.load(Ordering::Relaxed);
        last != 0 && self.epoch.elapsed().as_millis() as u64 - last < IDLE_AFTER.as_millis() as u64
    }
}

pub fn spawn(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || run(shared))
        .expect("spawn sampler thread");
}

fn run(shared: Arc<Shared>) {
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
            .with_memory(MemoryRefreshKind::everything()),
    );
    let mut users = Users::new_with_refreshed_list();
    let hostname = System::host_name().unwrap_or_default();
    let brand = sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default();

    let gpu = GpuSampler::new();
    let mut gpu_state = gpu.as_ref().map(|g| g.state());

    let proc_kind = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet);

    let mut was_active = false;
    let mut user_cache: HashMap<String, String> = HashMap::new();

    loop {
        if !shared.active() {
            if was_active {
                was_active = false;
                if let Some(g) = gpu_state.as_mut() {
                    g.reset();
                }
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        if !was_active {
            // Prime CPU counters so the first published reading is meaningful.
            was_active = true;
            sys.refresh_cpu_usage();
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind);
            if let Some(g) = gpu_state.as_mut() {
                g.sample();
            }
            std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(300)));
        }

        let start = Instant::now();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind);
        let (gpus, gpu_procs) = gpu_state.as_mut().map(|g| g.sample()).unwrap_or_default();

        let mut procs = Vec::with_capacity(sys.processes().len());
        for (pid, p) in sys.processes() {
            // Skip kernel threads / userland threads listed as tasks.
            if p.thread_kind().is_some() {
                continue;
            }
            let pid = pid.as_u32();
            let user = match p.user_id() {
                Some(uid) => {
                    let key = uid.to_string();
                    if let Some(n) = user_cache.get(&key) {
                        n.clone()
                    } else {
                        if users.get_user_by_id(uid).is_none() {
                            users.refresh();
                        }
                        let n = users
                            .get_user_by_id(uid)
                            .map(|u| u.name().to_string())
                            .unwrap_or_else(|| key.clone());
                        user_cache.insert(key, n.clone());
                        n
                    }
                }
                None => String::new(),
            };
            let mut cmd = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            if cmd.len() > 300 {
                let mut cut = 300;
                while !cmd.is_char_boundary(cut) {
                    cut -= 1;
                }
                cmd.truncate(cut);
            }
            let g = gpu_procs.get(&pid);
            procs.push(Proc {
                pid,
                ppid: p.parent().map(|pp| pp.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                user,
                cpu: p.cpu_usage(),
                mem: p.memory(),
                vram: g.map(|g| g.vram),
                gpu: g.and_then(|g| g.sm),
                status: p.status().to_string(),
                cmd,
            });
        }

        let load = System::load_average();
        let snap = Snapshot {
            interval: shared.interval().as_millis() as u64,
            ts: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
            hostname: &hostname,
            uptime: System::uptime(),
            load: [load.one, load.five, load.fifteen],
            cpu: CpuStats {
                total: sys.global_cpu_usage(),
                cores: sys.cpus().iter().map(|c| c.cpu_usage()).collect(),
                brand: brand.clone(),
            },
            mem: MemStats {
                used: sys.used_memory(),
                total: sys.total_memory(),
                available: sys.available_memory(),
                swap_used: sys.used_swap(),
                swap_total: sys.total_swap(),
            },
            gpus,
            procs,
        };
        if let Ok(json) = serde_json::to_string(&snap) {
            *shared.latest.write().unwrap() = Arc::new(json);
        }

        // Sleep in short slices so a faster interval requested mid-sleep applies promptly.
        while start.elapsed() < shared.interval() {
            std::thread::sleep(shared.interval().saturating_sub(start.elapsed()).min(Duration::from_millis(100)));
        }
    }
}

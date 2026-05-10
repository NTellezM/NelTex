// ═══════════════════════════════════════════════════════════════════
// NTvidia v3 — Scheduler de GPU para Linux
// ═══════════════════════════════════════════════════════════════════
//
// Aprende qué procesos usan la GPU por observación directa de
// /proc/PID/fd sin listas hardcodeadas.
//
// Flujo:
//   1. App arranca → Intel por defecto (score=0)
//   2. Daemon detecta /dev/nvidia* abierto → gpu_rate_ema sube
//   3. EMA cruza umbral (~10s) → score en /run/ntvidia/scores/
//   4. Próximo arranque → preload shim inyecta PRIME automáticamente
//
// Modelo: misma gaussiana que TGD (μ=−0.10, herencia V/E/T)

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MU: f64          = -0.10;
const SIGMA_MIN: f64   =  0.15;
const SIGMA_MAX: f64   =  0.50;
const ALPHA_SIGMA: f64 =  0.60;
const ALPHA_EMA: f64   =  0.35;
const GPU_THRESHOLD: f64   = 0.50;
const DECAY_RATE: f64      = 0.98;
const CYCLE_MS: u64        = 800;
const SAVE_EVERY: u64      = 75;
const SMI_EVERY: u64       = 5;
const CLEAN_EVERY: u64     = 150;
const SCORE_MAX_AGE_S: u64 = 300;

const SCORES_DIR: &str   = "/run/ntvidia/scores";
const LEARNED_FILE: &str = "/var/lib/ntvidia/learned.csv";
const SHUTDOWN_FLAG: &str = "/tmp/ntvidia-shutdown";

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

extern "C" fn on_shutdown(_: i32) {
    let _ = std::fs::write(SHUTDOWN_FLAG, "1");
}

fn install_signal_handlers() {
    let _ = fs::remove_file(SHUTDOWN_FLAG);
    unsafe {
        signal(15, on_shutdown as usize);
        signal(2,  on_shutdown as usize);
    }
}

fn shutdown_requested() -> bool {
    Path::new(SHUTDOWN_FLAG).exists()
}

// ══════════════════════════════════════════════════════════════════
// MODELO GAUSSIANO
// ══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
enum ProcClass { Kernel, System, User }

impl ProcClass {
    fn x_priv(&self) -> f64 {
        match self {
            ProcClass::Kernel => -0.30,
            ProcClass::System => -0.10,
            ProcClass::User   =>  0.20,
        }
    }
    fn from_comm(comm: &str, uid: u32) -> Self {
        if uid == 0
            || comm.starts_with("kworker")
            || comm.starts_with("irq")
            || comm.starts_with("rcu")
            || comm.starts_with("migration")
            || comm.starts_with("ksoftirq")
        { return ProcClass::Kernel; }
        if uid < 1000 { return ProcClass::System; }
        ProcClass::User
    }
}

#[derive(Debug, Clone)]
struct ProcState {
    comm:         String,
    uid:          u32,
    x_pos:        f64,
    density:      f64,
    gpu_rate_ema: f64,
    use_gpu:      bool,
    sigma:        f64,
    last_updated_ts: u64,
}

impl ProcState {
    fn new(comm: &str, uid: u32) -> Self {
        let class  = ProcClass::from_comm(comm, uid);
        let x_pos  = compute_x(0.0, class.x_priv());
        let sigma  = (SIGMA_MIN + SIGMA_MAX) / 2.0;
        ProcState {
            comm: comm.to_string(), uid, x_pos,
            density: gaussian(x_pos, sigma),
            gpu_rate_ema: 0.0, use_gpu: false, sigma,
            last_updated_ts: unix_now(),
        }
    }

    fn new_with_ema(comm: &str, uid: u32, ema: f64) -> Self {
        let mut s  = ProcState::new(comm, uid);
        s.gpu_rate_ema = ema;
        let class  = ProcClass::from_comm(comm, uid);
        s.x_pos    = compute_x(ema, class.x_priv());
        s.density  = gaussian(s.x_pos, s.sigma);
        s.use_gpu  = s.x_pos > GPU_THRESHOLD;
        s
    }

    fn update(&mut self, gpu_signal: f64, proc_count: usize) {
        let class = ProcClass::from_comm(&self.comm, self.uid);
        if class != ProcClass::User { self.use_gpu = false; return; }

        self.gpu_rate_ema = (1.0 - ALPHA_EMA) * self.gpu_rate_ema
                          + ALPHA_EMA * gpu_signal;
        self.x_pos = compute_x(self.gpu_rate_ema, class.x_priv());

        let sigma_target = SIGMA_MIN
            + (proc_count as f64 / 16.0).min(1.0) * (SIGMA_MAX - SIGMA_MIN);
        self.sigma = (self.sigma + ALPHA_SIGMA * (sigma_target - self.sigma))
                     .clamp(SIGMA_MIN, SIGMA_MAX);
        self.density         = gaussian(self.x_pos, self.sigma);
        self.use_gpu         = self.x_pos > GPU_THRESHOLD;
        self.last_updated_ts = unix_now();
    }

    fn decay(&mut self, proc_count: usize) {
        let decayed = self.gpu_rate_ema * DECAY_RATE;
        self.update(decayed, proc_count);
    }
}

fn compute_x(gpu_rate: f64, x_priv: f64) -> f64 {
    (0.60 * gpu_rate + 0.40 * x_priv).clamp(-1.0, 1.0)
}

fn gaussian(x: f64, sigma: f64) -> f64 {
    let z = (x - MU) / sigma;
    (-0.5 * z * z).exp()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ══════════════════════════════════════════════════════════════════
// SCAN ÚNICO DE /proc
// ══════════════════════════════════════════════════════════════════

#[derive(Default)]
struct ProcScanResult {
    nvidia_users: HashMap<String, u32>,
    all_comms:    HashMap<String, u32>,
}

fn scan_proc() -> ProcScanResult {
    let mut result = ProcScanResult::default();
    let Ok(proc_dir) = fs::read_dir("/proc") else { return result };

    for entry in proc_dir.flatten() {
        let name    = entry.file_name();
        let pid_str = name.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) { continue; }

        let Ok(comm_raw) = fs::read_to_string(format!("/proc/{}/comm", pid_str))
            else { continue };
        let comm = comm_raw.trim().to_string();
        let uid  = read_uid_ntvidia(&pid_str).unwrap_or(1000);

        result.all_comms.entry(comm.clone())
            .and_modify(|u| { if uid > *u { *u = uid; } })
            .or_insert(uid);

        let fd_dir = format!("/proc/{}/fd", pid_str);
        let Ok(fds) = fs::read_dir(&fd_dir) else { continue };

        for fd in fds.flatten() {
            if let Ok(target) = fs::read_link(fd.path()) {
                if target.to_string_lossy().contains("/dev/nvidia") {
                    result.nvidia_users.entry(comm.clone())
                        .and_modify(|u| { if uid > *u { *u = uid; } })
                        .or_insert(uid);
                    break;
                }
            }
        }
    }
    result
}

fn read_uid_ntvidia(pid: &str) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if line.starts_with("Uid:") {
            return line.split_whitespace().nth(1)?.parse().ok();
        }
    }
    None
}

// ══════════════════════════════════════════════════════════════════
// NVIDIA-SMI (cacheado)
// ══════════════════════════════════════════════════════════════════

#[derive(Debug, Default, Clone)]
struct GpuMetrics {
    util_pct:     f64,
    mem_used_mib: f64,
    mem_total_mib:f64,
    compute_procs: Vec<(u32, String)>,
}

fn query_nvidia_smi() -> Option<GpuMetrics> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu,memory.used,memory.total",
               "--format=csv,noheader,nounits"])
        .output().ok()?;
    let s     = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = s.trim().split(',').collect();
    if parts.len() < 3 { return None; }
    let util  = parts[0].trim().parse::<f64>().ok()? / 100.0;
    let used  = parts[1].trim().parse::<f64>().ok()?;
    let total = parts[2].trim().parse::<f64>().ok()?;

    let out2 = Command::new("nvidia-smi")
        .args(["--query-compute-apps=pid,process_name", "--format=csv,noheader"])
        .output().ok()?;
    let mut procs = Vec::new();
    for line in String::from_utf8_lossy(&out2.stdout).lines() {
        let p: Vec<&str> = line.splitn(2, ',').collect();
        if p.len() == 2 {
            if let Ok(pid) = p[0].trim().parse::<u32>() {
                let name = Path::new(p[1].trim())
                    .file_name().unwrap_or_default()
                    .to_string_lossy().to_string();
                procs.push((pid, name));
            }
        }
    }
    Some(GpuMetrics { util_pct: util, mem_used_mib: used,
                      mem_total_mib: total, compute_procs: procs })
}

// ══════════════════════════════════════════════════════════════════
// PERSISTENCIA
// ══════════════════════════════════════════════════════════════════

fn load_learned(states: &mut HashMap<String, ProcState>) {
    let Ok(content) = fs::read_to_string(LEARNED_FILE) else { return };
    let mut loaded = 0u32;
    for line in content.lines() {
        if line.starts_with('#') || line.is_empty() { continue; }
        let parts: Vec<&str> = line.splitn(3, ',').collect();
        if parts.len() < 2 { continue; }
        let comm = parts[0].trim();
        let Ok(ema) = parts[1].trim().parse::<f64>() else { continue };
        let uid: u32 = parts.get(2)
            .and_then(|s| s.trim().parse().ok()).unwrap_or(1000);
        if ema > 0.01 {
            states.entry(comm.to_string())
                .or_insert_with(|| ProcState::new_with_ema(comm, uid, ema));
            loaded += 1;
        }
    }
    if loaded > 0 {
        eprintln!("[ntvidia] {} scores cargados", loaded);
    }
}

fn save_learned(states: &HashMap<String, ProcState>) {
    if let Some(parent) = Path::new(LEARNED_FILE).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut lines = String::from("# NTvidia learned — comm,gpu_rate_ema,uid\n");
    let mut entries: Vec<_> = states.values()
        .filter(|s| s.gpu_rate_ema > 0.01).collect();
    entries.sort_by(|a, b| b.gpu_rate_ema.partial_cmp(&a.gpu_rate_ema).unwrap());
    for s in entries {
        lines.push_str(&format!("{},{:.4},{}\n", s.comm, s.gpu_rate_ema, s.uid));
    }
    let _ = fs::write(LEARNED_FILE, lines);
}

// ══════════════════════════════════════════════════════════════════
// SCORES
// ══════════════════════════════════════════════════════════════════

fn sanitize_comm(comm: &str) -> String {
    comm.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn write_score(comm: &str, state: &ProcState) {
    let _ = fs::create_dir_all(SCORES_DIR);
    let safe    = sanitize_comm(comm);
    let content = format!(
        "use_gpu={}\nx={:.4}\ndensity={:.4}\ngpu_ema={:.4}\nsigma={:.4}\nts={}\n",
        state.use_gpu as u8, state.x_pos, state.density,
        state.gpu_rate_ema, state.sigma, state.last_updated_ts,
    );
    let _ = fs::write(Path::new(SCORES_DIR).join(&safe), content);
}

fn clean_stale_scores(states: &HashMap<String, ProcState>) {
    let now = unix_now();
    let Ok(entries) = fs::read_dir(SCORES_DIR) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let stale = match states.get(&name) {
            Some(s) => now.saturating_sub(s.last_updated_ts) > SCORE_MAX_AGE_S
                       && s.gpu_rate_ema < 0.01,
            None    => true,
        };
        if stale { let _ = fs::remove_file(&path); }
    }
}

// ══════════════════════════════════════════════════════════════════
// MAIN
// ══════════════════════════════════════════════════════════════════

fn main() {
    eprintln!("[ntvidia] v3 — NTvidia GPU Scheduler | μ={} umbral={}",
              MU, GPU_THRESHOLD);
    let _ = fs::create_dir_all(SCORES_DIR);

    install_signal_handlers();

    let mut states: HashMap<String, ProcState> = HashMap::new();
    let mut gpu_cache = GpuMetrics::default();
    let mut cycle: u64 = 0;

    load_learned(&mut states);
    for (comm, s) in &states { write_score(comm, s); }

    loop {
        let t0 = Instant::now();

        if shutdown_requested() {
            eprintln!("[ntvidia] shutdown — guardando scores...");
            save_learned(&states);
            let _ = fs::remove_file(SHUTDOWN_FLAG);
            std::process::exit(0);
        }

        if cycle % SMI_EVERY == 0 {
            if let Some(gpu) = query_nvidia_smi() { gpu_cache = gpu; }
        }
        let gpu = &gpu_cache;

        let scan        = scan_proc();
        let proc_count  = scan.all_comms.len().max(1);

        for (comm, uid) in &scan.all_comms {
            states.entry(comm.clone()).or_insert_with(|| {
                let s = ProcState::new(comm, *uid);
                write_score(comm, &s);
                s
            });
        }

        for (comm, uid) in &scan.nvidia_users {
            let entry = states.entry(comm.clone())
                .or_insert_with(|| ProcState::new(comm, *uid));
            entry.uid = *uid;
            let prev = entry.use_gpu;
            entry.update(1.0, proc_count);
            if entry.use_gpu != prev { write_score(comm, entry); }
        }

        for (_pid, name) in &gpu.compute_procs {
            if !scan.nvidia_users.contains_key(name) {
                let entry = states.entry(name.clone())
                    .or_insert_with(|| ProcState::new(name, 1000));
                let prev = entry.use_gpu;
                entry.update(gpu.util_pct.max(0.5), proc_count);
                if entry.use_gpu != prev { write_score(name, entry); }
            }
        }

        let keys: Vec<String> = states.keys().cloned().collect();
        for key in &keys {
            let is_active = scan.nvidia_users.contains_key(key)
                || gpu.compute_procs.iter().any(|(_, n)| n == key);
            if !is_active {
                if let Some(s) = states.get_mut(key) {
                    let prev = s.use_gpu;
                    s.decay(proc_count);
                    if s.use_gpu != prev { write_score(key, s); }
                }
            }
        }

        if cycle % 10 == 0 {
            let gpu_procs: Vec<_> = states.values()
                .filter(|s| s.use_gpu)
                .collect();
            if !gpu_procs.is_empty() {
                eprintln!("[ntvidia] GPU util={:.0}% | {} procesos activos",
                    gpu.util_pct * 100.0, gpu_procs.len());
            }
        }

        if cycle % SAVE_EVERY == 0 && cycle > 0 {
            save_learned(&states);
        }

        if cycle % CLEAN_EVERY == 0 && cycle > 0 {
            clean_stale_scores(&states);
        }

        cycle += 1;
        thread::sleep(Duration::from_millis(CYCLE_MS).saturating_sub(t0.elapsed()));
    }
}

// ═══════════════════════════════════════════════════════════════════
// TGD-SCHED  v9.3  —  Topologic Gaussian Density Scheduler
// ═══════════════════════════════════════════════════════════════════
//
// Scheduler de CPU para Linux basado en el modelo gaussiano del
// Teorema Fundamental de la Acústica Topológica (V/E/T).
//
// Mecanismo: setpriority() / nice values (sin cgroups)
// Integración GPU: lee scores de NTvidia desde /run/ntvidia/scores/
//
// Fórmula topológica:
//   x = 0.45·(cpu_rate·0.80)
//     + 0.25·(gpu_effective·0.80)   gpu_effective = gpu_ema × cpu_rate
//     + 0.10·(−syscr/max·0.30)
//     + 0.20·x_priv
//
// Mapeo gaussiano → nice:
//   norm(x) = exp(−0.5·((x−μ)/σ)²)
//   nice = −5 + 24·(1 − norm)    rango [−5, +19]
//
// Parámetros calibrados:
//   μ = −0.20, σ ∈ [0.15, 0.40], σ_inicial = 0.35

use std::collections::HashMap;
use std::fs;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::time::{Duration, Instant};
use std::f64::consts::PI;

extern "C" {
    fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    fn getpriority(which: i32, who: u32) -> i32;
    fn syscall(num: i64, ...) -> i64;
    fn signal(signum: i32, handler: usize) -> usize;
    fn sysconf(name: i32) -> i64;
}
const PRIO_PROCESS:        i32 = 0;
const SC_CLK_TCK:          i32 = 2;
const SYS_TIMERFD_CREATE:  i64 = 283;
const SYS_TIMERFD_SETTIME: i64 = 286;
const CLOCK_MONOTONIC:     i32 = 1;
type Pid = i32;

// ── Parámetros del modelo ─────────────────────────────────────────
const MU:         f64 = -0.20;
const SIGMA_MIN:  f64 =  0.15;
const SIGMA_MAX:  f64 =  0.40;
const NICE_BASE:  i32 = -5;
const NICE_RANGE: i32 = 24;

// ── Ciclos periódicos ─────────────────────────────────────────────
const SAVE_EVERY:    u64 = 75;
const CLEAN_EVERY:   u64 = 150;
const STATUS_EVERY:  u64 = 10;
const CYCLE_MS:      u64 = 800;

// ── Rutas ─────────────────────────────────────────────────────────
const LEARNED_FILE:   &str = "/var/lib/tgd-sched/learned.csv";
const SHUTDOWN_FLAG:  &str = "/tmp/tgd-shutdown";
const NTVIDIA_SCORES: &str = "/run/ntvidia/scores";

// ══════════════════════════════════════════════════════════════════
// SEÑALES DE SHUTDOWN
// ══════════════════════════════════════════════════════════════════

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

fn gaussian(x: f64, sigma: f64) -> f64 {
    let z = (x - MU) / sigma;
    (-0.5 * z * z).exp()
}

fn derive_x(cpu_rate: f64, gpu_ema: f64, syscr_rate: f64,
            max_syscr: f64, uid: u32) -> f64 {
    let x_cpu  = cpu_rate * 0.80;
    let gpu_eff = gpu_ema * cpu_rate.min(1.0);
    let x_gpu  = gpu_eff  * 0.80;
    let x_io   = -(syscr_rate / max_syscr).min(1.0) * 0.30;
    let x_priv = if uid == 0 { -0.30 }
                 else if uid < 1000 { -0.10 }
                 else { 0.20 };
    (0.45 * x_cpu + 0.25 * x_gpu + 0.10 * x_io + 0.20 * x_priv)
        .clamp(-1.0, 1.0)
}

fn norm_to_nice(norm: f64) -> i32 {
    (NICE_BASE + (NICE_RANGE as f64 * (1.0 - norm)).round() as i32)
        .clamp(-5, 19)
}

struct Gauss { sigma: f64 }

impl Gauss {
    fn new() -> Self { Self { sigma: 0.35 } }
    fn norm(&self, x: f64) -> f64 { gaussian(x, self.sigma) }
    fn update(&mut self, n: usize) {
        let target = SIGMA_MIN + (n as f64 / 16.0).min(1.0)
                     * (SIGMA_MAX - SIGMA_MIN);
        self.sigma += 0.60 * (target - self.sigma);
        self.sigma  = self.sigma.clamp(SIGMA_MIN, SIGMA_MAX);
    }
}

// ══════════════════════════════════════════════════════════════════
// SCAN ÚNICO DE /proc
// ══════════════════════════════════════════════════════════════════

struct ProcEntry {
    pid:       Pid,
    comm:      String,
    uid:       u32,
    cpu_ticks: u64,
    syscr:     u64,
    gpu_ema:   f64,
}

fn scan_proc_once() -> Vec<ProcEntry> {
    let mut result = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return result };

    for entry in dir.flatten() {
        let name    = entry.file_name();
        let pid_str = name.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) { continue; }
        let Ok(pid) = pid_str.parse::<Pid>() else { continue };

        let Ok(comm_raw) = fs::read_to_string(format!("/proc/{}/comm", pid))
            else { continue };
        let comm = comm_raw.trim().to_string();

        let uid       = read_uid(pid).unwrap_or(1000);
        let cpu_ticks = read_cpu_ticks(pid);
        let syscr     = read_syscr(pid);
        let gpu_ema   = read_ntvidia_gpu_ema(&comm);

        result.push(ProcEntry { pid, comm, uid, cpu_ticks, syscr, gpu_ema });
    }
    result
}

fn read_uid(pid: Pid) -> Option<u32> {
    let s = fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    s.lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn read_cpu_ticks(pid: Pid) -> u64 {
    let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pid))
        else { return 0 };
    let ne = stat.rfind(')').unwrap_or(0);
    let fields: Vec<&str> = stat[ne+2..].split_whitespace().collect();
    let ut: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let st: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    ut + st
}

fn read_syscr(pid: Pid) -> u64 {
    let Ok(io) = fs::read_to_string(format!("/proc/{}/io", pid))
        else { return 0 };
    io.lines()
        .find(|l| l.starts_with("syscr: "))
        .and_then(|l| l[7..].trim().parse().ok())
        .unwrap_or(0)
}

fn read_ntvidia_gpu_ema(comm: &str) -> f64 {
    let safe: String = comm.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c }
                 else { '_' })
        .collect();
    let Ok(content) = fs::read_to_string(
        format!("{}/{}", NTVIDIA_SCORES, safe)) else { return 0.0 };
    content.lines()
        .find(|l| l.starts_with("gpu_ema="))
        .and_then(|l| l[8..].trim().parse().ok())
        .unwrap_or(0.0)
}

// ══════════════════════════════════════════════════════════════════
// ESTADO POR PROCESO
// ══════════════════════════════════════════════════════════════════

struct ProcState {
    comm:         String,
    last_ticks:   u64,
    last_syscr:   u64,
    last_ts:      Instant,
    smooth_cpu:   f64,
    current_nice: i32,
}

impl ProcState {
    fn new(comm: String, ticks: u64, syscr: u64, learned_cpu: f64) -> Self {
        Self {
            comm, last_ticks: ticks, last_syscr: syscr,
            last_ts:      Instant::now() - Duration::from_millis(CYCLE_MS),
            smooth_cpu:   learned_cpu,
            current_nice: 0,
        }
    }

    fn update(&mut self, ticks: u64, syscr: u64, hz: f64) -> (f64, f64) {
        let dt_s      = (Instant::now() - self.last_ts).as_secs_f64().max(0.1);
        let d_ticks   = ticks.saturating_sub(self.last_ticks) as f64;
        let d_syscr   = syscr.saturating_sub(self.last_syscr) as f64;
        let cpu_raw   = (d_ticks / (hz * dt_s)).clamp(0.0, 1.0);
        let syscr_rate = d_syscr / dt_s;
        self.smooth_cpu   = self.smooth_cpu * 0.65 + cpu_raw * 0.35;
        self.last_ticks   = ticks;
        self.last_syscr   = syscr;
        self.last_ts      = Instant::now();
        (self.smooth_cpu, syscr_rate)
    }
}

// ══════════════════════════════════════════════════════════════════
// PERSISTENCIA
// ══════════════════════════════════════════════════════════════════

fn load_learned() -> HashMap<String, f64> {
    let mut map = HashMap::new();
    let Ok(content) = fs::read_to_string(LEARNED_FILE) else { return map };
    let mut n = 0u32;
    for line in content.lines() {
        if line.starts_with('#') || line.is_empty() { continue; }
        let parts: Vec<&str> = line.splitn(2, ',').collect();
        if parts.len() < 2 { continue; }
        let Ok(ema) = parts[1].trim().parse::<f64>() else { continue };
        if ema > 0.001 {
            map.insert(parts[0].trim().to_string(), ema);
            n += 1;
        }
    }
    if n > 0 {
        eprintln!("[TGD] {} procesos cargados desde {}", n, LEARNED_FILE);
    }
    map
}

fn save_learned(states: &HashMap<Pid, ProcState>) {
    if let Some(p) = Path::new(LEARNED_FILE).parent() {
        let _ = fs::create_dir_all(p);
    }
    let mut by_comm: HashMap<&str, f64> = HashMap::new();
    for s in states.values() {
        by_comm.entry(&s.comm)
            .and_modify(|v| { if s.smooth_cpu > *v { *v = s.smooth_cpu; } })
            .or_insert(s.smooth_cpu);
    }
    let mut lines = String::from("# TGD learned — comm,smooth_cpu\n");
    let mut entries: Vec<_> = by_comm.iter()
        .filter(|(_, &v)| v > 0.001).collect();
    entries.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
    for (comm, cpu) in entries {
        lines.push_str(&format!("{},{:.4}\n", comm, cpu));
    }
    let _ = fs::write(LEARNED_FILE, lines);
}

// ══════════════════════════════════════════════════════════════════
// TIMERFD
// ══════════════════════════════════════════════════════════════════

struct TimerFd { fd: RawFd }

impl TimerFd {
    fn new(ms: u64) -> Self {
        let fd  = unsafe { syscall(SYS_TIMERFD_CREATE, CLOCK_MONOTONIC, 0) } as RawFd;
        let ns  = (ms * 1_000_000) as i64;
        let spec: [i64; 4] = [0, ns, 0, ns];
        unsafe { syscall(SYS_TIMERFD_SETTIME, fd, 0,
                         spec.as_ptr() as *const _, 0usize) };
        Self { fd }
    }
    fn wait(&self) {
        let mut buf = [0u8; 8];
        unsafe { syscall(0i64, self.fd as i64, buf.as_mut_ptr() as *mut _, 8usize) };
    }
}

// ══════════════════════════════════════════════════════════════════
// MAIN
// ══════════════════════════════════════════════════════════════════

fn main() {
    let ntvidia_status = if Path::new(NTVIDIA_SCORES).exists()
        { "activo" } else { "no instalado" };
    eprintln!("[TGD] v9.3 — sin cgroups | nice∈[-5,+19] | NTvidia:{} | μ={}",
              ntvidia_status, MU);

    install_signal_handlers();

    let hz      = unsafe { sysconf(SC_CLK_TCK) } as f64;
    let learned = load_learned();
    let mut states: HashMap<Pid, ProcState> = HashMap::new();
    let mut gauss   = Gauss::new();
    let mut cycle:  u64 = 0;
    let     timer   = TimerFd::new(CYCLE_MS);
    let     my_pid  = std::process::id() as Pid;

    loop {
        if shutdown_requested() {
            eprintln!("[TGD] shutdown — restaurando nice y guardando...");
            for pid in states.keys() {
                unsafe { setpriority(PRIO_PROCESS, *pid as u32, 0) };
            }
            save_learned(&states);
            let _ = fs::remove_file(SHUTDOWN_FLAG);
            eprintln!("[TGD] listo. {} procesos restaurados.", states.len());
            std::process::exit(0);
        }

        timer.wait();

        let entries = scan_proc_once();
        let alive: std::collections::HashSet<Pid> =
            entries.iter().map(|e| e.pid).collect();

        for e in &entries {
            if e.pid == my_pid { continue; }
            let learned_cpu = learned.get(&e.comm).copied().unwrap_or(0.0);
            states.entry(e.pid).or_insert_with(||
                ProcState::new(e.comm.clone(), e.cpu_ticks, e.syscr, learned_cpu)
            );
        }

        let mut metrics: Vec<(Pid, u32, f64, f64, f64)> = Vec::new();

        for e in &entries {
            if e.pid == my_pid { continue; }
            let Some(state) = states.get_mut(&e.pid) else { continue };
            let (cpu_rate, syscr_rate) = state.update(e.cpu_ticks, e.syscr, hz);
            metrics.push((e.pid, e.uid, cpu_rate, e.gpu_ema, syscr_rate));
        }

        if !metrics.is_empty() {
            let max_syscr = metrics.iter()
                .map(|(_, _, _, _, s)| *s)
                .fold(0.0_f64, f64::max)
                .max(1.0);

            gauss.update(metrics.len());

            let mut sorted = metrics.clone();
            sorted.sort_by(|a, b|
                (b.2 + b.3).partial_cmp(&(a.2 + a.3)).unwrap());

            for &(pid, uid, cpu, gpu, syscr) in &metrics {
                let x    = derive_x(cpu, gpu, syscr, max_syscr, uid);
                let norm = gauss.norm(x);
                let nice = norm_to_nice(norm);
                if let Some(s) = states.get_mut(&pid) {
                    if s.current_nice != nice {
                        unsafe { setpriority(PRIO_PROCESS, pid as u32, nice) };
                        s.current_nice = nice;
                    }
                }
            }

            if cycle % STATUS_EVERY == 0 {
                for (pid, _, cpu, gpu, _) in sorted.iter().take(4) {
                    if *cpu < 0.02 && *gpu < 0.05 { continue; }
                    if let Some(s) = states.get(pid) {
                        let x    = derive_x(*cpu, *gpu, 0.0, 1.0, 1000);
                        let norm = gauss.norm(x);
                        let nice = norm_to_nice(norm);
                        let gpu_str = if *gpu > 0.05 {
                            format!(" gpu={:.2}", gpu)
                        } else { String::new() };
                        eprintln!("[TGD] {} cpu={:.2}{} x={:+.3} σ={:.3} nice={}",
                            s.comm, cpu, gpu_str, x, gauss.sigma, nice);
                    }
                }
            }
        }

        if cycle % CLEAN_EVERY == 0 && cycle > 0 {
            let dead: Vec<Pid> = states.keys()
                .filter(|&&pid| !alive.contains(&pid))
                .cloned().collect();
            for pid in &dead {
                unsafe { setpriority(PRIO_PROCESS, *pid as u32, 0) };
                states.remove(pid);
            }
            if !dead.is_empty() {
                eprintln!("[TGD] cleanup: {} procesos terminados", dead.len());
            }
        }

        if cycle % SAVE_EVERY == 0 && cycle > 0 {
            save_learned(&states);
        }

        cycle += 1;
    }
}

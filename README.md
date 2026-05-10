# NelTex — CPU+GPU Scheduler for Linux

> *"Order from chaos"* — Topologic Gaussian Density applied to OS scheduling

NelTex is a Linux CPU+GPU scheduler based on the **V/E/T Theorem** (Vibración/Eco/Tensión — Fundamental Theorem of Topological Acoustics). The same Gaussian function that models voice density in a symphony is used here to assign CPU priority to system processes.

Two daemons, one mathematical framework:
- **TGD-SCHED** — CPU scheduler using `setpriority()` / nice values
- **NTvidia** — GPU scheduler that learns which processes use the GPU

---

## How it works

Every process gets a topological position `x ∈ [−1, +1]`:

```
x = 0.45·(cpu_rate·0.80)
  + 0.25·(gpu_effective·0.80)    ← gpu_effective = gpu_ema × cpu_rate
  + 0.10·(−syscr/max·0.30)
  + 0.20·x_priv

x_priv: uid=0 → −0.30  |  uid<1000 → −0.10  |  uid≥1000 → +0.20
```

A Gaussian centered at μ = −0.20 assigns density to each process:

```
norm(x) = exp(−0.5·((x − μ) / σ)²)    σ ∈ [0.15, 0.40]
```

High density (near μ) = more CPU priority. Low density (far from μ) = less priority.

The density maps to a nice value:

```
nice = −5 + 24·(1 − norm)    range [−5, +19]
```

### Real results on production hardware

| Process | CPU | GPU | x | nice | Effect |
|---------|-----|-----|---|------|--------|
| terminal (idle) | 0% | 0% | +0.04 | −4 | High priority |
| gnome-shell | 5% | GPU | +0.07 | 0 | Protected |
| compiler | 85% | 0% | +0.35 | +10 | Reduced |
| brave (hog) | 89% | 100% | +0.56 | +15 | Heavily penalized |
| game (cpu+gpu) | 80% | 85% | +0.50 | +13 | Penalized |

**11-point separation between hogs and UI processes.**

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  NTvidia daemon (Rust)                                  │
│  Scans /proc/PID/fd for /dev/nvidia* every 800ms       │
│  Learns GPU usage per process via EMA                   │
│  Writes scores to /run/ntvidia/scores/<comm>            │
│  Preload shim (C) injects PRIME vars before main()     │
└──────────────────────┬──────────────────────────────────┘
                       │ reads gpu_ema
┌──────────────────────▼──────────────────────────────────┐
│  TGD-SCHED daemon (Rust)                                │
│  Single /proc scan per cycle (800ms)                    │
│  derive_x(cpu, gpu, syscr, uid) → topological position  │
│  gaussian(x, σ) → normalized density                    │
│  norm_to_nice(norm) → nice ∈ [−5, +19]                 │
│  setpriority() — no cgroups, clean shutdown             │
│  Persistence: /var/lib/tgd-sched/learned.csv            │
└─────────────────────────────────────────────────────────┘
```

### Key design decisions

- **No cgroups** — `setpriority()` only, no conflicts with systemd, clean shutdown
- **Single /proc scan** — one pass per cycle, 50% less I/O than naive approach
- **GPU×CPU signal** — `gpu_effective = gpu_ema × cpu_rate` prevents penalizing Xorg/gnome-shell which use GPU for rendering but not compute
- **Persistent learning** — `learned.csv` carries EMA history across reboots
- **SIGTERM clean exit** — restores nice=0 for all processes before saving and exiting

---

## Components

```
NelTex/
├── tgd-sched/
│   ├── src/main.rs         # TGD daemon (~400 lines Rust)
│   └── Cargo.toml
├── ntvidia/
│   ├── src/main.rs         # NTvidia daemon (~485 lines Rust)
│   ├── preload/
│   │   └── ntvidia_preload.c   # LD_PRELOAD shim
│   └── Cargo.toml
└── scripts/
    ├── install.sh          # Full installer
    └── tgd-ctl             # Management tool
```

---

## Requirements

- Linux with NVIDIA driver
- Rust 1.75+
- GCC
- `nvidia-smi` available
- Kernel with cgroups v2 (Ubuntu 22.04+, Mint 21+)

---

## Installation

```bash
git clone https://github.com/NTellezM/NelTex
cd NelTex
sudo env PATH="$PATH:$HOME/.cargo/bin" bash scripts/install.sh
```

---

## Usage

```bash
tgd-ctl status     # daemon status
tgd-ctl nice       # current nice values
tgd-ctl top        # top processes with TGD context
tgd-ctl learned    # learning history
tgd-ctl log        # live log
sudo tgd-ctl force <pid> <nice>
sudo tgd-ctl stop
```

---

## Calibrated parameters

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| μ | −0.20 | Centroid of real process distribution |
| σ_min | 0.15 | Minimum differentiation |
| σ_max | 0.40 | Cap for sharp differentiation |
| σ_initial | 0.35 | Pre-calibrated (not cold start) |
| NICE_BASE | −5 | Maximum priority for lightest processes |
| NICE_RANGE | 24 | Spread across the Gaussian |
| Cycle | 800ms | Balance between reactivity and overhead |

---

## Mathematical foundation

The V/E/T Theorem (Vibración/Eco/Tensión) models dynamic systems as a nonlinear ODE. The Gaussian density function used here is the same one that models voice density in a symphony — processes competing for CPU are like voices competing for acoustic space, and the scheduler acts like the conductor.

**Formally verified with z3 SMT solver — 8/8 invariants proven over infinite domains.**

---

## Tested hardware

- Lenovo Y520-15IKBN
- NVIDIA GeForce GTX 1050 (2GB VRAM), Driver 535.288.01
- Kernel 6.17.0-23-generic, Ubuntu 24.04.4 LTS

---

## Roadmap

- [ ] **v10** — sched_ext BPF scheduler (kernel ≥6.12, Ubuntu 26.04)
- [ ] Migrate to Ubuntu 26.04.1 (August 2026)
- [ ] Recompile `tgd.c` kernel module against kernel 7.0

---

## Related

- [Metriplex](https://github.com/NTellezM/Metriplex) — Layer 1 blockchain with fractal identity

---

## License

MIT

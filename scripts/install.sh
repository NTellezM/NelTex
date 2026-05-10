#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════
# NelTex — Instalador completo
# TGD-SCHED v9.3 + NTvidia v3
# Uso: sudo env PATH="$PATH:$HOME/.cargo/bin" bash scripts/install.sh
# ═══════════════════════════════════════════════════════════════════

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'

info()  { echo -e "${GREEN}[neltex]${NC} $*"; }
warn()  { echo -e "${YELLOW}[warn]${NC} $*"; }
error() { echo -e "${RED}[error]${NC} $*"; exit 1; }

[[ $EUID -ne 0 ]] && error "Ejecutar con sudo"

REAL_USER="${SUDO_USER:-$USER}"
CARGO_BIN="/home/${REAL_USER}/.cargo/bin/cargo"
command -v cargo &>/dev/null || [[ -x "$CARGO_BIN" ]] || \
    error "Rust no encontrado. Instalar: curl https://sh.rustup.rs | sh"
export PATH="$PATH:/home/${REAL_USER}/.cargo/bin"
command -v gcc >/dev/null || apt-get install -y gcc

info "Instalando NelTex para usuario: $REAL_USER"
echo ""

# ── 1. Compilar TGD-SCHED ─────────────────────────────────────────
info "Compilando TGD-SCHED v9.3..."
cd "$PROJECT_DIR/tgd-sched"
sudo -u "$REAL_USER" env PATH="$PATH" cargo build --release 2>&1 | tail -3
install -m 755 target/release/tgd-sched /usr/local/sbin/tgd-sched
info "tgd-sched instalado en /usr/local/sbin/tgd-sched"

# ── 2. Compilar NTvidia ───────────────────────────────────────────
if command -v nvidia-smi >/dev/null; then
    info "Compilando NTvidia v3..."
    cd "$PROJECT_DIR/ntvidia"
    sudo -u "$REAL_USER" env PATH="$PATH" cargo build --release 2>&1 | tail -3
    install -m 755 target/release/ntvidia /usr/local/sbin/ntvidia

    info "Compilando preload shim..."
    gcc -O2 -shared -fPIC \
        -o /usr/local/lib/ntvidia_preload.so \
        "$PROJECT_DIR/ntvidia/preload/ntvidia_preload.c"
    ldconfig
    info "NTvidia instalado"
else
    warn "nvidia-smi no encontrado — NTvidia no se instalará"
fi

# ── 3. tgd-ctl ───────────────────────────────────────────────────
install -m 755 "$PROJECT_DIR/scripts/tgd-ctl" /usr/local/bin/tgd-ctl
info "tgd-ctl instalado en /usr/local/bin/tgd-ctl"

# ── 4. Script de cleanup ─────────────────────────────────────────
cat > /usr/local/sbin/tgd-cleanup << 'EOF'
#!/bin/bash
# NelTex cleanup — restaura nice=0 antes de apagar
touch /tmp/tgd-shutdown
sleep 2
pkill -TERM -f tgd-sched 2>/dev/null || true
pkill -TERM -f ntvidia    2>/dev/null || true
sleep 1
rm -f /tmp/tgd-shutdown /tmp/ntvidia-shutdown
echo "[neltex] cleanup completado"
EOF
chmod 755 /usr/local/sbin/tgd-cleanup

# ── 5. Directorios de runtime ─────────────────────────────────────
mkdir -p /run/ntvidia/scores
chmod 777 /run/ntvidia/scores
mkdir -p /var/lib/tgd-sched
mkdir -p /var/lib/ntvidia
info "Directorios creados"

# ── 6. sudo sin contraseña ────────────────────────────────────────
cat > /etc/sudoers.d/neltex << EOF
$REAL_USER ALL=(root) NOPASSWD: /usr/local/sbin/tgd-sched
$REAL_USER ALL=(root) NOPASSWD: /usr/local/sbin/ntvidia
$REAL_USER ALL=(root) NOPASSWD: /usr/local/sbin/tgd-cleanup
EOF
chmod 440 /etc/sudoers.d/neltex
info "sudo sin contraseña configurado"

# ── 7. Autostart GNOME ───────────────────────────────────────────
AUTOSTART_DIR="/home/${REAL_USER}/.config/autostart"
mkdir -p "$AUTOSTART_DIR"

cat > "$AUTOSTART_DIR/tgd-sched.desktop" << EOF
[Desktop Entry]
Type=Application
Name=TGD Scheduler
Exec=bash -c 'sleep 5 && sudo /usr/local/sbin/tgd-sched > /tmp/tgd.log 2>&1'
Hidden=false
NoDisplay=false
X-GNOME-Autostart-enabled=true
EOF

if command -v nvidia-smi >/dev/null; then
    cat > "$AUTOSTART_DIR/ntvidia.desktop" << EOF
[Desktop Entry]
Type=Application
Name=NTvidia GPU Scheduler
Exec=bash -c 'sleep 3 && sudo /usr/local/sbin/ntvidia'
Hidden=false
NoDisplay=false
X-GNOME-Autostart-enabled=true
EOF

    # Activar preload shim
    PRELOAD_ENTRY="/usr/local/lib/ntvidia_preload.so"
    if ! grep -qF "$PRELOAD_ENTRY" /etc/ld.so.preload 2>/dev/null; then
        echo "$PRELOAD_ENTRY" >> /etc/ld.so.preload
        info "Preload shim activado en /etc/ld.so.preload"
    fi
fi

chown -R "$REAL_USER:$REAL_USER" "$AUTOSTART_DIR"
info "Autostart GNOME configurado"

# ── 8. Servicio systemd de cleanup ───────────────────────────────
cat > /etc/systemd/system/neltex-cleanup.service << 'EOF'
[Unit]
Description=NelTex Cleanup on Shutdown
DefaultDependencies=no
Before=shutdown.target reboot.target halt.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/tgd-cleanup
TimeoutStartSec=10

[Install]
WantedBy=shutdown.target reboot.target halt.target
EOF

systemctl daemon-reload
systemctl enable neltex-cleanup.service
info "Servicio de cleanup en shutdown habilitado"

# ── 9. Estado final ───────────────────────────────────────────────
echo ""
info "════════════════════════════════════════"
info "Instalación completa."
echo ""
info "Para arrancar ahora:"
echo "  sudo /usr/local/sbin/tgd-sched > /tmp/tgd.log 2>&1 &"
[[ -f /usr/local/sbin/ntvidia ]] && \
    echo "  sudo /usr/local/sbin/ntvidia &"
echo ""
info "Comandos:"
echo "  tgd-ctl status   → estado del sistema"
echo "  tgd-ctl nice     → nice values actuales"
echo "  tgd-ctl learned  → historial aprendido"
echo "  tgd-ctl log      → log en tiempo real"
echo ""
info "Al próximo inicio de sesión GNOME, los daemons arrancan automáticamente."
info "════════════════════════════════════════"

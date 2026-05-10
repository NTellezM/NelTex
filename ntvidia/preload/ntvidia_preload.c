// ═══════════════════════════════════════════════════════════════════
// NTvidia Preload Shim v3
// ═══════════════════════════════════════════════════════════════════
//
// Cargado via /etc/ld.so.preload en todos los procesos.
// El constructor corre ANTES de main() y ANTES de que libGL se
// inicialice. Lee el score del daemon y si use_gpu=1 inyecta las
// variables PRIME necesarias para activar la GPU NVIDIA.
//
// Compilar: gcc -O2 -shared -fPIC -o ntvidia_preload.so ntvidia_preload.c
// Instalar: echo /usr/local/lib/ntvidia_preload.so >> /etc/ld.so.preload

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>

#define SCORES_DIR "/run/ntvidia/scores"
#define MAX_COMM   64
#define MAX_LINE   256

// Sanitizar comm: reemplazar caracteres no válidos con '_'
static void sanitize_comm(const char *in, char *out, size_t max) {
    size_t i;
    for (i = 0; i < max - 1 && in[i]; i++) {
        char c = in[i];
        out[i] = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                 (c >= '0' && c <= '9') || c == '-' || c == '_' ? c : '_';
    }
    out[i] = '\0';
}

// Leer el comm del proceso actual desde /proc/self/comm
static int get_comm(char *buf, size_t max) {
    int fd = open("/proc/self/comm", O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, buf, max - 1);
    close(fd);
    if (n <= 0) return -1;
    buf[n] = '\0';
    // Eliminar newline
    char *nl = strchr(buf, '\n');
    if (nl) *nl = '\0';
    return 0;
}

// Leer score del daemon NTvidia para el comm actual
static int read_score(const char *comm_safe, int *use_gpu) {
    char path[512];
    snprintf(path, sizeof(path), "%s/%s", SCORES_DIR, comm_safe);

    FILE *f = fopen(path, "r");
    if (!f) return -1;

    char line[MAX_LINE];
    *use_gpu = 0;

    while (fgets(line, sizeof(line), f)) {
        if (strncmp(line, "use_gpu=", 8) == 0) {
            *use_gpu = atoi(line + 8);
        }
    }
    fclose(f);
    return 0;
}

// Constructor: corre antes de main()
__attribute__((constructor(101)))
static void ntvidia_init(void) {
    // No activar en el propio daemon
    const char *prog = getenv("_");
    if (prog && (strstr(prog, "ntvidia") || strstr(prog, "tgd-sched"))) {
        return;
    }

    char comm_raw[MAX_COMM] = {0};
    char comm_safe[MAX_COMM] = {0};

    if (get_comm(comm_raw, sizeof(comm_raw)) < 0) return;
    sanitize_comm(comm_raw, comm_safe, sizeof(comm_safe));

    int use_gpu = 0;
    if (read_score(comm_safe, &use_gpu) < 0) return;

    if (use_gpu) {
        // Activar NVIDIA PRIME render offload
        setenv("__NV_PRIME_RENDER_OFFLOAD", "1", 0);
        setenv("__NV_PRIME_RENDER_OFFLOAD_PROVIDER", "NVIDIA-G0", 0);
        setenv("__GLX_VENDOR_LIBRARY_NAME", "nvidia", 0);
        setenv("__EGL_VENDOR_LIBRARY_FILENAMES",
               "/usr/share/glvnd/egl_vendor.d/10_nvidia.json", 0);

        if (getenv("NTVIDIA_DEBUG")) {
            fprintf(stderr, "[ntvidia] %s → NVIDIA PRIME activado\n", comm_raw);
        }
    } else {
        if (getenv("NTVIDIA_DEBUG")) {
            fprintf(stderr, "[ntvidia] %s → Intel (sin score GPU)\n", comm_raw);
        }
    }
}

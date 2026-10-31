/* ipsample: user-space leaf-IP sampler via perf_event_open (no ptrace).
 *
 * The host has no perf userland tool and ptrace_scope=1 blocks gdb attach
 * to non-children, but perf_event_paranoid=1 still allows user-space event
 * sampling of any own-uid process. One event per target thread (inherit
 * forbids the ring buffer), samples instruction pointers, aggregates
 * per-symbol counts across the binary and every mapped DSO, prints a
 * histogram. Kernel frames are excluded (paranoid=1), which suits a
 * userspace hypervisor's hot paths.
 *
 * Build: cc -O2 -o bench/run/bin/ipsample bench/ipsample.c
 * Use:   ipsample PID SECONDS > out.txt
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <linux/perf_event.h>

static long perf_event_open(struct perf_event_attr *attr, pid_t pid, int cpu,
                            int group_fd, unsigned long flags) {
    return syscall(__NR_perf_event_open, attr, pid, cpu, group_fd, flags);
}

struct sym {
    uint64_t addr;
    uint64_t size;
    char *name;
};

struct dso {
    char path[512];
    uint64_t base;
    uint64_t start, end;
    struct sym *syms;
    int n;
};

static struct dso dsos[48];
static int ndso;

static int load_dso_symbols(const char *path, uint64_t base, int dynamic) {
    int output[2];
    if (pipe(output) < 0) return -1;
    pid_t child = fork();
    if (child < 0) {
        close(output[0]);
        close(output[1]);
        return -1;
    }
    if (child == 0) {
        close(output[0]);
        dup2(output[1], STDOUT_FILENO);
        close(output[1]);
        int quiet = open("/dev/null", O_WRONLY);
        if (quiet >= 0) dup2(quiet, STDERR_FILENO);
        if (dynamic) execlp("nm", "nm", "-DnS", "--defined-only", path, NULL);
        else execlp("nm", "nm", "-nS", "--defined-only", path, NULL);
        _exit(127);
    }
    close(output[1]);
    FILE *nm = fdopen(output[0], "r");
    if (!nm) {
        close(output[0]);
        waitpid(child, NULL, 0);
        return -1;
    }
    int cap = 8192, n = 0;
    struct sym *syms = malloc(cap * sizeof(*syms));
    char line[1100];
    while (fgets(line, sizeof(line), nm)) {
        uint64_t addr, size;
        char type;
        char name[1000];
        if (sscanf(line, "%" SCNx64 " %" SCNx64 " %c %999s", &addr, &size, &type, name) != 4)
            continue;
        if (!strchr("tTwW", type))
            continue;
        if (n == cap) {
            cap *= 2;
            syms = realloc(syms, cap * sizeof(*syms));
        }
        syms[n].addr = addr;
        syms[n].size = size;
        syms[n].name = strdup(name);
        n++;
    }
    fclose(nm);
    waitpid(child, NULL, 0);
    if (n == 0) {
        free(syms);
        if (!dynamic) return load_dso_symbols(path, base, 1);
        return -1;
    }
    snprintf(dsos[ndso].path, sizeof(dsos[ndso].path), "%s", path);
    dsos[ndso].base = base;
    dsos[ndso].syms = syms;
    dsos[ndso].n = n;
    ndso++;
    return 0;
}

static void load_dsos(pid_t pid) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/maps", pid);
    FILE *maps = fopen(path, "r");
    if (!maps)
        return;
    char line[1100];
    while (fgets(line, sizeof(line), maps) && ndso < 48) {
        char file[900] = "";
        unsigned long start, end, offset;
        if (sscanf(line, "%lx-%lx %*s %lx %*s %*s %899s", &start, &end, &offset, file) != 4)
            continue;
        if (!strstr(line, "r-xp") || file[0] != '/')
            continue;
        int seen = 0;
        for (int i = 0; i < ndso; i++)
            if (strcmp(dsos[i].path, file) == 0)
                seen = 1;
        if (!seen) {
            FILE *elf = fopen(file, "rb");
            Elf64_Ehdr eh;
            if (!elf || fread(&eh, sizeof(eh), 1, elf) != 1) {
                if (elf) fclose(elf);
                continue;
            }
            uint64_t base = 0;
            int matched = 0;
            for (int p = 0; p < eh.e_phnum; p++) {
                Elf64_Phdr ph;
                fseek(elf, eh.e_phoff + p * eh.e_phentsize, SEEK_SET);
                if (fread(&ph, sizeof(ph), 1, elf) != 1) break;
                if (ph.p_type == PT_LOAD && (ph.p_flags & PF_X)
                    && (ph.p_offset & ~4095UL) == offset) {
                    base = start - (ph.p_vaddr & ~4095UL);
                    matched = 1;
                    break;
                }
            }
            fclose(elf);
            if (matched && load_dso_symbols(file, base, 0) == 0) {
                dsos[ndso - 1].start = start;
                dsos[ndso - 1].end = end;
            }
        }
    }
    fclose(maps);
}

static const char *resolve(uint64_t ip) {
    static char buf[700];
    for (int d = 0; d < ndso; d++) {
        if (ip < dsos[d].start || ip >= dsos[d].end) continue;
        struct sym *syms = dsos[d].syms;
        int n = dsos[d].n;
        uint64_t rel = ip - dsos[d].base;
        int lo = 0, hi = n - 1, best = -1;
        while (lo <= hi) {
            int mid = (lo + hi) / 2;
            if (syms[mid].addr <= rel) {
                best = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        if (best < 0 || rel - syms[best].addr >= syms[best].size)
            continue;
        const char *slash = strrchr(dsos[d].path, '/');
        snprintf(buf, sizeof(buf), "%.60s:%.500s",
                 slash ? slash + 1 : dsos[d].path, syms[best].name);
        return buf;
    }
    snprintf(buf, sizeof(buf), "?0x%" PRIx64, ip);
    return buf;
}

struct agg {
    char name[704];
    uint64_t count;
};

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--self-test") == 0) {
        load_dsos(getpid());
        const char *name = resolve((uint64_t)(uintptr_t)&main);
        if (!strstr(name, ":main")) {
            fprintf(stderr, "incorrect main symbol: %s\n", name);
            return 1;
        }
        if (resolve(1)[0] != '?') return 1;
        puts("ELF symbol resolution passed");
        return 0;
    }
    if (argc != 3) {
        fprintf(stderr, "usage: ipsample PID SECONDS\n");
        return 2;
    }
    pid_t pid = (pid_t)atoi(argv[1]);
    long secs = atol(argv[2]);

    load_dsos(pid);
    for (int i = 0; i < ndso; i++)
        fprintf(stderr, "dso %s base=%" PRIu64 " (%d syms)\n",
                dsos[i].path, dsos[i].base, dsos[i].n);
    if (ndso == 0) {
        fprintf(stderr, "no symbolisable objects mapped\n");
        return 2;
    }

    struct {
        int fd;
        struct perf_event_mmap_page *mp;
        uint64_t prev;
    } ev[512];
    int nev = 0;
    char taskdir[64];
    snprintf(taskdir, sizeof(taskdir), "/proc/%d/task", pid);
    DIR *d = opendir(taskdir);
    if (!d) {
        perror("opendir tasks");
        return 2;
    }
    struct dirent *de;
    while ((de = readdir(d)) && nev < 512) {
        if (de->d_name[0] == '.')
            continue;
        pid_t tid = atoi(de->d_name);
        struct perf_event_attr attr;
        memset(&attr, 0, sizeof(attr));
        attr.size = sizeof(attr);
        attr.type = PERF_TYPE_HARDWARE;
        attr.config = PERF_COUNT_HW_CPU_CYCLES;
        attr.exclude_kernel = 1;
        int probe = syscall(__NR_perf_event_open, &attr, tid, -1, -1, 0);
        if (probe < 0) {
            attr.type = PERF_TYPE_SOFTWARE;
            attr.config = PERF_COUNT_SW_CPU_CLOCK;
        }
        if (probe >= 0) close(probe);
        attr.freq = 1;
        attr.sample_freq = 1000;
        attr.wakeup_events = 1;
        attr.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_TID;
        attr.exclude_kernel = 1;
        attr.exclude_guest = 1;
        int fd = (int)perf_event_open(&attr, tid, -1, -1, 0);
        if (fd < 0)
            continue;
        size_t page = sysconf(_SC_PAGESIZE);
        void *ring = mmap(NULL, (8 + 1) * page, PROT_READ | PROT_WRITE,
                          MAP_SHARED, fd, 0);
        if (ring == MAP_FAILED) {
            close(fd);
            continue;
        }
        ev[nev].fd = fd;
        ev[nev].mp = ring;
        ev[nev].prev = 0;
        nev++;
    }
    closedir(d);
    fprintf(stderr, "sampling %d threads\n", nev);
    if (nev == 0) {
        fprintf(stderr, "no events opened: %s\n", strerror(errno));
        return 2;
    }

    int acap = 8192, naggs = 0;
    struct agg *agg = malloc(acap * sizeof(*agg));
    uint64_t total = 0, unknown = 0;
    size_t page = sysconf(_SC_PAGESIZE);
    struct pollfd pfds[512];
    for (int i = 0; i < nev; i++) {
        pfds[i].fd = ev[i].fd;
        pfds[i].events = POLLIN;
    }
    long end = time(NULL) + secs;
    while (time(NULL) < end) {
        poll(pfds, nev, 200);
        for (int i = 0; i < nev; i++) {
            struct perf_event_mmap_page *mp = ev[i].mp;
            uint64_t head = __atomic_load_n(&mp->data_head, __ATOMIC_ACQUIRE);
            if (head == ev[i].prev)
                continue;
            unsigned char *data = (unsigned char *)mp + page;
            uint64_t mask = (8 * page) - 1;
            while (ev[i].prev < head) {
                struct perf_event_header *hdr =
                    (struct perf_event_header *)(data + (ev[i].prev & mask));
                if (hdr->type == PERF_RECORD_SAMPLE) {
                    uint64_t ip = *(uint64_t *)(data + ((ev[i].prev + 8) & mask));
                    const char *sym = resolve(ip);
                    int found = 0;
                    for (int k = 0; k < naggs; k++) {
                        if (strcmp(agg[k].name, sym) == 0) {
                            agg[k].count++;
                            found = 1;
                            break;
                        }
                    }
                    if (!found && naggs < acap) {
                        snprintf(agg[naggs].name, sizeof(agg[naggs].name), "%s", sym);
                        agg[naggs].count = 1;
                        naggs++;
                    }
                    total++;
                    if (sym[0] == '?')
                        unknown++;
                }
                ev[i].prev += hdr->size;
                if (hdr->size == 0)
                    break;
            }
            __atomic_store_n(&mp->data_tail, head, __ATOMIC_RELEASE);
        }
    }
    for (int i = 0; i < nev; i++)
        ioctl(ev[i].fd, PERF_EVENT_IOC_DISABLE, 0);

    for (int i = 0; i < naggs; i++)
        for (int j = i + 1; j < naggs; j++)
            if (agg[j].count > agg[i].count) {
                struct agg tmp = agg[i];
                agg[i] = agg[j];
                agg[j] = tmp;
            }
    printf("%" PRIu64 " samples (%" PRIu64 " unresolved)\n\n", total, unknown);
    for (int i = 0; i < naggs && i < 40; i++)
        printf("%6.2f%%  %s\n", agg[i].count * 100.0 / (total ? total : 1),
               agg[i].name);
    return 0;
}

/* randblke: the bench's guest-side direct-I/O tool (bench/README.md).
 *
 * The Ubuntu template ships no fio, and GNU dd's iflag=direct buffer is
 * rejected with EINVAL by the virtio-blk device (an aligned preadv on the
 * same device works), so the bench copies this statically linked binary in
 * through the agent's exec stdin and uses it for every direct-I/O
 * measurement: sequential and random, read and write, against the fresh
 * qcow2 overlay (the rootfs block device for reads, a preallocated file on
 * it for writes).
 *
 * All I/O is O_DIRECT with page-aligned buffers. Random offsets come from a
 * fixed-seed xorshift inside a caller-chosen byte span (the template's
 * allocated extent, so reads do real I/O instead of hitting the zero fast
 * path). Writes are closed with fdatasync inside the timed window.
 *
 * Output is one JSON line: {"ops":N,"bytes":N,"secs":T,"iops":F}
 *
 * Build: cc -O2 -static -o randblke randblke.c
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/fs.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define BS 4096
#define SEQ_BS (1024 * 1024)

static double now_secs(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

static void usage(void) {
    fprintf(stderr,
            "usage: randblke randread PATH OPS [SEED] [START] [END]\n"
            "       randblke randwrite PATH OPS [SEED]\n"
            "       randblke seqread PATH [BS] [START] [END]\n"
            "       randblke seqwrite PATH TOTAL [BS]\n"
            "  *read ops stay inside [START, END) (bytes; END defaults to the\n"
            "   device/file size). randwrite needs a preallocated file (span =\n"
            "   its size). seqwrite writes TOTAL bytes from offset 0. BS is the\n"
            "   sequential op size (default 1 MiB).\n");
    exit(2);
}

static int open_direct(const char *path, int extra_flags) {
    int fd = open(path, extra_flags | O_DIRECT, 0644);
    if (fd < 0) {
        fprintf(stderr, "open %s: %s\n", path, strerror(errno));
        exit(2);
    }
    return fd;
}

static unsigned long long object_size(int fd) {
    struct stat st;
    if (fstat(fd, &st) < 0) {
        perror("fstat");
        exit(2);
    }
    if (S_ISBLK(st.st_mode)) {
        unsigned long long size;
        if (ioctl(fd, BLKGETSIZE64, &size) < 0) {
            perror("BLKGETSIZE64");
            exit(2);
        }
        return size;
    }
    if (!S_ISREG(st.st_mode)) {
        fprintf(stderr, "not a block device or regular file\n");
        exit(2);
    }
    return (unsigned long long)st.st_size;
}

static void run_ops(int fd, int write_mode, size_t bs, long ops,
                    unsigned long long (*offset)(unsigned long long span, long i)) {
    unsigned long long size = object_size(fd);
    unsigned long long span = size / bs * bs;
    if (span < bs) {
        fprintf(stderr, "object too small for bs=%zu: %llu\n", bs, size);
        exit(2);
    }
    void *buf;
    if (posix_memalign(&buf, 4096, bs) != 0) {
        perror("posix_memalign");
        exit(2);
    }
    memset(buf, write_mode ? 0x5a : 0, bs);
    double t0 = now_secs();
    long done = 0;
    for (long i = 0; i < ops; i++) {
        unsigned long long off = offset(span, i);
        ssize_t n = write_mode ? pwrite(fd, buf, bs, (off_t)off)
                               : pread(fd, buf, bs, (off_t)off);
        if (n < 0) {
            fprintf(stderr, "op %ld at %llu (bs=%zu): %s\n", i, off, bs, strerror(errno));
            exit(3);
        }
        if (n != (ssize_t)bs) {
            fprintf(stderr, "short op %ld at %llu: %zd of %zu\n", i, off, n, bs);
            exit(3);
        }
        done++;
    }
    if (write_mode && fdatasync(fd) < 0) {
        perror("fdatasync");
        exit(3);
    }
    double secs = now_secs() - t0;
    printf("{\"ops\":%ld,\"bytes\":%ld,\"bs\":%zu,\"secs\":%.6f,\"iops\":%.1f}\n",
           done, done * (long)bs, bs, secs, done / secs);
    free(buf);
}

/* Fixed-seed xorshift64*: the same offsets on every run, every backend. */
static unsigned long g_state;
static unsigned long long rand_offset(unsigned long long span, long i) {
    (void)i;
    g_state ^= g_state << 13;
    g_state ^= g_state >> 7;
    g_state ^= g_state << 17;
    return (g_state % (span / BS)) * BS;
}

static unsigned long long seq_offset(unsigned long long span, long i) {
    /* Walk forward, wrapping at the object end. */
    unsigned long long step = (unsigned long long)i * BS;
    return step % span;
}

int main(int argc, char **argv) {
    if (argc < 3) usage();
    const char *mode = argv[1];
    const char *path = argv[2];
    long ops;
    size_t bs;

    if (strcmp(mode, "randread") == 0 && argc >= 4 && argc <= 7) {
        ops = atol(argv[3]);
        g_state = argc > 4 ? strtoul(argv[4], NULL, 0) : 42;
        g_state = g_state ? g_state : 1;
        int fd = open_direct(path, O_RDONLY);
        unsigned long long start = argc > 5 ? strtoull(argv[5], NULL, 0) : 0;
        unsigned long long end = argc > 6 ? strtoull(argv[6], NULL, 0) : 0;
        unsigned long long size = object_size(fd);
        if (!end || end > size) end = size;
        if (start + BS > end) {
            fprintf(stderr, "span too small\n");
            return 2;
        }
        /* Fold the span into the offset generator via a private wrapper. */
        unsigned long long span = (end - start) / BS * BS;
        void *buf;
        if (posix_memalign(&buf, 4096, BS) != 0) exit(2);
        memset(buf, 0, BS);
        double t0 = now_secs();
        long done = 0;
        for (long i = 0; i < ops; i++) {
            unsigned long long off = start + rand_offset(span, i);
            ssize_t n = pread(fd, buf, BS, (off_t)off);
            if (n < 0) {
                fprintf(stderr, "op %ld at %llu: %s\n", i, off, strerror(errno));
                return 3;
            }
            if (n != BS) {
                fprintf(stderr, "short op %ld at %llu: %zd\n", i, off, n);
                return 3;
            }
            done++;
        }
        double secs = now_secs() - t0;
        printf("{\"ops\":%ld,\"bytes\":%ld,\"bs\":%d,\"secs\":%.6f,\"iops\":%.1f}\n",
               done, done * BS, BS, secs, done / secs);
        return 0;
    }
    if (strcmp(mode, "randwrite") == 0 && (argc == 4 || argc == 5)) {
        ops = atol(argv[3]);
        g_state = argc > 4 ? strtoul(argv[4], NULL, 0) : 7;
        g_state = g_state ? g_state : 1;
        int fd = open_direct(path, O_WRONLY);
        run_ops(fd, 1, BS, ops, rand_offset);
        return 0;
    }
    if (strcmp(mode, "seqread") == 0 && argc >= 3 && argc <= 6) {
        bs = argc > 3 ? (size_t)atol(argv[3]) : SEQ_BS;
        if (bs < 512 || bs % 512) usage();
        int fd = open_direct(path, O_RDONLY);
        unsigned long long start = argc > 4 ? strtoull(argv[4], NULL, 0) : 0;
        unsigned long long end = argc > 5 ? strtoull(argv[5], NULL, 0) : 0;
        unsigned long long size = object_size(fd);
        if (!end || end > size) end = size;
        if (start + bs > end) {
            fprintf(stderr, "span too small\n");
            return 2;
        }
        unsigned long long span = (end - start) / bs * bs;
        long ops = (long)(span / bs);
        void *buf;
        if (posix_memalign(&buf, 4096, bs) != 0) exit(2);
        memset(buf, 0, bs);
        double t0 = now_secs();
        long done = 0;
        for (long i = 0; i < ops; i++) {
            unsigned long long off = start + (unsigned long long)i * bs;
            ssize_t n = pread(fd, buf, bs, (off_t)off);
            if (n < 0) {
                fprintf(stderr, "op %ld at %llu: %s\n", i, off, strerror(errno));
                return 3;
            }
            done++;
        }
        double secs = now_secs() - t0;
        printf("{\"ops\":%ld,\"bytes\":%ld,\"bs\":%zu,\"secs\":%.6f,\"iops\":%.1f}\n",
               done, done * (long)bs, bs, secs, done / secs);
        free(buf);
        return 0;
    }
    if (strcmp(mode, "seqwrite") == 0 && (argc == 4 || argc == 5)) {
        long long total = atoll(argv[3]);
        bs = argc > 4 ? (size_t)atol(argv[4]) : SEQ_BS;
        if (total <= 0 || bs < 512 || bs % 512 || total % (long long)bs) usage();
        int fd = open_direct(path, O_WRONLY | O_CREAT | O_TRUNC);
        void *buf;
        if (posix_memalign(&buf, 4096, bs) != 0) exit(2);
        memset(buf, 0x5a, bs);
        long ops = (long)(total / (long long)bs);
        double t0 = now_secs();
        long done = 0;
        for (long i = 0; i < ops; i++) {
            unsigned long long off = (unsigned long long)i * bs;
            ssize_t n = pwrite(fd, buf, bs, (off_t)off);
            if (n < 0) {
                fprintf(stderr, "op %ld at %llu: %s\n", i, off, strerror(errno));
                return 3;
            }
            if (n != (ssize_t)bs) {
                fprintf(stderr, "short op %ld: %zd\n", i, n);
                return 3;
            }
            done++;
        }
        if (fdatasync(fd) < 0) {
            perror("fdatasync");
            return 3;
        }
        double secs = now_secs() - t0;
        printf("{\"ops\":%ld,\"bytes\":%ld,\"bs\":%zu,\"secs\":%.6f,\"iops\":%.1f}\n",
               done, done * (long)bs, bs, secs, done / secs);
        free(buf);
        return 0;
    }
    usage();
    return 2;
}

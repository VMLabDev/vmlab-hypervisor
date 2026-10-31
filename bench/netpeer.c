/* netpeer: the host side of the bench's network segment (bench/README.md).
 *
 * vmlab attaches every VM NIC as a stream-socket client of its switch,
 * exchanging 4-byte big-endian length-prefixed Ethernet frames
 * (vmlab src/net/framing.rs). The bench serves that socket with this peer
 * instead of the daemon's switch: one process, no lab, the same framing.
 *
 * Modes:
 *   netpeer peer SOCK CTL OWN_IP OWN_MAC GUEST_IP GUEST_MAC UDP_PORT
 *       One NIC connection. Answers ARP for OWN_IP, counts received UDP
 *       payload bytes, and on request (JSON lines on CTL) blasts UDP frames
 *       at GUEST_MAC for a fixed window. CTL commands:
 *         {"cmd":"blast","secs":N,"payload":L}  -> {"ok":true} when done
 *         {"cmd":"report"}                      -> {"rx_bytes":..,"rx_frames":..,
 *                                                   "rx_arp":..,"tx_frames":..}
 *       `report` snapshots and resets the counters, so two reports bracket
 *       a measurement window.
 *   netpeer hub SOCK
 *       Two NIC connections, raw bytes pumped both ways (a two-port hub:
 *       exactly what a segment switch does between two isolated guests).
 *       Prints {"a_to_b":..,"b_to_a":..} byte totals on exit.
 *
 * Build: cc -O2 -static -o netpeer netpeer.c
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <netdb.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define MAX_FRAME (65536 + 14 + 4) /* vmlab framing cap */
#define ETH_HDR 14
#define IP_HDR 20
#define UDP_HDR 8

static void die(const char *what) {
    perror(what);
    exit(2);
}

static double now_secs(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

/* ------------------------------------------------------------------ */
/* Ethernet/IP/UDP frame construction and parsing (no library deps)    */
/* ------------------------------------------------------------------ */

static int parse_mac(const char *text, uint8_t out[6]) {
    unsigned v[6];
    if (sscanf(text, "%x:%x:%x:%x:%x:%x", &v[0], &v[1], &v[2], &v[3], &v[4], &v[5]) != 6)
        return -1;
    for (int i = 0; i < 6; i++) {
        if (v[i] > 0xff) return -1;
        out[i] = (uint8_t)v[i];
    }
    return 0;
}

static uint32_t parse_ip(const char *text) {
    struct in_addr addr;
    if (inet_pton(AF_INET, text, &addr) != 1) die("inet_pton");
    return ntohl(addr.s_addr);
}

static uint16_t checksum(const uint8_t *data, size_t len) {
    uint32_t sum = 0;
    for (size_t i = 0; i + 1 < len; i += 2) sum += (data[i] << 8) | data[i + 1];
    if (len & 1) sum += data[len - 1] << 8;
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return (uint16_t)(~sum & 0xffff);
}

static void build_udp_frame(uint8_t *frame, size_t payload_len,
                            const uint8_t dst_mac[6], const uint8_t src_mac[6],
                            uint32_t src_ip, uint32_t dst_ip, uint16_t port,
                            uint16_t ip_id, uint8_t fill) {
    memset(frame, 0, ETH_HDR + IP_HDR + UDP_HDR);
    memcpy(frame + 0, dst_mac, 6);
    memcpy(frame + 6, src_mac, 6);
    frame[12] = 0x08;
    frame[13] = 0x00; /* EtherType IPv4 */
    uint8_t *ip = frame + ETH_HDR;
    ip[0] = 0x45; /* v4, IHL 5 */
    ip[2] = (uint8_t)((IP_HDR + UDP_HDR + payload_len) >> 8);
    ip[3] = (uint8_t)((IP_HDR + UDP_HDR + payload_len) & 0xff);
    ip[4] = (uint8_t)(ip_id >> 8);
    ip[5] = (uint8_t)(ip_id & 0xff);
    ip[6] = 0x40; /* DF, guest stacks respect it and never fragment */
    ip[8] = 64;   /* hops */
    ip[9] = 17;   /* UDP */
    ip[12] = (uint8_t)(src_ip >> 24);
    ip[13] = (uint8_t)(src_ip >> 16);
    ip[14] = (uint8_t)(src_ip >> 8);
    ip[15] = (uint8_t)(src_ip & 0xff);
    ip[16] = (uint8_t)(dst_ip >> 24);
    ip[17] = (uint8_t)(dst_ip >> 16);
    ip[18] = (uint8_t)(dst_ip >> 8);
    ip[19] = (uint8_t)(dst_ip & 0xff);
    uint16_t csum = checksum(ip, IP_HDR);
    ip[10] = (uint8_t)(csum >> 8);
    ip[11] = (uint8_t)(csum & 0xff);
    uint8_t *udp = frame + ETH_HDR + IP_HDR;
    udp[0] = (uint8_t)(port >> 8);
    udp[1] = (uint8_t)(port & 0xff);
    udp[2] = (uint8_t)(port >> 8);
    udp[3] = (uint8_t)(port & 0xff);
    uint16_t ulen = (uint16_t)(UDP_HDR + payload_len);
    udp[4] = (uint8_t)(ulen >> 8);
    udp[5] = (uint8_t)(ulen & 0xff);
    /* udp[6..7] checksum 0: legal "not computed" for IPv4. */
    memset(frame + ETH_HDR + IP_HDR + UDP_HDR, fill, payload_len);
}

/* Is `frame` an ARP request for `want_ip`? Fills `reply` if so. */
static int arp_request_for(const uint8_t *frame, size_t len, uint32_t want_ip,
                           const uint8_t own_mac[6], uint8_t *reply) {
    if (len < ETH_HDR + 28) return 0;
    if (frame[12] != 0x08 || frame[13] != 0x06) return 0; /* not ARP */
    if (frame[ETH_HDR + 6] != 0 || frame[ETH_HDR + 7] != 1) return 0; /* not request */
    uint32_t tip = ((uint32_t)frame[ETH_HDR + 24] << 24) | ((uint32_t)frame[ETH_HDR + 25] << 16) |
                   ((uint32_t)frame[ETH_HDR + 26] << 8) | (uint32_t)frame[ETH_HDR + 27];
    if (tip != want_ip) return 0;
    memcpy(reply + 0, frame + 6, 6);             /* to the asker */
    memcpy(reply + 6, own_mac, 6);
    reply[12] = 0x08;
    reply[13] = 0x06;
    memcpy(reply + 14, frame + 14, 8); /* htype/ptype/hlen/plen/oper=request */
    reply[ETH_HDR + 6] = 0;
    reply[ETH_HDR + 7] = 2; /* reply */
    memcpy(reply + ETH_HDR + 8, own_mac, 6); /* sender MAC */
    memcpy(reply + ETH_HDR + 14, frame + ETH_HDR + 24, 4); /* sender IP = asked */
    memcpy(reply + ETH_HDR + 18, frame + 6, 6); /* target MAC = asker */
    memcpy(reply + ETH_HDR + 22, frame + ETH_HDR + 14, 4); /* target IP */
    return ETH_HDR + 28;
}

/* UDP payload length of an IPv4 UDP frame, or -1. */
static ssize_t udp_payload_len(const uint8_t *frame, size_t len) {
    if (len < ETH_HDR + IP_HDR + UDP_HDR) return -1;
    if (frame[12] != 0x08 || frame[13] != 0x00) return -1;
    if (frame[ETH_HDR + 9] != 17) return -1;
    size_t ip_total = ((size_t)frame[ETH_HDR + 2] << 8) | frame[ETH_HDR + 3];
    if (ip_total < IP_HDR + UDP_HDR || ETH_HDR + ip_total > len) return -1;
    return (ssize_t)(ip_total - IP_HDR - UDP_HDR);
}

/* ------------------------------------------------------------------ */
/* Framed stream I/O                                                   */
/* ------------------------------------------------------------------ */

static ssize_t read_full(int fd, uint8_t *buf, size_t len) {
    size_t done = 0;
    while (done < len) {
        ssize_t n = read(fd, buf + done, len - done);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        done += (size_t)n;
    }
    return (ssize_t)done;
}

static int write_all(int fd, const uint8_t *buf, size_t len) {
    size_t done = 0;
    while (done < len) {
        ssize_t n = write(fd, buf + done, len - done);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) return -1;
        done += (size_t)n;
    }
    return 0;
}

static int send_frame(int fd, const uint8_t *frame, size_t len) {
    uint8_t prefix[4] = {(uint8_t)(len >> 24), (uint8_t)(len >> 16), (uint8_t)(len >> 8),
                         (uint8_t)len};
    if (write_all(fd, prefix, 4) < 0) return -1;
    return write_all(fd, frame, len);
}

/* ------------------------------------------------------------------ */
/* peer mode                                                           */
/* ------------------------------------------------------------------ */

struct counters {
    uint64_t rx_bytes;   /* UDP payload bytes received */
    uint64_t rx_frames;  /* UDP frames received */
    uint64_t rx_arp;     /* ARP requests answered */
    uint64_t tx_frames;  /* frames sent (ARP replies + blast) */
};

struct peer {
    int nic_fd;
    int blast_fd; /* -1 when idle */
    pthread_mutex_t lock;
    struct counters c;
    uint8_t frame[MAX_FRAME + 4];
};

/* Set once from argv before threads start; the two blast parameters arrive
 * per control command. */
static uint8_t g_own_mac[6], g_guest_mac[6];
static uint32_t g_own_ip, g_guest_ip;
static uint16_t g_port;
static double g_blast_secs;
static size_t g_blast_payload;

static void *blast_thread(void *arg) {
    struct peer *p = arg;
    double secs = g_blast_secs;
    size_t payload = g_blast_payload;
    uint8_t *frame = malloc(ETH_HDR + IP_HDR + UDP_HDR + payload);
    if (!frame) return NULL;
    double end = now_secs() + secs;
    uint16_t ip_id = 1;
    uint64_t sent = 0;
    while (now_secs() < end) {
        build_udp_frame(frame, payload, g_guest_mac, g_own_mac, g_own_ip, g_guest_ip,
                        g_port, ip_id++, (uint8_t)(sent & 0xff));
        int fd;
        pthread_mutex_lock(&p->lock);
        fd = p->nic_fd;
        pthread_mutex_unlock(&p->lock);
        if (fd < 0) break;
        if (send_frame(fd, frame, ETH_HDR + IP_HDR + UDP_HDR + payload) < 0) break;
        sent++;
    }
    pthread_mutex_lock(&p->lock);
    p->c.tx_frames += sent;
    p->blast_fd = -1;
    pthread_mutex_unlock(&p->lock);
    free(frame);
    return NULL;
}

static void *nic_thread(void *arg) {
    struct peer *p = arg;
    uint8_t prefix[4], reply[ETH_HDR + 28];
    for (;;) {
        if (read_full(p->nic_fd, prefix, 4) < 0) break;
        size_t len = ((size_t)prefix[0] << 24) | ((size_t)prefix[1] << 16) |
                     ((size_t)prefix[2] << 8) | (size_t)prefix[3];
        if (len > MAX_FRAME) break;
        if (read_full(p->nic_fd, p->frame, len) < 0) break;
        int arp = arp_request_for(p->frame, len, g_own_ip, g_own_mac, reply);
        if (arp > 0) {
            pthread_mutex_lock(&p->lock);
            if (p->nic_fd >= 0 && send_frame(p->nic_fd, reply, (size_t)arp) == 0) p->c.tx_frames++;
            p->c.rx_arp++;
            pthread_mutex_unlock(&p->lock);
            continue;
        }
        ssize_t payload = udp_payload_len(p->frame, len);
        if (payload > 0) {
            pthread_mutex_lock(&p->lock);
            /* Ethernet-level bytes: the same quantity the guest's
             * interface counter reports on the RX side. */
            p->c.rx_bytes += (uint64_t)len;
            p->c.rx_frames++;
            pthread_mutex_unlock(&p->lock);
        }
    }
    pthread_mutex_lock(&p->lock);
    p->nic_fd = -1;
    pthread_mutex_unlock(&p->lock);
    return NULL;
}

static int listen_unix(const char *path, int backlog) {
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    if (strlen(path) >= sizeof(addr.sun_path)) {
        fprintf(stderr, "socket path too long\n");
        exit(2);
    }
    strcpy(addr.sun_path, path);
    unlink(path);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) die("socket");
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) die("bind");
    if (listen(fd, backlog) < 0) die("listen");
    return fd;
}

static int run_peer(const char *sock_path, const char *ctl_path, const char *own_ip,
                    const char *own_mac, const char *guest_ip, const char *guest_mac,
                    const char *port) {
    if (parse_mac(own_mac, g_own_mac) < 0 || parse_mac(guest_mac, g_guest_mac) < 0) {
        fprintf(stderr, "bad MAC\n");
        return 2;
    }
    g_own_ip = parse_ip(own_ip);
    g_guest_ip = parse_ip(guest_ip);
    g_port = (uint16_t)atoi(port);

    int nic_listen = listen_unix(sock_path, 1);
    int ctl_listen = listen_unix(ctl_path, 1);
    printf("{\"type\":\"listening\"}\n");
    fflush(stdout);

    int nic_fd = accept(nic_listen, NULL, NULL);
    if (nic_fd < 0) die("accept nic");
    close(nic_listen);

    struct peer *p = calloc(1, sizeof(*p));
    p->nic_fd = nic_fd;
    p->blast_fd = -1;
    pthread_mutex_init(&p->lock, NULL);

    /* Serve the NIC the moment it connects; the control client comes and
     * goes later, around each measurement window. */
    pthread_t nic;
    pthread_create(&nic, NULL, nic_thread, p);

    int ctl_fd = accept(ctl_listen, NULL, NULL);
    if (ctl_fd < 0) die("accept ctl");
    close(ctl_listen);


    /* Control: one JSON line in, one JSON line out. Minimal parsing: the
     * bench sends exactly two command shapes. */
    char line[512];
    FILE *ctl = fdopen(ctl_fd, "r+");
    while (fgets(line, sizeof(line), ctl)) {
        if (strstr(line, "\"report\"")) {
            pthread_mutex_lock(&p->lock);
            fprintf(ctl,
                    "{\"ok\":true,\"rx_bytes\":%" PRIu64 ",\"rx_frames\":%" PRIu64
                    ",\"rx_arp\":%" PRIu64 ",\"tx_frames\":%" PRIu64 "}\n",
                    p->c.rx_bytes, p->c.rx_frames, p->c.rx_arp, p->c.tx_frames);
            memset(&p->c, 0, sizeof(p->c));
            pthread_mutex_unlock(&p->lock);
        } else if (strstr(line, "\"blast\"")) {
            double secs = 0;
            long payload = 0;
            char *s = strstr(line, "\"secs\":");
            char *l = strstr(line, "\"payload\":");
            if (s) secs = atof(s + 7);
            if (l) payload = atol(l + 10);
            if (secs <= 0 || payload <= 0 || payload > MAX_FRAME - ETH_HDR - IP_HDR - UDP_HDR) {
                fprintf(ctl, "{\"ok\":false,\"why\":\"bad blast args\"}\n");
            } else {
                pthread_mutex_lock(&p->lock);
                int busy = p->blast_fd >= 0;
                pthread_mutex_unlock(&p->lock);
                if (busy) {
                    fprintf(ctl, "{\"ok\":false,\"why\":\"blast already running\"}\n");
                } else {
                    p->blast_fd = 1; /* mark busy */
                    g_blast_secs = secs;
                    g_blast_payload = (size_t)payload;
                    pthread_t t;
                    pthread_create(&t, NULL, blast_thread, p);
                    pthread_detach(t);
                    fprintf(ctl, "{\"ok\":true,\"blast_started\":\"yes\"}\n");
                }
            }
        } else if (strstr(line, "\"quit\"")) {
            fprintf(ctl, "{\"ok\":true}\n");
            fflush(ctl);
            break;
        } else {
            fprintf(ctl, "{\"ok\":false,\"why\":\"unknown command\"}\n");
        }
        fflush(ctl);
    }
    pthread_mutex_lock(&p->lock);
    if (p->nic_fd >= 0) shutdown(p->nic_fd, SHUT_RDWR);
    pthread_mutex_unlock(&p->lock);
    pthread_join(nic, NULL);
    return 0;
}

/* ------------------------------------------------------------------ */
/* hub mode                                                            */
/* ------------------------------------------------------------------ */

static volatile sig_atomic_t g_stop = 0;
static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
}

static void pump(int from, int to, uint64_t *total) {
    char buf[128 * 1024];
    ssize_t n = read(from, buf, sizeof(buf));
    if (n <= 0) return;
    if (write_all(to, (const uint8_t *)buf, (size_t)n) < 0) return;
    *total += (uint64_t)n;
}

static int run_hub(const char *sock_path) {
    int listen_fd = listen_unix(sock_path, 2);
    printf("{\"type\":\"listening\"}\n");
    fflush(stdout);
    int a = accept(listen_fd, NULL, NULL);
    if (a < 0) die("accept a");
    int b = accept(listen_fd, NULL, NULL);
    if (b < 0) die("accept b");
    close(listen_fd);
    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    uint64_t a_to_b = 0, b_to_a = 0;
    struct pollfd fds[2] = {{a, POLLIN, 0}, {b, POLLIN, 0}};
    while (!g_stop) {
        fds[0].revents = fds[1].revents = 0;
        if (poll(fds, 2, 500) < 0) {
            if (errno == EINTR) continue;
            die("poll");
        }
        if (fds[0].revents & POLLIN) pump(a, b, &a_to_b);
        if (fds[1].revents & POLLIN) pump(b, a, &b_to_a);
        if ((fds[0].revents | fds[1].revents) & (POLLHUP | POLLERR)) break;
    }
    printf("{\"type\":\"hub\",\"a_to_b\":%" PRIu64 ",\"b_to_a\":%" PRIu64 "}\n", a_to_b, b_to_a);
    fflush(stdout);
    close(a);
    close(b);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc == 2 && strcmp(argv[1], "hub") == 0) {
        fprintf(stderr, "usage: netpeer hub SOCK\n");
        return 2;
    }
    if (argc == 3 && strcmp(argv[1], "hub") == 0) return run_hub(argv[2]);
    if (argc == 9 && strcmp(argv[1], "peer") == 0)
        return run_peer(argv[2], argv[3], argv[4], argv[5], argv[6], argv[7], argv[8]);
    fprintf(stderr,
            "usage:\n"
            "  netpeer peer SOCK CTL OWN_IP OWN_MAC GUEST_IP GUEST_MAC UDP_PORT\n"
            "  netpeer hub SOCK\n");
    return 2;
}

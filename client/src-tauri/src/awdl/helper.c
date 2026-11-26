// macOS：Airhand 的 AWDL 助手（Airhand 这边的说明见 awdl/mod.rs）。
//
// 由 launchd 以 root 身份按需启动（/Library/LaunchDaemons/app.airhand.awdl.plist，有连接来了才启动），
// 只做一件事：有远程控制会话"拿着"它的时候让 awdl0 一直关着——系统（隔空投送、接力、随航……）随时会把它打开，
// 路由套接字一报接口变了就再关上；最后一个拿着的连接断开（会话结束，Airhand 退出或者崩溃也一样）时恢复成
// 拿着之前的样子。没有连接 30 秒之后退出，下一次连接来了 launchd 再启动它。
//
// 协议（Unix 套接字，一行一条，都是 ASCII）：
//   助手 → 连上来的：  "airhand-awdl <版本>\n"
//   连上来的 → 助手：  "hold\n"           拿着：这条连接开着期间 awdl0 一直关着
//   助手 → 连上来的：  "ok\n"             已经关上了（或者这台 Mac 没有 awdl0）
//                     "error <errno>\n"  没关上（之后照样一直在关）
// 别的输入、太长的一行：断开这条连接。不接受任何参数（接口名、路径……都是写死的）。
//
// 调试：`app.airhand.awdl --socket <路径>` 不经过 launchd，自己监听这个路径（不是 root 时关不上 awdl0，回答 error）。

#include <errno.h>
#include <fcntl.h>
#include <launch.h>
#include <net/if.h>
#include <os/log.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/sockio.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

// 改了协议或者行为就加一：Airhand 读已安装的助手里的这一行，旧了就提示更新（见 awdl/helper.rs）
#define GREETING "airhand-awdl 1\n"
#define IFNAME "awdl0"
// 同时连着的最多这么多（每个远程控制会话一条，再多的直接断开）
#define MAX_CLIENTS 16
// 最多同时从 launchd 拿到这么多个监听套接字
#define MAX_LISTENERS 4
// 没有连接这么久就退出
#define IDLE_EXIT_MS (30 * 1000)
// 连上来却一直不说话的连接，这么久之后断开（不然助手退出不了）
#define SILENT_MS (10 * 1000)
// 拿着的时候，除了等路由套接字的通知，还隔这么久看一眼（唤醒之后、漏了通知）
#define RECHECK_MS 2000
// 系统和我们抢的时候（一直有东西要用 AWDL），一秒最多关这么多次，剩下的留给下一次定时检查
#define MAX_LOWER_PER_SEC 20

struct client {
    int fd;
    bool holds;
    long long since;
    size_t len;
    char buf[16];
};

static os_log_t logger;
static int ioc_fd = -1;
static int route_fd = -1;
static int sig_pipe[2] = {-1, -1};
static bool holding;
static bool was_up;
static long long window_start;
static int lowered_in_window;
static unsigned long lowered_total;
static struct client clients[MAX_CLIENTS];
static int nclients;

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void prepare_fd(int fd) {
    fcntl(fd, F_SETFD, FD_CLOEXEC);
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
}

/// awdl0 的标志。没有这个接口（没有 Wi-Fi 网卡）时 errno 是 ENXIO
static int get_flags(short *flags) {
    struct ifreq r;
    memset(&r, 0, sizeof r);
    strlcpy(r.ifr_name, IFNAME, sizeof r.ifr_name);
    if (ioctl(ioc_fd, SIOCGIFFLAGS, &r) < 0) return -1;
    *flags = r.ifr_flags;
    return 0;
}

/// 打开 / 关上 awdl0。返回 0 或者 errno；没有这个接口算成功
static int set_up(bool up) {
    struct ifreq r;
    memset(&r, 0, sizeof r);
    strlcpy(r.ifr_name, IFNAME, sizeof r.ifr_name);
    if (ioctl(ioc_fd, SIOCGIFFLAGS, &r) < 0) return errno == ENXIO ? 0 : errno;
    if (((r.ifr_flags & IFF_UP) != 0) == up) return 0;
    if (up) {
        r.ifr_flags |= IFF_UP;
    } else {
        r.ifr_flags &= ~IFF_UP;
    }
    if (ioctl(ioc_fd, SIOCSIFFLAGS, &r) < 0) return errno;
    return 0;
}

/// 拿着的时候：开着就关上
static int keep_down(void) {
    short flags;
    if (get_flags(&flags) < 0) return errno == ENXIO ? 0 : errno;
    if (!(flags & IFF_UP)) return 0;
    long long t = now_ms();
    if (t - window_start >= 1000) {
        window_start = t;
        lowered_in_window = 0;
    }
    if (lowered_in_window >= MAX_LOWER_PER_SEC) return 0;
    lowered_in_window++;
    int e = set_up(false);
    if (e == 0) {
        lowered_total++;
        os_log_debug(logger, "awdl0 was brought up again, took it down (%lu so far)", lowered_total);
    } else {
        os_log_error(logger, "can't take awdl0 down: %{public}s", strerror(e));
    }
    return e;
}

static int start_hold(void) {
    short flags;
    was_up = get_flags(&flags) == 0 && (flags & IFF_UP);
    lowered_total = 0;
    route_fd = socket(PF_ROUTE, SOCK_RAW, AF_UNSPEC);
    if (route_fd >= 0) {
        prepare_fd(route_fd);
    } else {
        os_log_error(logger, "can't open a routing socket: %{public}s", strerror(errno));
    }
    holding = true;
    os_log(logger, "pausing AWDL (awdl0 was %{public}s)", was_up ? "up" : "down");
    return keep_down();
}

static void stop_hold(void) {
    if (!holding) return;
    holding = false;
    if (route_fd >= 0) {
        close(route_fd);
        route_fd = -1;
    }
    int e = was_up ? set_up(true) : 0;
    if (e == 0) {
        os_log(logger, "AWDL resumed (took awdl0 down %lu times)", lowered_total);
    } else {
        os_log_error(logger, "can't bring awdl0 back up: %{public}s", strerror(e));
    }
}

static void drain_route(void) {
    char buf[2048];
    for (;;) {
        ssize_t n = read(route_fd, buf, sizeof buf);
        if (n > 0) continue;
        if (n < 0 && errno == EINTR) continue;
        break;
    }
}

static bool anyone_holds(void) {
    for (int i = 0; i < nclients; i++) {
        if (clients[i].holds) return true;
    }
    return false;
}

static void drop_client(int i) {
    close(clients[i].fd);
    clients[i] = clients[--nclients];
    if (holding && !anyone_holds()) stop_hold();
}

static void say(int fd, const char *s) {
    // 很短，套接字缓冲区一定放得下；对方已经走了也不要紧
    (void)!write(fd, s, strlen(s));
}

static void accept_client(int listener) {
    int fd = accept(listener, NULL, NULL);
    if (fd < 0) return;
    if (nclients == MAX_CLIENTS) {
        close(fd);
        return;
    }
    prepare_fd(fd);
    int on = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, sizeof on);
    uid_t uid = (uid_t)-1;
    gid_t gid;
    getpeereid(fd, &uid, &gid);
    os_log_debug(logger, "connection from uid %d", (int)uid);
    clients[nclients++] = (struct client){.fd = fd, .since = now_ms()};
    say(fd, GREETING);
}

/// 读这个连接说的话。返回 false：要断开它
static bool serve_client(struct client *c) {
    for (;;) {
        ssize_t n = read(c->fd, c->buf + c->len, sizeof c->buf - c->len);
        if (n == 0) return false;
        if (n < 0) return errno == EAGAIN || errno == EINTR;
        // 拿着之后对方不该再说什么
        if (c->holds) return false;
        c->len += (size_t)n;
        char *nl = memchr(c->buf, '\n', c->len);
        if (!nl) {
            if (c->len == sizeof c->buf) return false;
            continue;
        }
        *nl = 0;
        if (strcmp(c->buf, "hold") != 0) return false;
        c->holds = true;
        c->len = 0;
        int e = holding ? keep_down() : start_hold();
        if (e == 0) {
            say(c->fd, "ok\n");
        } else {
            char line[32];
            snprintf(line, sizeof line, "error %d\n", e);
            say(c->fd, line);
        }
    }
}

static void on_signal(int sig) {
    (void)sig;
    int saved = errno;
    (void)!write(sig_pipe[1], "x", 1);
    errno = saved;
}

/// 调试用：自己监听 `path`
static int listen_at(const char *path) {
    struct sockaddr_un a;
    memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    if (strlcpy(a.sun_path, path, sizeof a.sun_path) >= sizeof a.sun_path) return -1;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    unlink(path);
    if (bind(fd, (struct sockaddr *)&a, sizeof a) < 0 || listen(fd, 8) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

int main(int argc, char **argv) {
    logger = os_log_create("app.airhand.awdl", "helper");
    signal(SIGPIPE, SIG_IGN);

    int listeners[MAX_LISTENERS];
    size_t nlisteners = 0;
    const char *own_path = NULL;
    if (argc == 3 && strcmp(argv[1], "--socket") == 0) {
        own_path = argv[2];
        int fd = listen_at(own_path);
        if (fd < 0) {
            fprintf(stderr, "can't listen at %s: %s\n", own_path, strerror(errno));
            return 1;
        }
        listeners[nlisteners++] = fd;
    } else if (argc == 1) {
        int *fds = NULL;
        size_t n = 0;
        int e = launch_activate_socket("Listeners", &fds, &n);
        if (e != 0) {
            os_log_error(logger, "no socket from launchd: %{public}s", strerror(e));
            return 1;
        }
        for (size_t i = 0; i < n; i++) {
            if (nlisteners < MAX_LISTENERS) {
                listeners[nlisteners++] = fds[i];
            } else {
                close(fds[i]);
            }
        }
        free(fds);
    } else {
        fprintf(stderr, "usage: %s [--socket <path>]\n", argv[0]);
        return 2;
    }
    for (size_t i = 0; i < nlisteners; i++) prepare_fd(listeners[i]);

    ioc_fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (ioc_fd < 0 || pipe(sig_pipe) < 0) {
        os_log_error(logger, "can't set up: %{public}s", strerror(errno));
        return 1;
    }
    prepare_fd(ioc_fd);
    prepare_fd(sig_pipe[0]);
    prepare_fd(sig_pipe[1]);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGHUP, &sa, NULL);

    long long last_activity = now_ms();
    long long last_check = last_activity;
    for (;;) {
        struct pollfd fds[2 + MAX_LISTENERS + MAX_CLIENTS];
        int n = 0;
        fds[n++] = (struct pollfd){.fd = sig_pipe[0], .events = POLLIN};
        int first_listener = n;
        for (size_t i = 0; i < nlisteners; i++) {
            fds[n++] = (struct pollfd){.fd = listeners[i], .events = POLLIN};
        }
        int route_at = -1;
        if (holding && route_fd >= 0) {
            route_at = n;
            fds[n++] = (struct pollfd){.fd = route_fd, .events = POLLIN};
        }
        int first_client = n;
        for (int i = 0; i < nclients; i++) {
            fds[n++] = (struct pollfd){.fd = clients[i].fd, .events = POLLIN};
        }

        long long t = now_ms();
        int timeout;
        if (holding) {
            timeout = (int)(last_check + RECHECK_MS - t);
        } else if (nclients > 0) {
            timeout = 1000;
        } else {
            timeout = (int)(last_activity + IDLE_EXIT_MS - t);
        }
        if (timeout < 0) timeout = 0;

        int ready = poll(fds, (nfds_t)n, timeout);
        if (ready < 0 && errno != EINTR) {
            os_log_error(logger, "poll: %{public}s", strerror(errno));
            break;
        }
        if (ready > 0 && fds[0].revents) break;
        t = now_ms();

        if (ready > 0) {
            // 先看已有的连接（从后往前：断开的那个由最后一个顶上）
            for (int i = nclients - 1; i >= 0; i--) {
                short ev = fds[first_client + i].revents;
                if (ev && !serve_client(&clients[i])) drop_client(i);
            }
            for (size_t i = 0; i < nlisteners; i++) {
                if (fds[first_listener + (int)i].revents & POLLIN) accept_client(listeners[i]);
            }
            if (route_at >= 0 && holding && route_fd >= 0 && fds[route_at].revents) {
                drain_route();
                keep_down();
            }
        }
        // 连上来却一直不说话的
        for (int i = nclients - 1; i >= 0; i--) {
            if (!clients[i].holds && t - clients[i].since >= SILENT_MS) drop_client(i);
        }
        if (holding && t - last_check >= RECHECK_MS) {
            last_check = t;
            keep_down();
        }
        if (nclients > 0 || holding) {
            last_activity = t;
        } else if (t - last_activity >= IDLE_EXIT_MS) {
            break;
        }
    }
    stop_hold();
    if (own_path) unlink(own_path);
    return 0;
}

// Test of the MayOS Linux layer pieces Firefox relies on: signal
// handlers, SIGSEGV recovery, masks, SIGCHLD, timerfd, pwrite, select,
// symlinks, /proc and main-thread stack discovery (mremap probing).
// Build: musl-gcc -static -O2 -o linux-signals signals.c -lpthread
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/stat.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;
#define CHECK(c, name) do { if (c) printf("ok   %s\n", name); else { printf("FAIL %s (errno %d)\n", name, errno); failures++; } } while (0)

static volatile sig_atomic_t got_usr1, got_chld, usr1_info_ok;
static sigjmp_buf jb;

static void on_usr1(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    got_usr1 = sig;
    usr1_info_ok = si->si_signo == SIGUSR1;
}
static void on_chld(int sig) { (void)sig; got_chld = 1; }
static void on_segv(int sig) { (void)sig; siglongjmp(jb, 1); }

static void *thread_fn(void *arg) { (void)arg; return (void *)42; }
static volatile sig_atomic_t got_pipe;
static void on_pipe(int s) { (void)s; got_pipe = 1; }
static void on_usr2(int s) { (void)s; }
static pthread_t main_thread;
static void *kick(void *arg) { (void)arg; usleep(50000); pthread_kill(main_thread, SIGUSR2); return 0; }

static void sockets(void) {
    int sv[2];
    char b[16];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    fcntl(sv[0], F_SETFL, fcntl(sv[0], F_GETFL) | O_NONBLOCK);
    CHECK((fcntl(sv[0], F_GETFL) & O_NONBLOCK) && read(sv[0], b, 1) == -1 && errno == EAGAIN, "unix O_NONBLOCK via fcntl");
    CHECK(recv(sv[1], b, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN, "recv MSG_DONTWAIT");
    int one = 1;
    ioctl(sv[1], FIONBIO, &one);
    CHECK(read(sv[1], b, 1) == -1 && errno == EAGAIN, "FIONBIO");
    write(sv[1], "hello", 5);
    CHECK(recv(sv[0], b, 5, MSG_PEEK) == 5 && read(sv[0], b, 16) == 5 && !memcmp(b, "hello", 5), "MSG_PEEK");

    int pp[2];
    pipe2(pp, O_NONBLOCK);
    CHECK(read(pp[0], b, 1) == -1 && errno == EAGAIN, "pipe2 O_NONBLOCK");
    int q[2];
    pipe(q);
    fcntl(q[0], F_SETFL, O_NONBLOCK);
    CHECK(read(q[0], b, 1) == -1 && errno == EAGAIN && (fcntl(q[0], F_GETFL) & O_NONBLOCK), "pipe fcntl O_NONBLOCK");

    // Listening socket, accept4(SOCK_NONBLOCK)
    int ls = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_un a = {0};
    a.sun_family = AF_UNIX;
    strcpy(a.sun_path, "/tmp/sigtest.sock");
    unlink(a.sun_path);
    bind(ls, (struct sockaddr *)&a, sizeof a);
    listen(ls, 4);
    CHECK(accept4(ls, 0, 0, SOCK_NONBLOCK) == -1 && errno == EAGAIN, "accept on nonblocking listener");
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    connect(c, (struct sockaddr *)&a, sizeof a);
    int s = accept4(ls, 0, 0, SOCK_NONBLOCK | SOCK_CLOEXEC);
    CHECK(s >= 0 && read(s, b, 1) == -1 && errno == EAGAIN && (fcntl(s, F_GETFL) & O_NONBLOCK), "accept4 SOCK_NONBLOCK");

    // epoll, level-triggered, on a Unix socket
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLRDHUP, .data.fd = s}, out[4];
    epoll_ctl(ep, EPOLL_CTL_ADD, s, &ev);
    CHECK(epoll_wait(ep, out, 4, 0) == 0, "epoll: nothing yet");
    write(c, "x", 1);
    CHECK(epoll_wait(ep, out, 4, 1000) == 1 && (out[0].events & EPOLLIN), "epoll: unix socket readable");
    CHECK(epoll_wait(ep, out, 4, 0) == 1, "epoll: level-triggered repeats");
    read(s, b, 1);
    close(c);
    CHECK(epoll_wait(ep, out, 4, 1000) == 1 && (out[0].events & EPOLLRDHUP), "epoll: EPOLLRDHUP on peer close");

    // epoll, edge-triggered, on an eventfd
    int efd = eventfd(0, EFD_NONBLOCK);
    struct epoll_event e2 = {.events = EPOLLIN | EPOLLET, .data.fd = efd};
    int ep2 = epoll_create1(0);
    epoll_ctl(ep2, EPOLL_CTL_ADD, efd, &e2);
    uint64_t v = 1;
    write(efd, &v, 8);
    CHECK(epoll_wait(ep2, out, 4, 1000) == 1, "epoll ET: first write");
    CHECK(epoll_wait(ep2, out, 4, 0) == 0, "epoll ET: no repeat without new data");
    write(efd, &v, 8);
    CHECK(epoll_wait(ep2, out, 4, 1000) == 1, "epoll ET: new write re-triggers");

    // SIGPIPE / EPIPE
    signal(SIGPIPE, on_pipe);
    int r[2];
    pipe(r);
    close(r[0]);
    CHECK(write(r[1], "x", 1) == -1 && errno == EPIPE && got_pipe, "SIGPIPE on a closed pipe");
    got_pipe = 0;
    CHECK(send(sv[0], "x", 1, MSG_NOSIGNAL) == 1 && !got_pipe, "send MSG_NOSIGNAL");
    signal(SIGPIPE, SIG_DFL);

    // EINTR from a blocking read
    struct sigaction sa2 = {0};
    sa2.sa_handler = on_usr2;
    sigaction(SIGUSR2, &sa2, 0);
    int bl[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, bl);
    main_thread = pthread_self();
    pthread_t k;
    pthread_create(&k, 0, kick, 0);
    CHECK(read(bl[0], b, 1) == -1 && errno == EINTR, "EINTR from a blocking read");
    pthread_join(k, 0);
    unlink(a.sun_path);
}

int main(int argc, char **argv) {
    int only = argc > 1 ? atoi(argv[1]) : 0;
#define PART(n) if (only == 0 || only == n)
    struct sigaction sa = {0};
    sa.sa_sigaction = on_usr1;
    sa.sa_flags = SA_SIGINFO;
    CHECK(sigaction(SIGUSR1, &sa, 0) == 0, "sigaction");
    raise(SIGUSR1);
    CHECK(got_usr1 == SIGUSR1 && usr1_info_ok, "handler runs with siginfo");

    // Floating point and registers survive a handler.
    volatile double x = 1.5;
    got_usr1 = 0;
    kill(getpid(), SIGUSR1);
    CHECK(got_usr1 && x * 2 == 3.0, "state restored after handler");

    // Blocked signals wait.
    sigset_t set, old;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, &old);
    got_usr1 = 0;
    raise(SIGUSR1);
    sigset_t pend;
    sigpending(&pend);
    CHECK(!got_usr1 && sigismember(&pend, SIGUSR1), "blocked signal stays pending");
    sigprocmask(SIG_SETMASK, &old, 0);
    CHECK(got_usr1, "unblocked signal delivered");

    PART(2) {
    // SIGSEGV handler recovers from a bad access.
    signal(SIGSEGV, on_segv);
    if (sigsetjmp(jb, 1) == 0) {
        *(volatile int *)16 = 1;
        CHECK(0, "SIGSEGV caught");
    } else {
        CHECK(1, "SIGSEGV caught");
    }
    // PROT_NONE guard page.
    char *g = mmap(0, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (sigsetjmp(jb, 1) == 0) {
        g[0] = 1;
        CHECK(0, "guard page fault caught");
    } else {
        CHECK(1, "guard page fault caught");
    }
    signal(SIGSEGV, SIG_DFL);
    }
    PART(3) {

    // SIGCHLD and wait.
    signal(SIGCHLD, on_chld);
    pid_t pid = fork();
    if (pid == 0) _exit(7);
    int st = 0;
    waitpid(pid, &st, 0);
    for (int i = 0; i < 100 && !got_chld; i++) usleep(1000);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 7 && got_chld, "fork, wait, SIGCHLD");

    // A child killed by a signal.
    pid = fork();
    if (pid == 0) { for (;;) pause(); }
    usleep(20000);
    kill(pid, SIGTERM);
    waitpid(pid, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM, "kill child with SIGTERM");

    }
    PART(4) {
    // Threads and the main thread's stack (musl probes it with mremap).
    pthread_t t;
    void *r = 0;
    pthread_create(&t, 0, thread_fn, 0);
    pthread_join(t, &r);
    CHECK(r == (void *)42, "threads");
    pthread_attr_t a;
    void *stk; size_t ss = 0;
    CHECK(pthread_getattr_np(pthread_self(), &a) == 0 && pthread_attr_getstack(&a, &stk, &ss) == 0 && ss > 0, "pthread_getattr_np main thread");

    }
    PART(5) {
    // timerfd
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec its = {{0, 0}, {0, 20 * 1000 * 1000}};
    timerfd_settime(tfd, 0, &its, 0);
    unsigned long long n = 0;
    CHECK(tfd >= 0 && read(tfd, &n, 8) == 8 && n == 1, "timerfd");

    // select on a pipe
    int p[2];
    pipe(p);
    write(p[1], "x", 1);
    fd_set rs;
    FD_ZERO(&rs);
    FD_SET(p[0], &rs);
    struct timeval tv = {1, 0};
    CHECK(select(p[0] + 1, &rs, 0, 0, &tv) == 1 && FD_ISSET(p[0], &rs), "select");

    // pwrite / pread
    mkdir("/tmp", 0777);
    int fd = open("/tmp/sigtest.txt", O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(fd, "hello world", 11);
    pwrite(fd, "HELLO", 5, 0);
    char buf[16] = {0};
    pread(fd, buf, 11, 0);
    CHECK(strcmp(buf, "HELLO world") == 0 && lseek(fd, 0, SEEK_CUR) == 11, "pwrite / pread");
    close(fd);

    // symlink / readlink
    unlink("/tmp/sigtest.lnk");
    char lb[64] = {0};
    CHECK(symlink("somewhere:+123", "/tmp/sigtest.lnk") == 0 && readlink("/tmp/sigtest.lnk", lb, 63) == 14 && strcmp(lb, "somewhere:+123") == 0, "symlink / readlink");
    CHECK(symlink("x", "/tmp/sigtest.lnk") == -1 && errno == EEXIST, "symlink EEXIST");
    unlink("/tmp/sigtest.lnk");
    unlink("/tmp/sigtest.txt");

    }
    PART(6) {
    // /proc
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/status", getpid());
    int pf = open(path, O_RDONLY);
    char pb[4096] = {0};
    CHECK(pf >= 0 && read(pf, pb, 4095) > 0 && strstr(pb, "Threads:"), "/proc/<pid>/status");
    pf = open("/proc/self/maps", O_RDONLY);
    CHECK(pf >= 0 && read(pf, pb, 255) > 0 && strchr(pb, '-'), "/proc/self/maps");

    }
    PART(7) {
    sockets();
    }
    // nanosleep interrupted by a signal from another process
    printf("%s\n", failures ? "SIGNALS FAILED" : "SIGNALS PASSED");
    return failures;
}

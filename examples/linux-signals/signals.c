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
    // nanosleep interrupted by a signal from another process
    printf("%s\n", failures ? "SIGNALS FAILED" : "SIGNALS PASSED");
    return failures;
}

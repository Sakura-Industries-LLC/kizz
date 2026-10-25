// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

/*
 * The program of the tracer conformance test, phase 10 of architecture.md.
 *
 * The cached coverage-test task runs it in the task directory. It makes ordinary
 * file system calls on the committed inputs under in/ and on outputs that it creates
 * under out/, and the test compares what the tracer recorded with expected.txt.
 *
 * The portable core uses only the C library and POSIX, under in/core and out/core.
 * The Linux section makes each tracked system call of architecture.md through
 * syscall(), so each is made as that exact call whatever the C library's wrappers do,
 * under in/linux and out/linux. A call that some architectures lack is compiled only
 * where its number exists, together with every call on the paths it touches, so each
 * expected entry is touched on exactly the platforms that it names.
 *
 * Every result is checked against the outcome that the program expects. Each
 * mismatch is printed with its call and path, and the program exits with 1 if there
 * is any; otherwise it prints one summary line and exits with 0. It creates out/ and
 * fails if out/ exists, so it needs a fresh copy of the fixture.
 *
 * Run with a mode and the task directory as its arguments, the program is a child
 * that a process of the run started with exec, and it makes only the calls of that
 * mode on paths of its own. Run as the interpreter of a fixture script, with the
 * argument `interpreted` first, it makes no call and exits with 0.
 *
 * Run with one argument, a mode that starts with `change-`, the program is a step of
 * the coverage-changes task: it first reads or runs one committed file under
 * in/changes through the one call of that mode, then rewrites that file with
 * content of another size, so the step's result is not cached. The `control` mode
 * instead creates a file under out/changes, reads it and rewrites it, which stays
 * cached. Each mode touches only its own paths; the modes through the C library,
 * fopen and execv, are in the portable core, and the others are Linux calls.
 *
 * Linux names some paths through out/linux/made-link, a symbolic link to
 * out/linux/made: a file created through the link is recorded under the link's
 * name, and a later call on out/linux/made, or on a descriptor that names it, is
 * then the only call recorded on that path, so a tracer that missed it would record
 * the path differently.
 *
 * Linux also sets the extended attribute user.kizz, only on files that it creates
 * under out/linux, through out/linux/made-link, before a call reads, lists or
 * removes it under out/linux/made. It watches paths through an inotify instance and
 * a fanotify group that it creates itself, and with file_setattr it sets only the
 * file attributes that file_getattr read.
 *
 * Six tracked calls are newer than some kernels that run the test: file_getattr
 * and file_setattr, which Linux 6.17 added, and getxattrat, listxattrat,
 * setxattrat and removexattrat, which Linux 6.13 added. Before the Linux section
 * makes any of them, the program makes each once on /, outside the task directory,
 * with arguments that the kernel rejects with EINVAL before it looks the path up:
 * a structure size of 0, or a flag that the call does not accept. So the check can
 * neither succeed nor change anything, and it records nothing under the task
 * directory. A call that returns ENOSYS is unavailable: ENOSYS means that the
 * kernel lacks the call, or that a seccomp filter installed before kizz's own
 * refuses it. EINVAL means that the call is available; any other result is a
 * mismatch, and the call then counts as available. For each of the six, in the
 * order of TRACKED_SYSTEM_CALLS, the program prints "coverage: available: NAME" or
 * "coverage: unavailable: NAME" before its summary line. It makes the calls of a
 * group, and creates its files, only where every gated call that the group needs
 * is available: the group of file_setattr needs file_getattr too, because it sets
 * only the attributes that file_getattr read. The entries of those groups in
 * expected.txt name the calls they need.
 *
 * Run with one argument that starts with `incomplete-`, the program is a step of
 * the coverage-incomplete task: it makes only the one call that the mode names, a
 * call that marks the run as incompletely traced, in a way that changes nothing if
 * it succeeds, and exits with 0 whatever the call returns. For open_tree_attr and
 * fchroot, which a kernel may lack, it also prints "coverage: available: NAME", or
 * "coverage: unavailable: NAME" when the call failed with ENOSYS. With
 * `unknown-400` or `unknown-1000000`, it makes only that system call, which the
 * tracer does not know, and exits with 0 whatever it returns; those modes exist only
 * on x86_64, whose table the numbers belong to.
 *
 * The main program also calls unshare without CLONE_NEWNS and setns with another
 * namespace type, which leave the run completely traced.
 *
 * Both sections also bind, connect and send to Unix sockets under their output
 * directories, the portable core through the C library and the Linux section
 * through syscall(), and connect and send through UDP sockets to the loopback
 * addresses on fixed ports, one for each call: the portable core to 127.0.0.1,
 * the only loopback address that every platform has, and the Linux section to
 * distinct addresses in 127.0.0.0/8. Connecting a UDP socket and sending a
 * datagram succeed whether or not anything listens, so the results do not depend
 * on the machine. Each Unix socket that a send call sends to is a datagram socket
 * of its own, bound to its own path, so the path of each call has that call as its
 * only probe.
 */

#define _GNU_SOURCE

#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#ifdef __linux__
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <sys/syscall.h>
#endif

/* The line that the program prints when every call returned what it expected. */
#define SUCCESS_LINE "coverage: every call returned what it expected"

/*
 * The mode in which the program, run as the interpreter of the fixture script
 * in/linux/interp/inner at the end of the chain from in/linux/interp/outer,
 * makes no call and exits with 0.
 */
#define INTERPRETED "interpreted"

/* The start of each mode of the coverage-changes task. */
#define CHANGE_PREFIX "change-"
/* The committed inputs of the coverage-changes task. */
#define CHANGES "in/changes"
/* The output directory of the control mode of the coverage-changes task. */
#define CHANGES_OUT "out/changes"
/* The start of each mode of the coverage-incomplete task for a call that marks the run. */
#define INCOMPLETE_PREFIX "incomplete-"
/* The start of each mode of the coverage-incomplete task for a number that the tracer
 * does not know. */
#define UNKNOWN_PREFIX "unknown-"

/* Serializes the counters and the output of the threads. */
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
/* The number of results checked. */
static long checks;
/* The number of results that differed from the expected outcome. */
static long mismatches;
/* The absolute path of the task directory. */
static char task_dir[PATH_MAX];

/* The name of the error number `error`. */
static const char *error_name(int error)
{
    switch (error) {
    case ENOENT:
        return "ENOENT";
    case EEXIST:
        return "EEXIST";
    case ELOOP:
        return "ELOOP";
    case ENOTDIR:
        return "ENOTDIR";
    case ENOSYS:
        return "ENOSYS";
    case EACCES:
        return "EACCES";
    case EPERM:
        return "EPERM";
    case EINVAL:
        return "EINVAL";
    case EBADF:
        return "EBADF";
    case ERANGE:
        return "ERANGE";
#ifdef ENODATA
    case ENODATA:
        return "ENODATA";
#endif
#ifdef EOPNOTSUPP
    case EOPNOTSUPP:
        return "EOPNOTSUPP";
#endif
    default:
        return strerror(error);
    }
}

/*
 * Check the `result` of `call` on `path`, which failed with `error` when it is
 * negative, against `expected`: 0 for success, or the error number of the
 * expected failure. Print a mismatch, and return `result`.
 */
static long check(const char *call, const char *path, long result, int error, int expected)
{
    int matched = expected == 0 ? result >= 0 : (result < 0 && error == expected);
    pthread_mutex_lock(&lock);
    checks++;
    if (!matched) {
        mismatches++;
        printf("mismatch: %s %s: expected %s, got %s\n", call, path,
               expected == 0 ? "success" : error_name(expected),
               result >= 0 ? "success" : error_name(error));
        fflush(stdout);
    }
    pthread_mutex_unlock(&lock);
    return result;
}

/* The result of the call that EXPECT is checking, for each thread. */
static _Thread_local long expect_result;
/* The pointer that EXPECT_POINTER is checking, for each thread. */
static _Thread_local void *expect_pointer;

/*
 * Make the call `expression`, which returns a negative number on failure and sets
 * errno, check its result against `expected`, and return the result.
 */
#define EXPECT(expected, call, path, expression)                                                   \
    (errno = 0, expect_result = (long)(expression),                                                 \
     check((call), (path), expect_result, errno, (expected)))

/*
 * Make the call `expression`, which returns a null pointer on failure and sets
 * errno, check its result against `expected`, and return the pointer.
 */
#define EXPECT_POINTER(expected, call, path, expression)                                           \
    (errno = 0, expect_pointer = (void *)(expression),                                              \
     check((call), (path), expect_pointer != NULL ? 0 : -1, errno, (expected)), expect_pointer)

/* Check that reading from `fd`, opened on `path`, returns some content. */
static void read_some(const char *path, int fd)
{
    char buffer[256];
    if (fd < 0) {
        return;
    }
    long got = EXPECT(0, "read", path, read(fd, buffer, sizeof buffer));
    if (got == 0) {
        check("read", path, -1, 0, 0);
    }
}

/* Close `fd` if it is open. */
static void close_fd(int fd)
{
    if (fd >= 0) {
        (void)close(fd);
    }
}

/* Read a line of the file `path` with the C library's stream functions. */
static void read_stream(const char *path)
{
    char line[256];
    FILE *file = EXPECT_POINTER(0, "fopen", path, fopen(path, "r"));
    if (file == NULL) {
        return;
    }
    if (EXPECT_POINTER(0, "fgets", path, fgets(line, sizeof line, file)) == NULL) {
        (void)fclose(file);
        return;
    }
    (void)fclose(file);
}

/* Write a line to the file `path` with the C library's stream functions. */
static void write_stream(const char *path)
{
    FILE *file = EXPECT_POINTER(0, "fopen", path, fopen(path, "w"));
    if (file == NULL) {
        return;
    }
    EXPECT(0, "fputs", path, fputs("written by the coverage program\n", file));
    EXPECT(0, "fclose", path, fclose(file) == 0 ? 0 : -1);
}

/* Wait for the child `pid` that does `what`, and check that it exited with 0. */
static void wait_child(const char *what, pid_t pid)
{
    int status = 0;
    if (pid < 0) {
        return;
    }
    if (EXPECT(0, "waitpid", what, waitpid(pid, &status, 0)) < 0) {
        return;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        check("child", what, -1, 0, 0);
    }
}

/* Start a child process, after flushing the output that it would copy. */
static pid_t start_child(const char *what)
{
    fflush(stdout);
    return (pid_t)EXPECT(0, "fork", what, fork());
}

/* The path of `relative` in the task directory, in `buffer`. */
static const char *in_task(char *buffer, size_t size, const char *relative)
{
    snprintf(buffer, size, "%s/%s", task_dir, relative);
    return buffer;
}

/* The payload of every datagram that the program sends. */
static char payload[] = "kizz";

/* The address of the Unix socket `path`, with its length, its NUL byte included, in `length`. */
static struct sockaddr_un unix_address(const char *path, socklen_t *length)
{
    struct sockaddr_un address;
    memset(&address, 0, sizeof address);
    address.sun_family = AF_UNIX;
    snprintf(address.sun_path, sizeof address.sun_path, "%s", path);
    *length = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + strlen(address.sun_path) + 1);
    return address;
}

/* The IPv4 address 127.0.0.`last`, a loopback address, with `port`. */
static struct sockaddr_in loopback(unsigned last, unsigned short port)
{
    struct sockaddr_in address;
    memset(&address, 0, sizeof address);
    address.sin_family = AF_INET;
    address.sin_port = htons(port);
    address.sin_addr.s_addr = htonl((127u << 24) | last);
    return address;
}

/* Open a socket of `domain` and `type`, for `what`, and check the result. */
static int open_socket(const char *what, int domain, int type)
{
    return (int)EXPECT(0, "socket", what, socket(domain, type, 0));
}

/*
 * The portable core's sockets, through the C library: a Unix stream socket bound to
 * out/core/stream.sock that a second socket connects to, a connection to the missing
 * out/core/missing.sock, and UDP sockets that connect to 127.0.0.1 port 47011 and send
 * to 127.0.0.1 port 47012.
 */
static void core_sockets(void)
{
    socklen_t length;
    struct sockaddr_un stream = unix_address("out/core/stream.sock", &length);
    int listener = open_socket("out/core/stream.sock", AF_UNIX, SOCK_STREAM);
    EXPECT(0, "bind", "out/core/stream.sock", bind(listener, (struct sockaddr *)&stream, length));
    EXPECT(0, "listen", "out/core/stream.sock", listen(listener, 1));
    int client = open_socket("out/core/stream.sock", AF_UNIX, SOCK_STREAM);
    EXPECT(0, "connect", "out/core/stream.sock",
           connect(client, (struct sockaddr *)&stream, length));
    close_fd(client);
    close_fd(listener);

    struct sockaddr_un missing = unix_address("out/core/missing.sock", &length);
    client = open_socket("out/core/missing.sock", AF_UNIX, SOCK_STREAM);
    EXPECT(ENOENT, "connect", "out/core/missing.sock",
           connect(client, (struct sockaddr *)&missing, length));
    close_fd(client);

    struct sockaddr_in connected = loopback(1, 47011);
    int udp = open_socket("127.0.0.1:47011", AF_INET, SOCK_DGRAM);
    EXPECT(0, "connect", "127.0.0.1:47011",
           connect(udp, (struct sockaddr *)&connected, sizeof connected));
    close_fd(udp);
    struct sockaddr_in sent = loopback(1, 47012);
    udp = open_socket("127.0.0.1:47012", AF_INET, SOCK_DGRAM);
    EXPECT(0, "sendto", "127.0.0.1:47012",
           sendto(udp, payload, sizeof payload - 1, 0, (struct sockaddr *)&sent, sizeof sent));
    close_fd(udp);
}

/* The second thread of the portable core, which makes calls of its own. */
static void *core_thread(void *argument)
{
    (void)argument;
    read_stream("in/core/thread.txt");
    write_stream("out/core/thread.txt");
    return NULL;
}

/* The portable core: the C library's and POSIX functions under in/core and out/core. */
static void core(void)
{
    char path[PATH_MAX + 64];
    struct stat status;
    char target[PATH_MAX];

    /* Outputs: the output directory, which must not exist yet. */
    EXPECT(0, "mkdir", "out", mkdir("out", 0777));
    EXPECT(0, "mkdir", "out/core", mkdir("out/core", 0777));
    EXPECT(EEXIST, "mkdir", "out/core", mkdir("out/core", 0777));

    /* Reads, with streams and with a descriptor. */
    read_stream("in/core/read.txt");
    int fd = (int)EXPECT(0, "open", "in/core/open.txt", open("in/core/open.txt", O_RDONLY));
    read_some("in/core/open.txt", fd);
    close_fd(fd);
    (void)EXPECT_POINTER(ENOENT, "fopen", "in/core/fopen-missing",
                         fopen("in/core/fopen-missing", "r"));

    /* Existence and metadata. */
    EXPECT(0, "stat", "in/core/checked.txt", stat("in/core/checked.txt", &status));
    EXPECT(0, "stat", "in/core/checked-dir", stat("in/core/checked-dir", &status));
    EXPECT(ENOENT, "stat", "in/core/missing", stat("in/core/missing", &status));
    EXPECT(ENOENT, "stat", "in/core/no-dir/missing", stat("in/core/no-dir/missing", &status));
    EXPECT(0, "access", "in/core/access.txt", access("in/core/access.txt", R_OK));

    /* Links to a file and to a directory, followed and not. */
    EXPECT(0, "stat", "in/core/file-link", stat("in/core/file-link", &status));
    EXPECT(0, "lstat", "in/core/file-link", lstat("in/core/file-link", &status));
    EXPECT(0, "stat", "in/core/dir-link", stat("in/core/dir-link", &status));
    EXPECT(0, "lstat", "in/core/dir-link", lstat("in/core/dir-link", &status));

    /* A listing with the C library's directory functions. */
    DIR *dir = EXPECT_POINTER(0, "opendir", "in/core/listed", opendir("in/core/listed"));
    if (dir != NULL) {
        int entries = 0;
        for (;;) {
            errno = 0;
            struct dirent *entry = readdir(dir);
            if (entry == NULL) {
                check("readdir", "in/core/listed", errno == 0 ? 0 : -1, errno, 0);
                break;
            }
            entries++;
        }
        if (entries < 4) {
            check("readdir", "in/core/listed", -1, 0, 0);
        }
        (void)closedir(dir);
    }

    /* Changes, every one under out/core. */
    write_stream("out/core/written.txt");
    write_stream("out/core/rewritten.txt");
    read_stream("out/core/rewritten.txt");
    write_stream("out/core/rename-from");
    EXPECT(0, "rename", "out/core/rename-from", rename("out/core/rename-from", "out/core/rename-to"));
    write_stream("out/core/unlinked");
    EXPECT(0, "unlink", "out/core/unlinked", unlink("out/core/unlinked"));
    EXPECT(0, "mkdir", "out/core/removed-dir", mkdir("out/core/removed-dir", 0777));
    EXPECT(0, "rmdir", "out/core/removed-dir", rmdir("out/core/removed-dir"));
    EXPECT(0, "symlink", "out/core/symlink", symlink("written.txt", "out/core/symlink"));
    EXPECT(0, "lchown", "out/core/symlink", lchown("out/core/symlink", geteuid(), getegid()));
    EXPECT(0, "symlink", "out/core/dangling", symlink("nowhere", "out/core/dangling"));
    EXPECT(0, "lstat", "out/core/dangling", lstat("out/core/dangling", &status));
    EXPECT(0, "readlink", "out/core/dangling",
           readlink("out/core/dangling", target, sizeof target));
    write_stream("out/core/link-from");
    EXPECT(0, "link", "out/core/link-from", link("out/core/link-from", "out/core/link-to"));
    EXPECT(0, "mkfifo", "out/core/fifo", mkfifo("out/core/fifo", 0644));
    write_stream("out/core/truncated");
    EXPECT(0, "truncate", "out/core/truncated", truncate("out/core/truncated", 0));
    write_stream("out/core/mode-changed");
    EXPECT(0, "chmod", "out/core/mode-changed", chmod("out/core/mode-changed", 0600));
    write_stream("out/core/owner-changed");
    EXPECT(0, "chown", "out/core/owner-changed",
           chown("out/core/owner-changed", geteuid(), getegid()));
    write_stream("out/core/touched");
    EXPECT(0, "utimes", "out/core/touched", utimes("out/core/touched", NULL));

    /* Sockets. */
    core_sockets();

    /* A second thread. */
    pthread_t thread;
    int created = pthread_create(&thread, NULL, core_thread, NULL);
    check("pthread_create", "core thread", created == 0 ? 0 : -1, created, 0);
    if (created == 0) {
        check("pthread_join", "core thread", pthread_join(thread, NULL) == 0 ? 0 : -1, 0, 0);
    }

    /* A child that changes its working directory and runs the program again. */
    in_task(path, sizeof path, "coverage");
    pid_t pid = start_child("child-core");
    if (pid == 0) {
        char *arguments[] = {"coverage", "child-core", task_dir, NULL};
        EXPECT(0, "chdir", "in/core/cwd", chdir("in/core/cwd"));
        read_stream("cwd.txt");
        if (mismatches == 0) {
            EXPECT(0, "execv", path, execv(path, arguments));
        }
        _exit(1);
    }
    wait_child("child-core", pid);
    char *missing[] = {"exec-missing", NULL};
    in_task(path, sizeof path, "in/core/exec-missing");
    EXPECT(ENOENT, "execv", path, execv(path, missing));
}

#ifdef __linux__

/* The output directory of the Linux section. */
#define OUT "out/linux"
/* The directory that MADE_LINK leads to. */
#define MADE "out/linux/made"
/* A symbolic link to MADE, through which files are created. */
#define MADE_LINK "out/linux/made-link"
/* The fixture's #! scripts. */
#define INTERP "in/linux/interp"

/* A descriptor of in/linux/at. */
static int in_fd = -1;
/* A descriptor of out/linux/made. */
static int made_fd = -1;

/* `struct open_how`, the third argument of openat2. */
struct open_how_argument {
    uint64_t flags;
    uint64_t mode;
    uint64_t resolve;
};

/* The resolve flag of openat2 that makes the directory descriptor the root. */
#ifndef RESOLVE_IN_ROOT
#define RESOLVE_IN_ROOT 0x10
#endif

/* A buffer large enough for every kernel stat structure. */
static union {
    struct stat status;
    unsigned char bytes[512];
} stat_buffer;

/* A buffer for directory entries. */
static union {
    long align;
    char bytes[4096];
} entries_buffer;

/* Create MADE_LINK/name for writing and return its descriptor, which names MADE/name. */
static int make_fd(const char *name)
{
    static char path[PATH_MAX];
    snprintf(path, sizeof path, MADE_LINK "/%s", name);
    return (int)EXPECT(0, "openat", path,
                       syscall(SYS_openat, AT_FDCWD, path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC,
                               0644));
}

/* Create the file MADE_LINK/name, which is also MADE/name. */
static void make(const char *name)
{
    close_fd(make_fd(name));
}

/* Create the file `path` for writing. */
static void create(const char *path)
{
    close_fd((int)EXPECT(0, "openat", path,
                         syscall(SYS_openat, AT_FDCWD, path,
                                 O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0644)));
}

/* Open `path` for reading with openat relative to `dirfd`, and read from it. */
static void read_at(int dirfd, const char *path)
{
    int fd = (int)EXPECT(0, "openat", path,
                         syscall(SYS_openat, dirfd, path, O_RDONLY | O_CLOEXEC));
    read_some(path, fd);
    close_fd(fd);
}

/* Read the whole directory behind `fd` with the system call `number`, named `call`. */
static void list(const char *call, long number, const char *path, int fd)
{
    for (;;) {
        long got = EXPECT(0, call, path,
                          syscall(number, fd, entries_buffer.bytes, sizeof entries_buffer.bytes));
        if (got <= 0) {
            break;
        }
    }
}

/*
 * Start a child that runs the program in `mode` with execve, when `dirfd` is
 * negative and not AT_FDCWD, or else with execveat and `flags`, and wait for it.
 */
static void exec_child(const char *call, int dirfd, const char *path, int flags, const char *mode)
{
    char *arguments[] = {"coverage", (char *)mode, task_dir, NULL};
    char *environment[] = {NULL};
    (void)dirfd;
    (void)flags;
    pid_t pid = start_child(mode);
    if (pid == 0) {
        if (strcmp(call, "execve") == 0) {
            EXPECT(0, call, path, syscall(SYS_execve, path, arguments, environment));
        } else {
#ifdef SYS_execveat
            EXPECT(0, call, path,
                   syscall(SYS_execveat, dirfd, path, arguments, environment, flags));
#endif
        }
        _exit(1);
    }
    wait_child(mode, pid);
}

/*
 * Copy the program to `path`, executable, reading it through the symbolic link
 * `link`, so the program's own path is not touched.
 */
static void copy_program(const char *link, const char *path)
{
    char buffer[65536];
    int from = (int)EXPECT(0, "openat", link,
                           syscall(SYS_openat, AT_FDCWD, link, O_RDONLY | O_CLOEXEC));
    int to = (int)EXPECT(0, "openat", path,
                         syscall(SYS_openat, AT_FDCWD, path,
                                 O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0700));
    if (from >= 0 && to >= 0) {
        for (;;) {
            ssize_t got = read(from, buffer, sizeof buffer);
            if (got <= 0) {
                check("read", link, got, errno, 0);
                break;
            }
            if (EXPECT(0, "write", path, write(to, buffer, (size_t)got)) != got) {
                check("write", path, -1, 0, 0);
                break;
            }
        }
    }
    close_fd(from);
    close_fd(to);
}

/*
 * Run the fixture script `path` with execve in a child, from the task directory,
 * and check that the script exits with 0 when `expected` is 0, or else that
 * execve fails with `expected`.
 */
static void run_script(const char *path, int expected)
{
    char *arguments[] = {(char *)path, NULL};
    char *environment[] = {NULL};
    pid_t pid = start_child(path);
    if (pid == 0) {
        EXPECT(expected, "execve", path, syscall(SYS_execve, path, arguments, environment));
        _exit(expected != 0 && mismatches == 0 ? 0 : 1);
    }
    wait_child(path, pid);
}

/* Opening files: open, openat, openat2 and creat. */
static void linux_open(void)
{
    char path[PATH_MAX + 64];
    int fd;

#ifdef SYS_open
    fd = (int)EXPECT(0, "open", "in/linux/open.txt",
                     syscall(SYS_open, "in/linux/open.txt", O_RDONLY | O_CLOEXEC));
    read_some("in/linux/open.txt", fd);
    close_fd(fd);
    EXPECT(ENOENT, "open", "in/linux/open-missing",
           syscall(SYS_open, "in/linux/open-missing", O_RDONLY | O_CLOEXEC));
#endif

    /* openat: each access mode, creation, truncation, and the open flags that look up. */
    read_at(AT_FDCWD, "in/linux/openat.txt");
    read_at(in_fd, "openat.txt");
    EXPECT(ENOENT, "openat", "in/linux/openat-missing",
           syscall(SYS_openat, AT_FDCWD, "in/linux/openat-missing", O_RDONLY | O_CLOEXEC));
    EXPECT(ENOENT, "openat", OUT "/no-dir/openat",
           syscall(SYS_openat, AT_FDCWD, OUT "/no-dir/openat",
                   O_WRONLY | O_CREAT | O_CLOEXEC, 0644));
    EXPECT(ENOTDIR, "openat", "in/linux/openat.txt/below",
           syscall(SYS_openat, AT_FDCWD, "in/linux/openat.txt/below", O_RDONLY | O_CLOEXEC));
    create(OUT "/openat-wronly");
    close_fd((int)EXPECT(0, "openat", OUT "/openat-wronly",
                         syscall(SYS_openat, AT_FDCWD, OUT "/openat-wronly",
                                 O_WRONLY | O_TRUNC | O_CLOEXEC)));
    EXPECT(EEXIST, "openat", OUT "/openat-wronly",
           syscall(SYS_openat, AT_FDCWD, OUT "/openat-wronly",
                   O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0644));
    close_fd((int)EXPECT(0, "openat", OUT "/openat-rdwr",
                         syscall(SYS_openat, AT_FDCWD, OUT "/openat-rdwr",
                                 O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0644)));
    close_fd((int)EXPECT(0, "openat", "in/linux/openat-dir",
                         syscall(SYS_openat, AT_FDCWD, "in/linux/openat-dir",
                                 O_RDONLY | O_DIRECTORY | O_CLOEXEC)));
    close_fd((int)EXPECT(0, "openat", "in/linux/openat-path.txt",
                         syscall(SYS_openat, AT_FDCWD, "in/linux/openat-path.txt",
                                 O_PATH | O_CLOEXEC)));
    EXPECT(ELOOP, "openat", "in/linux/openat-nofollow-link",
           syscall(SYS_openat, AT_FDCWD, "in/linux/openat-nofollow-link",
                   O_RDONLY | O_NOFOLLOW | O_CLOEXEC));
    read_at(AT_FDCWD, "in/linux/openat-file-link");
    close_fd((int)EXPECT(0, "openat", "in/linux/openat-dir-link",
                         syscall(SYS_openat, AT_FDCWD, "in/linux/openat-dir-link",
                                 O_RDONLY | O_DIRECTORY | O_CLOEXEC)));

    /* Spellings: ./ and a repeated / are dropped, and .. is kept. */
    read_at(AT_FDCWD, "./in//linux/./spelled.txt");
    read_at(in_fd, "../parent.txt");

    /* Absolute paths, from the working directory and from a descriptor. */
    read_at(AT_FDCWD, in_task(path, sizeof path, "in/linux/absolute.txt"));
    read_at(in_fd, in_task(path, sizeof path, "in/linux/absolute-at.txt"));

#ifdef SYS_openat2
    /* openat2, without and with RESOLVE_IN_ROOT. */
    struct open_how_argument how = {O_RDONLY | O_CLOEXEC, 0, 0};
    fd = (int)EXPECT(0, "openat2", "in/linux/openat2.txt",
                     syscall(SYS_openat2, AT_FDCWD, "in/linux/openat2.txt", &how, sizeof how));
    read_some("in/linux/openat2.txt", fd);
    close_fd(fd);
    fd = (int)EXPECT(0, "openat2", "openat2.txt",
                     syscall(SYS_openat2, in_fd, "openat2.txt", &how, sizeof how));
    read_some("openat2.txt", fd);
    close_fd(fd);
    EXPECT(ENOENT, "openat2", "in/linux/openat2-missing",
           syscall(SYS_openat2, AT_FDCWD, "in/linux/openat2-missing", &how, sizeof how));
    int root = (int)EXPECT(0, "openat", "in/linux/root",
                           syscall(SYS_openat, AT_FDCWD, "in/linux/root",
                                   O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    struct open_how_argument in_root = {O_RDONLY | O_CLOEXEC, 0, RESOLVE_IN_ROOT};
    fd = (int)EXPECT(0, "openat2", "escape-link",
                     syscall(SYS_openat2, root, "escape-link", &in_root, sizeof in_root));
    read_some("escape-link", fd);
    close_fd(fd);
    fd = (int)EXPECT(0, "openat2", "/inner.txt",
                     syscall(SYS_openat2, root, "/inner.txt", &in_root, sizeof in_root));
    read_some("/inner.txt", fd);
    close_fd(fd);
    close_fd(root);
#endif

#ifdef SYS_creat
    close_fd((int)EXPECT(0, "creat", OUT "/creat", syscall(SYS_creat, OUT "/creat", 0644)));
    EXPECT(ENOENT, "creat", OUT "/no-dir/creat", syscall(SYS_creat, OUT "/no-dir/creat", 0644));
#endif
}

/* Metadata and existence: stat, lstat, newfstatat, statx, access, faccessat and faccessat2. */
static void linux_metadata(void)
{
    int fd;

#ifdef SYS_stat
    EXPECT(0, "stat", "in/linux/stat.txt", syscall(SYS_stat, "in/linux/stat.txt", &stat_buffer));
    EXPECT(ENOENT, "stat", "in/linux/stat-missing",
           syscall(SYS_stat, "in/linux/stat-missing", &stat_buffer));
    EXPECT(0, "stat", "in/linux/stat-file-link",
           syscall(SYS_stat, "in/linux/stat-file-link", &stat_buffer));
    EXPECT(0, "stat", "in/linux/stat-dir-link",
           syscall(SYS_stat, "in/linux/stat-dir-link", &stat_buffer));
#endif
#ifdef SYS_lstat
    EXPECT(0, "lstat", "in/linux/lstat.txt",
           syscall(SYS_lstat, "in/linux/lstat.txt", &stat_buffer));
    EXPECT(ENOENT, "lstat", "in/linux/lstat-missing",
           syscall(SYS_lstat, "in/linux/lstat-missing", &stat_buffer));
    EXPECT(0, "lstat", "in/linux/lstat-file-link",
           syscall(SYS_lstat, "in/linux/lstat-file-link", &stat_buffer));
    EXPECT(0, "lstat", "in/linux/lstat-dir-link",
           syscall(SYS_lstat, "in/linux/lstat-dir-link", &stat_buffer));
#endif

    EXPECT(0, "newfstatat", "in/linux/newfstatat.txt",
           syscall(SYS_newfstatat, AT_FDCWD, "in/linux/newfstatat.txt", &stat_buffer, 0));
    EXPECT(0, "newfstatat", "newfstatat.txt",
           syscall(SYS_newfstatat, in_fd, "newfstatat.txt", &stat_buffer, 0));
    EXPECT(ENOENT, "newfstatat", "in/linux/newfstatat-missing",
           syscall(SYS_newfstatat, AT_FDCWD, "in/linux/newfstatat-missing", &stat_buffer, 0));
    EXPECT(ENOENT, "newfstatat", "in/linux/no-dir/newfstatat",
           syscall(SYS_newfstatat, AT_FDCWD, "in/linux/no-dir/newfstatat", &stat_buffer, 0));
    EXPECT(ENOTDIR, "newfstatat", "in/linux/newfstatat.txt/below",
           syscall(SYS_newfstatat, AT_FDCWD, "in/linux/newfstatat.txt/below", &stat_buffer, 0));
    EXPECT(0, "newfstatat", "in/linux/newfstatat-link",
           syscall(SYS_newfstatat, AT_FDCWD, "in/linux/newfstatat-link", &stat_buffer, 0));
    EXPECT(0, "newfstatat", "in/linux/newfstatat-link",
           syscall(SYS_newfstatat, AT_FDCWD, "in/linux/newfstatat-link", &stat_buffer,
                   AT_SYMLINK_NOFOLLOW));
    fd = (int)EXPECT(0, "openat", "in/linux/newfstatat-empty.txt",
                     syscall(SYS_openat, AT_FDCWD, "in/linux/newfstatat-empty.txt",
                             O_PATH | O_CLOEXEC));
    EXPECT(0, "newfstatat", "in/linux/newfstatat-empty.txt",
           syscall(SYS_newfstatat, fd, "", &stat_buffer, AT_EMPTY_PATH));
    close_fd(fd);

#ifdef SYS_statx
    EXPECT(0, "statx", "in/linux/statx.txt",
           syscall(SYS_statx, AT_FDCWD, "in/linux/statx.txt", 0, 0x7ff, &stat_buffer));
    EXPECT(0, "statx", "statx.txt", syscall(SYS_statx, in_fd, "statx.txt", 0, 0x7ff, &stat_buffer));
    EXPECT(ENOENT, "statx", "in/linux/statx-missing",
           syscall(SYS_statx, AT_FDCWD, "in/linux/statx-missing", 0, 0x7ff, &stat_buffer));
    EXPECT(0, "statx", "in/linux/statx-dir-link",
           syscall(SYS_statx, AT_FDCWD, "in/linux/statx-dir-link", 0, 0x7ff, &stat_buffer));
    EXPECT(0, "statx", "in/linux/statx-dir-link",
           syscall(SYS_statx, AT_FDCWD, "in/linux/statx-dir-link", AT_SYMLINK_NOFOLLOW, 0x7ff,
                   &stat_buffer));
    fd = (int)EXPECT(0, "openat", "in/linux/statx-empty.txt",
                     syscall(SYS_openat, AT_FDCWD, "in/linux/statx-empty.txt",
                             O_PATH | O_CLOEXEC));
    EXPECT(0, "statx", "in/linux/statx-empty.txt",
           syscall(SYS_statx, fd, "", AT_EMPTY_PATH, 0x7ff, &stat_buffer));
    close_fd(fd);
#endif

#ifdef SYS_access
    EXPECT(0, "access", "in/linux/access.txt", syscall(SYS_access, "in/linux/access.txt", R_OK));
    EXPECT(ENOENT, "access", "in/linux/access-missing",
           syscall(SYS_access, "in/linux/access-missing", F_OK));
#endif

    EXPECT(0, "faccessat", "in/linux/faccessat.txt",
           syscall(SYS_faccessat, AT_FDCWD, "in/linux/faccessat.txt", R_OK));
    EXPECT(0, "faccessat", "faccessat.txt", syscall(SYS_faccessat, in_fd, "faccessat.txt", R_OK));
    EXPECT(ENOENT, "faccessat", "in/linux/faccessat-missing",
           syscall(SYS_faccessat, AT_FDCWD, "in/linux/faccessat-missing", F_OK));

#ifdef SYS_faccessat2
    EXPECT(0, "faccessat2", "in/linux/faccessat2.txt",
           syscall(SYS_faccessat2, AT_FDCWD, "in/linux/faccessat2.txt", R_OK, 0));
    EXPECT(0, "faccessat2", "faccessat2.txt",
           syscall(SYS_faccessat2, in_fd, "faccessat2.txt", R_OK, 0));
    EXPECT(ENOENT, "faccessat2", "in/linux/faccessat2-missing",
           syscall(SYS_faccessat2, AT_FDCWD, "in/linux/faccessat2-missing", F_OK, 0));
    EXPECT(0, "faccessat2", "in/linux/faccessat2-link",
           syscall(SYS_faccessat2, AT_FDCWD, "in/linux/faccessat2-link", R_OK, 0));
    EXPECT(0, "faccessat2", "in/linux/faccessat2-link",
           syscall(SYS_faccessat2, AT_FDCWD, "in/linux/faccessat2-link", F_OK,
                   AT_SYMLINK_NOFOLLOW));
    fd = (int)EXPECT(0, "openat", "in/linux/faccessat2-empty.txt",
                     syscall(SYS_openat, AT_FDCWD, "in/linux/faccessat2-empty.txt",
                             O_PATH | O_CLOEXEC));
    EXPECT(0, "faccessat2", "in/linux/faccessat2-empty.txt",
           syscall(SYS_faccessat2, fd, "", R_OK, AT_EMPTY_PATH));
    close_fd(fd);
#endif
}

/* Links: readlink, readlinkat, link, linkat, symlink and symlinkat. */
static void linux_links(void)
{
    char target[PATH_MAX];
    int fd;

#ifdef SYS_readlink
    EXPECT(0, "readlink", "in/linux/readlink-link",
           syscall(SYS_readlink, "in/linux/readlink-link", target, sizeof target));
    EXPECT(ENOENT, "readlink", "in/linux/readlink-missing",
           syscall(SYS_readlink, "in/linux/readlink-missing", target, sizeof target));
#endif

    EXPECT(0, "readlinkat", "in/linux/readlinkat-link",
           syscall(SYS_readlinkat, AT_FDCWD, "in/linux/readlinkat-link", target, sizeof target));
    EXPECT(0, "readlinkat", "readlinkat-link",
           syscall(SYS_readlinkat, in_fd, "readlinkat-link", target, sizeof target));
    EXPECT(ENOENT, "readlinkat", "in/linux/readlinkat-missing",
           syscall(SYS_readlinkat, AT_FDCWD, "in/linux/readlinkat-missing", target,
                   sizeof target));
    fd = (int)EXPECT(0, "openat", "in/linux/readlinkat-empty-link",
                     syscall(SYS_openat, AT_FDCWD, "in/linux/readlinkat-empty-link",
                             O_PATH | O_NOFOLLOW | O_CLOEXEC));
    EXPECT(0, "readlinkat", "in/linux/readlinkat-empty-link",
           syscall(SYS_readlinkat, fd, "", target, sizeof target));
    close_fd(fd);

#ifdef SYS_link
    create(OUT "/link-from");
    EXPECT(0, "link", OUT "/link-to", syscall(SYS_link, OUT "/link-from", OUT "/link-to"));
    EXPECT(ENOENT, "link", OUT "/link-missing",
           syscall(SYS_link, OUT "/link-missing", OUT "/link-missing-to"));
#endif

    /* linkat; with AT_EMPTY_PATH it needs CAP_DAC_READ_SEARCH, which the tests lack. */
    create(OUT "/linkat-from");
    EXPECT(0, "linkat", OUT "/linkat-to",
           syscall(SYS_linkat, AT_FDCWD, OUT "/linkat-from", AT_FDCWD, OUT "/linkat-to", 0));
    make("linkat-at-from");
    EXPECT(0, "linkat", "linkat-at-to",
           syscall(SYS_linkat, made_fd, "linkat-at-from", made_fd, "linkat-at-to", 0));
    EXPECT(0, "symlinkat", OUT "/linkat-symlink",
           syscall(SYS_symlinkat, "linkat-from", AT_FDCWD, OUT "/linkat-symlink"));
    EXPECT(0, "linkat", OUT "/linkat-nofollow",
           syscall(SYS_linkat, AT_FDCWD, OUT "/linkat-symlink", AT_FDCWD, OUT "/linkat-nofollow",
                   0));
    EXPECT(0, "linkat", OUT "/linkat-follow",
           syscall(SYS_linkat, AT_FDCWD, OUT "/linkat-symlink", AT_FDCWD, OUT "/linkat-follow",
                   AT_SYMLINK_FOLLOW));
    EXPECT(ENOENT, "linkat", OUT "/linkat-missing",
           syscall(SYS_linkat, AT_FDCWD, OUT "/linkat-missing", AT_FDCWD,
                   OUT "/linkat-missing-to", 0));

#ifdef SYS_symlink
    EXPECT(0, "symlink", OUT "/symlink", syscall(SYS_symlink, "nowhere", OUT "/symlink"));
    EXPECT(ENOENT, "symlink", OUT "/no-dir/symlink",
           syscall(SYS_symlink, "nowhere", OUT "/no-dir/symlink"));
#ifdef SYS_lstat
    EXPECT(0, "lstat", OUT "/symlink", syscall(SYS_lstat, OUT "/symlink", &stat_buffer));
#endif
#endif

    EXPECT(0, "symlinkat", "symlinkat-at",
           syscall(SYS_symlinkat, "nowhere", made_fd, "symlinkat-at"));
    EXPECT(ENOENT, "symlinkat", OUT "/no-dir/symlinkat",
           syscall(SYS_symlinkat, "nowhere", AT_FDCWD, OUT "/no-dir/symlinkat"));
}

/* Directory listings, programs, and the working directory. */
static void linux_listings_and_processes(void)
{
    char *missing_arguments[] = {"missing", NULL};
    char *environment[] = {NULL};
    int fd;
    pid_t pid;

#ifdef SYS_getdents
    fd = (int)EXPECT(0, "openat", "in/linux/getdents",
                     syscall(SYS_openat, AT_FDCWD, "in/linux/getdents",
                             O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    list("getdents", SYS_getdents, "in/linux/getdents", fd);
    close_fd(fd);
#endif
    fd = (int)EXPECT(0, "openat", "in/linux/getdents64",
                     syscall(SYS_openat, AT_FDCWD, "in/linux/getdents64",
                             O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    list("getdents64", SYS_getdents64, "in/linux/getdents64", fd);
    close_fd(fd);

    /* execve, through a symbolic link to the program. */
    EXPECT(0, "symlinkat", OUT "/execve-link",
           syscall(SYS_symlinkat, "../../coverage", AT_FDCWD, OUT "/execve-link"));
    exec_child("execve", AT_FDCWD, OUT "/execve-link", 0, "child-execve");
    EXPECT(ENOENT, "execve", "in/linux/execve-missing",
           syscall(SYS_execve, "in/linux/execve-missing", missing_arguments, environment));

#ifdef SYS_execveat
    /* execveat, from the working directory, from a descriptor, and on a descriptor. */
    EXPECT(0, "symlinkat", OUT "/execveat-link",
           syscall(SYS_symlinkat, "../../coverage", AT_FDCWD, OUT "/execveat-link"));
    exec_child("execveat", AT_FDCWD, OUT "/execveat-link", 0, "child-execveat");
    EXPECT(ELOOP, "execveat", OUT "/execveat-link",
           syscall(SYS_execveat, AT_FDCWD, OUT "/execveat-link", missing_arguments, environment,
                   AT_SYMLINK_NOFOLLOW));
    EXPECT(0, "symlinkat", "execveat-at-link",
           syscall(SYS_symlinkat, "../../../coverage", made_fd, "execveat-at-link"));
    exec_child("execveat", made_fd, "execveat-at-link", 0, "child-execveat-at");
    EXPECT(0, "symlinkat", OUT "/execveat-copy-link",
           syscall(SYS_symlinkat, "../../coverage", AT_FDCWD, OUT "/execveat-copy-link"));
    copy_program(OUT "/execveat-copy-link", OUT "/execveat-copy");
    fd = (int)EXPECT(0, "openat", OUT "/execveat-copy",
                     syscall(SYS_openat, AT_FDCWD, OUT "/execveat-copy", O_PATH | O_CLOEXEC));
    exec_child("execveat", fd, "", AT_EMPTY_PATH, "child-execveat-empty");
    close_fd(fd);
    EXPECT(ENOENT, "execveat", "in/linux/execveat-missing",
           syscall(SYS_execveat, AT_FDCWD, "in/linux/execveat-missing", missing_arguments,
                   environment, 0));
#endif

    /*
     * #! scripts: the kernel runs in/linux/interp/outer through its interpreter
     * in/linux/interp/middle, a script whose interpreter in/linux/interp/inner is
     * a script whose interpreter is the program in the mode INTERPRETED;
     * in/linux/interp/broken names a missing interpreter.
     */
    run_script(INTERP "/outer", 0);
    run_script(INTERP "/broken", ENOENT);

    /* chdir, then a relative path, in a child. */
    pid = start_child("chdir");
    if (pid == 0) {
        EXPECT(0, "chdir", "in/linux/chdir", syscall(SYS_chdir, "in/linux/chdir"));
        read_at(AT_FDCWD, "chdir.txt");
        _exit(mismatches == 0 ? 0 : 1);
    }
    wait_child("chdir", pid);
    EXPECT(ENOENT, "chdir", "in/linux/chdir-missing", syscall(SYS_chdir, "in/linux/chdir-missing"));

    /* fchdir to a directory opened through a link to it, then a relative path, in a child. */
    pid = start_child("fchdir");
    if (pid == 0) {
        fd = (int)EXPECT(0, "openat", "in/linux/fchdir-link",
                         syscall(SYS_openat, AT_FDCWD, "in/linux/fchdir-link",
                                 O_RDONLY | O_DIRECTORY | O_CLOEXEC));
        EXPECT(0, "fchdir", "in/linux/fchdir", syscall(SYS_fchdir, fd));
        read_at(AT_FDCWD, "fchdir.txt");
        _exit(mismatches == 0 ? 0 : 1);
    }
    wait_child("fchdir", pid);
}

/* Renames, removals and creations. */
static void linux_names(void)
{
#ifdef SYS_rename
    create(OUT "/rename-from");
    EXPECT(0, "rename", OUT "/rename-to", syscall(SYS_rename, OUT "/rename-from", OUT "/rename-to"));
    EXPECT(ENOENT, "rename", OUT "/rename-missing",
           syscall(SYS_rename, OUT "/rename-missing", OUT "/rename-missing-to"));
#endif

    create(OUT "/renameat-from");
    EXPECT(0, "renameat", OUT "/renameat-to",
           syscall(SYS_renameat, AT_FDCWD, OUT "/renameat-from", AT_FDCWD, OUT "/renameat-to"));
    make("renameat-at-from");
    EXPECT(0, "renameat", "renameat-at-to",
           syscall(SYS_renameat, made_fd, "renameat-at-from", made_fd, "renameat-at-to"));
    EXPECT(ENOENT, "renameat", OUT "/renameat-missing",
           syscall(SYS_renameat, AT_FDCWD, OUT "/renameat-missing", AT_FDCWD,
                   OUT "/renameat-missing-to"));

    create(OUT "/renameat2-from");
    EXPECT(0, "renameat2", OUT "/renameat2-to",
           syscall(SYS_renameat2, AT_FDCWD, OUT "/renameat2-from", AT_FDCWD, OUT "/renameat2-to",
                   0));
    make("renameat2-at-from");
    EXPECT(0, "renameat2", "renameat2-at-to",
           syscall(SYS_renameat2, made_fd, "renameat2-at-from", made_fd, "renameat2-at-to", 0));
    EXPECT(ENOENT, "renameat2", OUT "/renameat2-missing",
           syscall(SYS_renameat2, AT_FDCWD, OUT "/renameat2-missing", AT_FDCWD,
                   OUT "/renameat2-missing-to", 0));

#ifdef SYS_unlink
    make("unlink");
    EXPECT(0, "unlink", MADE "/unlink", syscall(SYS_unlink, MADE "/unlink"));
    EXPECT(ENOENT, "unlink", OUT "/unlink-missing", syscall(SYS_unlink, OUT "/unlink-missing"));
#endif

    make("unlinkat");
    EXPECT(0, "unlinkat", MADE "/unlinkat", syscall(SYS_unlinkat, AT_FDCWD, MADE "/unlinkat", 0));
    EXPECT(0, "mkdirat", MADE_LINK "/unlinkat-dir",
           syscall(SYS_mkdirat, AT_FDCWD, MADE_LINK "/unlinkat-dir", 0777));
    EXPECT(0, "unlinkat", MADE "/unlinkat-dir",
           syscall(SYS_unlinkat, AT_FDCWD, MADE "/unlinkat-dir", AT_REMOVEDIR));
    make("unlinkat-at");
    EXPECT(0, "unlinkat", "unlinkat-at", syscall(SYS_unlinkat, made_fd, "unlinkat-at", 0));
    EXPECT(ENOENT, "unlinkat", OUT "/unlinkat-missing",
           syscall(SYS_unlinkat, AT_FDCWD, OUT "/unlinkat-missing", 0));

#ifdef SYS_rmdir
    EXPECT(0, "mkdirat", MADE_LINK "/rmdir",
           syscall(SYS_mkdirat, AT_FDCWD, MADE_LINK "/rmdir", 0777));
    EXPECT(0, "rmdir", MADE "/rmdir", syscall(SYS_rmdir, MADE "/rmdir"));
    EXPECT(ENOENT, "rmdir", OUT "/rmdir-missing", syscall(SYS_rmdir, OUT "/rmdir-missing"));
#endif

#ifdef SYS_mkdir
    EXPECT(0, "mkdir", OUT "/mkdir", syscall(SYS_mkdir, OUT "/mkdir", 0777));
    EXPECT(EEXIST, "mkdir", OUT "/mkdir", syscall(SYS_mkdir, OUT "/mkdir", 0777));
    EXPECT(ENOENT, "mkdir", OUT "/no-dir/mkdir", syscall(SYS_mkdir, OUT "/no-dir/mkdir", 0777));
#endif

    EXPECT(0, "mkdirat", "mkdirat-at", syscall(SYS_mkdirat, made_fd, "mkdirat-at", 0777));
    EXPECT(ENOENT, "mkdirat", OUT "/no-dir/mkdirat",
           syscall(SYS_mkdirat, AT_FDCWD, OUT "/no-dir/mkdirat", 0777));

#ifdef SYS_mknod
    EXPECT(0, "mknod", OUT "/mknod-fifo", syscall(SYS_mknod, OUT "/mknod-fifo", S_IFIFO | 0644, 0));
    EXPECT(ENOENT, "mknod", OUT "/no-dir/mknod",
           syscall(SYS_mknod, OUT "/no-dir/mknod", S_IFIFO | 0644, 0));
#endif

    EXPECT(0, "mknodat", OUT "/mknodat-file",
           syscall(SYS_mknodat, AT_FDCWD, OUT "/mknodat-file", S_IFREG | 0644, 0));
    EXPECT(0, "mknodat", "mknodat-at-fifo",
           syscall(SYS_mknodat, made_fd, "mknodat-at-fifo", S_IFIFO | 0644, 0));
    EXPECT(ENOENT, "mknodat", OUT "/no-dir/mknodat",
           syscall(SYS_mknodat, AT_FDCWD, OUT "/no-dir/mknodat", S_IFREG | 0644, 0));
}

/* Changes to content and metadata, by path and through descriptors. */
static void linux_changes(void)
{
    uid_t user = geteuid();
    gid_t group = getegid();
    int fd;

    make("truncate");
    EXPECT(0, "truncate", MADE "/truncate", syscall(SYS_truncate, MADE "/truncate", 0));
    EXPECT(ENOENT, "truncate", OUT "/truncate-missing",
           syscall(SYS_truncate, OUT "/truncate-missing", 0));

#ifdef SYS_chmod
    make("chmod");
    EXPECT(0, "chmod", MADE "/chmod", syscall(SYS_chmod, MADE "/chmod", 0600));
    EXPECT(ENOENT, "chmod", OUT "/chmod-missing", syscall(SYS_chmod, OUT "/chmod-missing", 0600));
#endif

    make("fchmodat");
    EXPECT(0, "fchmodat", MADE "/fchmodat",
           syscall(SYS_fchmodat, AT_FDCWD, MADE "/fchmodat", 0600));
    make("fchmodat-at");
    EXPECT(0, "fchmodat", "fchmodat-at", syscall(SYS_fchmodat, made_fd, "fchmodat-at", 0600));
    EXPECT(ENOENT, "fchmodat", OUT "/fchmodat-missing",
           syscall(SYS_fchmodat, AT_FDCWD, OUT "/fchmodat-missing", 0600));

#ifdef SYS_chown
    make("chown");
    EXPECT(0, "chown", MADE "/chown", syscall(SYS_chown, MADE "/chown", user, group));
    EXPECT(ENOENT, "chown", OUT "/chown-missing",
           syscall(SYS_chown, OUT "/chown-missing", user, group));
#ifdef SYS_lchown
    /* chown follows a link, and lchown changes the link itself. */
    EXPECT(0, "symlinkat", MADE_LINK "/chown-link",
           syscall(SYS_symlinkat, "chown", AT_FDCWD, MADE_LINK "/chown-link"));
    EXPECT(0, "chown", MADE "/chown-link", syscall(SYS_chown, MADE "/chown-link", user, group));
    EXPECT(0, "lchown", MADE "/chown-link", syscall(SYS_lchown, MADE "/chown-link", user, group));
#endif
#endif

#ifdef SYS_lchown
    EXPECT(0, "symlinkat", MADE_LINK "/lchown",
           syscall(SYS_symlinkat, "nowhere", AT_FDCWD, MADE_LINK "/lchown"));
    EXPECT(0, "lchown", MADE "/lchown", syscall(SYS_lchown, MADE "/lchown", user, group));
    EXPECT(ENOENT, "lchown", OUT "/lchown-missing",
           syscall(SYS_lchown, OUT "/lchown-missing", user, group));
#endif

    make("fchownat");
    EXPECT(0, "fchownat", MADE "/fchownat",
           syscall(SYS_fchownat, AT_FDCWD, MADE "/fchownat", user, group, 0));
    make("fchownat-at");
    EXPECT(0, "fchownat", "fchownat-at",
           syscall(SYS_fchownat, made_fd, "fchownat-at", user, group, 0));
    EXPECT(0, "symlinkat", MADE_LINK "/fchownat-link",
           syscall(SYS_symlinkat, "nowhere", AT_FDCWD, MADE_LINK "/fchownat-link"));
    EXPECT(0, "fchownat", MADE "/fchownat-link",
           syscall(SYS_fchownat, AT_FDCWD, MADE "/fchownat-link", user, group,
                   AT_SYMLINK_NOFOLLOW));
    fd = make_fd("fchownat-empty");
    EXPECT(0, "fchownat", MADE "/fchownat-empty",
           syscall(SYS_fchownat, fd, "", user, group, AT_EMPTY_PATH));
    close_fd(fd);
    EXPECT(ENOENT, "fchownat", OUT "/fchownat-missing",
           syscall(SYS_fchownat, AT_FDCWD, OUT "/fchownat-missing", user, group, 0));

#ifdef SYS_utime
    make("utime");
    EXPECT(0, "utime", MADE "/utime", syscall(SYS_utime, MADE "/utime", NULL));
    EXPECT(ENOENT, "utime", OUT "/utime-missing", syscall(SYS_utime, OUT "/utime-missing", NULL));
#endif

#ifdef SYS_utimes
    make("utimes");
    EXPECT(0, "utimes", MADE "/utimes", syscall(SYS_utimes, MADE "/utimes", NULL));
    EXPECT(ENOENT, "utimes", OUT "/utimes-missing",
           syscall(SYS_utimes, OUT "/utimes-missing", NULL));
#endif

    make("utimensat");
    EXPECT(0, "utimensat", MADE "/utimensat",
           syscall(SYS_utimensat, AT_FDCWD, MADE "/utimensat", NULL, 0));
    make("utimensat-at");
    EXPECT(0, "utimensat", "utimensat-at",
           syscall(SYS_utimensat, made_fd, "utimensat-at", NULL, 0));
    EXPECT(0, "symlinkat", MADE_LINK "/utimensat-link",
           syscall(SYS_symlinkat, "nowhere", AT_FDCWD, MADE_LINK "/utimensat-link"));
    EXPECT(0, "utimensat", MADE "/utimensat-link",
           syscall(SYS_utimensat, AT_FDCWD, MADE "/utimensat-link", NULL, AT_SYMLINK_NOFOLLOW));
    fd = make_fd("utimensat-null");
    EXPECT(0, "utimensat", MADE "/utimensat-null", syscall(SYS_utimensat, fd, NULL, NULL, 0));
    close_fd(fd);
    fd = make_fd("utimensat-empty");
    EXPECT(0, "utimensat", MADE "/utimensat-empty",
           syscall(SYS_utimensat, fd, "", NULL, AT_EMPTY_PATH));
    close_fd(fd);
    EXPECT(ENOENT, "utimensat", OUT "/utimensat-missing",
           syscall(SYS_utimensat, AT_FDCWD, OUT "/utimensat-missing", NULL, 0));

#ifdef SYS_futimesat
    make("futimesat");
    EXPECT(0, "futimesat", MADE "/futimesat",
           syscall(SYS_futimesat, AT_FDCWD, MADE "/futimesat", NULL));
    make("futimesat-at");
    EXPECT(0, "futimesat", "futimesat-at", syscall(SYS_futimesat, made_fd, "futimesat-at", NULL));
    fd = make_fd("futimesat-null");
    EXPECT(0, "futimesat", MADE "/futimesat-null", syscall(SYS_futimesat, fd, NULL, NULL));
    close_fd(fd);
    EXPECT(ENOENT, "futimesat", OUT "/futimesat-missing",
           syscall(SYS_futimesat, AT_FDCWD, OUT "/futimesat-missing", NULL));
#endif

    /* Changes through descriptors, which name the files under out/linux/made. */
    fd = make_fd("ftruncate");
    EXPECT(0, "ftruncate", MADE "/ftruncate", syscall(SYS_ftruncate, fd, 0));
    close_fd(fd);
    fd = make_fd("fchmod");
    EXPECT(0, "fchmod", MADE "/fchmod", syscall(SYS_fchmod, fd, 0600));
    close_fd(fd);
    fd = make_fd("fchown");
    EXPECT(0, "fchown", MADE "/fchown", syscall(SYS_fchown, fd, user, group));
    close_fd(fd);
}

/* The extended attribute that the Linux section sets, only on files that it creates. */
#define ATTRIBUTE "user.kizz"
/* The value of ATTRIBUTE. */
#define ATTRIBUTE_VALUE "coverage"

/*
 * The numbers of the calls that older C library headers lack. Each was added after
 * the system call tables were unified, so it has the same number on every
 * architecture, as the kernel's x86_64 table and its generic table give it.
 */
#ifndef SYS_fchmodat2
#define SYS_fchmodat2 452
#endif
#ifndef SYS_setxattrat
#define SYS_setxattrat 463
#endif
#ifndef SYS_getxattrat
#define SYS_getxattrat 464
#endif
#ifndef SYS_listxattrat
#define SYS_listxattrat 465
#endif
#ifndef SYS_removexattrat
#define SYS_removexattrat 466
#endif
#ifndef SYS_file_getattr
#define SYS_file_getattr 468
#endif
#ifndef SYS_file_setattr
#define SYS_file_setattr 469
#endif

/* The class of a fanotify group that an unprivileged process may create since Linux
 * 5.13: FAN_CLASS_NOTIF (0) with FAN_REPORT_FID, and FAN_CLOEXEC. */
#define FANOTIFY_CLASS 0x201
/* FAN_MARK_ADD, which adds a mark. */
#define FANOTIFY_MARK_ADD 0x1
/* FAN_OPEN, the event of opening a file. */
#define FANOTIFY_OPEN 0x20
/* IN_OPEN, the event of opening a file. */
#define INOTIFY_OPEN 0x20

/* `struct file_attr`, which file_getattr and file_setattr take: FILE_ATTR_SIZE_VER0 bytes. */
struct file_attr_argument {
    uint64_t xflags;
    uint32_t extsize;
    uint32_t nextents;
    uint32_t projid;
    uint32_t cowextsize;
};

/* `struct xattr_args`, which the xattrat calls take: where a value is, and its size. */
struct xattr_args_argument {
    uint64_t value;
    uint32_t size;
    uint32_t flags;
};

/* `struct file_handle` with room for MAX_HANDLE_SZ bytes of the handle. */
struct handle_argument {
    uint32_t bytes;
    int32_t type;
    unsigned char handle[128];
};

/* A buffer for the values and names of extended attributes. */
static char attribute_buffer[256];

/* Set ATTRIBUTE on the file MADE_LINK/name, which is also MADE/name. */
static void set_attribute(const char *name)
{
    char path[PATH_MAX];
    snprintf(path, sizeof path, MADE_LINK "/%s", name);
    EXPECT(0, "setxattr", path,
           syscall(SYS_setxattr, path, ATTRIBUTE, ATTRIBUTE_VALUE, strlen(ATTRIBUTE_VALUE), 0));
}

/* Create MADE_LINK/name and set ATTRIBUTE on it. */
static void make_with_attribute(const char *name)
{
    make(name);
    set_attribute(name);
}

/* The arguments of an xattrat call whose value is `size` bytes at `value`. */
static struct xattr_args_argument xattr_args(const void *value, size_t size)
{
    struct xattr_args_argument arguments = {(uint64_t)(uintptr_t)value, (uint32_t)size, 0};
    return arguments;
}

/*
 * The tracked calls that a kernel may lack because a newer Linux added them, in
 * the order of TRACKED_SYSTEM_CALLS.
 */
enum gated_call {
    GATED_FILE_GETATTR,
    GATED_GETXATTRAT,
    GATED_LISTXATTRAT,
    GATED_SETXATTRAT,
    GATED_REMOVEXATTRAT,
    GATED_FILE_SETATTR,
};

/* The number of gated calls. */
#define GATED_CALLS 6

/* The name of each gated call. */
static const char *const gated_names[GATED_CALLS] = {
    "file_getattr", "getxattrat", "listxattrat", "setxattrat", "removexattrat", "file_setattr",
};

/* Whether each gated call is available, as check_availability found. */
static int gated_available[GATED_CALLS];

/* A flag that listxattrat and removexattrat do not accept. */
#define UNACCEPTED_FLAG AT_REMOVEDIR

/* The path of each availability check: the root, outside the task directory. */
#define CHECKED_PATH "/"

/*
 * Make the gated call `call` once on CHECKED_PATH with arguments that the kernel
 * rejects with EINVAL before it looks the path up, where it has the call: a
 * structure size of 0, or a flag that the call does not accept. The call can
 * neither succeed nor change anything.
 */
static long check_call(enum gated_call call)
{
    struct xattr_args_argument arguments = xattr_args(attribute_buffer, sizeof attribute_buffer);
    struct file_attr_argument attributes;
    memset(&attributes, 0, sizeof attributes);
    switch (call) {
    case GATED_FILE_GETATTR:
        return syscall(SYS_file_getattr, AT_FDCWD, CHECKED_PATH, &attributes, 0, 0);
    case GATED_GETXATTRAT:
        return syscall(SYS_getxattrat, AT_FDCWD, CHECKED_PATH, 0, ATTRIBUTE, &arguments, 0);
    case GATED_LISTXATTRAT:
        return syscall(SYS_listxattrat, AT_FDCWD, CHECKED_PATH, UNACCEPTED_FLAG, attribute_buffer,
                       sizeof attribute_buffer);
    case GATED_SETXATTRAT:
        return syscall(SYS_setxattrat, AT_FDCWD, CHECKED_PATH, 0, ATTRIBUTE, &arguments, 0);
    case GATED_REMOVEXATTRAT:
        return syscall(SYS_removexattrat, AT_FDCWD, CHECKED_PATH, UNACCEPTED_FLAG, ATTRIBUTE);
    case GATED_FILE_SETATTR:
        return syscall(SYS_file_setattr, AT_FDCWD, CHECKED_PATH, &attributes, 0, 0);
    }
    /* No other call is gated; a success here would be a mismatch. */
    return 0;
}

/*
 * Check whether each gated call is available: ENOSYS means that the kernel lacks
 * the call, or that a seccomp filter installed before kizz's own refuses it, so the
 * call is unavailable, and EINVAL that it is available; any other result is a
 * mismatch, and the call then counts as available. Print one line for each, in
 * order.
 */
static void check_availability(void)
{
    for (int index = 0; index < GATED_CALLS; index++) {
        errno = 0;
        long result = check_call((enum gated_call)index);
        int error = errno;
        gated_available[index] = !(result < 0 && error == ENOSYS);
        if (gated_available[index]) {
            check(gated_names[index], CHECKED_PATH, result, error, EINVAL);
        }
    }
    for (int index = 0; index < GATED_CALLS; index++) {
        printf("coverage: %s: %s\n", gated_available[index] ? "available" : "unavailable",
               gated_names[index]);
    }
    fflush(stdout);
}

/*
 * File systems, file attributes, file handles and watches: statfs and fstatfs,
 * file_getattr and file_setattr, name_to_handle_at, inotify_add_watch and
 * fanotify_mark, each on files made through out/linux/made-link.
 */
static void linux_file_systems_and_watches(void)
{
    struct file_attr_argument attributes;
    struct handle_argument handle;
    int mount_id = 0;
    int fd;

    make("statfs");
    EXPECT(0, "statfs", MADE "/statfs", syscall(SYS_statfs, MADE "/statfs", &stat_buffer));
    EXPECT(ENOENT, "statfs", OUT "/statfs-missing",
           syscall(SYS_statfs, OUT "/statfs-missing", &stat_buffer));
    fd = make_fd("fstatfs");
    EXPECT(0, "fstatfs", MADE "/fstatfs", syscall(SYS_fstatfs, fd, &stat_buffer));
    close_fd(fd);

    /* The group of file_getattr, only where it is available. */
    memset(&attributes, 0, sizeof attributes);
    if (gated_available[GATED_FILE_GETATTR]) {
        make("file_getattr");
        EXPECT(0, "file_getattr", MADE "/file_getattr",
               syscall(SYS_file_getattr, AT_FDCWD, MADE "/file_getattr", &attributes,
                       sizeof attributes, 0));
        fd = make_fd("file_getattr-empty");
        EXPECT(0, "file_getattr", MADE "/file_getattr-empty",
               syscall(SYS_file_getattr, fd, "", &attributes, sizeof attributes, AT_EMPTY_PATH));
        close_fd(fd);
        EXPECT(ENOENT, "file_getattr", OUT "/file_getattr-missing",
               syscall(SYS_file_getattr, AT_FDCWD, OUT "/file_getattr-missing", &attributes,
                       sizeof attributes, 0));
    }

    /*
     * The group of file_setattr, which sets only the attributes that file_getattr
     * read through the link, only where both calls are available.
     */
    if (gated_available[GATED_FILE_GETATTR] && gated_available[GATED_FILE_SETATTR]) {
        make("file_setattr");
        EXPECT(0, "file_getattr", MADE_LINK "/file_setattr",
               syscall(SYS_file_getattr, AT_FDCWD, MADE_LINK "/file_setattr", &attributes,
                       sizeof attributes, 0));
        EXPECT(0, "file_setattr", MADE "/file_setattr",
               syscall(SYS_file_setattr, AT_FDCWD, MADE "/file_setattr", &attributes,
                       sizeof attributes, 0));
        EXPECT(ENOENT, "file_setattr", OUT "/file_setattr-missing",
               syscall(SYS_file_setattr, AT_FDCWD, OUT "/file_setattr-missing", &attributes,
                       sizeof attributes, 0));
    }

    make("name_to_handle_at");
    handle.bytes = sizeof handle.handle;
    EXPECT(0, "name_to_handle_at", MADE "/name_to_handle_at",
           syscall(SYS_name_to_handle_at, AT_FDCWD, MADE "/name_to_handle_at", &handle,
                   &mount_id, 0));
    handle.bytes = sizeof handle.handle;
    EXPECT(ENOENT, "name_to_handle_at", OUT "/name_to_handle_at-missing",
           syscall(SYS_name_to_handle_at, AT_FDCWD, OUT "/name_to_handle_at-missing", &handle,
                   &mount_id, 0));

    /* Watches, through an inotify instance and a fanotify group of the program's own. */
    int watcher = (int)EXPECT(0, "inotify_init1", "(none)",
                              syscall(SYS_inotify_init1, O_CLOEXEC));
    make("inotify");
    EXPECT(0, "inotify_add_watch", MADE "/inotify",
           syscall(SYS_inotify_add_watch, watcher, MADE "/inotify", INOTIFY_OPEN));
    EXPECT(ENOENT, "inotify_add_watch", OUT "/inotify-missing",
           syscall(SYS_inotify_add_watch, watcher, OUT "/inotify-missing", INOTIFY_OPEN));
    close_fd(watcher);
    int group = (int)EXPECT(0, "fanotify_init", "(none)",
                            syscall(SYS_fanotify_init, FANOTIFY_CLASS, O_RDONLY | O_CLOEXEC));
    make("fanotify");
    EXPECT(0, "fanotify_mark", MADE "/fanotify",
           syscall(SYS_fanotify_mark, group, FANOTIFY_MARK_ADD, (uint64_t)FANOTIFY_OPEN,
                   AT_FDCWD, MADE "/fanotify"));
    EXPECT(ENOENT, "fanotify_mark", OUT "/fanotify-missing",
           syscall(SYS_fanotify_mark, group, FANOTIFY_MARK_ADD, (uint64_t)FANOTIFY_OPEN,
                   AT_FDCWD, OUT "/fanotify-missing"));
    close_fd(group);
}

/*
 * Reading and listing extended attributes: getxattr, lgetxattr, fgetxattr and
 * getxattrat, listxattr, llistxattr, flistxattr and listxattrat, each on a file whose
 * attribute was set through out/linux/made-link; getxattr also through a link to
 * such a file, and with ENODATA and ERANGE, which record the xattr kind.
 */
static void linux_attribute_reads(void)
{
    struct xattr_args_argument arguments = xattr_args(attribute_buffer, sizeof attribute_buffer);
    int fd;

    make_with_attribute("getxattr");
    EXPECT(0, "getxattr", MADE "/getxattr",
           syscall(SYS_getxattr, MADE "/getxattr", ATTRIBUTE, attribute_buffer,
                   sizeof attribute_buffer));
    EXPECT(ENOENT, "getxattr", OUT "/getxattr-missing",
           syscall(SYS_getxattr, OUT "/getxattr-missing", ATTRIBUTE, attribute_buffer,
                   sizeof attribute_buffer));
    make("getxattr-nodata");
    EXPECT(ENODATA, "getxattr", MADE "/getxattr-nodata",
           syscall(SYS_getxattr, MADE "/getxattr-nodata", ATTRIBUTE, attribute_buffer,
                   sizeof attribute_buffer));
    make_with_attribute("getxattr-range");
    EXPECT(ERANGE, "getxattr", MADE "/getxattr-range",
           syscall(SYS_getxattr, MADE "/getxattr-range", ATTRIBUTE, attribute_buffer, 1));
    EXPECT(0, "symlinkat", MADE_LINK "/getxattr-link",
           syscall(SYS_symlinkat, "getxattr", AT_FDCWD, MADE_LINK "/getxattr-link"));
    EXPECT(0, "getxattr", MADE "/getxattr-link",
           syscall(SYS_getxattr, MADE "/getxattr-link", ATTRIBUTE, attribute_buffer,
                   sizeof attribute_buffer));

    make_with_attribute("lgetxattr");
    EXPECT(0, "lgetxattr", MADE "/lgetxattr",
           syscall(SYS_lgetxattr, MADE "/lgetxattr", ATTRIBUTE, attribute_buffer,
                   sizeof attribute_buffer));
    EXPECT(ENOENT, "lgetxattr", OUT "/lgetxattr-missing",
           syscall(SYS_lgetxattr, OUT "/lgetxattr-missing", ATTRIBUTE, attribute_buffer,
                   sizeof attribute_buffer));

    fd = make_fd("fgetxattr");
    set_attribute("fgetxattr");
    EXPECT(0, "fgetxattr", MADE "/fgetxattr",
           syscall(SYS_fgetxattr, fd, ATTRIBUTE, attribute_buffer, sizeof attribute_buffer));
    close_fd(fd);

    /* The group of getxattrat, only where it is available. */
    if (gated_available[GATED_GETXATTRAT]) {
        make_with_attribute("getxattrat");
        EXPECT(0, "getxattrat", MADE "/getxattrat",
               syscall(SYS_getxattrat, AT_FDCWD, MADE "/getxattrat", 0, ATTRIBUTE, &arguments,
                       sizeof arguments));
        make_with_attribute("getxattrat-at");
        EXPECT(0, "getxattrat", "getxattrat-at",
               syscall(SYS_getxattrat, made_fd, "getxattrat-at", 0, ATTRIBUTE, &arguments,
                       sizeof arguments));
        fd = make_fd("getxattrat-empty");
        set_attribute("getxattrat-empty");
        EXPECT(0, "getxattrat", MADE "/getxattrat-empty",
               syscall(SYS_getxattrat, fd, "", AT_EMPTY_PATH, ATTRIBUTE, &arguments,
                       sizeof arguments));
        close_fd(fd);
        EXPECT(ENOENT, "getxattrat", OUT "/getxattrat-missing",
               syscall(SYS_getxattrat, AT_FDCWD, OUT "/getxattrat-missing", 0, ATTRIBUTE,
                       &arguments, sizeof arguments));
    }

    make_with_attribute("listxattr");
    EXPECT(0, "listxattr", MADE "/listxattr",
           syscall(SYS_listxattr, MADE "/listxattr", attribute_buffer, sizeof attribute_buffer));
    EXPECT(ENOENT, "listxattr", OUT "/listxattr-missing",
           syscall(SYS_listxattr, OUT "/listxattr-missing", attribute_buffer,
                   sizeof attribute_buffer));
    make_with_attribute("llistxattr");
    EXPECT(0, "llistxattr", MADE "/llistxattr",
           syscall(SYS_llistxattr, MADE "/llistxattr", attribute_buffer,
                   sizeof attribute_buffer));
    EXPECT(ENOENT, "llistxattr", OUT "/llistxattr-missing",
           syscall(SYS_llistxattr, OUT "/llistxattr-missing", attribute_buffer,
                   sizeof attribute_buffer));
    fd = make_fd("flistxattr");
    set_attribute("flistxattr");
    EXPECT(0, "flistxattr", MADE "/flistxattr",
           syscall(SYS_flistxattr, fd, attribute_buffer, sizeof attribute_buffer));
    close_fd(fd);
    /* The group of listxattrat, only where it is available. */
    if (gated_available[GATED_LISTXATTRAT]) {
        make_with_attribute("listxattrat");
        EXPECT(0, "listxattrat", MADE "/listxattrat",
               syscall(SYS_listxattrat, AT_FDCWD, MADE "/listxattrat", 0, attribute_buffer,
                       sizeof attribute_buffer));
        EXPECT(ENOENT, "listxattrat", OUT "/listxattrat-missing",
               syscall(SYS_listxattrat, AT_FDCWD, OUT "/listxattrat-missing", 0,
                       attribute_buffer, sizeof attribute_buffer));
    }
}

/*
 * Changing extended attributes, the mode with fchmodat2, and the allocation with
 * fallocate: setxattr, lsetxattr, fsetxattr and setxattrat set ATTRIBUTE on files
 * made through out/linux/made-link, and removexattr, lremovexattr, fremovexattr and
 * removexattrat remove it from files on which it was set through the link.
 */
static void linux_attribute_changes(void)
{
    size_t length = strlen(ATTRIBUTE_VALUE);
    struct xattr_args_argument arguments = xattr_args(ATTRIBUTE_VALUE, length);
    int fd;

    make("setxattr");
    EXPECT(0, "setxattr", MADE "/setxattr",
           syscall(SYS_setxattr, MADE "/setxattr", ATTRIBUTE, ATTRIBUTE_VALUE, length, 0));
    EXPECT(ENOENT, "setxattr", OUT "/setxattr-missing",
           syscall(SYS_setxattr, OUT "/setxattr-missing", ATTRIBUTE, ATTRIBUTE_VALUE, length, 0));
    make("lsetxattr");
    EXPECT(0, "lsetxattr", MADE "/lsetxattr",
           syscall(SYS_lsetxattr, MADE "/lsetxattr", ATTRIBUTE, ATTRIBUTE_VALUE, length, 0));
    EXPECT(ENOENT, "lsetxattr", OUT "/lsetxattr-missing",
           syscall(SYS_lsetxattr, OUT "/lsetxattr-missing", ATTRIBUTE, ATTRIBUTE_VALUE, length,
                   0));
    fd = make_fd("fsetxattr");
    EXPECT(0, "fsetxattr", MADE "/fsetxattr",
           syscall(SYS_fsetxattr, fd, ATTRIBUTE, ATTRIBUTE_VALUE, length, 0));
    close_fd(fd);
    /* The group of setxattrat, only where it is available. */
    if (gated_available[GATED_SETXATTRAT]) {
        make("setxattrat");
        EXPECT(0, "setxattrat", MADE "/setxattrat",
               syscall(SYS_setxattrat, AT_FDCWD, MADE "/setxattrat", 0, ATTRIBUTE, &arguments,
                       sizeof arguments));
        EXPECT(ENOENT, "setxattrat", OUT "/setxattrat-missing",
               syscall(SYS_setxattrat, AT_FDCWD, OUT "/setxattrat-missing", 0, ATTRIBUTE,
                       &arguments, sizeof arguments));
    }

    make_with_attribute("removexattr");
    EXPECT(0, "removexattr", MADE "/removexattr",
           syscall(SYS_removexattr, MADE "/removexattr", ATTRIBUTE));
    EXPECT(ENOENT, "removexattr", OUT "/removexattr-missing",
           syscall(SYS_removexattr, OUT "/removexattr-missing", ATTRIBUTE));
    make_with_attribute("lremovexattr");
    EXPECT(0, "lremovexattr", MADE "/lremovexattr",
           syscall(SYS_lremovexattr, MADE "/lremovexattr", ATTRIBUTE));
    EXPECT(ENOENT, "lremovexattr", OUT "/lremovexattr-missing",
           syscall(SYS_lremovexattr, OUT "/lremovexattr-missing", ATTRIBUTE));
    fd = make_fd("fremovexattr");
    set_attribute("fremovexattr");
    EXPECT(0, "fremovexattr", MADE "/fremovexattr", syscall(SYS_fremovexattr, fd, ATTRIBUTE));
    close_fd(fd);
    /* The group of removexattrat, only where it is available. */
    if (gated_available[GATED_REMOVEXATTRAT]) {
        make_with_attribute("removexattrat");
        EXPECT(0, "removexattrat", MADE "/removexattrat",
               syscall(SYS_removexattrat, AT_FDCWD, MADE "/removexattrat", 0, ATTRIBUTE));
        EXPECT(ENOENT, "removexattrat", OUT "/removexattrat-missing",
               syscall(SYS_removexattrat, AT_FDCWD, OUT "/removexattrat-missing", 0,
                       ATTRIBUTE));
    }

    make("fchmodat2");
    EXPECT(0, "fchmodat2", MADE "/fchmodat2",
           syscall(SYS_fchmodat2, AT_FDCWD, MADE "/fchmodat2", 0600, 0));
    fd = make_fd("fchmodat2-empty");
    EXPECT(0, "fchmodat2", MADE "/fchmodat2-empty",
           syscall(SYS_fchmodat2, fd, "", 0600, AT_EMPTY_PATH));
    close_fd(fd);
    EXPECT(ENOENT, "fchmodat2", OUT "/fchmodat2-missing",
           syscall(SYS_fchmodat2, AT_FDCWD, OUT "/fchmodat2-missing", 0600, 0));

    fd = make_fd("fallocate");
    EXPECT(0, "fallocate", MADE "/fallocate", syscall(SYS_fallocate, fd, 0, 0, 4096));
    close_fd(fd);
}

/*
 * Calls that would mark the run with other arguments but leave it complete with
 * these: unshare without CLONE_NEWNS, and setns with another namespace type on an
 * invalid descriptor. Their results do not matter, because an environment may
 * forbid them, so they are not checked.
 */
static void linux_namespaces(void)
{
    (void)syscall(SYS_unshare, CLONE_FILES);
    (void)syscall(SYS_setns, -1, CLONE_NEWUTS);
}

/*
 * Send the payload from the socket `fd` to `address`, `length` bytes long, named `what`, with the
 * system call `call`, sendto, sendmsg or sendmmsg with one message, through syscall(), and check
 * the result against `expected`.
 */
static void send_with(const char *call, int fd, const void *address, socklen_t length,
                      const char *what, int expected)
{
    struct iovec vector = {payload, sizeof payload - 1};
    struct mmsghdr messages[1];
    memset(messages, 0, sizeof messages);
    messages[0].msg_hdr.msg_name = (void *)address;
    messages[0].msg_hdr.msg_namelen = length;
    messages[0].msg_hdr.msg_iov = &vector;
    messages[0].msg_hdr.msg_iovlen = 1;
    if (strcmp(call, "sendto") == 0) {
        EXPECT(expected, call, what,
               syscall(SYS_sendto, fd, payload, sizeof payload - 1, 0, address, length));
    } else if (strcmp(call, "sendmsg") == 0) {
        EXPECT(expected, call, what, syscall(SYS_sendmsg, fd, &messages[0].msg_hdr, 0));
    } else {
        EXPECT(expected, call, what, syscall(SYS_sendmmsg, fd, messages, 1, 0));
    }
}

/*
 * Bind a datagram socket to OUT/CALL.sock and send to it with `call`, then send to the missing
 * OUT/CALL-missing.sock, which fails with ENOENT.
 */
static void unix_datagrams(const char *call)
{
    char path[64];
    char missing_path[64];
    snprintf(path, sizeof path, OUT "/%s.sock", call);
    snprintf(missing_path, sizeof missing_path, OUT "/%s-missing.sock", call);
    socklen_t length;
    struct sockaddr_un address = unix_address(path, &length);
    int receiver = open_socket(path, AF_UNIX, SOCK_DGRAM);
    EXPECT(0, "bind", path, syscall(SYS_bind, receiver, &address, length));
    int sender = open_socket(path, AF_UNIX, SOCK_DGRAM);
    send_with(call, sender, &address, length, path, 0);
    socklen_t missing_length;
    struct sockaddr_un missing = unix_address(missing_path, &missing_length);
    send_with(call, sender, &missing, missing_length, missing_path, ENOENT);
    close_fd(sender);
    close_fd(receiver);
}

/*
 * Sockets: bind, connect, sendto, sendmsg and sendmmsg, each through syscall(), on Unix sockets
 * under OUT, with a missing path for each, and through UDP sockets to 127.0.0.21 through
 * 127.0.0.25, one address for each call and port 47021 through 47025.
 */
static void linux_sockets(void)
{
    socklen_t length;
    struct sockaddr_un bound = unix_address(OUT "/bound.sock", &length);
    int listener = open_socket(OUT "/bound.sock", AF_UNIX, SOCK_STREAM);
    EXPECT(0, "bind", OUT "/bound.sock", syscall(SYS_bind, listener, &bound, length));
    EXPECT(0, "listen", OUT "/bound.sock", listen(listener, 1));
    int client = open_socket(OUT "/bound.sock", AF_UNIX, SOCK_STREAM);
    EXPECT(0, "connect", OUT "/bound.sock", syscall(SYS_connect, client, &bound, length));
    close_fd(client);
    close_fd(listener);

    client = open_socket(OUT "/no-dir/bind.sock", AF_UNIX, SOCK_STREAM);
    struct sockaddr_un missing = unix_address(OUT "/no-dir/bind.sock", &length);
    EXPECT(ENOENT, "bind", OUT "/no-dir/bind.sock", syscall(SYS_bind, client, &missing, length));
    missing = unix_address(OUT "/connect-missing.sock", &length);
    EXPECT(ENOENT, "connect", OUT "/connect-missing.sock",
           syscall(SYS_connect, client, &missing, length));
    close_fd(client);

    unix_datagrams("sendto");
    unix_datagrams("sendmsg");
    unix_datagrams("sendmmsg");

    struct sockaddr_in address = loopback(21, 47021);
    int udp = open_socket("127.0.0.21:47021", AF_INET, SOCK_DGRAM);
    EXPECT(0, "connect", "127.0.0.21:47021",
           syscall(SYS_connect, udp, &address, sizeof address));
    close_fd(udp);
    address = loopback(22, 47022);
    udp = open_socket("127.0.0.22:47022", AF_INET, SOCK_DGRAM);
    send_with("sendto", udp, &address, sizeof address, "127.0.0.22:47022", 0);
    close_fd(udp);
    address = loopback(23, 47023);
    udp = open_socket("127.0.0.23:47023", AF_INET, SOCK_DGRAM);
    send_with("sendmsg", udp, &address, sizeof address, "127.0.0.23:47023", 0);
    close_fd(udp);

    /* sendmmsg sends two messages, to two addresses. */
    struct sockaddr_in first = loopback(24, 47024);
    struct sockaddr_in second = loopback(25, 47025);
    struct iovec vector = {payload, sizeof payload - 1};
    struct mmsghdr messages[2];
    memset(messages, 0, sizeof messages);
    messages[0].msg_hdr.msg_name = &first;
    messages[0].msg_hdr.msg_namelen = sizeof first;
    messages[1].msg_hdr.msg_name = &second;
    messages[1].msg_hdr.msg_namelen = sizeof second;
    for (int index = 0; index < 2; index++) {
        messages[index].msg_hdr.msg_iov = &vector;
        messages[index].msg_hdr.msg_iovlen = 1;
    }
    udp = open_socket("127.0.0.24:47024", AF_INET, SOCK_DGRAM);
    long sent = EXPECT(0, "sendmmsg", "127.0.0.24:47024 and 127.0.0.25:47025",
                       syscall(SYS_sendmmsg, udp, messages, 2, 0));
    if (sent >= 0 && sent != 2) {
        check("sendmmsg", "127.0.0.24:47024 and 127.0.0.25:47025", -1, 0, 0);
    }
    close_fd(udp);
}

/*
 * The Linux section: every tracked system call, made through syscall(), after the
 * check of the gated calls, whose groups it makes only where they are available.
 */
static void linux_section(void)
{
    check_availability();
    EXPECT(0, "mkdirat", OUT, syscall(SYS_mkdirat, AT_FDCWD, OUT, 0777));
    EXPECT(0, "mkdirat", MADE, syscall(SYS_mkdirat, AT_FDCWD, MADE, 0777));
    EXPECT(0, "symlinkat", MADE_LINK, syscall(SYS_symlinkat, "made", AT_FDCWD, MADE_LINK));
    made_fd = (int)EXPECT(0, "openat", MADE,
                          syscall(SYS_openat, AT_FDCWD, MADE, O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    in_fd = (int)EXPECT(0, "openat", "in/linux/at",
                        syscall(SYS_openat, AT_FDCWD, "in/linux/at",
                                O_RDONLY | O_DIRECTORY | O_CLOEXEC));

    linux_open();
    linux_metadata();
    linux_links();
    linux_listings_and_processes();
    linux_names();
    linux_changes();
    linux_file_systems_and_watches();
    linux_attribute_reads();
    linux_attribute_changes();
    linux_namespaces();
    linux_sockets();

#ifdef SYS_io_uring_setup
    /* io_uring would bypass the tracer, so it is not available. */
    unsigned char parameters[120];
    memset(parameters, 0, sizeof parameters);
    EXPECT(ENOSYS, "io_uring_setup", "(none)", syscall(SYS_io_uring_setup, 1, parameters));
#endif

    close_fd(in_fd);
    close_fd(made_fd);
}

/* Run the fixture script `path` with execveat from the working directory in a child. */
static void run_script_at(const char *path)
{
    char *arguments[] = {(char *)path, NULL};
    char *environment[] = {NULL};
    pid_t pid = start_child(path);
    if (pid == 0) {
#ifdef SYS_execveat
        EXPECT(0, "execveat", path,
               syscall(SYS_execveat, AT_FDCWD, path, arguments, environment, 0));
#else
        check("execveat", path, -1, ENOSYS, 0);
#endif
        _exit(1);
    }
    wait_child(path, pid);
}

/*
 * The Linux modes of the coverage-changes task: each first reads or runs its
 * committed file under in/changes through one Linux call, then rewrites the file
 * that it read or ran. Return whether `mode` is one of them.
 */
static int linux_change(const char *mode)
{
    int fd;
    if (strcmp(mode, "open") == 0) {
#ifdef SYS_open
        fd = (int)EXPECT(0, "open", CHANGES "/open.txt",
                         syscall(SYS_open, CHANGES "/open.txt", O_RDONLY | O_CLOEXEC));
        read_some(CHANGES "/open.txt", fd);
        close_fd(fd);
#else
        check("open", CHANGES "/open.txt", -1, ENOSYS, 0);
#endif
        write_stream(CHANGES "/open.txt");
        return 1;
    }
    if (strcmp(mode, "openat") == 0) {
        read_at(AT_FDCWD, CHANGES "/openat.txt");
        write_stream(CHANGES "/openat.txt");
        return 1;
    }
    if (strcmp(mode, "openat2") == 0) {
#ifdef SYS_openat2
        struct open_how_argument how = {O_RDONLY | O_CLOEXEC, 0, 0};
        fd = (int)EXPECT(0, "openat2", CHANGES "/openat2.txt",
                         syscall(SYS_openat2, AT_FDCWD, CHANGES "/openat2.txt", &how, sizeof how));
        read_some(CHANGES "/openat2.txt", fd);
        close_fd(fd);
#else
        check("openat2", CHANGES "/openat2.txt", -1, ENOSYS, 0);
#endif
        write_stream(CHANGES "/openat2.txt");
        return 1;
    }
    if (strcmp(mode, "execve") == 0) {
        run_script(CHANGES "/execve", 0);
        write_stream(CHANGES "/execve");
        return 1;
    }
    if (strcmp(mode, "execveat") == 0) {
        run_script_at(CHANGES "/execveat");
        write_stream(CHANGES "/execveat");
        return 1;
    }
    if (strcmp(mode, "interpreter") == 0) {
        /* The kernel runs the script through in/changes/interpreter, a script itself. */
        run_script(CHANGES "/through-interpreter", 0);
        write_stream(CHANGES "/interpreter");
        return 1;
    }
    return 0;
}

#endif

/* Run the fixture script `path` with the C library's execv in a child, and wait for it. */
static void run_execv(const char *path)
{
    char *arguments[] = {(char *)path, NULL};
    pid_t pid = start_child(path);
    if (pid == 0) {
        EXPECT(0, "execv", path, execv(path, arguments));
        _exit(1);
    }
    wait_child(path, pid);
}

/*
 * A step of the coverage-changes task in `mode`, without its prefix: the portable
 * modes, which read or run their committed file through the C library and then
 * rewrite it, the control, which writes its file before it reads it, and on Linux
 * the modes of linux_change.
 */
static void change(const char *mode)
{
    if (strcmp(mode, "fopen") == 0) {
        read_stream(CHANGES "/fopen.txt");
        write_stream(CHANGES "/fopen.txt");
        return;
    }
    if (strcmp(mode, "execv") == 0) {
        run_execv(CHANGES "/execv");
        write_stream(CHANGES "/execv");
        return;
    }
    if (strcmp(mode, "control") == 0) {
        EXPECT(0, "mkdir", "out", mkdir("out", 0777));
        EXPECT(0, "mkdir", CHANGES_OUT, mkdir(CHANGES_OUT, 0777));
        write_stream(CHANGES_OUT "/control");
        read_stream(CHANGES_OUT "/control");
        write_stream(CHANGES_OUT "/control");
        return;
    }
#ifdef __linux__
    if (linux_change(mode)) {
        return;
    }
#endif
    check("change", mode, -1, EINVAL, 0);
}

/* The calls of a child in `mode`, which an exec started. */
static void child(const char *mode)
{
    char path[PATH_MAX + 64];
    if (strcmp(mode, "child-core") == 0) {
        /* Its working directory is in/core/cwd, so it names its file absolutely. */
        read_stream(in_task(path, sizeof path, "in/core/child-core.txt"));
        return;
    }
#ifdef __linux__
    snprintf(path, sizeof path, "in/linux/%s.txt", mode);
    read_at(AT_FDCWD, path);
#else
    check("child", mode, -1, EINVAL, 0);
#endif
}

#ifdef __linux__

/* A path that names nothing in the task directory, for the calls that take one. */
#define MISSING "in/incomplete/missing"

/* The numbers of the calls that older C library headers lack, as the kernel's
 * x86_64 table and its generic table give them. */
#ifndef SYS_open_tree
#define SYS_open_tree 428
#endif
#ifndef SYS_move_mount
#define SYS_move_mount 429
#endif
#ifndef SYS_fsopen
#define SYS_fsopen 430
#endif
#ifndef SYS_fsconfig
#define SYS_fsconfig 431
#endif
#ifndef SYS_fsmount
#define SYS_fsmount 432
#endif
#ifndef SYS_fspick
#define SYS_fspick 433
#endif
#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef SYS_mount_setattr
#define SYS_mount_setattr 442
#endif
#ifndef SYS_quotactl_fd
#define SYS_quotactl_fd 443
#endif
#ifndef SYS_open_tree_attr
#define SYS_open_tree_attr 467
#endif
#ifndef SYS_fchroot
#define SYS_fchroot 472
#endif

/* QCMD(Q_GETFMT, USRQUOTA): read the quota format of a file system. */
#define QUOTA_GET_FORMAT 0x80000400

/* `struct clone_args`, which clone3 takes: CLONE_ARGS_SIZE_VER0 bytes. */
struct clone_args_argument {
    uint64_t flags;
    uint64_t pidfd;
    uint64_t child_tid;
    uint64_t parent_tid;
    uint64_t exit_signal;
    uint64_t stack;
    uint64_t stack_size;
    uint64_t tls;
};

/*
 * Make the call `call` of the coverage-incomplete task in a way that changes
 * nothing if it succeeds: on a missing path, an invalid descriptor or handle, or
 * with flags that the kernel rejects before it acts. Set `*known` when `call` is one
 * of them, and return the call's result.
 */
static long incomplete_call(const char *call, int *known)
{
    struct handle_argument handle;
    struct clone_args_argument clone_args;
    memset(&handle, 0, sizeof handle);
    memset(&clone_args, 0, sizeof clone_args);
    /* The kernel rejects a new mount namespace that shares its file system information. */
    clone_args.flags = CLONE_NEWNS | CLONE_FS;
    *known = 1;
    if (strcmp(call, "open_by_handle_at") == 0) {
        return syscall(SYS_open_by_handle_at, -1, &handle, O_RDONLY);
    }
    if (strcmp(call, "chroot") == 0) {
        return syscall(SYS_chroot, MISSING);
    }
    if (strcmp(call, "fchroot") == 0) {
        return syscall(SYS_fchroot, -1);
    }
    if (strcmp(call, "pivot_root") == 0) {
        return syscall(SYS_pivot_root, MISSING, MISSING);
    }
    if (strcmp(call, "mount") == 0) {
        return syscall(SYS_mount, MISSING, MISSING, "none", 0, NULL);
    }
    if (strcmp(call, "umount2") == 0) {
        return syscall(SYS_umount2, MISSING, 0);
    }
    if (strcmp(call, "fsopen") == 0) {
        return syscall(SYS_fsopen, "kizz-no-such-file-system", 0);
    }
    if (strcmp(call, "fsconfig") == 0) {
        return syscall(SYS_fsconfig, -1, 0, "ro", NULL, 0);
    }
    if (strcmp(call, "fsmount") == 0) {
        return syscall(SYS_fsmount, -1, 0, 0);
    }
    if (strcmp(call, "fspick") == 0) {
        return syscall(SYS_fspick, AT_FDCWD, MISSING, 0);
    }
    if (strcmp(call, "move_mount") == 0) {
        return syscall(SYS_move_mount, AT_FDCWD, MISSING, AT_FDCWD, MISSING, 0);
    }
    if (strcmp(call, "open_tree") == 0) {
        return syscall(SYS_open_tree, AT_FDCWD, MISSING, 0);
    }
    if (strcmp(call, "open_tree_attr") == 0) {
        return syscall(SYS_open_tree_attr, AT_FDCWD, MISSING, 0, NULL, 0);
    }
    if (strcmp(call, "mount_setattr") == 0) {
        return syscall(SYS_mount_setattr, -1, "", AT_EMPTY_PATH, NULL, 0);
    }
    if (strcmp(call, "unshare") == 0) {
        /* CLONE_VFORK is not a flag of unshare, so the kernel rejects the call. */
        return syscall(SYS_unshare, CLONE_NEWNS | CLONE_VFORK);
    }
    if (strcmp(call, "setns") == 0) {
        return syscall(SYS_setns, -1, CLONE_NEWNS);
    }
    if (strcmp(call, "clone") == 0) {
        return syscall(SYS_clone, CLONE_NEWNS | CLONE_FS | SIGCHLD, 0, 0, 0, 0);
    }
    if (strcmp(call, "clone3") == 0) {
        /* A size of 0 is smaller than any struct clone_args, which the kernel rejects. */
        return syscall(SYS_clone3, &clone_args, 0);
    }
#ifdef SYS_uselib
    if (strcmp(call, "uselib") == 0) {
        return syscall(SYS_uselib, MISSING);
    }
#endif
    if (strcmp(call, "acct") == 0) {
        return syscall(SYS_acct, MISSING);
    }
    if (strcmp(call, "quotactl") == 0) {
        return syscall(SYS_quotactl, QUOTA_GET_FORMAT, MISSING, 0, attribute_buffer);
    }
    if (strcmp(call, "quotactl_fd") == 0) {
        return syscall(SYS_quotactl_fd, -1, QUOTA_GET_FORMAT, 0, attribute_buffer);
    }
    if (strcmp(call, "swapon") == 0) {
        return syscall(SYS_swapon, MISSING, 0);
    }
    if (strcmp(call, "swapoff") == 0) {
        return syscall(SYS_swapoff, MISSING);
    }
    *known = 0;
    return -1;
}

/*
 * A step of the coverage-incomplete task: make the one call `call`, which marks the
 * run as incompletely traced, and exit with 0 whatever it returns. For
 * open_tree_attr and fchroot, which a kernel may lack, also print whether the call
 * is available: it is unavailable when it failed with ENOSYS, because the kernel
 * lacks it or a seccomp filter installed before kizz's own refuses it.
 */
static int incomplete(const char *call)
{
    int known = 0;
    errno = 0;
    long result = incomplete_call(call, &known);
    int error = errno;
    if (!known) {
        fprintf(stderr, "coverage: %s is not a call of the coverage-incomplete task\n", call);
        return 2;
    }
    if (strcmp(call, "open_tree_attr") == 0 || strcmp(call, "fchroot") == 0) {
        printf("coverage: %s: %s\n",
               result < 0 && error == ENOSYS ? "unavailable" : "available", call);
    }
    printf("coverage: made %s: %s\n", call, result < 0 ? error_name(error) : "success");
    return 0;
}

/*
 * A step of the coverage-incomplete task: make the system call `number`, 400 or
 * 1000000, which the tracer does not know, and exit with 0 whatever it returns. The
 * numbers belong to the x86_64 table, so the mode exists only there.
 */
static int unknown(const char *number)
{
#ifdef __x86_64__
    long call = 0;
    if (strcmp(number, "400") == 0) {
        call = 400;
    } else if (strcmp(number, "1000000") == 0) {
        call = 1000000;
    } else {
        fprintf(stderr, "coverage: %s is not a number of the coverage-incomplete task\n", number);
        return 2;
    }
    errno = 0;
    long result = syscall(call);
    int error = errno;
    printf("coverage: made system call %ld: %s\n", call, result < 0 ? error_name(error) : "success");
    return 0;
#else
    fprintf(stderr, "coverage: system call %s belongs to the x86_64 table\n", number);
    return 2;
#endif
}

#else

/* The coverage-incomplete task makes Linux calls only. */
static int incomplete(const char *call)
{
    fprintf(stderr, "coverage: %s is a Linux call\n", call);
    return 2;
}

/* The coverage-incomplete task makes Linux calls only. */
static int unknown(const char *number)
{
    fprintf(stderr, "coverage: system call %s belongs to the x86_64 table of Linux\n", number);
    return 2;
}

#endif

int main(int argc, char **argv)
{
    if (argc >= 2 && strcmp(argv[1], INTERPRETED) == 0) {
        return 0;
    }
    if (argc == 3) {
        snprintf(task_dir, sizeof task_dir, "%s", argv[2]);
        child(argv[1]);
        return mismatches == 0 ? 0 : 1;
    }
    if (argc == 2 && strncmp(argv[1], INCOMPLETE_PREFIX, strlen(INCOMPLETE_PREFIX)) == 0) {
        return incomplete(argv[1] + strlen(INCOMPLETE_PREFIX));
    }
    if (argc == 2 && strncmp(argv[1], UNKNOWN_PREFIX, strlen(UNKNOWN_PREFIX)) == 0) {
        return unknown(argv[1] + strlen(UNKNOWN_PREFIX));
    }
    if (argc == 2 && strncmp(argv[1], CHANGE_PREFIX, strlen(CHANGE_PREFIX)) == 0) {
        change(argv[1] + strlen(CHANGE_PREFIX));
        if (mismatches != 0) {
            printf("coverage: %ld of %ld checks failed\n", mismatches, checks);
            return 1;
        }
        printf(SUCCESS_LINE ", %ld checks\n", checks);
        return 0;
    }
    if (argc != 1) {
        fprintf(stderr, "usage: coverage [change-MODE | incomplete-CALL | unknown-NUMBER | MODE "
                        "TASK-DIRECTORY]\n");
        return 2;
    }
    if (getcwd(task_dir, sizeof task_dir) == NULL) {
        fprintf(stderr, "coverage: cannot read the working directory: %s\n", strerror(errno));
        return 1;
    }
    core();
#ifdef __linux__
    linux_section();
#endif
    if (mismatches != 0) {
        printf("coverage: %ld of %ld checks failed\n", mismatches, checks);
        return 1;
    }
    printf(SUCCESS_LINE ", %ld checks\n", checks);
    return 0;
}

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
 * mode on paths of its own.
 *
 * Linux names some paths through out/linux/made-link, a symbolic link to
 * out/linux/made: a file created through the link is recorded under the link's
 * name, and a later call on out/linux/made, or on a descriptor that names it, is
 * then the only call recorded on that path, so a tracer that missed it would record
 * the path differently.
 */

#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifdef __linux__
#include <stdint.h>
#include <sys/syscall.h>
#endif

/* The line that the program prints when every call returned what it expected. */
#define SUCCESS_LINE "coverage: every call returned what it expected"

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

/* The Linux section: every tracked system call, made through syscall(). */
static void linux_section(void)
{
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

#ifdef SYS_io_uring_setup
    /* io_uring would bypass the tracer, so it is not available. */
    unsigned char parameters[120];
    memset(parameters, 0, sizeof parameters);
    EXPECT(ENOSYS, "io_uring_setup", "(none)", syscall(SYS_io_uring_setup, 1, parameters));
#endif

    close_fd(in_fd);
    close_fd(made_fd);
}

#endif

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

int main(int argc, char **argv)
{
    if (argc == 3) {
        snprintf(task_dir, sizeof task_dir, "%s", argv[2]);
        child(argv[1]);
        return mismatches == 0 ? 0 : 1;
    }
    if (argc != 1) {
        fprintf(stderr, "usage: coverage, or coverage MODE TASK-DIRECTORY\n");
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

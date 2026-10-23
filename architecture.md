# Kizz: Architectural Specification
Version: 1.0 (2026 Spec)
License: AGPL-3.0

## 1. System Vision
Kizz is a minimalist, implicitly hermetic build engine, container image builder, and task runner scoped strictly to the Linux ecosystem. It eliminates declarative build configurations (e.g., Makefiles, YAML files, JSON arrays) by treating the underlying operating system kernel as the source of truth for build dependencies. 

By trapping system calls synchronously, Kizz builds a language-agnostic Directed Acyclic Graph (DAG) based purely on the raw file read, write, and missing-file (ENOENT) footprints of spawned processes.

---

## 2. Core Constraints & Guarantees
1. Linux Native Boundary: Kizz targets native Linux primitives (seccomp, ptrace, overlayfs, namespaces). Running on macOS or Windows explicitly requires a lightweight container (Docker) or micro-VM (WSL2/Lima) environment.
2. Zero Configuration Philosophy: Build steps are discovered recursively by parsing standard Markdown documentation (README.md / *.md). Build logic is written in pure Starlark code blocks inside these files.
3. Implicit Dependency Discovery: Dependencies are never explicitly declared by the user. If a process opens a file for reading, it is an input. If it writes a file, it is an output.
4. Virtual Monorepo Portability: The engine operates relative to a runtime project root. All filesystem interactions inside this boundary are tracked via relative paths, ensuring execution metadata maps identically inside local systems, isolated containers, or virtual monorepo assemblies.

---

## 3. Caching & Content-Addressable Cryptography

### 3.1 The Deterministic Cache Key Formula
Kizz models every individual command execution (a "step") as a pure mathematical function. A step cache key is generated via a cryptographically secure BLAKE3 hash of its structural constraints:

Cache Key = BLAKE3( Cmd String + Starlark Block Content + Hashes of Input Files + Hashes of Toolchain Binaries + Env Vars )

### 3.2 The Metadata Fast-Path & Hashing Pipeline
To prevent filesystem I/O bottlenecks during hash evaluation, Kizz employs a two-tier concurrent validation loop:
1. The Metadata Comparison: On initialization, background worker threads crawl the project space executing low-overhead stat() scans. If a file's size, modification time (mtime), permissions, or node properties perfectly match the historical database, the file is short-circuited and verified as unchanged.
2. The Hashing Queue: If stat() reveals a modification delta, or if mtime checking is explicitly disabled in the run state, the file path is immediately pushed to a high-concurrency multi-threaded worker pool that streams chunks to a native BLAKE3 hasher engine.

---

## 4. The Runtime Execution Matrix

### 4.1 The Starlark Sandbox Isolation
Build logic is executed via a sandboxed Starlark interpreter running on the host system. The Starlark context has all disk and network I/O disabled, completely preventing side-effect contamination outside the engine's supervision. 

Interaction with the system is strictly limited to an injected, proxying sh.* API layer. When Starlark calls a function (e.g., sh.gcc("-c", "main.c")), the execution string is parsed sequentially as an isolated mini-command step and routed to the native tracking core.

### 4.2 The Seccomp Kernel Tracer Loop
When the engine executes an un-cached shell command, it forks a child process and applies a strict seccomp system call filter before execution.

- Host / Starlark Runtime Lane spawns child process.
- Kernel Seccomp Gateway monitors calls to openat(), stat(), clone().
- If the call is a SUCCESS (0), it logs as an active Dependency or output file.
- If the call is a FAILURE (-ENOENT), it intercepts, suspends the child, and passes the path to the scheduler.

### 4.3 Iterative Graph Resolution & Deferral Scheduler
Kizz runs a zero-config, highly parallel speculative execution pool:
1. All ready or sequential pipeline commands are fired simultaneously across available processor threads.
2. If a child process returns an -ENOENT (File Not Found) result for an internal relative path, the engine captures the missing target, pauses or aborts the child, and moves it to a Deferred Queue, logging exactly what file asset it is waiting for.
3. When a sister command successfully writes that missing asset, the scheduler instantly unblocks the deferred task and passes it back to the active queue.
4. Deadlock Detection: If the thread execution pool empties while items remain in the Deferred Queue and no files were generated during the last cycle, the system halts immediately and outputs a hyper-precise diagnostic map detailing the mutually blocking files.

---

## 5. Advanced Infrastructure Extensions

### 5.1 Speculative Layer Container Building
When building container images, Kizz evaluates Starlark logic entirely on the host side, mounting the underlying environment inside isolated Linux namespaces and ephemeral overlayfs layers. 

Commands run speculatively inside these isolated virtual filesystems under full seccomp audit loop conditions. If a command runs and the kernel records zero net-new mutations to disk, the engine discards the redundant execution layer completely and pulls downstream cache pointers forward, entirely bypassing traditional Dockerfile positional layer invalidation traps.

### 5.2 The Vendored Build Graph for CI
Kizz can output a compiled execution state roadmap as a static .build/graph.json artifact.
- This file acts as a pre-computed blueprint mapping explicit input/output relationships for every task.
- In CI/CD pipelines, Kizz reads this file to run multi-threaded tasks at native speeds without launching seccomp or ptrace layers, completely bypassing unprivileged container execution constraints in virtual runner environments.

### 5.3 Unified Side-Effect (PUSH) Insulation
Commands can be decorated with an always_run: true or forced=True flag to accommodate deployment steps, testing routines, or remote asset pushes. The engine enforces strict topological sequence rules on these nodes. Because the seccomp loop verifies these steps mutate no code files inside the workspace root, their execution leaves downstream caching entirely untouched.

---

## 6. Flag-Driven Union Orchestration
Kizz manages multi-repo codespaces and virtual mono-repos using target command execution flags passed dynamically via the CLI:

kizz build ci         # Pulls tasks flagged exclusively for CI
kizz build check      # Pulls local validation / pre-build tasks
kizz build check+ci   # Dynamically builds a single optimized Union Graph

When flags are joined via a union string (+), the engine executes a single unified scanning pass across the file system. It flattens all matching code blocks into a single execution plane. If a compilation task inside check mirrors a task inside ci, they share a single unified cache entry, forcing parallel processes to seamlessly cross-pollinate results without running overlapping disk mutations.

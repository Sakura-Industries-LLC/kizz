+++
title = "The kizz execute report"
weight = 3
+++

This page describes the report that `kizz execute` and `kizz run --output` write, in each of its formats, with every JSON field.

## Overview

The report is text by default or JSON with `--json`, and hierarchical by default or compact with `--compact`.
All four forms come from the same observations.

* `kizz execute` writes a report of the one command that it runs, as [the command line](@/reference/command-line.md) says.
* `kizz run --output` writes a report of the commands of its task, in the order they ran, and of the task program's own accesses.
  Each command that ran is the root of its own process tree, and a command that came from the cache is listed as cached,
  without processes or observations.

A report aggregates what the tracer saw: each path appears once in its scope, with every access kind and outcome it had.
Every observation is reported, `/proc` and `/sys` included; the `ignore` patterns and other rules of builds do not apply.
The report is written to a new hidden file in the directory of the output file, which is then renamed over the output file.
[Observing a command](@/guide/execute.md) explains how to read a report.

Both formats follow these rules:

* Commands are in the order they ran, and each command's processes are in the order they started.
* Processes are numbered from 1 across the whole report, so a number names one process of one command.
* A process's programs are in the order the process ran them.
* Paths are sorted, and each path's access kinds are in the order `read`, `probe`, `list`, `write`, `exec`, `xattr`.
* Connections are sorted by kind, then by address, path or name.
* A command's reasons for an incomplete trace are sorted by process, then by kind, number and detail, each once.

## Access kinds

Each observation gives the kinds of access that a path had:

* `read`: the path's content was read.
* `probe`: the path's existence or metadata was checked, including a lookup that found nothing.
* `list`: a directory's entries were read.
* `write`: the path was created, modified, renamed or removed, or its metadata changed.
* `exec`: the path was run as a program, or the kernel loaded it to run one, as a `#!` interpreter or an ELF loader.
* `xattr`: the path's extended attributes were read or listed.

## Outcomes

Each access kind of a path has one or more outcomes, the ways in which the calls of that kind ended:

* `ok`: a call succeeded.
* An error name, such as `ENOENT` or `EACCES`: a call failed with that error.
* `errno` and a number, such as `errno 200`: a call failed with an error number that has no name.
* `unknown`: the tracer never saw the call's result, because the call's thread was killed during the call.

`ok` comes first, then the error names in the byte order of their names, then `unknown`.

A command's own outcome says how it ended, or why it did not run:

* `exited`, with the exit code;
* `signaled`, with the number of the signal that killed it;
* `not_found`, when the program was not found, with the error;
* `not_executable`, when it was found but cannot be run, with the error;
* `start_failed`, when it was found but could not be started for another reason, with the error;
* `failed`, when kizz or its tracer failed, with the error; the trace is then incomplete, with a `tracer_failed` reason.

When kizz started no process for a command, its one process has no process ID and no exit,
and its program is the name that kizz looked up, with every location that the lookup checked.

## Completeness

Each command that ran says whether it was traced completely, and lists every reason that it was not.
A reason has one of these kinds, which JSON names in the first form and text in the second:

* `non_native_system_call`, `non-native system call`: a process made a system call through a non-native interface,
  the 32-bit `int 0x80` or x32.
* `untraceable_system_call`, `untraceable system call`: a process made a call that can change what a path means,
  or use a path in a way that the tracer does not record, such as `unshare` with `CLONE_NEWNS`.
  It counts when the call is made, whether or not it succeeds.
* `unknown_system_call`, `unknown system call`: a process made a system call that the tracer does not know, by its number.
  It makes the report incomplete even when a build may cache such a result with `--cache-unknown-syscalls`.
* `tracer_failed`, `tracer failed`: the tracer itself failed, which also makes the command's outcome `failed`.
* `other`, `other`: any other reason that the tracer gives.

A command that came from the cache was not traced, so its completeness is `null` in JSON and `cached, not traced` in text.
The report as a whole is complete when every command that ran was traced completely; cached commands do not count.
A report never claims a complete trace when the tracer reported otherwise.

## Text

Text is for reading.
It starts with a header, then a block for each command, then the task program, if any, with a blank line between blocks.
Lines are indented by two spaces for each level.

The header has four lines: the format and its version with the mode, whether the trace is complete, the totals,
and the number of paths of each access kind:

```text
kizz observation report, version 1, hierarchical
trace: complete
totals: commands 1, cached 0, processes 1, programs 1, paths 6, connections 0
paths by access: read 3, probe 2, list 0, write 0, exec 2, xattr 0
```

Each command block starts with `command`, the command's number from 1, and its arguments, then gives:

* `program:` and the absolute path of the program, or `not found` when the lookup found none;
* `directory:` and the working directory;
* `outcome:` and how it ended: `exited with` and the code, `signaled with` and the signal, or `not found:`,
  `not executable:`, `start failed:` or `failed:` and the error;
* `trace:` and `complete`, `incomplete` or `cached, not traced`, with each reason on a line of its own below it:
  its kind, in a hierarchical report its process, and its detail.

Arguments are separated by spaces and quoted as a POSIX shell would read them: an argument that is empty,
or holds a character other than a letter, a digit or one of `-_./=:,+@%^`, is put in single quotes,
and each single quote in it is written `'\''`.

### Hierarchical text

A hierarchical report, the default, lists each command's processes after its block's lines, each under the process that created it,
in the order they started:

* `process` and its number, then `pid` and its process ID, or `no pid` when none is known,
  `child of process` and its parent's number when it has a parent, and how it ended: `exited with` and the code,
  `signaled with` and the signal, or `end not seen`.
  A process with neither a process ID nor an exit, such as the placeholder of a command that kizz never started, reads `process`,
  its number and `not started`.
* Under it, each program: `program`, its path, `inherited` when the process runs it because its creator ran it,
  and its arguments.
* Under each program, its paths grouped by access kind: a line with the kind, then each path with its outcomes,
  and last a `connections:` group with each connection: `ip` and the address, `unix` and the path, or `abstract` and the name.
  A group with nothing in it is left out.

The task program of `kizz run` follows the commands: `task program:` and the task, with its paths grouped by access kind.

```sh
PATH=/usr/bin kizz execute --output=report.txt -- wc -w notes.txt
```

```text
kizz observation report, version 1, hierarchical
trace: complete
totals: commands 1, cached 0, processes 1, programs 1, paths 6, connections 0
paths by access: read 3, probe 2, list 0, write 0, exec 2, xattr 0

command 1: wc -w notes.txt
  program: /usr/bin/wc
  directory: /tmp/ref/demo
  outcome: exited with 0
  trace: complete
  process 1, pid 636204, exited with 0
    program /usr/bin/wc: wc -w notes.txt
      read:
        /etc/ld.so.cache: ok
        /lib/x86_64-linux-gnu/libc.so.6: ok
        /tmp/ref/demo/notes.txt: ok
      probe:
        /etc/ld.so.preload: ENOENT
        /usr/bin/wc: ok
      exec:
        /lib64/ld-linux-x86-64.so.2: ok
        /usr/bin/wc: ok
```

A command whose trace is incomplete lists each reason under its `trace:` line:

```sh
PATH=/usr/bin kizz execute --output=report.txt -- unshare -m true
```

```text
kizz observation report, version 1, hierarchical
trace: incomplete
totals: commands 1, cached 0, processes 1, programs 1, paths 5, connections 0
paths by access: read 2, probe 2, list 0, write 0, exec 2, xattr 0

command 1: unshare -m true
  program: /usr/bin/unshare
  directory: /tmp/ref/demo
  outcome: exited with 1
  trace: incomplete
    untraceable system call, process 1: process 636228 made `unshare` with `CLONE_NEWNS`
  process 1, pid 636228, exited with 1
    program /usr/bin/unshare: unshare -m true
      read:
        /etc/ld.so.cache: ok
        /lib/x86_64-linux-gnu/libc.so.6: ok
      probe:
        /etc/ld.so.preload: ENOENT
        /usr/bin/unshare: ok
      exec:
        /lib64/ld-linux-x86-64.so.2: ok
        /usr/bin/unshare: ok
```

The [`kizz run` examples](@/reference/command-line.md) show a task program's block.

### Compact text

A compact report lists no process: after the command blocks and the task program's line,
a last block holds every path of the report once, grouped by access kind, and then every connection.
The paths of the task program are among them.

```sh
PATH=/usr/bin kizz execute --compact --output=report.txt -- wc -w notes.txt
```

```text
kizz observation report, version 1, compact
trace: complete
totals: commands 1, cached 0, processes 1, programs 1, paths 6, connections 0
paths by access: read 3, probe 2, list 0, write 0, exec 2, xattr 0

command 1: wc -w notes.txt
  program: /usr/bin/wc
  directory: /tmp/ref/demo
  outcome: exited with 0
  trace: complete

read:
  /etc/ld.so.cache: ok
  /lib/x86_64-linux-gnu/libc.so.6: ok
  /tmp/ref/demo/notes.txt: ok
probe:
  /etc/ld.so.preload: ENOENT
  /usr/bin/wc: ok
exec:
  /lib64/ld-linux-x86-64.so.2: ok
  /usr/bin/wc: ok
```

A compact report combines the observations that a hierarchical report lists by program, so its paths are the same.

### Escapes in text

Text escapes each backslash and control character of every string that it shows, such as paths, arguments, error messages,
reason details, the task and connections, so that no string can break its line:

* a backslash is written `\\`, a newline `\n`, a tab `\t`, and a carriage return `\r`;
* every other control character, U+0000 through U+001F and U+007F through U+009F,
  and the separators U+2028 and U+2029 are written `\u{`, the code in lowercase hexadecimal without leading zeros, and `}`,
  such as `\u{1b}` for the escape character and `\u{0}` for U+0000;
* every other character is written as it is.

Because the backslash itself is escaped, each escape reads one way, so the escapes never make two different strings read alike.
An argument is escaped first and then quoted, so an argument with an escape is always quoted.
JSON keeps its own standard escaping, and the escapes of text do not apply to it.

This command passes `sh` an argument that holds a tab and a backslash, and has it look for a name that holds the byte `0xFF`:

```sh
PATH=/usr/bin kizz execute --compact --output=report.txt -- \
    sh -c 'test -e "$1" || test -e "$(printf "d\377e")"' sh "$(printf 'a\tb\\c')"
```

```text
kizz observation report, version 1, compact
trace: complete
totals: commands 1, cached 0, processes 2, programs 1, paths 8, connections 0
paths by access: read 2, probe 5, list 0, write 0, exec 2, xattr 0

command 1: sh -c 'test -e "$1" || test -e "$(printf "d\\377e")"' sh 'a\tb\\c'
  program: /usr/bin/sh
  directory: /tmp/ref/demo
  outcome: exited with 1
  trace: complete

read:
  /etc/ld.so.cache: ok
  /lib/x86_64-linux-gnu/libc.so.6: ok
probe:
  /etc/ld.so.preload: ENOENT
  /tmp/ref/demo: ok
  /tmp/ref/demo/a\tb\\c: ENOENT
  /tmp/ref/demo/d�e: EILSEQ
  /usr/bin/sh: ok
exec:
  /lib64/ld-linux-x86-64.so.2: ok
  /usr/bin/sh: ok
```

The path with the tab reads `a\tb\\c`, and the byte `0xFF`, which is not valid UTF-8, reads as U+FFFD, as the next section says.
The file system of that machine refused the name with `EILSEQ`.

## Strings that are not valid UTF-8

Paths, arguments and the names of abstract Unix sockets
that are not valid UTF-8 appear with each invalid sequence replaced by U+FFFD, the replacement character `�`, in both formats.
So two names that differ only in their invalid bytes, or one with invalid bytes and one that holds U+FFFD itself,
read the same in a report.
In the example of the escapes, the JSON report of the same command holds the paths `"/tmp/ref/demo/a\tb\\c"`
and `"/tmp/ref/demo/d�e"`.

## JSON

JSON is for programs.
It is one object, indented by two spaces, with each member and element on a line of its own, and it ends with a newline.
Members appear in the order of the field list below, and an empty array is written `[]`.
Strings are escaped as JSON requires: `"` and `\` with a backslash, a newline, a carriage return, a tab,
a backspace and a form feed as `\n`, `\r`, `\t`, `\b` and `\f`,
every other character below U+0020 as `\u00` and two lowercase hexadecimal digits, and every other character as it is.

### Versions

`format` is `kizz-execute`, and `version` is 1.
A new field may appear without a new version, but a field that changes its meaning or disappears raises the version.
So a program that reads a report should check `format` and `version`, and ignore fields that it does not know.

### Fields that some objects lack

Every field below is always present in the mode that holds it, `null` when it has no value, with three exceptions:

* an outcome has `code`, `signal` or `error`, as its `kind` says;
* a process's `exit` has `code` or `signal`, as its `kind` says;
* a connection has `address`, `path` or `name`, as its `kind` says.

An observation's `access` object holds only the access kinds that its path had.
Some fields are in hierarchical reports only, and some in compact reports only, as each entry below says.

### Hierarchical JSON

```sh
PATH=/usr/bin kizz execute --json --output=report.json -- wc -w notes.txt
```

```json
{
  "format": "kizz-execute",
  "version": 1,
  "mode": "hierarchical",
  "complete": true,
  "totals": {
    "commands": 1,
    "cached": 0,
    "processes": 1,
    "programs": 1,
    "paths": 6,
    "connections": 0,
    "kinds": {
      "read": 3,
      "probe": 2,
      "list": 0,
      "write": 0,
      "exec": 2,
      "xattr": 0
    }
  },
  "commands": [
    {
      "argv": [
        "wc",
        "-w",
        "notes.txt"
      ],
      "program": "/usr/bin/wc",
      "cwd": "/tmp/ref/demo",
      "cached": false,
      "outcome": {
        "kind": "exited",
        "code": 0
      },
      "complete": true,
      "incomplete": [],
      "processes": [
        {
          "id": 1,
          "pid": 636214,
          "parent": null,
          "exit": {
            "kind": "exited",
            "code": 0
          },
          "programs": [
            {
              "path": "/usr/bin/wc",
              "args": [
                "wc",
                "-w",
                "notes.txt"
              ],
              "inherited": false,
              "observations": [
                {
                  "path": "/etc/ld.so.cache",
                  "access": {
                    "read": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/etc/ld.so.preload",
                  "access": {
                    "probe": [
                      "ENOENT"
                    ]
                  }
                },
                {
                  "path": "/lib/x86_64-linux-gnu/libc.so.6",
                  "access": {
                    "read": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/lib64/ld-linux-x86-64.so.2",
                  "access": {
                    "exec": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/tmp/ref/demo/notes.txt",
                  "access": {
                    "read": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/usr/bin/wc",
                  "access": {
                    "probe": [
                      "ok"
                    ],
                    "exec": [
                      "ok"
                    ]
                  }
                }
              ],
              "connections": []
            }
          ]
        }
      ]
    }
  ],
  "task_program": null
}
```

A program that is not found has the placeholder process, without a process ID or exit:

```sh
PATH=/usr/bin kizz execute --json --output=report.json -- no-such-tool
```

```json
{
  "format": "kizz-execute",
  "version": 1,
  "mode": "hierarchical",
  "complete": true,
  "totals": {
    "commands": 1,
    "cached": 0,
    "processes": 1,
    "programs": 1,
    "paths": 1,
    "connections": 0,
    "kinds": {
      "read": 0,
      "probe": 1,
      "list": 0,
      "write": 0,
      "exec": 0,
      "xattr": 0
    }
  },
  "commands": [
    {
      "argv": [
        "no-such-tool"
      ],
      "program": null,
      "cwd": "/tmp/ref/demo",
      "cached": false,
      "outcome": {
        "kind": "not_found",
        "error": "program `no-such-tool` was not found in PATH"
      },
      "complete": true,
      "incomplete": [],
      "processes": [
        {
          "id": 1,
          "pid": null,
          "parent": null,
          "exit": null,
          "programs": [
            {
              "path": "no-such-tool",
              "args": [
                "no-such-tool"
              ],
              "inherited": false,
              "observations": [
                {
                  "path": "/usr/bin/no-such-tool",
                  "access": {
                    "probe": [
                      "ENOENT"
                    ]
                  }
                }
              ],
              "connections": []
            }
          ]
        }
      ]
    }
  ],
  "task_program": null
}
```

The report of `kizz run --json --output=run.json count`, after the command of the task named `count` was cached,
lists the command as cached and the file that the task program wrote:

```json
{
  "format": "kizz-execute",
  "version": 1,
  "mode": "hierarchical",
  "complete": true,
  "totals": {
    "commands": 1,
    "cached": 1,
    "processes": 0,
    "programs": 0,
    "paths": 1,
    "connections": 0,
    "kinds": {
      "read": 0,
      "probe": 0,
      "list": 0,
      "write": 1,
      "exec": 0,
      "xattr": 0
    }
  },
  "commands": [
    {
      "argv": [
        "wc",
        "-w",
        "notes.txt"
      ],
      "program": "/usr/bin/wc",
      "cwd": "/tmp/ref/demo",
      "cached": true,
      "outcome": {
        "kind": "exited",
        "code": 0
      },
      "complete": null,
      "incomplete": [],
      "processes": []
    }
  ],
  "task_program": {
    "task": "kizz.md:5 (Count the words)",
    "observations": [
      {
        "path": "/tmp/ref/demo/out/count.txt",
        "access": {
          "write": [
            "ok"
          ]
        }
      }
    ]
  }
}
```

### Compact JSON

A compact report has no `processes`, and no `process` in its reasons.
It has the top-level `observations` and `connections` instead, and its `task_program` has only `task`.

```sh
PATH=/usr/bin kizz execute --compact --json --output=report.json -- unshare -m true
```

```json
{
  "format": "kizz-execute",
  "version": 1,
  "mode": "compact",
  "complete": false,
  "totals": {
    "commands": 1,
    "cached": 0,
    "processes": 1,
    "programs": 1,
    "paths": 5,
    "connections": 0,
    "kinds": {
      "read": 2,
      "probe": 2,
      "list": 0,
      "write": 0,
      "exec": 2,
      "xattr": 0
    }
  },
  "commands": [
    {
      "argv": [
        "unshare",
        "-m",
        "true"
      ],
      "program": "/usr/bin/unshare",
      "cwd": "/tmp/ref/demo",
      "cached": false,
      "outcome": {
        "kind": "exited",
        "code": 1
      },
      "complete": false,
      "incomplete": [
        {
          "kind": "untraceable_system_call",
          "detail": "process 636233 made `unshare` with `CLONE_NEWNS`",
          "number": null
        }
      ]
    }
  ],
  "task_program": null,
  "observations": [
    {
      "path": "/etc/ld.so.cache",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/etc/ld.so.preload",
      "access": {
        "probe": [
          "ENOENT"
        ]
      }
    },
    {
      "path": "/lib/x86_64-linux-gnu/libc.so.6",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/lib64/ld-linux-x86-64.so.2",
      "access": {
        "exec": [
          "ok"
        ]
      }
    },
    {
      "path": "/usr/bin/unshare",
      "access": {
        "probe": [
          "ok"
        ],
        "exec": [
          "ok"
        ]
      }
    }
  ],
  "connections": []
}
```

## JSON fields

Each field is named by its path from the top-level object, with `.` between members and `[]` after an array for its elements.
"Both modes" means that hierarchical and compact reports hold the field.

### `format`

A string, `kizz-execute`, the name of the format; both modes.

### `version`

A number, `1`, the version of the format; both modes.
[Versions](#versions) says when it changes.

### `mode`

A string, `hierarchical` or `compact`, which `--compact` chooses; both modes.

### `complete`

A boolean: `true` when every command that ran was traced completely, and `false` otherwise; both modes.
Cached commands do not count, so a report of cached commands alone is complete.

### `totals`

An object that counts what the report holds; both modes.

### `totals.commands`

A number: the commands of the report, cached ones included; both modes.

### `totals.cached`

A number: the commands that came from the cache; both modes.

### `totals.processes`

A number: the processes of every command, placeholder processes included; both modes.
A cached command has no process.

### `totals.programs`

A number: the programs of every process that are not inherited; both modes.

### `totals.paths`

A number: the distinct paths of the whole report, the task program's included; both modes.

### `totals.connections`

A number: the distinct connections of the whole report; both modes.

### `totals.kinds`

An object with a member for each access kind, in the order `read`, `probe`, `list`, `write`, `exec`, `xattr`; both modes.
Each member counts the distinct paths of the report that had that kind, so a path can count under several kinds.

### `totals.kinds.read`

A number: the paths whose content was read; both modes.

### `totals.kinds.probe`

A number: the paths whose existence or metadata was checked; both modes.

### `totals.kinds.list`

A number: the directories whose entries were read; both modes.

### `totals.kinds.write`

A number: the paths that were written; both modes.

### `totals.kinds.exec`

A number: the paths that were run as programs or loaded to run one; both modes.

### `totals.kinds.xattr`

A number: the paths whose extended attributes were read or listed; both modes.

### `commands`

An array of the commands, in the order they ran; both modes.
`kizz execute` has one, and `kizz run` one for each step that the run reached.

### `commands[].argv`

An array of strings: the command's arguments, starting with the program as given, such as `wc`; both modes.

### `commands[].program`

A string, the absolute path of the program, or `null` when the lookup found none; both modes.

### `commands[].cwd`

A string: the command's working directory, an absolute path; both modes.

### `commands[].cached`

A boolean: `true` when the result came from the cache, so nothing was traced; both modes.

### `commands[].outcome`

An object that says how the command ended, or why it did not run; both modes.

### `commands[].outcome.kind`

A string: `exited`, `signaled`, `not_found`, `not_executable`, `start_failed` or `failed`; both modes.
[Outcomes](#outcomes) explains each.

### `commands[].outcome.code`

A number: the exit code, only when `kind` is `exited`; both modes.

### `commands[].outcome.signal`

A number: the number of the signal that killed the command, only when `kind` is `signaled`; both modes.

### `commands[].outcome.error`

A string: the error message, only when `kind` is `not_found`, `not_executable`, `start_failed` or `failed`; both modes.

### `commands[].complete`

A boolean, `true` when the command was traced completely, or `null` when it came from the cache; both modes.

### `commands[].incomplete`

An array of the reasons that the command was traced incompletely, sorted, each once; both modes.
It is empty for a command traced completely and for a cached command.

### `commands[].incomplete[].kind`

A string: `non_native_system_call`, `untraceable_system_call`, `unknown_system_call`, `tracer_failed` or `other`; both modes.
[Completeness](#completeness) explains each.

### `commands[].incomplete[].detail`

A string that describes the reason, such as the process and the call; both modes.

### `commands[].incomplete[].number`

A number, the number of the unknown system call for `unknown_system_call`, and `null` for every other kind; both modes.

### `commands[].incomplete[].process`

A number, the number of the process whose program caused the reason, as `commands[].processes[].id` gives it;
hierarchical reports only.
It is `null` when no process is known.

### `commands[].processes`

An array of the command's processes, in the order they started; hierarchical reports only.
It is empty for a cached command; a command that started no process has one placeholder process.

### `commands[].processes[].id`

A number: the process's number, unique in the report, counting from 1 across every command; hierarchical reports only.
The first process of each command is the command itself.

### `commands[].processes[].pid`

A number, the process ID that the system gave the process, or `null` when kizz started no process or none is known;
hierarchical reports only.

### `commands[].processes[].parent`

A number, the `id` of the process that created this one, or `null` when it has none in the command's tree,
as the command's first process has none; hierarchical reports only.

### `commands[].processes[].exit`

An object that says how the process ended, or `null` when the tracer did not see it end; hierarchical reports only.

### `commands[].processes[].exit.kind`

A string, `exited` or `signaled`; hierarchical reports only.

### `commands[].processes[].exit.code`

A number: the process's exit code, only when `kind` is `exited`; hierarchical reports only.

### `commands[].processes[].exit.signal`

A number: the signal that killed the process, only when `kind` is `signaled`; hierarchical reports only.

### `commands[].processes[].programs`

An array of the programs that the process ran, in order; hierarchical reports only.
A process that a fork, a vfork or a clone created first runs its creator's program, which is listed, as inherited,
only when something is attributed to it; each successful `exec` then adds the program that it started.

### `commands[].processes[].programs[].path`

A string: the program's path, as the `exec` named it, resolved; hierarchical reports only.
For a placeholder process, it is the name that kizz looked up.

### `commands[].processes[].programs[].args`

An array of strings: the arguments that the program received, starting with its `argv[0]`; hierarchical reports only.

### `commands[].processes[].programs[].inherited`

A boolean: `true` when the process runs the program because its creator ran it; hierarchical reports only.

### `commands[].processes[].programs[].observations`

An array of the paths that the program touched, sorted by path, each once; hierarchical reports only.
A successful `exec` belongs to the program that it starts, and a failed one to the program that made the call.
The locations that kizz's own lookup of the command's program checked belong to the first program of the first process.

### `commands[].processes[].programs[].observations[].path`

A string: the absolute path; hierarchical reports only.

### `commands[].processes[].programs[].observations[].access`

An object that maps each access kind that the path had to its outcomes; hierarchical reports only.

### `commands[].processes[].programs[].observations[].access.read`

An array of outcome strings, such as `["ok"]`, when the path's content was read; hierarchical reports only.

### `commands[].processes[].programs[].observations[].access.probe`

An array of outcome strings, when the path's existence or metadata was checked; hierarchical reports only.

### `commands[].processes[].programs[].observations[].access.list`

An array of outcome strings, when the directory's entries were read; hierarchical reports only.

### `commands[].processes[].programs[].observations[].access.write`

An array of outcome strings, when the path was written; hierarchical reports only.

### `commands[].processes[].programs[].observations[].access.exec`

An array of outcome strings, when the path was run or loaded to run a program; hierarchical reports only.

### `commands[].processes[].programs[].observations[].access.xattr`

An array of outcome strings, when the path's extended attributes were read or listed; hierarchical reports only.

### `commands[].processes[].programs[].connections`

An array of the connections that the program made to the network or to Unix sockets, sorted, each once; hierarchical reports only.
Kizz cannot see what came back over a connection.

### `commands[].processes[].programs[].connections[].kind`

A string: `ip` for an IPv4 or IPv6 address and port, `unix` for a Unix socket by its path,
or `abstract` for a Unix socket in the abstract namespace; hierarchical reports only.

### `commands[].processes[].programs[].connections[].address`

A string, the address and port, such as `127.0.0.1:80` or `[::1]:443`, only when `kind` is `ip`; hierarchical reports only.

### `commands[].processes[].programs[].connections[].path`

A string, the absolute path of the socket, only when `kind` is `unix`; hierarchical reports only.

### `commands[].processes[].programs[].connections[].name`

A string, the socket's name without the zero byte that starts it, only when `kind` is `abstract`; hierarchical reports only.

### `task_program`

An object with the task program's own accesses for `kizz run`, and `null` for `kizz execute`; both modes.
The task program's accesses are those of the program itself, not of its commands, such as the files it writes with `fs.write`.

### `task_program.task`

A string: the task, as kizz's output names it, such as `kizz.md:5 (Count the words)`; both modes.

### `task_program.observations`

An array of the paths that the task program touched, sorted by path, each once; hierarchical reports only.
In a compact report, these paths are among the top-level `observations`.

### `task_program.observations[].path`

A string: the absolute path; hierarchical reports only.

### `task_program.observations[].access`

An object that maps each access kind that the path had to its outcomes; hierarchical reports only.

### `task_program.observations[].access.read`

An array of outcome strings, when the task program read the path's content; hierarchical reports only.

### `task_program.observations[].access.probe`

An array of outcome strings, when the task program checked the path's existence or metadata; hierarchical reports only.

### `task_program.observations[].access.list`

An array of outcome strings, when the task program read the directory's entries; hierarchical reports only.

### `task_program.observations[].access.write`

An array of outcome strings, when the task program wrote the path; hierarchical reports only.

### `task_program.observations[].access.exec`

An array of outcome strings for running the path; hierarchical reports only.
A task program's own accesses only read, check, list and write paths, so its paths never have `exec`.

### `task_program.observations[].access.xattr`

An array of outcome strings for extended attributes; hierarchical reports only.
A task program's own accesses only read, check, list and write paths, so its paths never have `xattr`.

### `observations`

An array of every path of the report, the task program's included, sorted by path, each once,
with every access kind and outcome that the path had anywhere; compact reports only.

### `observations[].path`

A string: the absolute path; compact reports only.

### `observations[].access`

An object that maps each access kind that the path had to its outcomes; compact reports only.

### `observations[].access.read`

An array of outcome strings, when the path's content was read; compact reports only.

### `observations[].access.probe`

An array of outcome strings, when the path's existence or metadata was checked; compact reports only.

### `observations[].access.list`

An array of outcome strings, when the directory's entries were read; compact reports only.

### `observations[].access.write`

An array of outcome strings, when the path was written; compact reports only.

### `observations[].access.exec`

An array of outcome strings, when the path was run or loaded to run a program; compact reports only.

### `observations[].access.xattr`

An array of outcome strings, when the path's extended attributes were read or listed; compact reports only.

### `connections`

An array of every connection of the report, sorted, each once; compact reports only.

### `connections[].kind`

A string, `ip`, `unix` or `abstract`, as for a program's connections; compact reports only.

### `connections[].address`

A string, the address and port, only when `kind` is `ip`; compact reports only.

### `connections[].path`

A string, the absolute path of the socket, only when `kind` is `unix`; compact reports only.

### `connections[].name`

A string, the socket's name without the zero byte that starts it, only when `kind` is `abstract`; compact reports only.

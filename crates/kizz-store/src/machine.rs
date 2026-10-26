// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The machine state that step records keep ("Machine state").
//!
//! Every step that runs records the [`MachineStateRecord`] that its command started
//! with, in the six parts that [`MachinePart`] names. A step names, with
//! `run(..., machine = [...])`, the parts that count as its inputs: the cache check
//! compares only those, value by value, with [`machine_changes`], and `kizz why`
//! shows their values, and with `--verbose` every part, with
//! [`MachineStateRecord::text`].
//!
//! The host name, the credentials, the umask, the machine architecture and the CPU
//! model are not per step: they are the [cache identity](crate::identity).

use std::fmt;

use minicbor::{Decode, Encode};

/// One part of the machine state, as `run(..., machine = [...])` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MachinePart {
    /// `kernel`: the kernel release and version.
    Kernel,
    /// `limits`: every resource limit, soft and hard.
    Limits,
    /// `affinity`: the CPUs the command may run on.
    Affinity,
    /// `memory`: the total and the available memory.
    Memory,
    /// `filesystem`: the type and mount flags of the file system that holds the
    /// working directory.
    Filesystem,
    /// `disk`: the size and free space of that file system.
    Disk,
}

impl MachinePart {
    /// Every part, in the order the spec lists them.
    pub const ALL: [Self; 6] = [
        Self::Kernel,
        Self::Limits,
        Self::Affinity,
        Self::Memory,
        Self::Filesystem,
        Self::Disk,
    ];

    /// The name of the part, as `run()` takes it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kernel => "kernel",
            Self::Limits => "limits",
            Self::Affinity => "affinity",
            Self::Memory => "memory",
            Self::Filesystem => "filesystem",
            Self::Disk => "disk",
        }
    }

    /// The part named `name`, or `None` when no part has that name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|part| part.name() == name)
    }

    /// The parts named in `names`, in that order, leaving out a name that no part
    /// has.
    #[must_use]
    pub fn parts(names: &[String]) -> Vec<Self> {
        names
            .iter()
            .filter_map(|name| Self::from_name(name))
            .collect()
    }

    /// The names of every part, as an error lists them: `kernel, limits, affinity,
    /// memory, filesystem and disk`.
    #[must_use]
    pub fn names_text() -> String {
        let names: Vec<&str> = Self::ALL.iter().map(|part| part.name()).collect();
        match names.split_last() {
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
            None => String::new(),
        }
    }
}

impl fmt::Display for MachinePart {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The kernel that ran a command.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct KernelRecord {
    /// The kernel release.
    #[n(0)]
    pub release: String,
    /// The kernel version.
    #[n(1)]
    pub version: String,
}

/// One resource limit, soft and hard.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct LimitRecord {
    /// The resource, as `prlimit` names it, such as `nofile`.
    #[n(0)]
    pub resource: String,
    /// The soft limit, or `None` when it is unlimited.
    #[n(1)]
    pub soft: Option<u64>,
    /// The hard limit, or `None` when it is unlimited.
    #[n(2)]
    pub hard: Option<u64>,
}

/// The memory of the machine, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct MemoryRecord {
    /// The total memory.
    #[n(0)]
    pub total: u64,
    /// The memory available to start new work without swapping.
    #[n(1)]
    pub available: u64,
}

/// The file system that held the working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct FileSystemRecord {
    /// The type of the file system, as the platform numbers it.
    #[n(0)]
    pub kind: u64,
    /// The mount flags, as the platform numbers them.
    #[n(1)]
    pub flags: u64,
}

/// The size and free space of the file system that held the working directory, in
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct DiskRecord {
    /// The size of the file system.
    #[n(0)]
    pub size: u64,
    /// The free space that a process without privileges may use.
    #[n(1)]
    pub free: u64,
}

/// The machine and process state that a command started with, as its step record
/// keeps it, in the six parts of [`MachinePart`]. A part that could not be read is
/// `None`, which explanations show as `unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Encode, Decode)]
pub struct MachineStateRecord {
    /// `kernel`: the kernel release and version.
    #[n(0)]
    pub kernel: Option<KernelRecord>,
    /// `limits`: every resource limit, soft and hard, in the platform's order.
    #[n(1)]
    pub limits: Option<Vec<LimitRecord>>,
    /// `affinity`: the CPUs the command could run on, by number, sorted.
    #[n(2)]
    pub affinity: Option<Vec<u32>>,
    /// `memory`: the total and the available memory.
    #[n(3)]
    pub memory: Option<MemoryRecord>,
    /// `filesystem`: the type and mount flags of the file system that held the
    /// working directory.
    #[n(4)]
    pub filesystem: Option<FileSystemRecord>,
    /// `disk`: the size and free space of that file system.
    #[n(5)]
    pub disk: Option<DiskRecord>,
}

/// A number of bytes as text, such as `1024 bytes`.
fn bytes(number: u64) -> String {
    format!("{number} bytes")
}

/// A resource limit as text: the number, or `unlimited`.
fn limit_text(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_owned(), |limit| limit.to_string())
}

/// CPU numbers as the kernel lists them, with runs as ranges, such as `0-3,6`, or
/// `none`.
fn cpu_list(cpus: &[u32]) -> String {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for &cpu in cpus {
        match ranges.last_mut() {
            Some((_, last)) if last.checked_add(1) == Some(cpu) => *last = cpu,
            _ => ranges.push((cpu, cpu)),
        }
    }
    if ranges.is_empty() {
        return "none".to_owned();
    }
    let shown: Vec<String> = ranges
        .iter()
        .map(|&(first, last)| {
            if first == last {
                first.to_string()
            } else {
                format!("{first}-{last}")
            }
        })
        .collect();
    shown.join(",")
}

impl MachineStateRecord {
    /// Each value of the part `part`, named, as text, in order, or `None` when the
    /// part could not be read.
    #[must_use]
    pub fn values(
        &self,
        part: MachinePart,
    ) -> Option<Vec<(String, String)>> {
        let named = |name: &str, value: String| (name.to_owned(), value);
        match part {
            MachinePart::Kernel => {
                self.kernel.as_ref().map(|kernel| {
                    vec![
                        named("kernel release", kernel.release.clone()),
                        named("kernel version", kernel.version.clone()),
                    ]
                })
            },
            MachinePart::Limits => {
                self.limits.as_ref().map(|limits| {
                    limits
                        .iter()
                        .flat_map(|limit| {
                            [
                                (
                                    format!("soft limit of {}", limit.resource),
                                    limit_text(limit.soft),
                                ),
                                (
                                    format!("hard limit of {}", limit.resource),
                                    limit_text(limit.hard),
                                ),
                            ]
                        })
                        .collect()
                })
            },
            MachinePart::Affinity => {
                self.affinity
                    .as_ref()
                    .map(|cpus| vec![named("CPUs", cpu_list(cpus))])
            },
            MachinePart::Memory => {
                self.memory.map(|memory| {
                    vec![
                        named("total memory", bytes(memory.total)),
                        named("available memory", bytes(memory.available)),
                    ]
                })
            },
            MachinePart::Filesystem => {
                self.filesystem.map(|filesystem| {
                    vec![
                        named("file system type", format!("{:#x}", filesystem.kind)),
                        named("mount flags", format!("{:#x}", filesystem.flags)),
                    ]
                })
            },
            MachinePart::Disk => {
                self.disk.map(|disk| {
                    vec![
                        named("file system size", bytes(disk.size)),
                        named("free space", bytes(disk.free)),
                    ]
                })
            },
        }
    }

    /// The part `part` on one line: each value with its name, such as `kernel
    /// release 6.12.0, kernel version #1 SMP`, or `unknown` when the part could not
    /// be read.
    #[must_use]
    pub fn text(
        &self,
        part: MachinePart,
    ) -> String {
        self.values(part).map_or_else(
            || "unknown".to_owned(),
            |values| {
                let shown: Vec<String> = values
                    .iter()
                    .map(|(name, value)| format!("{name} {value}"))
                    .collect();
                shown.join(", ")
            },
        )
    }
}

/// One value of the machine state that changed since a step's record.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct MachineChange {
    /// The name of the value, such as `kernel release`, or of the part when it could
    /// not be read on one side.
    #[n(0)]
    pub value: String,
    /// The recorded value, as text.
    #[n(1)]
    pub old: String,
    /// The current value, as text.
    #[n(2)]
    pub new: String,
}

impl fmt::Display for MachineChange {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{} from {} to {}", self.value, self.old, self.new)
    }
}

/// The value named `name` in `values`, or `none` when it is not there.
fn value_of(
    values: &[(String, String)],
    name: &str,
) -> String {
    values
        .iter()
        .find(|(each, _)| each == name)
        .map_or_else(|| "none".to_owned(), |(_, value)| value.clone())
}

/// Every value of the parts `parts` that differs between `old`, the recorded state,
/// and `new`, the current one, in the order of `parts` and then of the values.
///
/// Each part is compared value by value; a value that only one side has is `none`
/// on the other. A part that could be read on one side only is one change, named
/// by the part, from or to `unknown`.
#[must_use]
pub fn machine_changes(
    parts: &[MachinePart],
    old: &MachineStateRecord,
    new: &MachineStateRecord,
) -> Vec<MachineChange> {
    let mut changes = Vec::new();
    for &part in parts {
        let (Some(old_values), Some(new_values)) = (old.values(part), new.values(part)) else {
            let (old_text, new_text) = (old.text(part), new.text(part));
            if old_text != new_text {
                changes.push(MachineChange {
                    value: part.name().to_owned(),
                    old: old_text,
                    new: new_text,
                });
            }
            continue;
        };
        let mut names: Vec<&str> = old_values.iter().map(|(name, _)| name.as_str()).collect();
        for (name, _) in &new_values {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        for name in names {
            let (old_value, new_value) = (value_of(&old_values, name), value_of(&new_values, name));
            if old_value != new_value {
                changes.push(MachineChange {
                    value: name.to_owned(),
                    old: old_value,
                    new: new_value,
                });
            }
        }
    }
    changes
}

/// The changes `changes` on one line, such as `kernel release from 6.1 to 6.2,
/// available memory from 1 bytes to 2 bytes`, or `nothing` for none.
#[must_use]
pub fn machine_changes_text(changes: &[MachineChange]) -> String {
    if changes.is_empty() {
        return "nothing".to_owned();
    }
    let shown: Vec<String> = changes.iter().map(ToString::to_string).collect();
    shown.join(", ")
}

/// Unit tests for the machine state of step records.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use super::*;

    /// A state with every part, as a test machine might report it.
    fn state() -> MachineStateRecord {
        MachineStateRecord {
            kernel: Some(KernelRecord {
                release: "6.12.0".to_owned(),
                version: "#1 SMP".to_owned(),
            }),
            limits: Some(vec![
                LimitRecord {
                    resource: "cpu".to_owned(),
                    soft: None,
                    hard: None,
                },
                LimitRecord {
                    resource: "nofile".to_owned(),
                    soft: Some(1_024),
                    hard: Some(524_288),
                },
            ]),
            affinity: Some(vec![0, 1, 2, 3, 6]),
            memory: Some(MemoryRecord {
                total: 8_000,
                available: 4_000,
            }),
            filesystem: Some(FileSystemRecord {
                kind: 0xEF53,
                flags: 0x1020,
            }),
            disk: Some(DiskRecord {
                size: 100_000,
                free: 40_000,
            }),
        }
    }

    /// Each part has the name that `run()` takes, a name maps back to its part and an
    /// unknown one to none, and the names read as an error lists them.
    #[test]
    fn parts_have_their_names() {
        let names: Vec<&str> = MachinePart::ALL.iter().map(|part| part.name()).collect();
        assert_eq!(names, [
            "kernel",
            "limits",
            "affinity",
            "memory",
            "filesystem",
            "disk"
        ]);
        for part in MachinePart::ALL {
            assert_eq!(MachinePart::from_name(part.name()), Some(part));
            assert_eq!(part.to_string(), part.name());
        }
        assert_eq!(MachinePart::from_name("cpu"), None);
        assert_eq!(MachinePart::from_name("Kernel"), None);
        assert_eq!(
            MachinePart::parts(&["disk".to_owned(), "x".to_owned(), "kernel".to_owned()]),
            [MachinePart::Disk, MachinePart::Kernel]
        );
        assert_eq!(
            MachinePart::names_text(),
            "kernel, limits, affinity, memory, filesystem and disk"
        );
    }

    /// Every part reads as text with its values named, a part that could not be read
    /// reads as `unknown`, and an empty CPU list reads as `none`.
    #[test]
    fn every_part_reads_as_text() {
        let state = state();
        let shown: Vec<String> = MachinePart::ALL
            .iter()
            .map(|part| state.text(*part))
            .collect();
        assert_eq!(shown, [
            "kernel release 6.12.0, kernel version #1 SMP",
            "soft limit of cpu unlimited, hard limit of cpu unlimited, soft limit of nofile 1024, \
             hard limit of nofile 524288",
            "CPUs 0-3,6",
            "total memory 8000 bytes, available memory 4000 bytes",
            "file system type 0xef53, mount flags 0x1020",
            "file system size 100000 bytes, free space 40000 bytes",
        ]);
        let unknown = MachineStateRecord::default();
        for part in MachinePart::ALL {
            assert_eq!(unknown.text(part), "unknown");
            assert_eq!(unknown.values(part), None);
        }
        let none = MachineStateRecord {
            affinity: Some(Vec::new()),
            ..MachineStateRecord::default()
        };
        assert_eq!(none.text(MachinePart::Affinity), "CPUs none");
    }

    /// Only the named parts are compared, value by value: a changed, missing or
    /// unreadable value is one change each, and equal or unknown states have none.
    #[test]
    fn only_the_named_parts_are_compared_value_by_value() {
        let old = state();
        let mut new = state();
        new.kernel = Some(KernelRecord {
            release: "6.13.0".to_owned(),
            version: "#1 SMP".to_owned(),
        });
        new.memory = Some(MemoryRecord {
            total: 8_000,
            available: 3_000,
        });
        if let Some(limits) = &mut new.limits {
            limits.retain(|limit| limit.resource != "cpu");
        }
        new.affinity = Some(vec![0, 1]);
        new.disk = None;
        assert_eq!(machine_changes(&[], &old, &new), []);
        assert_eq!(machine_changes(&[MachinePart::Filesystem], &old, &new), []);
        let change = |value: &str, old: &str, new: &str| {
            MachineChange {
                value: value.to_owned(),
                old: old.to_owned(),
                new: new.to_owned(),
            }
        };
        assert_eq!(machine_changes(&MachinePart::ALL, &old, &new), [
            change("kernel release", "6.12.0", "6.13.0"),
            change("soft limit of cpu", "unlimited", "none"),
            change("hard limit of cpu", "unlimited", "none"),
            change("CPUs", "0-3,6", "0-1"),
            change("available memory", "4000 bytes", "3000 bytes"),
            change(
                "disk",
                "file system size 100000 bytes, free space 40000 bytes",
                "unknown"
            ),
        ]);
        assert_eq!(machine_changes(&MachinePart::ALL, &old, &old), []);
        let unknown = MachineStateRecord::default();
        assert_eq!(machine_changes(&MachinePart::ALL, &unknown, &unknown), []);
    }

    /// Changes read as one line, each from its old value to its new one, and no
    /// changes read as `nothing`.
    #[test]
    fn changes_read_as_text() {
        let changes = [
            MachineChange {
                value: "kernel release".to_owned(),
                old: "6.12.0".to_owned(),
                new: "6.13.0".to_owned(),
            },
            MachineChange {
                value: "CPUs".to_owned(),
                old: "0-3".to_owned(),
                new: "0".to_owned(),
            },
        ];
        assert_eq!(
            machine_changes_text(&changes),
            "kernel release from 6.12.0 to 6.13.0, CPUs from 0-3 to 0"
        );
        assert_eq!(machine_changes_text(&[]), "nothing");
    }

    /// The state and its changes encode with definite lengths, the same way every
    /// time, and round-trip, a state whose parts could not be read included.
    #[test]
    fn states_and_changes_encode_deterministically_and_round_trip() {
        for state in [state(), MachineStateRecord::default()] {
            let first = minicbor::to_vec(&state).unwrap();
            assert_eq!(first, minicbor::to_vec(state.clone()).unwrap());
            assert_eq!(
                minicbor::decode::<MachineStateRecord>(&first).unwrap(),
                state
            );
        }
        assert_eq!(minicbor::to_vec(state()).unwrap().first(), Some(&0x86));
        let change = MachineChange {
            value: "CPUs".to_owned(),
            old: "0".to_owned(),
            new: "1".to_owned(),
        };
        let bytes = minicbor::to_vec(&change).unwrap();
        assert_eq!(bytes, b"\x83\x64CPUs\x610\x611");
        assert_eq!(minicbor::decode::<MachineChange>(&bytes).unwrap(), change);
    }
}

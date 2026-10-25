// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The cache identity: the stamp of the machine and the user that a cache belongs to.
//!
//! The cache is not portable. When the store creates the database it stamps it with
//! the [`CacheIdentity`] of the process that creates it: the host name, the effective
//! user and group, the supplementary groups, the umask, the machine architecture, and
//! the CPU model. Every later [`Store::open`](crate::Store::open) gathers the current
//! identity and compares it with the stamp, value by value, and refuses a cache whose
//! stamp differs, naming each [`IdentityDifference`].

use std::{collections::BTreeSet, ffi::CStr, fmt, fs, mem::MaybeUninit};

use minicbor::{Decode, Encode};

use crate::{
    lock::host_name,
    records::{Family, Record, RecordKey},
};

/// The file whose `Umask` line holds the umask of this process.
const STATUS_FILE: &str = "/proc/self/status";

/// The file whose `model name` lines name the CPUs of the machine.
const CPU_INFO_FILE: &str = "/proc/cpuinfo";

/// The key of the cache identity, the stamp of the database, in the `meta/` family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdentityKey;

impl IdentityKey {
    /// The bytes of the key after the family prefix.
    const BYTES: &'static [u8] = b"identity";
}

impl RecordKey for IdentityKey {
    fn encode_key(
        &self,
        out: &mut Vec<u8>,
    ) {
        out.extend_from_slice(Self::BYTES);
    }

    fn decode_key(bytes: &[u8]) -> Option<Self> {
        (bytes == Self::BYTES).then_some(Self)
    }
}

/// The identity of the machine and the user that a cache belongs to, which the store
/// writes when it creates the database and compares on every later open.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct CacheIdentity {
    /// The host name of the machine.
    #[n(0)]
    pub host: String,
    /// The effective user ID.
    #[n(1)]
    pub user: u32,
    /// The effective group ID.
    #[n(2)]
    pub group: u32,
    /// The supplementary group IDs, sorted and distinct.
    #[n(3)]
    pub groups: Vec<u32>,
    /// The umask, from the `Umask` line of `/proc/self/status`, or `None` when the
    /// file has no such line.
    #[n(4)]
    pub umask: Option<u32>,
    /// The machine architecture, the `machine` field of `uname`.
    #[n(5)]
    pub machine: String,
    /// The distinct `model name` values of `/proc/cpuinfo`, sorted; empty when it has
    /// none.
    #[n(6)]
    pub cpu_models: Vec<String>,
}

impl Record for CacheIdentity {
    type Key = IdentityKey;

    const FAMILY: Family = Family::Meta;
}

impl CacheIdentity {
    /// The identity of this process on this machine.
    #[must_use]
    pub fn current() -> Self {
        Self {
            host: host_name(),
            // SAFETY: `geteuid` has no preconditions and cannot fail.
            user: unsafe { libc::geteuid() },
            // SAFETY: `getegid` has no preconditions and cannot fail.
            group: unsafe { libc::getegid() },
            groups: supplementary_groups(),
            umask: fs::read_to_string(STATUS_FILE)
                .ok()
                .and_then(|status| umask_of_status(&status)),
            machine: machine(),
            cpu_models: fs::read_to_string(CPU_INFO_FILE)
                .map(|info| cpu_models_of_info(&info))
                .unwrap_or_default(),
        }
    }

    /// Each value of this identity, the stored one, that differs from `current`, in
    /// the order of [`IdentityPart::ALL`]; empty when the two are the same.
    ///
    /// The values are compared as the record holds them, field by field, never by
    /// the text that messages show: two lists of CPU models can show the same text
    /// and still differ.
    #[must_use]
    pub fn differences(
        &self,
        current: &Self,
    ) -> Vec<IdentityDifference> {
        IdentityPart::ALL
            .into_iter()
            .filter(|part| part.differs(self, current))
            .map(|part| {
                IdentityDifference {
                    part,
                    stored: part.value(self),
                    current: part.value(current),
                }
            })
            .collect()
    }
}

/// One value of a [`CacheIdentity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityPart {
    /// The host name.
    Host,
    /// The effective user ID.
    User,
    /// The effective group ID.
    Group,
    /// The supplementary groups.
    Groups,
    /// The umask.
    Umask,
    /// The machine architecture.
    Machine,
    /// The CPU model.
    CpuModel,
}

impl IdentityPart {
    /// Every value, in the order of the fields of [`CacheIdentity`].
    pub const ALL: [Self; 7] = [
        Self::Host,
        Self::User,
        Self::Group,
        Self::Groups,
        Self::Umask,
        Self::Machine,
        Self::CpuModel,
    ];

    /// The name of the value in messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Host => "host name",
            Self::User => "effective user ID",
            Self::Group => "effective group ID",
            Self::Groups => "supplementary groups",
            Self::Umask => "umask",
            Self::Machine => "machine architecture",
            Self::CpuModel => "CPU model",
        }
    }

    /// Whether this value of `stored` differs from the same value of `current`,
    /// compared as the fields hold them, not as messages show them.
    #[must_use]
    pub fn differs(
        self,
        stored: &CacheIdentity,
        current: &CacheIdentity,
    ) -> bool {
        match self {
            Self::Host => stored.host != current.host,
            Self::User => stored.user != current.user,
            Self::Group => stored.group != current.group,
            Self::Groups => stored.groups != current.groups,
            Self::Umask => stored.umask != current.umask,
            Self::Machine => stored.machine != current.machine,
            Self::CpuModel => stored.cpu_models != current.cpu_models,
        }
    }

    /// This value of `identity`, as messages show it.
    ///
    /// Different values always show different text: names are quoted, with every
    /// quote, backslash and control character escaped; group IDs and CPU models are
    /// bracketed lists, so an empty list, a list of one empty name, and a list of one
    /// name that holds a separator all show apart; and the umask is four octal digits,
    /// or `unknown` when the status file had no `Umask` line.
    #[must_use]
    pub fn value(
        self,
        identity: &CacheIdentity,
    ) -> String {
        match self {
            Self::Host => quoted(&identity.host),
            Self::User => identity.user.to_string(),
            Self::Group => identity.group.to_string(),
            Self::Groups => bracketed(identity.groups.iter().map(u32::to_string)),
            Self::Umask => {
                identity
                    .umask
                    .map_or_else(|| String::from("unknown"), |umask| format!("{umask:04o}"))
            },
            Self::Machine => quoted(&identity.machine),
            Self::CpuModel => bracketed(identity.cpu_models.iter().map(String::as_str).map(quoted)),
        }
    }
}

/// `text` between double quotes, with each double quote and backslash escaped by a
/// backslash, and each control character written as `\u{…}` with its hexadecimal
/// code, so that two different texts never show the same.
fn quoted(text: &str) -> String {
    let mut shown = String::from('"');
    for character in text.chars() {
        match character {
            '"' | '\\' => {
                shown.push('\\');
                shown.push(character);
            },
            control if control.is_control() => shown.extend(control.escape_unicode()),
            other => shown.push(other),
        }
    }
    shown.push('"');
    shown
}

/// `items` separated by commas between square brackets, `[]` when there are none.
fn bracketed(items: impl Iterator<Item = String>) -> String {
    format!("[{}]", items.collect::<Vec<_>>().join(", "))
}

impl fmt::Display for IdentityPart {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A value of the cache identity that differs: as the cache stores it, and as this
/// process has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityDifference {
    /// The value that differs.
    pub part: IdentityPart,
    /// The value that the cache stores, as [`IdentityPart::value`] shows it.
    pub stored: String,
    /// The current value, as [`IdentityPart::value`] shows it.
    pub current: String,
}

impl fmt::Display for IdentityDifference {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(
            f,
            "{}: {} in the cache, {} now",
            self.part, self.stored, self.current
        )
    }
}

/// Every difference of `differences`, separated by semicolons.
#[must_use]
pub fn identity_differences_text(differences: &[IdentityDifference]) -> String {
    differences
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// The supplementary group IDs of this process, sorted and distinct; empty when they
/// cannot be read.
fn supplementary_groups() -> Vec<u32> {
    // SAFETY: a size of zero asks only for the number of groups, and writes nothing.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let Ok(len) = usize::try_from(count) else {
        return Vec::new();
    };
    let mut groups: Vec<libc::gid_t> = vec![0; len];
    // SAFETY: the buffer holds `count` group IDs, the size passed.
    let filled = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
    let Ok(filled) = usize::try_from(filled) else {
        return Vec::new();
    };
    groups.truncate(filled);
    groups.sort_unstable();
    groups.dedup();
    groups
}

/// The machine architecture from `uname`, or `(unknown)` when it cannot be read.
fn machine() -> String {
    let mut names = MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: the pointer is valid for writing a `utsname`.
    if unsafe { libc::uname(names.as_mut_ptr()) } != 0 {
        return String::from("(unknown)");
    }
    // SAFETY: `uname` succeeded, so it initialized `names`.
    let names = unsafe { names.assume_init() };
    // SAFETY: `uname` ends every field with a zero byte within the field.
    unsafe { CStr::from_ptr(names.machine.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// The umask of the `Umask` line of `status`, the text of `/proc/self/status`, or
/// `None` when it has no such line or its value is not octal.
fn umask_of_status(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix("Umask:")?;
        u32::from_str_radix(value.trim(), 8).ok()
    })
}

/// The distinct `model name` values of `info`, the text of `/proc/cpuinfo`, sorted.
fn cpu_models_of_info(info: &str) -> Vec<String> {
    info.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "model name").then(|| value.trim().to_owned())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Two lists of CPU models that the joined text of earlier versions showed alike, with
/// the text that messages show for each now.
#[cfg(test)]
pub(crate) struct CpuModelCollision {
    /// The list that the cache stores.
    pub(crate) stored: Vec<String>,
    /// The text that messages show for `stored`.
    pub(crate) stored_text: &'static str,
    /// The current list.
    pub(crate) current: Vec<String>,
    /// The text that messages show for `current`.
    pub(crate) current_text: &'static str,
}

/// The CPU-model lists that the joined text of earlier versions showed alike: an
/// empty list, a list of one empty name and a list of the name `none`, which all
/// showed `none`; a name that holds the separator `; `, which showed as two names; and
/// a name that holds quotes and a comma, which a list without escapes would show as
/// two names.
#[cfg(test)]
pub(crate) fn colliding_cpu_models() -> Vec<CpuModelCollision> {
    let collision = |stored: &[&str],
                     stored_text: &'static str,
                     current: &[&str],
                     current_text: &'static str| {
        CpuModelCollision {
            stored: stored.iter().copied().map(String::from).collect(),
            stored_text,
            current: current.iter().copied().map(String::from).collect(),
            current_text,
        }
    };
    vec![
        collision(&[], "[]", &["none"], r#"["none"]"#),
        collision(&[], "[]", &[""], r#"[""]"#),
        collision(&[""], r#"[""]"#, &["none"], r#"["none"]"#),
        collision(&["A; B"], r#"["A; B"]"#, &["A", "B"], r#"["A", "B"]"#),
        collision(
            &[r#"A", "B"#],
            r#"["A\", \"B"]"#,
            &["A", "B"],
            r#"["A", "B"]"#,
        ),
    ]
}

/// Unit tests for parsing the identity and describing its differences.
#[cfg(test)]
mod tests {
    use super::*;

    /// An identity whose values are all made up.
    fn identity() -> CacheIdentity {
        CacheIdentity {
            host: String::from("builder"),
            user: 1000,
            group: 100,
            groups: vec![10, 100],
            umask: Some(0o022),
            machine: String::from("x86_64"),
            cpu_models: vec![String::from("Example CPU")],
        }
    }

    /// The umask is the octal value of the `Umask` line of `/proc/self/status`, and
    /// unknown when the line is missing or its value is not octal.
    #[test]
    fn the_umask_is_the_octal_value_of_its_status_line() {
        let status = "Name:\tkizz\nUmask:\t0027\nState:\tR (running)\n";
        assert_eq!(umask_of_status(status), Some(0o027));
        assert_eq!(umask_of_status("Name:\tkizz\n"), None);
        assert_eq!(umask_of_status("Umask:\t0089\n"), None);
    }

    /// The CPU models are the `model name` values of `/proc/cpuinfo`, sorted and
    /// distinct, and none when it has no such line.
    #[test]
    fn cpu_models_are_the_sorted_distinct_model_names() {
        let info = "processor\t: 0\nmodel name\t: Fast Core\nflags\t\t: fpu\n\nprocessor\t: \
                    1\nmodel name\t: Efficient Core\n\nprocessor\t: 2\nmodel name\t: Fast Core\n";
        assert_eq!(cpu_models_of_info(info), vec![
            String::from("Efficient Core"),
            String::from("Fast Core"),
        ]);
        assert_eq!(
            cpu_models_of_info("processor\t: 0\nCPU part\t: 0xd0c\n"),
            Vec::<String>::new()
        );
    }

    /// An identity compared with itself has no differences.
    #[test]
    fn the_same_identity_has_no_differences() {
        assert_eq!(identity().differences(&identity()), Vec::new());
    }

    /// Values that differ are compared field by field, not by the text that messages
    /// show, and CPU-model lists that the old joined text showed alike differ in the
    /// CPU model alone and now show different text, in the value and in the message;
    /// each other field that changes alone differs alone as well.
    #[test]
    fn differences_compare_the_fields_not_their_text() {
        for CpuModelCollision {
            stored: stored_models,
            stored_text,
            current: current_models,
            current_text,
        } in colliding_cpu_models()
        {
            let stored = CacheIdentity {
                cpu_models: stored_models,
                ..identity()
            };
            let current = CacheIdentity {
                cpu_models: current_models,
                ..identity()
            };
            assert_eq!(IdentityPart::CpuModel.value(&stored), stored_text);
            assert_eq!(IdentityPart::CpuModel.value(&current), current_text);
            assert_ne!(stored_text, current_text);
            let differences = stored.differences(&current);
            assert_eq!(
                differences,
                vec![IdentityDifference {
                    part: IdentityPart::CpuModel,
                    stored: String::from(stored_text),
                    current: String::from(current_text),
                }],
                "{stored:?} and {current:?}"
            );
            assert_eq!(
                identity_differences_text(&differences),
                format!("CPU model: {stored_text} in the cache, {current_text} now")
            );
            assert!(IdentityPart::CpuModel.differs(&stored, &current));
            assert!(!IdentityPart::CpuModel.differs(&stored, &stored));
        }
        let changed = [
            (IdentityPart::Host, CacheIdentity {
                host: String::from("other-builder"),
                ..identity()
            }),
            (IdentityPart::User, CacheIdentity {
                user: 1001,
                ..identity()
            }),
            (IdentityPart::Group, CacheIdentity {
                group: 101,
                ..identity()
            }),
            (IdentityPart::Groups, CacheIdentity {
                groups: vec![10],
                ..identity()
            }),
            (IdentityPart::Umask, CacheIdentity {
                umask: None,
                ..identity()
            }),
            (IdentityPart::Machine, CacheIdentity {
                machine: String::from("other-architecture"),
                ..identity()
            }),
            (IdentityPart::CpuModel, CacheIdentity {
                cpu_models: Vec::new(),
                ..identity()
            }),
        ];
        for (part, other) in changed {
            for candidate in IdentityPart::ALL {
                assert_eq!(
                    candidate.differs(&identity(), &other),
                    candidate == part,
                    "{candidate} when {part} changed"
                );
            }
        }
    }

    /// Messages show each kind of value: names quoted, with quotes, backslashes and
    /// control characters escaped; group IDs and CPU models as bracketed lists, `[]`
    /// when empty; and the umask in octal, or unknown.
    #[test]
    fn values_show_every_case() {
        let mut other = identity();
        other.host = String::from("say \"hi\"\\\tthere\n");
        other.groups.clear();
        other.umask = None;
        other.cpu_models = vec![String::from("A"), String::from("B")];
        let differences = identity().differences(&other);
        assert_eq!(
            identity_differences_text(&differences),
            concat!(
                r#"host name: "builder" in the cache, "say \"hi\"\\\u{9}there\u{a}" now; "#,
                "supplementary groups: [10, 100] in the cache, [] now; ",
                "umask: 0022 in the cache, unknown now; ",
                r#"CPU model: ["Example CPU"] in the cache, ["A", "B"] now"#,
            )
        );
        assert_eq!(IdentityPart::User.value(&identity()), "1000");
        assert_eq!(IdentityPart::Group.value(&identity()), "100");
        assert_eq!(IdentityPart::Machine.value(&identity()), r#""x86_64""#);
        assert_eq!(IdentityPart::Groups.value(&other), "[]");
        other.groups = vec![0];
        assert_eq!(IdentityPart::Groups.value(&other), "[0]");
        other.umask = Some(0);
        assert_eq!(IdentityPart::Umask.value(&other), "0000");
        other.cpu_models.clear();
        assert_eq!(IdentityPart::CpuModel.value(&other), "[]");
    }

    /// The identity record encodes the same bytes each time, as a definite-length
    /// array of seven fields, and decodes back to itself, with or without values.
    #[test]
    fn the_identity_encodes_deterministically_and_round_trips() {
        let mut unknown = identity();
        unknown.groups.clear();
        unknown.umask = None;
        unknown.cpu_models.clear();
        for record in [identity(), unknown] {
            let first = minicbor::to_vec(&record).unwrap();
            assert_eq!(minicbor::to_vec(&record).unwrap(), first);
            assert_eq!(minicbor::decode::<CacheIdentity>(&first).unwrap(), record);
            // A definite-length array of seven items, then the host name, `builder`,
            // a text string of seven bytes.
            assert_eq!(first.get(..2), Some(&[0x87, 0x67][..]));
        }
    }

    /// The identity key is `identity` in the `meta/` family, and decodes only from
    /// its own bytes.
    #[test]
    fn identity_keys_round_trip() {
        let mut bytes = Vec::new();
        IdentityKey.encode_key(&mut bytes);
        assert_eq!(bytes, b"identity");
        assert_eq!(IdentityKey::decode_key(&bytes), Some(IdentityKey));
        assert_eq!(IdentityKey::decode_key(b"schema-version"), None);
        assert_eq!(
            crate::records::database_key::<CacheIdentity>(&IdentityKey),
            b"meta/identity"
        );
    }
}

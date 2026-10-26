// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The names of Linux's error numbers, which the tracer gives the outcome of a
//! failed call and the answer of the error-name query.
//!
//! The names are those of Linux's `errno` values on `x86_64`, one for each number;
//! where two names share a number, such as `EAGAIN` and `EWOULDBLOCK`, the first
//! that Linux defines is used. The kernel's internal restart errors, which a traced
//! call can return when a signal interrupts it, keep the kernel's names:
//! `ERESTARTSYS`, `ERESTARTNOINTR`, `ERESTARTNOHAND` and `ERESTART_RESTARTBLOCK`.
//! A number without a name is written as `errno` and the number, such as
//! `errno 200`.

use kizz_trace::unnamed_error;
use libc::c_int;

/// The kernel's `ERESTARTSYS`: a call that a signal interrupted restarts unless
/// the handler says otherwise.
const ERESTARTSYS: c_int = 512;
/// The kernel's `ERESTARTNOINTR`: a call that a signal interrupted always restarts.
const ERESTARTNOINTR: c_int = 513;
/// The kernel's `ERESTARTNOHAND`: a call that a signal interrupted restarts when no
/// handler ran.
const ERESTARTNOHAND: c_int = 514;
/// The kernel's `ERESTART_RESTARTBLOCK`: a call that a signal interrupted restarts
/// through `restart_syscall`.
const ERESTART_RESTARTBLOCK: c_int = 516;

/// Every named error number with its name, in the order of the numbers.
const NAMES: [(c_int, &str); 135] = [
    (libc::EPERM, "EPERM"),
    (libc::ENOENT, "ENOENT"),
    (libc::ESRCH, "ESRCH"),
    (libc::EINTR, "EINTR"),
    (libc::EIO, "EIO"),
    (libc::ENXIO, "ENXIO"),
    (libc::E2BIG, "E2BIG"),
    (libc::ENOEXEC, "ENOEXEC"),
    (libc::EBADF, "EBADF"),
    (libc::ECHILD, "ECHILD"),
    (libc::EAGAIN, "EAGAIN"),
    (libc::ENOMEM, "ENOMEM"),
    (libc::EACCES, "EACCES"),
    (libc::EFAULT, "EFAULT"),
    (libc::ENOTBLK, "ENOTBLK"),
    (libc::EBUSY, "EBUSY"),
    (libc::EEXIST, "EEXIST"),
    (libc::EXDEV, "EXDEV"),
    (libc::ENODEV, "ENODEV"),
    (libc::ENOTDIR, "ENOTDIR"),
    (libc::EISDIR, "EISDIR"),
    (libc::EINVAL, "EINVAL"),
    (libc::ENFILE, "ENFILE"),
    (libc::EMFILE, "EMFILE"),
    (libc::ENOTTY, "ENOTTY"),
    (libc::ETXTBSY, "ETXTBSY"),
    (libc::EFBIG, "EFBIG"),
    (libc::ENOSPC, "ENOSPC"),
    (libc::ESPIPE, "ESPIPE"),
    (libc::EROFS, "EROFS"),
    (libc::EMLINK, "EMLINK"),
    (libc::EPIPE, "EPIPE"),
    (libc::EDOM, "EDOM"),
    (libc::ERANGE, "ERANGE"),
    (libc::EDEADLK, "EDEADLK"),
    (libc::ENAMETOOLONG, "ENAMETOOLONG"),
    (libc::ENOLCK, "ENOLCK"),
    (libc::ENOSYS, "ENOSYS"),
    (libc::ENOTEMPTY, "ENOTEMPTY"),
    (libc::ELOOP, "ELOOP"),
    (libc::ENOMSG, "ENOMSG"),
    (libc::EIDRM, "EIDRM"),
    (libc::ECHRNG, "ECHRNG"),
    (libc::EL2NSYNC, "EL2NSYNC"),
    (libc::EL3HLT, "EL3HLT"),
    (libc::EL3RST, "EL3RST"),
    (libc::ELNRNG, "ELNRNG"),
    (libc::EUNATCH, "EUNATCH"),
    (libc::ENOCSI, "ENOCSI"),
    (libc::EL2HLT, "EL2HLT"),
    (libc::EBADE, "EBADE"),
    (libc::EBADR, "EBADR"),
    (libc::EXFULL, "EXFULL"),
    (libc::ENOANO, "ENOANO"),
    (libc::EBADRQC, "EBADRQC"),
    (libc::EBADSLT, "EBADSLT"),
    (libc::EBFONT, "EBFONT"),
    (libc::ENOSTR, "ENOSTR"),
    (libc::ENODATA, "ENODATA"),
    (libc::ETIME, "ETIME"),
    (libc::ENOSR, "ENOSR"),
    (libc::ENONET, "ENONET"),
    (libc::ENOPKG, "ENOPKG"),
    (libc::EREMOTE, "EREMOTE"),
    (libc::ENOLINK, "ENOLINK"),
    (libc::EADV, "EADV"),
    (libc::ESRMNT, "ESRMNT"),
    (libc::ECOMM, "ECOMM"),
    (libc::EPROTO, "EPROTO"),
    (libc::EMULTIHOP, "EMULTIHOP"),
    (libc::EDOTDOT, "EDOTDOT"),
    (libc::EBADMSG, "EBADMSG"),
    (libc::EOVERFLOW, "EOVERFLOW"),
    (libc::ENOTUNIQ, "ENOTUNIQ"),
    (libc::EBADFD, "EBADFD"),
    (libc::EREMCHG, "EREMCHG"),
    (libc::ELIBACC, "ELIBACC"),
    (libc::ELIBBAD, "ELIBBAD"),
    (libc::ELIBSCN, "ELIBSCN"),
    (libc::ELIBMAX, "ELIBMAX"),
    (libc::ELIBEXEC, "ELIBEXEC"),
    (libc::EILSEQ, "EILSEQ"),
    (libc::ERESTART, "ERESTART"),
    (libc::ESTRPIPE, "ESTRPIPE"),
    (libc::EUSERS, "EUSERS"),
    (libc::ENOTSOCK, "ENOTSOCK"),
    (libc::EDESTADDRREQ, "EDESTADDRREQ"),
    (libc::EMSGSIZE, "EMSGSIZE"),
    (libc::EPROTOTYPE, "EPROTOTYPE"),
    (libc::ENOPROTOOPT, "ENOPROTOOPT"),
    (libc::EPROTONOSUPPORT, "EPROTONOSUPPORT"),
    (libc::ESOCKTNOSUPPORT, "ESOCKTNOSUPPORT"),
    (libc::EOPNOTSUPP, "EOPNOTSUPP"),
    (libc::EPFNOSUPPORT, "EPFNOSUPPORT"),
    (libc::EAFNOSUPPORT, "EAFNOSUPPORT"),
    (libc::EADDRINUSE, "EADDRINUSE"),
    (libc::EADDRNOTAVAIL, "EADDRNOTAVAIL"),
    (libc::ENETDOWN, "ENETDOWN"),
    (libc::ENETUNREACH, "ENETUNREACH"),
    (libc::ENETRESET, "ENETRESET"),
    (libc::ECONNABORTED, "ECONNABORTED"),
    (libc::ECONNRESET, "ECONNRESET"),
    (libc::ENOBUFS, "ENOBUFS"),
    (libc::EISCONN, "EISCONN"),
    (libc::ENOTCONN, "ENOTCONN"),
    (libc::ESHUTDOWN, "ESHUTDOWN"),
    (libc::ETOOMANYREFS, "ETOOMANYREFS"),
    (libc::ETIMEDOUT, "ETIMEDOUT"),
    (libc::ECONNREFUSED, "ECONNREFUSED"),
    (libc::EHOSTDOWN, "EHOSTDOWN"),
    (libc::EHOSTUNREACH, "EHOSTUNREACH"),
    (libc::EALREADY, "EALREADY"),
    (libc::EINPROGRESS, "EINPROGRESS"),
    (libc::ESTALE, "ESTALE"),
    (libc::EUCLEAN, "EUCLEAN"),
    (libc::ENOTNAM, "ENOTNAM"),
    (libc::ENAVAIL, "ENAVAIL"),
    (libc::EISNAM, "EISNAM"),
    (libc::EREMOTEIO, "EREMOTEIO"),
    (libc::EDQUOT, "EDQUOT"),
    (libc::ENOMEDIUM, "ENOMEDIUM"),
    (libc::EMEDIUMTYPE, "EMEDIUMTYPE"),
    (libc::ECANCELED, "ECANCELED"),
    (libc::ENOKEY, "ENOKEY"),
    (libc::EKEYEXPIRED, "EKEYEXPIRED"),
    (libc::EKEYREVOKED, "EKEYREVOKED"),
    (libc::EKEYREJECTED, "EKEYREJECTED"),
    (libc::EOWNERDEAD, "EOWNERDEAD"),
    (libc::ENOTRECOVERABLE, "ENOTRECOVERABLE"),
    (libc::ERFKILL, "ERFKILL"),
    (libc::EHWPOISON, "EHWPOISON"),
    (ERESTARTSYS, "ERESTARTSYS"),
    (ERESTARTNOINTR, "ERESTARTNOINTR"),
    (ERESTARTNOHAND, "ERESTARTNOHAND"),
    (ERESTART_RESTARTBLOCK, "ERESTART_RESTARTBLOCK"),
];

/// The name of the error number `errno`, or `errno` and the number when it has
/// none.
pub(crate) fn name(errno: i64) -> String {
    NAMES
        .iter()
        .find(|(number, _)| i64::from(*number) == errno)
        .map_or_else(
            || i32::try_from(errno).map_or_else(|_| format!("errno {errno}"), unnamed_error),
            |(_, name)| (*name).to_owned(),
        )
}

/// Unit tests of the error names.
#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Every number from 1 through 133 has exactly one name, the four restart errors
    /// have theirs, and no name or number is listed twice.
    #[test]
    fn every_error_number_has_one_name() {
        let numbers: BTreeSet<c_int> = NAMES.iter().map(|(number, _)| *number).collect();
        let names: BTreeSet<&str> = NAMES.iter().map(|(_, name)| *name).collect();
        assert_eq!(numbers.len(), NAMES.len());
        assert_eq!(names.len(), NAMES.len());
        let ordinary: BTreeSet<c_int> = (1..=133)
            .filter(|number| *number != 41 && *number != 58)
            .collect();
        let restart = BTreeSet::from([512, 513, 514, 516]);
        assert_eq!(
            numbers,
            ordinary.union(&restart).copied().collect::<BTreeSet<_>>()
        );
        for (number, name) in NAMES {
            assert!(name.starts_with('E'), "{number} is named {name}");
        }
    }

    /// Known numbers have their Linux names, the restart errors the kernel's, and
    /// any other number is `errno` and the number.
    #[test]
    fn errors_are_named_as_linux_names_them() {
        assert_eq!(name(i64::from(libc::ENOENT)), "ENOENT");
        assert_eq!(name(2), "ENOENT");
        assert_eq!(name(4), "EINTR");
        assert_eq!(name(11), "EAGAIN");
        assert_eq!(name(35), "EDEADLK");
        assert_eq!(name(61), "ENODATA");
        assert_eq!(name(95), "EOPNOTSUPP");
        assert_eq!(name(512), "ERESTARTSYS");
        assert_eq!(name(513), "ERESTARTNOINTR");
        assert_eq!(name(514), "ERESTARTNOHAND");
        assert_eq!(name(516), "ERESTART_RESTARTBLOCK");
        assert_eq!(name(41), "errno 41");
        assert_eq!(name(200), "errno 200");
        assert_eq!(name(515), "errno 515");
        assert_eq!(name(0), "errno 0");
        assert_eq!(name(1_099_511_627_776), "errno 1099511627776");
    }
}

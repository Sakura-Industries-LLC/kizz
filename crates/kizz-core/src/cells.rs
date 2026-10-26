// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Values that one thread changes through shared references, without `RefCell`
//! ("Kizz never panics").
//!
//! The runtime and the step executor keep state that Starlark's callbacks and the
//! runtime's executor reach through `&self`. A `RefCell` would give it with
//! `borrow` and `borrow_mut`, which panic while the cell is borrowed, so the state
//! lives in a [`Cell`] instead: [`update`] takes the value out, changes it and puts
//! it back, and [`cloned`] takes it out, copies it and puts it back. `Cell::take`
//! and `Cell::set` cannot fail, so neither function can panic or return an error.
//!
//! While a value is out, its cell holds the default value. Every change that kizz
//! makes is a few operations on the value itself, such as a push, an insert or a
//! clone, or the file operations of [`Files`](crate::files::Files), which reach no
//! other state; none of them reaches the same cell, so nothing reads the cell while
//! its value is out, and no change is lost.

use std::cell::Cell;

/// Change the value in `cell` with `change`, which must not reach `cell`, and
/// return what `change` returns.
pub(crate) fn update<T: Default, R>(
    cell: &Cell<T>,
    change: impl FnOnce(&mut T) -> R,
) -> R {
    let mut value = cell.take();
    let result = change(&mut value);
    cell.set(value);
    result
}

/// A copy of the value in `cell`, which stays in place.
pub(crate) fn cloned<T: Default + Clone>(cell: &Cell<T>) -> T {
    update(cell, |value| value.clone())
}

/// Unit tests of the changes and the copies.
#[cfg(test)]
#[allow(clippy::disallowed_macros, reason = "test code: the tests assert")]
mod tests {
    use std::cell::Cell;

    use super::{cloned, update};

    /// A change stays in the cell, and its result is returned.
    #[test]
    fn a_change_stays_and_returns_its_result() {
        let cell = Cell::new(vec![1]);
        let length = update(&cell, |values| {
            values.push(2);
            values.len()
        });
        assert_eq!(length, 2);
        assert_eq!(cell.take(), [1, 2]);
    }

    /// A copy leaves the value in the cell.
    #[test]
    fn a_copy_leaves_the_value_in_place() {
        let cell = Cell::new(String::from("kept"));
        assert_eq!(cloned(&cell), "kept");
        assert_eq!(cloned(&cell), "kept");
        assert_eq!(cell.into_inner(), "kept");
    }
}

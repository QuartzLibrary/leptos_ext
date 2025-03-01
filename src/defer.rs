use std::{panic::Location, sync::LazyLock};

use leptos::prelude::{Owner, on_cleanup, queue_microtask};

use crate::util::SharedBox;

static LEAK: LazyLock<Owner> = LazyLock::new(|| Owner::new_root(None));

/// Defers function execution with queue_microtask.
///
/// NOTE: This preserves the calling owner, so:
/// - Using `on_cleanup` inside works as normal, and will run if the owner is cleaned or disposed.
/// - The deferred function will not run if the owner is cleaned or disposed before it has a chance to.
#[track_caller]
pub fn defer(f: impl FnOnce() + 'static) {
    match Owner::current() {
        Some(owner) => defer_with(owner, f),
        None => defer_leak(f),
    }
}

#[track_caller]
pub fn defer_with(owner: Owner, f: impl FnOnce() + 'static) {
    let location = Location::caller();

    // If the parent is cleaned up, we do not excute the deferred function.
    let cleanup_marker = SharedBox::new(false);
    owner.with({
        let cleanup_marker = cleanup_marker.clone();
        move || on_cleanup(move || cleanup_marker.set(true))
    });

    // We wrap the function to check for a cleanup before running it.
    let f = move || {
        if cleanup_marker.get() {
            log::warn!("Skipping deferred function ({location}) because its owner was cleaned.");
        } else {
            f();
        }
    };

    queue_microtask(move || owner.with(f));
}

/// Defers function execution with queue_microtask,
/// this will always be executed even if the calling scope is cleaned up.
#[track_caller]
pub fn defer_leak(f: impl FnOnce() + 'static) {
    defer_with(LEAK.clone(), f);
}

/// Defers function execution with queue_microtask.
///
/// NOTE: This preserves the calling owner, so:
/// - Using `on_cleanup` inside works as normal, and will run if the owner is cleaned or disposed.
/// - The deferred function will not run if the owner is cleaned or disposed before it has a chance to.
#[track_caller]
pub fn defer(f: impl FnOnce() + 'static) {
    defer_with(Scope::current(), f);
}

#[track_caller]
pub fn defer_with(scope: Scope, f: impl FnOnce() + 'static) {
    let location = Location::caller();

    // If the parent is cleaned up, we do not excute the deferred function.
    let cleanup_marker = SharedBox::new(false);
    if scope != Scope::LEAK {
        scope.with_owner({
            let cleanup_marker = cleanup_marker.clone();
            move || on_cleanup(move || cleanup_marker.set(true))
        });
    }

    // We wrap the function to check for a cleanup before running it.
    let f = move || {
        if cleanup_marker.get() {
            log::warn!("Skipping deferred function ({location}) because its owner was cleaned.");
        } else {
            f();
        }
    };

    queue_microtask(move || match scope.try_with_owner(f) {
        Err(ReactiveSystemError::OwnerDisposed(_)) => {
            log::warn!("Skipping deferred function ({location}) because its owner was disposed.");
        }
        Err(e @ (ReactiveSystemError::RuntimeDisposed(_) | ReactiveSystemError::Borrow(_))) => {
            panic!("Error while attempting to execute deferred function ({location}). {e:?}")
        }
        Ok(()) => {}
    });
}

/// Defers function execution with queue_microtask,
/// this will always be executed even if the calling scope is cleaned up.
#[track_caller]
pub fn defer_leak(f: impl FnOnce() + 'static) {
    defer_with(Scope::LEAK, f);
}

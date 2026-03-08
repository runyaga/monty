//! A resource tracker wrapper that adds preemptive cancellation support.
//!
//! `CancellableTracker` wraps any `ResourceTracker` implementation and adds an
//! `Arc<AtomicBool>` cancel flag. External code (e.g., another thread, Dart FFI,
//! a signal handler) can set the flag to `true` to request cancellation. On the
//! next `check_time` or `on_allocate` call the tracker will return a
//! `ResourceError`, terminating the running script.
//!
//! The cancel flag is checked with `Ordering::Relaxed` because we only need
//! eventual visibility — a few extra bytecodes executing before the flag is
//! observed is acceptable, and relaxed loads are essentially free on all
//! architectures Monty targets (x86-64, aarch64, wasm32).

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    ExcType, MontyException,
    resource::{ResourceError, ResourceTracker},
};

/// A resource tracker that wraps an inner tracker with a cooperative cancel flag.
///
/// On each `check_time` and `on_allocate` call the atomic flag is inspected
/// first. If set, a `ResourceError::Exception` carrying a `KeyboardInterrupt`
/// is returned immediately — the same exception CPython raises on Ctrl+C.
/// Because resource errors (other than `RecursionError`) are converted to
/// uncatchable exceptions by the VM, Python code cannot suppress the
/// cancellation with a bare `except:`.
///
/// # Thread safety
///
/// The cancel flag is an `Arc<AtomicBool>` — clone it via [`cancel_flag`] and
/// hand the clone to whatever external context needs the ability to cancel
/// (another OS thread, a Dart isolate via FFI, a signal handler, etc.).
///
/// [`cancel_flag`]: CancellableTracker::cancel_flag
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use std::sync::atomic::AtomicBool;
/// use monty::{CancellableTracker, NoLimitTracker};
///
/// let tracker = CancellableTracker::new(NoLimitTracker);
/// let flag = tracker.cancel_flag();
///
/// // Hand `flag` to another thread, then later:
/// flag.store(true, std::sync::atomic::Ordering::Relaxed);
/// ```
pub struct CancellableTracker<T> {
    /// The wrapped resource tracker that handles allocation/time/memory limits.
    inner: T,
    /// Shared cancel flag. `true` means cancellation has been requested.
    cancelled: Arc<AtomicBool>,
}

impl<T: fmt::Debug> fmt::Debug for CancellableTracker<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellableTracker")
            .field("inner", &self.inner)
            .field("cancelled", &self.cancelled.load(Ordering::Relaxed))
            .finish()
    }
}

impl<T> CancellableTracker<T> {
    /// Creates a new `CancellableTracker` wrapping the given inner tracker.
    ///
    /// The cancel flag starts as `false` (not cancelled).
    #[must_use]
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Creates a new `CancellableTracker` with an externally provided cancel flag.
    ///
    /// This is useful when the flag must be created before the tracker — for
    /// example, when a Dart FFI handle needs to store the flag pointer before
    /// execution begins.
    #[must_use]
    pub fn with_flag(inner: T, cancelled: Arc<AtomicBool>) -> Self {
        Self { inner, cancelled }
    }

    /// Returns a clone of the cancel flag.
    ///
    /// The returned `Arc<AtomicBool>` can be sent to another thread or stored
    /// in an FFI handle. Setting it to `true` (with any ordering) will cause
    /// the next resource check to abort execution.
    #[must_use]
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    /// Returns `true` if cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Requests cancellation of the running script.
    ///
    /// Equivalent to `cancel_flag().store(true, Relaxed)` but slightly more
    /// convenient when you have direct access to the tracker.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Resets the cancel flag to `false`.
    ///
    /// Call this before re-using the tracker for a new execution run.
    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::Relaxed);
    }

    /// Returns a shared reference to the inner tracker.
    #[must_use]
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// Returns a mutable reference to the inner tracker.
    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Checks the cancel flag and returns an error if set.
    #[inline]
    fn check_cancelled(&self) -> Result<(), ResourceError> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(ResourceError::Exception(MontyException::new_full(
                ExcType::KeyboardInterrupt,
                Some("Script execution cancelled".to_string()),
                vec![],
            )))
        } else {
            Ok(())
        }
    }
}

impl<T: ResourceTracker> ResourceTracker for CancellableTracker<T> {
    #[inline]
    fn on_allocate(&mut self, get_size: impl FnOnce() -> usize) -> Result<(), ResourceError> {
        self.check_cancelled()?;
        self.inner.on_allocate(get_size)
    }

    #[inline]
    fn on_free(&mut self, get_size: impl FnOnce() -> usize) {
        self.inner.on_free(get_size);
    }

    #[inline]
    fn check_time(&self) -> Result<(), ResourceError> {
        self.check_cancelled()?;
        self.inner.check_time()
    }

    #[inline]
    fn check_recursion_depth(&self, current_depth: usize) -> Result<(), ResourceError> {
        self.inner.check_recursion_depth(current_depth)
    }

    #[inline]
    fn check_large_result(&self, estimated_bytes: usize) -> Result<(), ResourceError> {
        self.inner.check_large_result(estimated_bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::resource::{LimitedTracker, NoLimitTracker, ResourceLimits};

    #[test]
    fn uncancelled_delegates_to_inner() {
        let mut tracker = CancellableTracker::new(NoLimitTracker);
        assert!(tracker.on_allocate(|| 100).is_ok());
        assert!(tracker.check_time().is_ok());
        assert!(tracker.check_recursion_depth(5).is_ok());
        assert!(tracker.check_large_result(1_000_000).is_ok());
    }

    #[test]
    fn cancel_flag_stops_check_time() {
        let tracker = CancellableTracker::new(NoLimitTracker);
        let flag = tracker.cancel_flag();

        assert!(tracker.check_time().is_ok());

        flag.store(true, Ordering::Relaxed);

        let err = tracker.check_time().unwrap_err();
        assert!(matches!(err, ResourceError::Exception(ref e) if e.exc_type() == ExcType::KeyboardInterrupt));
    }

    #[test]
    fn cancel_flag_stops_on_allocate() {
        let mut tracker = CancellableTracker::new(NoLimitTracker);
        let flag = tracker.cancel_flag();

        flag.store(true, Ordering::Relaxed);

        let err = tracker.on_allocate(|| 64).unwrap_err();
        assert!(matches!(err, ResourceError::Exception(ref e) if e.exc_type() == ExcType::KeyboardInterrupt));
    }

    #[test]
    fn reset_clears_cancel() {
        let mut tracker = CancellableTracker::new(NoLimitTracker);

        tracker.cancel();
        assert!(tracker.is_cancelled());
        assert!(tracker.check_time().is_err());

        tracker.reset();
        assert!(!tracker.is_cancelled());
        assert!(tracker.check_time().is_ok());
        assert!(tracker.on_allocate(|| 64).is_ok());
    }

    #[test]
    fn with_flag_uses_external_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let tracker = CancellableTracker::with_flag(NoLimitTracker, Arc::clone(&flag));

        assert!(tracker.check_time().is_ok());

        flag.store(true, Ordering::Relaxed);
        assert!(tracker.check_time().is_err());
    }

    #[test]
    fn inner_limits_still_enforced() {
        let limits = ResourceLimits::new().max_allocations(2);
        let mut tracker = CancellableTracker::new(LimitedTracker::new(limits));

        assert!(tracker.on_allocate(|| 10).is_ok());
        assert!(tracker.on_allocate(|| 10).is_ok());

        // Third allocation should hit the inner tracker's limit, not the cancel flag
        let err = tracker.on_allocate(|| 10).unwrap_err();
        assert!(matches!(err, ResourceError::Allocation { limit: 2, .. }));
    }

    #[test]
    fn cancel_takes_priority_over_inner_limits() {
        let limits = ResourceLimits::new().max_allocations(100);
        let mut tracker = CancellableTracker::new(LimitedTracker::new(limits));

        tracker.cancel();

        // Cancel flag is checked first, so we get a cancellation error
        // even though the allocation limit hasn't been reached
        let err = tracker.on_allocate(|| 10).unwrap_err();
        assert!(matches!(err, ResourceError::Exception(ref e) if e.exc_type() == ExcType::KeyboardInterrupt));
    }

    #[test]
    fn on_free_delegates() {
        let limits = ResourceLimits::new().max_memory(1000);
        let mut tracker = CancellableTracker::new(LimitedTracker::new(limits));

        tracker.on_allocate(|| 500).unwrap();
        assert_eq!(tracker.inner().current_memory(), 500);

        tracker.on_free(|| 200);
        assert_eq!(tracker.inner().current_memory(), 300);
    }

    #[test]
    fn debug_impl() {
        let tracker = CancellableTracker::new(NoLimitTracker);
        let debug_str = format!("{tracker:?}");
        assert!(debug_str.contains("CancellableTracker"));
        assert!(debug_str.contains("cancelled: false"));
    }

    #[test]
    fn error_message_content() {
        let tracker = CancellableTracker::new(NoLimitTracker);
        tracker.cancel();

        let err = tracker.check_time().unwrap_err();
        let msg = format!("{err}");
        assert_eq!(msg, "KeyboardInterrupt: Script execution cancelled");
    }
}

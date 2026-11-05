# Platform support requirements

The crate's no-`std` registration path depends on platform-local storage with
correct teardown. The current implementation documents Unix thread-local
storage and Windows fiber-local storage; these have different lifetimes and
must keep the registration lease valid until its guards are gone.

Before adding a target, establish that a real caller needs it and define the
supported target and feature combinations. Then specify the storage primitive,
its thread or fiber lifetime, destructor/teardown ordering, and how a guard
that outlives a lease remains safe. Review the implementation against those
requirements and update the target-specific dependencies and coverage plan.

An unknown target must not be treated as single-threaded: separate threads
could then share registration state and invalidate reclamation safety. Do not
add a generic fallback without a documented, caller-backed design.

No additional platform is currently identified by the repository evidence.

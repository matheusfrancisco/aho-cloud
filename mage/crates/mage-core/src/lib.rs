//! ahomage core: pure domain, ports shared by more than one role, wire DTOs, fakes.
//! No I/O. See `mage/docs/internals/{README,domain}.md`.

pub mod domain;
pub mod ports;
pub mod wire;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

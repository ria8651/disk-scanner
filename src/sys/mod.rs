//! macOS-specific layer. Everything that touches a syscall lives here so the
//! model and rollup above it stay portable.

pub mod attrlist;
pub mod ffi;
pub mod iopolicy;
pub mod mounts;
pub mod tcc;

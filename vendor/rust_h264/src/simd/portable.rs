//! Portable SIMD backend placeholders.
//!
//! Phase A keeps these symbols wired to the scalar reference implementation so
//! enabling `portable-simd` validates dispatch and feature plumbing without
//! changing decoded output. Phase B replaces selected exports with `core::simd`
//! kernels.

pub use crate::simd::scalar::*;

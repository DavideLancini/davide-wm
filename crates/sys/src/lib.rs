//! Hand-written bindings for the few C libraries davide-wm links against.
//! Only the functions actually used are declared.

#![allow(non_camel_case_types, clippy::missing_safety_doc)]

pub mod egl;
pub mod gbm;
pub mod drm;
pub mod gl;
pub mod input;
pub mod xkb;

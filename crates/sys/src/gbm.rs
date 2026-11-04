use std::ffi::c_void;

pub type gbm_device = c_void;
pub type gbm_bo = c_void;

pub const FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");

pub const BO_USE_SCANOUT: u32 = 1 << 0;
pub const BO_USE_RENDERING: u32 = 1 << 2;
pub const BO_USE_LINEAR: u32 = 1 << 4;

#[link(name = "gbm")]
extern "C" {
    pub fn gbm_create_device(fd: i32) -> *mut gbm_device;
    pub fn gbm_device_destroy(dev: *mut gbm_device);
    pub fn gbm_bo_create(dev: *mut gbm_device, width: u32, height: u32, format: u32, flags: u32) -> *mut gbm_bo;
    pub fn gbm_bo_destroy(bo: *mut gbm_bo);
    pub fn gbm_bo_get_fd(bo: *mut gbm_bo) -> i32;
    pub fn gbm_bo_get_stride(bo: *mut gbm_bo) -> u32;
    pub fn gbm_bo_get_offset(bo: *mut gbm_bo, plane: i32) -> u32;
    pub fn gbm_bo_get_modifier(bo: *mut gbm_bo) -> u64;
}

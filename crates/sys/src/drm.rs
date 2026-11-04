//! Kernel DRM/KMS uapi (`drm.h`, `drm_mode.h`): the ioctls used for atomic
//! modesetting, called directly without libdrm.

use std::io;
use std::os::fd::RawFd;

const fn iowr(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | nr
}

const fn iow(nr: u64, size: usize) -> u64 {
    (1 << 30) | ((size as u64) << 16) | ((b'd' as u64) << 8) | nr
}

#[repr(C)]
#[derive(Default)]
pub struct SetClientCap {
    pub capability: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct CardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct GetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub kind: u32,
    pub name: [u8; 32],
}

impl ModeInfo {
    /// Refresh rate in mHz, computed from the timings.
    pub fn refresh_mhz(&self) -> u32 {
        let (htotal, vtotal) = (self.htotal as u64, self.vtotal as u64);
        if htotal == 0 || vtotal == 0 {
            return self.vrefresh * 1000;
        }
        ((self.clock as u64 * 1_000_000 / htotal + vtotal / 2) / vtotal) as u32
    }
}

pub const MODE_TYPE_PREFERRED: u32 = 1 << 3;

#[repr(C)]
#[derive(Default)]
pub struct GetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct GetPlaneRes {
    pub plane_id_ptr: u64,
    pub count_planes: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct GetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct ObjGetProperties {
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct GetProperty {
    pub values_ptr: u64,
    pub enum_blob_ptr: u64,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [u8; 32],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct CreateBlob {
    pub data: u64,
    pub length: u32,
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct DestroyBlob {
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct FbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

#[repr(C)]
#[derive(Default)]
pub struct Atomic {
    pub flags: u32,
    pub count_objs: u32,
    pub objs_ptr: u64,
    pub count_props_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub reserved: u64,
    pub user_data: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct GetCap {
    pub capability: u64,
    pub value: u64,
}

pub const IOCTL_GET_CAP: u64 = iowr(0x0c, std::mem::size_of::<GetCap>());
pub const IOCTL_SET_CLIENT_CAP: u64 = iow(0x0d, std::mem::size_of::<SetClientCap>());
pub const IOCTL_MODE_GETRESOURCES: u64 = iowr(0xA0, std::mem::size_of::<CardRes>());
pub const IOCTL_MODE_GETENCODER: u64 = iowr(0xA6, std::mem::size_of::<GetEncoder>());
pub const IOCTL_MODE_GETCONNECTOR: u64 = iowr(0xA7, std::mem::size_of::<GetConnector>());
pub const IOCTL_MODE_GETPROPERTY: u64 = iowr(0xAA, std::mem::size_of::<GetProperty>());
pub const IOCTL_MODE_RMFB: u64 = iowr(0xAF, std::mem::size_of::<u32>());
pub const IOCTL_MODE_GETPLANERESOURCES: u64 = iowr(0xB5, std::mem::size_of::<GetPlaneRes>());
pub const IOCTL_MODE_GETPLANE: u64 = iowr(0xB6, std::mem::size_of::<GetPlane>());
pub const IOCTL_MODE_ADDFB2: u64 = iowr(0xB8, std::mem::size_of::<FbCmd2>());
pub const IOCTL_MODE_OBJ_GETPROPERTIES: u64 = iowr(0xB9, std::mem::size_of::<ObjGetProperties>());
pub const IOCTL_MODE_ATOMIC: u64 = iowr(0xBC, std::mem::size_of::<Atomic>());
pub const IOCTL_MODE_CREATEPROPBLOB: u64 = iowr(0xBD, std::mem::size_of::<CreateBlob>());
pub const IOCTL_MODE_DESTROYPROPBLOB: u64 = iowr(0xBE, std::mem::size_of::<DestroyBlob>());

pub const CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const CLIENT_CAP_ATOMIC: u64 = 3;
pub const CAP_ADDFB2_MODIFIERS: u64 = 0x10;

pub const MODE_CONNECTED: u32 = 1;

pub const OBJECT_CRTC: u32 = 0xcccccccc;
pub const OBJECT_CONNECTOR: u32 = 0xc0c0c0c0;
pub const OBJECT_PLANE: u32 = 0xeeeeeeee;

pub const PLANE_TYPE_PRIMARY: u64 = 1;

pub const MODE_FB_MODIFIERS: u32 = 1 << 1;

pub const MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub const MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
pub const MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;

pub const EVENT_FLIP_COMPLETE: u32 = 0x02;

#[repr(C)]
#[derive(Default)]
pub struct EventVblank {
    pub kind: u32,
    pub length: u32,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
}

/// Connector type names as the kernel and other compositors print them.
pub fn connector_type_name(t: u32) -> &'static str {
    match t {
        1 => "VGA",
        2 => "DVI-I",
        3 => "DVI-D",
        4 => "DVI-A",
        5 => "Composite",
        6 => "SVIDEO",
        7 => "LVDS",
        8 => "Component",
        9 => "DIN",
        10 => "DP",
        11 => "HDMI-A",
        12 => "HDMI-B",
        13 => "TV",
        14 => "eDP",
        15 => "Virtual",
        16 => "DSI",
        17 => "DPI",
        18 => "Writeback",
        19 => "SPI",
        20 => "USB",
        _ => "Unknown",
    }
}

/// `ioctl` retrying on EINTR/EAGAIN.
pub fn ioctl<T>(fd: RawFd, request: u64, arg: &mut T) -> io::Result<()> {
    loop {
        let r = unsafe { libc::ioctl(fd, request as _, arg as *mut T) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
            _ => return Err(e),
        }
    }
}

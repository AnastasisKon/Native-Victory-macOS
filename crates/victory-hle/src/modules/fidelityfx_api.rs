//! The FidelityFX SDK's descriptions as the title passes them: type ids, return codes, the
//! upscaler's context and dispatch, and the console and host backend descriptions.
//!
//! No Windows and no GPU in here, so both providers of the same API can share them: the PC build's
//! (`super::fidelityfx`, Windows only) and MetalFX's (`super::metalfx_ffx`, on a native host).

#![allow(dead_code)]

use std::ffi::c_void;
use std::mem::size_of;

// Return codes.
pub(crate) const OK: u32 = 0;
pub(crate) const UNKNOWN_TYPE: u32 = 2;
pub(crate) const NO_PROVIDER: u32 = 4;
pub(crate) const PARAMETER: u32 = 6;

// Description types.
pub(crate) const CREATE_UPSCALE: u64 = 0x1_0000;
pub(crate) const DISPATCH_UPSCALE: u64 = 0x1_0001;
pub(crate) const CONSOLE_BACKEND: u64 = 0x100_0002;
pub(crate) const HOST_BACKEND: u64 = 2;
pub(crate) const GET_VERSIONS: u64 = 4;
pub(crate) const GET_PROVIDER_VERSION: u64 = 6;
pub(crate) const MEMORY_USAGE_WITH_DEVICE: u64 = 0x1_0009;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Header {
    pub(crate) kind: u64,
    pub(crate) next: *mut Header,
}

/// An upscaler context: flags, largest render and output sizes, a message
/// callback.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CreateUpscale {
    pub(crate) header: Header,
    pub(crate) flags: u32,
    pub(crate) max_render: [u32; 2],
    pub(crate) max_output: [u32; 2],
    pub(crate) message: usize,
}

/// The backend description: a device.
#[repr(C)]
pub(crate) struct Backend {
    pub(crate) header: Header,
    pub(crate) device: *mut c_void,
}

/// A resource, its description (type, format, sizes, mips, flags, usage)
/// and the state it is in.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Resource {
    pub(crate) resource: *mut c_void,
    pub(crate) description: [u32; 8],
    pub(crate) state: u32,
}

/// One upscaling: the command list, then colour, depth, motion vectors,
/// exposure, reactive mask, transparency mask and output; then jitter,
/// motion-vector scale, sizes, sharpening, frame time, pre-exposure, reset,
/// camera and flags, which pass through.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct DispatchUpscale {
    pub(crate) header: Header,
    pub(crate) list: *mut c_void,
    pub(crate) resources: [Resource; 7],
    pub(crate) rest: [u32; 18],
}

const _: () = assert!(size_of::<CreateUpscale>() == 0x30);
const _: () = assert!(size_of::<Resource>() == 0x30);
const _: () = assert!(size_of::<DispatchUpscale>() == 0x1b0);

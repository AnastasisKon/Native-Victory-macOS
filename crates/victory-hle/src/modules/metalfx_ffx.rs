//! `amd_fidelityfx_loader_xbox.dll` served by MetalFX on a native host: the title's own upscaler
//! calls answer here instead of loading its FidelityFX DLLs, and each upscale becomes a MetalFX
//! pass on the title's own command list (`victory_gpu::native::metalfx_upscale`).
//!
//! Off unless asked for (`VICTORY_METALFX=upscale`), and only where the system has MetalFX: with
//! neither, this module stays out of the registry and the title's own DLLs run as before. Nothing
//! else of FidelityFX's is served — another effect is refused rather than half-done.
//!
//! Which of MetalFX's two scalers a frame gets is decided by what the dispatch carries: colour,
//! depth and motion vectors make a temporal upscale, colour alone a spatial one. The frame's
//! jitter, its motion-vector scale and its reset come from FSR's dispatch, which is where the
//! title states them.

use std::ffi::c_void;
use std::ptr::{null_mut, null};
use std::sync::OnceLock;

use super::fidelityfx_api::*;
use crate::{HleModule, hle_export};
use victory_gpu::object::Object;

/// Whether MetalFX serves the title's upscaler: `VICTORY_METALFX=upscale` on a host whose system
/// has MetalFX. Read once.
pub fn serving() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if !std::env::var("VICTORY_METALFX").is_ok_and(|v| v == "upscale") {
            return false;
        }
        if victory_gpu::native::metalfx_available() {
            tracing::info!(
                "FidelityFX: MetalFX serves the title's upscaler (VICTORY_METALFX=upscale); its own \
                 FidelityFX DLLs do not run"
            );
            true
        } else {
            tracing::warn!(
                "FidelityFX: VICTORY_METALFX=upscale, but this system has no MetalFX upscaler: the \
                 title's own FidelityFX DLLs run"
            );
            false
        }
    })
}

/// One upscaler context: the sizes the title said it would scale between, and how many frames it
/// has dispatched.
#[repr(C)]
struct Context {
    max_render: [u32; 2],
    max_output: [u32; 2],
    frames: u64,
}

/// The `Context` a context pointer names, when it names one of ours.
///
/// # Safety
/// `context` must be a context pointer this module handed out, or null.
unsafe fn context_of(context: *mut *mut c_void) -> Option<*mut Context> {
    if context.is_null() {
        return None;
    }
    // SAFETY: the title's context pointer, by the caller's promise.
    let object = unsafe { *context };
    (!object.is_null()).then(|| object.cast::<Context>())
}

extern "C" fn ffxCreateContext(
    context: *mut *mut c_void,
    desc: *mut Header,
    _memory: *const c_void,
) -> u32 {
    if context.is_null() || desc.is_null() {
        return PARAMETER;
    }
    // SAFETY: the title passes a chain of descriptions.
    let kind = unsafe { (*desc).kind };
    if kind != CREATE_UPSCALE {
        tracing::warn!(
            kind = format_args!("{kind:#x}"),
            "FidelityFX: MetalFX serves the upscaler only; another effect was refused"
        );
        return NO_PROVIDER;
    }
    // SAFETY: an upscaler description, by its type.
    let upscale = unsafe { desc.cast::<CreateUpscale>().read() };
    let object = Box::into_raw(Box::new(Context {
        max_render: upscale.max_render,
        max_output: upscale.max_output,
        frames: 0,
    }));
    // SAFETY: the title's context pointer, which it starts null.
    unsafe { *context = object.cast() };
    tracing::info!(
        render = ?upscale.max_render,
        output = ?upscale.max_output,
        flags = format_args!("{:#x}", upscale.flags),
        "FidelityFX: upscaler context on MetalFX"
    );
    OK
}

extern "C" fn ffxDestroyContext(context: *mut *mut c_void, _memory: *const c_void) -> u32 {
    // SAFETY: a context this module made, which the title is done with.
    if let Some(object) = unsafe { context_of(context) } {
        // SAFETY: the box `ffxCreateContext` leaked, with the same layout.
        drop(unsafe { Box::from_raw(object) });
        // SAFETY: the title's context pointer, which it keeps until the next call.
        unsafe { *context = null_mut() };
    }
    OK
}

/// Nothing of FSR's is configurable here: the sizes come with the context and the rest comes with
/// each frame.
extern "C" fn ffxConfigure(_context: *mut *mut c_void, desc: *mut Header) -> u32 {
    if desc.is_null() { PARAMETER } else { OK }
}

/// No description of FSR's has an answer here (MetalFX works its memory out itself), so a query is
/// refused rather than left unfilled.
extern "C" fn ffxQuery(_context: *mut *mut c_void, desc: *mut Header) -> u32 {
    if desc.is_null() { PARAMETER } else { UNKNOWN_TYPE }
}

extern "C" fn ffxDispatch(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    if desc.is_null() {
        return PARAMETER;
    }
    // SAFETY: the title passes a description.
    let kind = unsafe { (*desc).kind };
    if kind != DISPATCH_UPSCALE {
        tracing::warn!(
            kind = format_args!("{kind:#x}"),
            "FidelityFX: MetalFX serves upscaling only; a dispatch was refused"
        );
        return UNKNOWN_TYPE;
    }
    // SAFETY: a context `ffxCreateContext` made.
    let Some(context) = (unsafe { context_of(context) }) else {
        return PARAMETER;
    };
    // SAFETY: an upscale dispatch, by its type.
    let dispatch = unsafe { desc.cast::<DispatchUpscale>().read() };
    // `rest` is `jitterOffset`, `motionVectorScale`, `renderSize`, `upscaleSize`,
    // `enableSharpening`, `sharpness`, `frameTimeDelta`, `preExposure`, `reset`, the camera's four
    // values and `flags`.
    let jitter = [
        f32::from_bits(dispatch.rest[0]),
        f32::from_bits(dispatch.rest[1]),
    ];
    let motion_scale = [
        f32::from_bits(dispatch.rest[2]),
        f32::from_bits(dispatch.rest[3]),
    ];
    let reset = dispatch.rest[12] != 0;
    let resource = |at: usize| dispatch.resources[at].resource.cast::<Object>();
    // SAFETY: the context `ffxCreateContext` made; only this call reads and writes it.
    let frames = unsafe { (*context).frames };
    let recorded = victory_gpu::native::metalfx_upscale(
        dispatch.list.cast::<Object>(),
        resource(0),
        resource(1),
        resource(2),
        resource(6),
        jitter,
        motion_scale,
        reset || frames == 0,
    );
    if !recorded {
        tracing::warn!(
            "FidelityFX: MetalFX could not take this frame's resources; the upscale was not \
             recorded and the title's own DLLs are not running"
        );
        return PARAMETER;
    }
    // SAFETY: as above.
    unsafe { (*context).frames = frames + 1 };
    OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "metalfx",
        dlls: &["amd_fidelityfx_loader_xbox.dll"],
        exports: vec![
            hle_export!(ffxConfigure),
            hle_export!(ffxCreateContext),
            hle_export!(ffxDestroyContext),
            hle_export!(ffxDispatch),
            hle_export!(ffxQuery),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u64, next: *mut Header) -> Header {
        Header { kind, next }
    }

    /// The upscaler's own description makes a context, and that context is the one handed back.
    #[test]
    fn a_context_round_trips() {
        let mut version = header(0x1_000b, null_mut());
        let mut console = Backend {
            header: header(CONSOLE_BACKEND, &raw mut version),
            device: null_mut(),
        };
        let mut upscale = CreateUpscale {
            header: header(CREATE_UPSCALE, &raw mut console.header),
            flags: 0x29,
            max_render: [1280, 720],
            max_output: [1920, 1080],
            message: 0,
        };
        let mut context: *mut c_void = null_mut();
        assert_eq!(ffxCreateContext(&mut context, &raw mut upscale.header, null()), OK);
        assert!(!context.is_null(), "no context was handed back");
        // SAFETY: the context this test just made.
        let made = unsafe { context_of(&mut context) }.expect("a context of ours");
        // SAFETY: the same context; nothing else holds it.
        assert_eq!(unsafe { (*made).frames }, 0, "a fresh context has dispatched nothing");
        // SAFETY: the same context, through the destroy the title calls.
        assert_eq!(ffxDestroyContext(&mut context, null()), OK);
        assert!(context.is_null(), "the context outlived its destroy");
    }

    /// Another effect is refused rather than half-served.
    #[test]
    fn another_effect_is_refused() {
        let mut other = header(0x1_00ff, null_mut());
        let mut context: *mut c_void = null_mut();
        assert_eq!(ffxCreateContext(&mut context, &raw mut other, null()), NO_PROVIDER);
        assert!(context.is_null(), "a refused context was handed back");
    }

    /// A dispatch of another effect is refused too.
    #[test]
    fn another_dispatch_is_refused() {
        let mut other = header(0x1_00ff, null_mut());
        assert_eq!(ffxDispatch(null_mut(), &raw mut other), UNKNOWN_TYPE);
    }

    /// Off unless `VICTORY_METALFX=upscale` names it.
    #[test]
    fn serving_only_when_asked() {
        if std::env::var("VICTORY_METALFX").is_ok() {
            return;
        }
        assert!(!serving(), "MetalFX took the title's upscaler over unasked");
    }
}

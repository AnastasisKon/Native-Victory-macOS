//! MetalFX (`MetalFX.framework`): the spatial upscaler, and what the rest of it would need.
//!
//! The framework is not linked and its classes are not in the Metal bindings this crate uses
//! (`objc2-metal`), so it is opened with `dlopen` and driven by message sends.
//!
//! What each of the three needs, against what this backend has:
//!
//! - **Spatial** ([`Spatial`]): a colour texture and an output texture. The present path has both
//!   (the title's presented plane and a texture the size of the drawable), so
//!   `VICTORY_METALFX=spatial` scales the plane up with MetalFX instead of taking it with the
//!   full-screen triangle's bilinear sample.
//! - **Temporal**: add depth, motion vectors, a jitter offset per frame and a reset flag.
//! - **Frame interpolator** (frame generation): add the previous frame's colour and depth, and an
//!   optional UI texture, which is composited without interpolation (interpolating a HUD smears
//!   it).
//!
//! The title's depth and motion-vector passes are not identified by this backend: nothing here
//! knows which of its textures holds either, so the temporal scaler and the interpolator cannot be
//! driven yet. [`temporal`] and [`interpolator`] say whether the system has them; that is all they
//! do, so that a later pass can build on the same loading and message-send code.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2_metal::{MTLCommandBuffer, MTLDevice, MTLPixelFormat, MTLTexture};

/// Opens `MetalFX.framework`, once. False where the system has no such framework.
fn load() -> bool {
    static LOADED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *LOADED.get_or_init(|| {
        let path = c"/System/Library/Frameworks/MetalFX.framework/MetalFX";
        // SAFETY: a constant path; the handle stays open for the life of the process, so every
        // class handed out below stays valid.
        !unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW) }.is_null()
    })
}

/// A class of the framework, or `None` when the system does not have it.
fn class(name: &'static std::ffi::CStr) -> Option<&'static AnyClass> {
    load().then(|| AnyClass::get(name)).flatten()
}

/// Whether the system has MetalFX's spatial upscaler.
pub fn spatial() -> bool {
    class(c"MTLFXSpatialScalerDescriptor").is_some()
}

/// Whether the system has MetalFX's temporal upscaler. Its inputs (depth, motion vectors, jitter)
/// are not available to this backend yet; see the module's documentation.
pub fn temporal() -> bool {
    class(c"MTLFXTemporalScalerDescriptor").is_some()
}

/// Whether the system has MetalFX's frame interpolator (frame generation). Its inputs are not
/// available to this backend yet; see the module's documentation.
pub fn interpolator() -> bool {
    class(c"MTLFXFrameInterpolatorDescriptor").is_some()
}

/// One MetalFX spatial upscaler, for input and output sizes and formats that do not change.
pub struct Spatial {
    object: Retained<AnyObject>,
}

impl Spatial {
    /// A scaler from `input` to `output`, or `None` where the system has no MetalFX spatial
    /// upscaler. `Err` is the system's own reason (an unsupported format, say).
    ///
    /// The descriptor takes the two formats and the two sizes and nothing else (its properties,
    /// as the runtime lists them); the output texture must be one the present pass can read, which
    /// is the caller's.
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        input: (usize, usize),
        output: (usize, usize),
        color: MTLPixelFormat,
        out: MTLPixelFormat,
    ) -> Result<Option<Self>, String> {
        let Some(descriptor) = class(c"MTLFXSpatialScalerDescriptor") else {
            return Ok(None);
        };
        // SAFETY: `new` hands over a descriptor this code owns.
        let fresh: *mut AnyObject = unsafe { msg_send![descriptor, new] };
        // SAFETY: `+1` from a `new` method.
        let Some(fresh) = (unsafe { Retained::from_raw(fresh) }) else {
            return Ok(None);
        };
        // SAFETY: a descriptor of the class the messages below belong to; the sizes are the ones
        // MetalFX documents, and the formats are the textures' own.
        unsafe {
            let _: () = msg_send![&*fresh, setInputWidth: input.0];
            let _: () = msg_send![&*fresh, setInputHeight: input.1];
            let _: () = msg_send![&*fresh, setOutputWidth: output.0];
            let _: () = msg_send![&*fresh, setOutputHeight: output.1];
            let _: () = msg_send![&*fresh, setColorTextureFormat: color];
            let _: () = msg_send![&*fresh, setOutputTextureFormat: out];
            // 0: perceptual. The title's swap chain holds display values, not linear light.
            let _: () = msg_send![&*fresh, setColorProcessingMode: 0usize];
        }
        // SAFETY: the class method takes the device and returns an object this code owns, or nil.
        let scaler: *mut AnyObject = unsafe {
            msg_send![&*fresh, newSpatialScalerWithDevice: device]
        };
        // SAFETY: `+1` from a `new` method.
        let Some(object) = (unsafe { Retained::from_raw(scaler) }) else {
            return Err("MetalFX made no spatial scaler for these sizes and formats".into());
        };
        Ok(Some(Self { object }))
    }

    /// Scales `color` into `out` on `cb`. Both must match the sizes and formats the scaler was
    /// made for.
    pub fn encode(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        color: &ProtocolObject<dyn MTLTexture>,
        out: &ProtocolObject<dyn MTLTexture>,
    ) {
        // SAFETY: the scaler's own messages, with textures of the sizes and formats it was made
        // for; the command buffer is the caller's.
        unsafe {
            let _: () = msg_send![&*self.object, setColorTexture: color];
            let _: () = msg_send![&*self.object, setOutputTexture: out];
            let _: () = msg_send![&*self.object, encodeToCommandBuffer: cb];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{interpolator, spatial, temporal, Spatial};
    use crate::metal::Metal;
    use objc2::runtime::ProtocolObject;
    use objc2_metal::{
        MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandQueue, MTLDevice, MTLPixelFormat,
        MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
    };

    fn texture(
        m: &Metal,
        width: usize,
        height: usize,
    ) -> objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> {
        // SAFETY: a fresh descriptor; Metal validates the sizes at creation.
        let d = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                width,
                height,
                false,
            )
        };
        d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
        d.setStorageMode(MTLStorageMode::Private);
        m.dev.newTextureWithDescriptor(&d).unwrap()
    }

    /// The system's MetalFX, and how much of it there is: spatial everywhere it exists, the other
    /// two only where the OS has them (they need inputs this backend does not have yet).
    #[test]
    fn metalfx_says_which_scalers_the_system_has() {
        let m = Metal::new().unwrap();
        let (s, t, i) = (spatial(), temporal(), interpolator());
        println!("MetalFX: spatial={s} temporal={t} interpolator={i}");
        assert!(s || !t, "a temporal scaler without a spatial one");
        let _ = m;
    }

    /// A scaler runs on the GPU: 64x64 into 128x128, both BGRA8, and the command buffer does not
    /// fail. Skipped where the system has no MetalFX.
    #[test]
    fn a_spatial_scale_runs_on_the_gpu() {
        if !spatial() {
            return;
        }
        let m = Metal::new().unwrap();
        let src = texture(&m, 64, 64);
        let dst = texture(&m, 128, 128);
        let Some(scaler) = Spatial::new(
            &m.dev,
            (64, 64),
            (128, 128),
            MTLPixelFormat::BGRA8Unorm,
            MTLPixelFormat::BGRA8Unorm,
        )
        .unwrap() else {
            return;
        };
        let q = m.dev.newCommandQueue().unwrap();
        let cb = q.commandBuffer().unwrap();
        scaler.encode(&cb, &src, &dst);
        cb.commit();
        cb.waitUntilCompleted();
        assert_eq!(
            cb.status(),
            MTLCommandBufferStatus::Completed,
            "the scale did not complete"
        );
    }
}

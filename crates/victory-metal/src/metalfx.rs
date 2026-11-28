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
//! knows which of its textures holds either, so [`Temporal`] and [`Interpolator`] have no caller
//! yet. Both are wrapped and both have a test that runs one on this machine's GPU and waits for it:
//! what is missing for them is the title's depth and motion vectors, not the MetalFX side. Once a
//! pass does identify those textures, it sets them on one of these and encodes.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject};
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

/// Whether the system has MetalFX's temporal upscaler ([`Temporal`]). Its inputs (depth, motion
/// vectors, jitter) are not available to this backend yet; see the module's documentation.
pub fn temporal() -> bool {
    class(c"MTLFXTemporalScalerDescriptor").is_some()
}

/// Whether the system has MetalFX's frame interpolator ([`Interpolator`], frame generation). Its
/// inputs are not available to this backend yet; see the module's documentation.
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

/// One MetalFX temporal upscaler: the spatial one, plus this frame's depth and motion vectors and a
/// reset flag, which is what lets it accumulate across frames instead of scaling each on its own.
pub struct Temporal {
    object: Retained<AnyObject>,
}

impl Temporal {
    /// A temporal scaler from `input` to `output`, or `None` where the system has no MetalFX
    /// temporal upscaler. `Err` is the system's own reason (an unsupported format, say).
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        input: (usize, usize),
        output: (usize, usize),
        color: MTLPixelFormat,
        depth: MTLPixelFormat,
        motion: MTLPixelFormat,
        out: MTLPixelFormat,
    ) -> Result<Option<Self>, String> {
        let Some(descriptor) = class(c"MTLFXTemporalScalerDescriptor") else {
            return Ok(None);
        };
        // SAFETY: `new` hands over a descriptor this code owns.
        let fresh: *mut AnyObject = unsafe { msg_send![descriptor, new] };
        // SAFETY: `+1` from a `new` method.
        let Some(fresh) = (unsafe { Retained::from_raw(fresh) }) else {
            return Ok(None);
        };
        // SAFETY: the descriptor's own setters, with the sizes MetalFX documents and the formats
        // of the textures the caller will hand it.
        unsafe {
            let _: () = msg_send![&*fresh, setInputWidth: input.0];
            let _: () = msg_send![&*fresh, setInputHeight: input.1];
            let _: () = msg_send![&*fresh, setOutputWidth: output.0];
            let _: () = msg_send![&*fresh, setOutputHeight: output.1];
            let _: () = msg_send![&*fresh, setColorTextureFormat: color];
            let _: () = msg_send![&*fresh, setDepthTextureFormat: depth];
            let _: () = msg_send![&*fresh, setMotionTextureFormat: motion];
            let _: () = msg_send![&*fresh, setOutputTextureFormat: out];
            // Unlike the spatial descriptor, this one has no colour-processing mode: it takes the
            // values it is handed and accumulates there.
        }
        // SAFETY: the class method takes the device and returns an object this code owns, or nil.
        let scaler: *mut AnyObject =
            unsafe { msg_send![&*fresh, newTemporalScalerWithDevice: device] };
        // SAFETY: `+1` from a `new` method.
        let Some(object) = (unsafe { Retained::from_raw(scaler) }) else {
            return Err("MetalFX made no temporal scaler for these sizes and formats".into());
        };
        Ok(Some(Self { object }))
    }

    /// Scales `color` into `out` on `cb`, from this frame's `depth` and `motion` vectors. `reset`
    /// goes on the first frame and after a cut: history from before one would smear across it.
    ///
    /// The jitter offset is left at zero, and the motion vectors are taken to be in pixels. A
    /// caller whose vectors are in another unit, or that jitters its projection, sets
    /// `motionVectorScaleX`/`Y` and `jitterOffsetX`/`Y` itself; this is the plain per-frame path.
    pub fn encode(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        color: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
        out: &ProtocolObject<dyn MTLTexture>,
        reset: bool,
    ) {
        // SAFETY: the scaler's own messages, with textures of the sizes and formats it was made
        // for; the command buffer is the caller's.
        unsafe {
            let _: () = msg_send![&*self.object, setColorTexture: color];
            let _: () = msg_send![&*self.object, setDepthTexture: depth];
            let _: () = msg_send![&*self.object, setMotionTexture: motion];
            let _: () = msg_send![&*self.object, setOutputTexture: out];
            let _: () = msg_send![&*self.object, setReset: Bool::new(reset)];
            let _: () = msg_send![&*self.object, setMotionVectorScaleX: 1.0f32];
            let _: () = msg_send![&*self.object, setMotionVectorScaleY: 1.0f32];
            let _: () = msg_send![&*self.object, setJitterOffsetX: 0.0f32];
            let _: () = msg_send![&*self.object, setJitterOffsetY: 0.0f32];
            let _: () = msg_send![&*self.object, encodeToCommandBuffer: cb];
        }
    }
}

/// One MetalFX frame interpolator: it makes the frame between two submitted ones, from their colour
/// and depth and their motion vectors. A frame generated this way is drawn and never simulated.
pub struct Interpolator {
    object: Retained<AnyObject>,
}

impl Interpolator {
    /// An interpolator from `input` to `output`, or `None` where the system has no MetalFX frame
    /// interpolator. `Err` is the system's own reason.
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        input: (usize, usize),
        output: (usize, usize),
        color: MTLPixelFormat,
        depth: MTLPixelFormat,
        motion: MTLPixelFormat,
        out: MTLPixelFormat,
    ) -> Result<Option<Self>, String> {
        let Some(descriptor) = class(c"MTLFXFrameInterpolatorDescriptor") else {
            return Ok(None);
        };
        // SAFETY: `new` hands over a descriptor this code owns.
        let fresh: *mut AnyObject = unsafe { msg_send![descriptor, new] };
        // SAFETY: `+1` from a `new` method.
        let Some(fresh) = (unsafe { Retained::from_raw(fresh) }) else {
            return Ok(None);
        };
        // SAFETY: the descriptor's own setters. Unlike the scalers' descriptors, this one has no
        // colour-processing mode: interpolation is done in the values it is handed.
        unsafe {
            let _: () = msg_send![&*fresh, setInputWidth: input.0];
            let _: () = msg_send![&*fresh, setInputHeight: input.1];
            let _: () = msg_send![&*fresh, setOutputWidth: output.0];
            let _: () = msg_send![&*fresh, setOutputHeight: output.1];
            let _: () = msg_send![&*fresh, setColorTextureFormat: color];
            let _: () = msg_send![&*fresh, setDepthTextureFormat: depth];
            let _: () = msg_send![&*fresh, setMotionTextureFormat: motion];
            let _: () = msg_send![&*fresh, setOutputTextureFormat: out];
        }
        // SAFETY: the class method takes the device and returns an object this code owns, or nil.
        let made: *mut AnyObject =
            unsafe { msg_send![&*fresh, newFrameInterpolatorWithDevice: device] };
        // SAFETY: `+1` from a `new` method.
        let Some(object) = (unsafe { Retained::from_raw(made) }) else {
            return Err("MetalFX made no frame interpolator for these sizes and formats".into());
        };
        Ok(Some(Self { object }))
    }

    /// Writes the frame between `prev` and `color` into `out` on `cb`, `delta` seconds after the
    /// previous frame. `reset` goes on the first pair and after a cut.
    ///
    /// The UI texture is not set, so the generated frame interpolates everything in it, HUD
    /// included; a caller that has the HUD apart from the scene hands it to MetalFX instead.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        color: &ProtocolObject<dyn MTLTexture>,
        prev: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
        out: &ProtocolObject<dyn MTLTexture>,
        delta: f32,
        reset: bool,
    ) {
        // SAFETY: the interpolator's own messages, with textures of the sizes and formats it was
        // made for; the command buffer is the caller's.
        unsafe {
            let _: () = msg_send![&*self.object, setColorTexture: color];
            let _: () = msg_send![&*self.object, setPrevColorTexture: prev];
            let _: () = msg_send![&*self.object, setDepthTexture: depth];
            let _: () = msg_send![&*self.object, setMotionTexture: motion];
            let _: () = msg_send![&*self.object, setOutputTexture: out];
            let _: () = msg_send![&*self.object, setDeltaTime: delta];
            let _: () = msg_send![&*self.object, setReset: Bool::new(reset)];
            let _: () = msg_send![&*self.object, encodeToCommandBuffer: cb];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Interpolator, Spatial, Temporal, interpolator, spatial, temporal};
    use crate::metal::Metal;
    use objc2::runtime::ProtocolObject;
    use objc2_metal::{
        MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandQueue, MTLDevice, MTLPixelFormat,
        MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
    };

    fn texture_of(
        m: &Metal,
        width: usize,
        height: usize,
        format: MTLPixelFormat,
    ) -> objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> {
        // SAFETY: a fresh descriptor; Metal validates the sizes and the format at creation.
        let d = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format, width, height, false,
            )
        };
        d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
        d.setStorageMode(MTLStorageMode::Private);
        m.dev.newTextureWithDescriptor(&d).unwrap()
    }

    fn texture(
        m: &Metal,
        width: usize,
        height: usize,
    ) -> objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> {
        texture_of(m, width, height, MTLPixelFormat::BGRA8Unorm)
    }

    /// A motion-vector texture, the format MetalFX's descriptors ask for.
    fn motion(
        m: &Metal,
        width: usize,
        height: usize,
    ) -> objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> {
        texture_of(m, width, height, MTLPixelFormat::RGBA16Float)
    }

    fn depth(
        m: &Metal,
        width: usize,
        height: usize,
    ) -> objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> {
        texture_of(m, width, height, MTLPixelFormat::Depth32Float)
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

    /// The temporal scaler runs: 64x64 colour, depth and motion vectors into 128x128, reset on
    /// this its first frame, and the command buffer does not fail. Skipped where the system has no
    /// MetalFX temporal scaler.
    #[test]
    fn a_temporal_scale_runs_on_the_gpu() {
        if !temporal() {
            return;
        }
        let m = Metal::new().unwrap();
        let (src, src_depth, src_motion) =
            (texture(&m, 64, 64), depth(&m, 64, 64), motion(&m, 64, 64));
        let dst = texture(&m, 128, 128);
        let Some(scaler) = Temporal::new(
            &m.dev,
            (64, 64),
            (128, 128),
            MTLPixelFormat::BGRA8Unorm,
            MTLPixelFormat::Depth32Float,
            MTLPixelFormat::RGBA16Float,
            MTLPixelFormat::BGRA8Unorm,
        )
        .unwrap() else {
            return;
        };
        let q = m.dev.newCommandQueue().unwrap();
        let cb = q.commandBuffer().unwrap();
        scaler.encode(&cb, &src, &src_depth, &src_motion, &dst, true);
        cb.commit();
        cb.waitUntilCompleted();
        assert_eq!(
            cb.status(),
            MTLCommandBufferStatus::Completed,
            "the temporal scale did not complete"
        );
    }

    /// The frame interpolator runs: two 128x128 frames, their depth and motion vectors, into a
    /// third 128x128 frame 16 ms after the first, and the command buffer does not fail. Skipped
    /// where the system has no MetalFX frame interpolator.
    #[test]
    fn a_frame_is_generated_between_two_others() {
        if !interpolator() {
            return;
        }
        let m = Metal::new().unwrap();
        let (color, prev) = (texture(&m, 128, 128), texture(&m, 128, 128));
        let (d, mv) = (depth(&m, 128, 128), motion(&m, 128, 128));
        let out = texture(&m, 128, 128);
        let Some(interpolator) = Interpolator::new(
            &m.dev,
            (128, 128),
            (128, 128),
            MTLPixelFormat::BGRA8Unorm,
            MTLPixelFormat::Depth32Float,
            MTLPixelFormat::RGBA16Float,
            MTLPixelFormat::BGRA8Unorm,
        )
        .unwrap() else {
            return;
        };
        let q = m.dev.newCommandQueue().unwrap();
        let cb = q.commandBuffer().unwrap();
        interpolator.encode(&cb, &color, &prev, &d, &mv, &out, 0.016, true);
        cb.commit();
        cb.waitUntilCompleted();
        assert_eq!(
            cb.status(),
            MTLCommandBufferStatus::Completed,
            "the generated frame did not complete"
        );
    }
}

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
//! yet. Both are wrapped and both have a test that runs one on this machine's GPU and waits for it.
//! What they need is the frame's depth and motion vectors, which the title already names in its
//! FidelityFX dispatch (`victory_hle::modules::fidelityfx`), so no pass has to be identified first:
//! a caller hands them over through [`Upscale`], which is made in the mode its inputs allow.
//!
//! [`Upscale`] is that seam: the temporal scaler where the caller has depth and motion vectors and
//! the system has it, the spatial one everywhere else, so a caller does not have to know which
//! exists. `gfx::Texture::mtl` is what lets a caller outside this crate give it a title texture.

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

/// Which of MetalFX's two upscalers a caller has the inputs for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Colour only: every frame is scaled on its own.
    Spatial,
    /// Colour, depth and motion vectors: frames accumulate, which is what keeps a scaled image
    /// still in the parts of it that do not move.
    Temporal,
}

/// The formats of the textures an upscale reads and writes. One in [`Mode::Spatial`] has no depth
/// or motion texture, which [`Formats::spatial`] leaves at a value nothing reads.
#[derive(Clone, Copy)]
pub struct Formats {
    pub color: MTLPixelFormat,
    pub depth: MTLPixelFormat,
    pub motion: MTLPixelFormat,
    pub out: MTLPixelFormat,
}

impl Formats {
    /// The formats of a caller with colour only.
    pub fn spatial(color: MTLPixelFormat, out: MTLPixelFormat) -> Self {
        Self {
            color,
            depth: MTLPixelFormat::Invalid,
            motion: MTLPixelFormat::Invalid,
            out,
        }
    }
}

/// What a scaler is told about one frame besides its textures: where the frame's projection was
/// jittered, how its motion vectors are scaled, and whether the history must be dropped. A
/// temporal upscale accumulates across frames, so these are what keep it from smearing; a spatial
/// one reads none of them.
#[derive(Clone, Copy)]
pub struct View {
    /// In pixels of the input, the offset the frame was rendered with.
    pub jitter: [f32; 2],
    /// Multiplied with the motion vectors before they are used. The title's FidelityFX dispatch
    /// carries a scale of its own, and MetalFX takes the vectors in pixels of the render target.
    pub motion_scale: [f32; 2],
    /// Drop the history: the first frame, and after a cut or a teleport.
    pub reset: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            jitter: [0.0; 2],
            motion_scale: [1.0; 2],
            reset: true,
        }
    }
}

/// What one frame hands an [`Upscale`]: its colour, and, in [`Mode::Temporal`], its depth and its
/// motion vectors.
pub enum Inputs<'a> {
    ColorOnly,
    ColorDepthMotion {
        depth: &'a ProtocolObject<dyn MTLTexture>,
        motion: &'a ProtocolObject<dyn MTLTexture>,
    },
}

/// One MetalFX upscale of an input size into an output size: the spatial scaler, or the temporal
/// one when the caller has depth and motion vectors and the system has it. Unlike [`Spatial`] and
/// [`Temporal`], a caller does not have to know which of them it can use.
pub struct Upscale {
    which: Which,
    mode: Mode,
    input: (usize, usize),
    output: (usize, usize),
}

enum Which {
    Spatial(Spatial),
    Temporal(Temporal),
}

impl Upscale {
    /// An upscale in `asked`'s mode, or in [`Mode::Spatial`] where the system has no temporal
    /// scaler; `None` where it has no MetalFX at all. `Err` is the system's own reason (an
    /// unsupported format, say).
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        asked: Mode,
        input: (usize, usize),
        output: (usize, usize),
        formats: Formats,
    ) -> Result<Option<Self>, String> {
        let temporal = asked == Mode::Temporal && temporal();
        let (which, mode) = if temporal {
            match Temporal::new(
                device,
                input,
                output,
                formats.color,
                formats.depth,
                formats.motion,
                formats.out,
            )? {
                Some(t) => (Which::Temporal(t), Mode::Temporal),
                None => return Ok(None),
            }
        } else {
            match Spatial::new(device, input, output, formats.color, formats.out)? {
                Some(s) => (Which::Spatial(s), Mode::Spatial),
                None => return Ok(None),
            }
        };
        Ok(Some(Self {
            which,
            mode,
            input,
            output,
        }))
    }

    /// The mode it ended up in: the one asked for, or [`Mode::Spatial`] where the system had no
    /// temporal scaler.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The input and output sizes it was made for.
    pub fn sizes(&self) -> ((usize, usize), (usize, usize)) {
        (self.input, self.output)
    }

    /// Whether [`Self::encode`] would encode `inputs`: a temporal upscale needs the frame's depth
    /// and motion vectors, which is what makes it one.
    pub fn takes(&self, inputs: &Inputs<'_>) -> bool {
        match (self.mode, inputs) {
            (Mode::Spatial, _) | (Mode::Temporal, Inputs::ColorDepthMotion { .. }) => true,
            (Mode::Temporal, Inputs::ColorOnly) => false,
        }
    }

    /// Scales `color` into `out` on `cb`, from `inputs` and, in [`Mode::Temporal`], `reset` on the
    /// first frame and after a cut. False, having encoded nothing, when the mode is temporal and
    /// `inputs` has no depth and motion vectors: a caller keeps its own path rather than scale a
    /// frame the scaler cannot accumulate from.
    pub fn encode(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        color: &ProtocolObject<dyn MTLTexture>,
        inputs: Inputs<'_>,
        out: &ProtocolObject<dyn MTLTexture>,
        view: View,
    ) -> bool {
        match (&self.which, inputs) {
            (Which::Spatial(s), _) => {
                s.encode(cb, color, out);
                true
            }
            (Which::Temporal(t), Inputs::ColorDepthMotion { depth, motion }) => {
                t.encode(cb, color, depth, motion, out, view);
                true
            }
            (Which::Temporal(_), Inputs::ColorOnly) => false,
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

    /// Scales `color` into `out` on `cb`, from this frame's `depth` and `motion` vectors, with
    /// `view`'s jitter, motion-vector scale and reset.
    pub fn encode(
        &self,
        cb: &ProtocolObject<dyn MTLCommandBuffer>,
        color: &ProtocolObject<dyn MTLTexture>,
        depth: &ProtocolObject<dyn MTLTexture>,
        motion: &ProtocolObject<dyn MTLTexture>,
        out: &ProtocolObject<dyn MTLTexture>,
        view: View,
    ) {
        // SAFETY: the scaler's own messages, with textures of the sizes and formats it was made
        // for; the command buffer is the caller's.
        unsafe {
            let _: () = msg_send![&*self.object, setColorTexture: color];
            let _: () = msg_send![&*self.object, setDepthTexture: depth];
            let _: () = msg_send![&*self.object, setMotionTexture: motion];
            let _: () = msg_send![&*self.object, setOutputTexture: out];
            let _: () = msg_send![&*self.object, setReset: Bool::new(view.reset)];
            let _: () = msg_send![&*self.object, setMotionVectorScaleX: view.motion_scale[0]];
            let _: () = msg_send![&*self.object, setMotionVectorScaleY: view.motion_scale[1]];
            let _: () = msg_send![&*self.object, setJitterOffsetX: view.jitter[0]];
            let _: () = msg_send![&*self.object, setJitterOffsetY: view.jitter[1]];
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
    use super::{Interpolator, Spatial, Temporal, Upscale, interpolator, spatial, temporal};
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
        scaler.encode(&cb, &src, &src_depth, &src_motion, &dst, super::View::default());
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
    /// The formats an upscale of 64x64 colour, depth and motion vectors into 128x128 takes.
    fn formats_temporal() -> super::Formats {
        super::Formats {
            color: MTLPixelFormat::BGRA8Unorm,
            depth: MTLPixelFormat::Depth32Float,
            motion: MTLPixelFormat::RGBA16Float,
            out: MTLPixelFormat::BGRA8Unorm,
        }
    }

    /// A caller that has depth and motion vectors gets the temporal scaler where the system has
    /// one, and the spatial one everywhere else: it never has to ask which exists.
    #[test]
    fn an_upscale_takes_the_mode_its_inputs_allow() {
        if !spatial() {
            return;
        }
        let m = Metal::new().unwrap();
        let asked = Upscale::new(
            &m.dev,
            super::Mode::Temporal,
            (64, 64),
            (128, 128),
            formats_temporal(),
        )
        .unwrap();
        let Some(asked) = asked else {
            println!("MetalFX: no upscale on this system");
            return;
        };
        let expected = if temporal() {
            super::Mode::Temporal
        } else {
            super::Mode::Spatial
        };
        assert_eq!(asked.mode(), expected, "the mode the inputs allow");
        assert_eq!(asked.sizes(), ((64, 64), (128, 128)));

        let spatial_only = Upscale::new(
            &m.dev,
            super::Mode::Spatial,
            (64, 64),
            (128, 128),
            super::Formats::spatial(MTLPixelFormat::BGRA8Unorm, MTLPixelFormat::BGRA8Unorm),
        )
        .unwrap()
        .expect("a spatial upscale where MetalFX exists");
        assert_eq!(spatial_only.mode(), super::Mode::Spatial);
    }

    /// A temporal upscale refuses a frame with colour only, and says so instead of encoding: the
    /// caller keeps its own path rather than scale a frame the scaler cannot accumulate from.
    #[test]
    fn a_temporal_upscale_will_not_take_colour_only() {
        if !temporal() {
            return;
        }
        let m = Metal::new().unwrap();
        let Some(up) = Upscale::new(
            &m.dev,
            super::Mode::Temporal,
            (64, 64),
            (128, 128),
            formats_temporal(),
        )
        .unwrap() else {
            return;
        };
        assert_eq!(up.mode(), super::Mode::Temporal);
        assert!(
            !up.takes(&super::Inputs::ColorOnly),
            "it would take colour only"
        );
        let (color, out) = (texture(&m, 64, 64), texture(&m, 128, 128));
        let q = m.dev.newCommandQueue().unwrap();
        let cb = q.commandBuffer().unwrap();
        assert!(
            !up.encode(&cb, &color, super::Inputs::ColorOnly, &out, super::View::default()),
            "it encoded a frame it cannot accumulate from"
        );
        cb.commit();
        cb.waitUntilCompleted();
    }

    /// Both modes run on the GPU through one type: 64x64 into 128x128, and the command buffer
    /// does not fail. Skipped where the system has no MetalFX.
    #[test]
    fn an_upscale_runs_in_both_modes() {
        if !spatial() {
            return;
        }
        let m = Metal::new().unwrap();
        let q = m.dev.newCommandQueue().unwrap();
        let (color, out) = (texture(&m, 64, 64), texture(&m, 128, 128));
        let (d, mv) = (depth(&m, 64, 64), motion(&m, 64, 64));

        let spatial_only = Upscale::new(
            &m.dev,
            super::Mode::Spatial,
            (64, 64),
            (128, 128),
            super::Formats::spatial(MTLPixelFormat::BGRA8Unorm, MTLPixelFormat::BGRA8Unorm),
        )
        .unwrap()
        .expect("a spatial upscale where MetalFX exists");
        let cb = q.commandBuffer().unwrap();
        assert!(spatial_only.encode(&cb, &color, super::Inputs::ColorOnly, &out, super::View::default()));
        cb.commit();
        cb.waitUntilCompleted();
        assert_eq!(
            cb.status(),
            MTLCommandBufferStatus::Completed,
            "the spatial upscale did not complete"
        );

        let Some(both) = Upscale::new(
            &m.dev,
            super::Mode::Temporal,
            (64, 64),
            (128, 128),
            formats_temporal(),
        )
        .unwrap() else {
            return;
        };
        let cb = q.commandBuffer().unwrap();
        let inputs = super::Inputs::ColorDepthMotion {
            depth: &d,
            motion: &mv,
        };
        assert!(both.takes(&inputs));
        assert!(both.encode(&cb, &color, inputs, &out, super::View::default()));
        cb.commit();
        cb.waitUntilCompleted();
        assert_eq!(
            cb.status(),
            MTLCommandBufferStatus::Completed,
            "the temporal upscale did not complete"
        );
    }
}

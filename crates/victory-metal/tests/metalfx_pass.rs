//! A MetalFX upscale as a pass of the backend's own submission: the scaler is picked from the
//! textures a `Pass::MetalFx` names, runs where the submission puts it, and its output is read
//! back. Both modes, because each is a different scaler.
//!
//! Skipped where the system has no MetalFX (`metalfx::spatial` is false), which is the same
//! condition the backend refuses the pass on.

#![cfg(target_os = "macos")]

mod common;

use common::backend;
use victory_metal::backend::ComputeBackend;
use victory_metal::metal::Metal;
use victory_metal::resources::*;
use victory_metal::metalfx;

/// Runs one clearing render pass over `texture` and waits for it.
fn clear(m: &Metal, texture: &<Metal as ResourceBackend>::Texture, color: [f32; 4]) {
    let q = m.create_queue().unwrap();
    let f = m.create_fence(0).unwrap();
    let passes = [Pass::Render(RenderPass {
        color: vec![ColorTarget {
            texture,
            clear: Some(color),
        }],
        depth: None,
        draws: vec![],
    })];
    m.submit_passes(
        &q,
        &PassSubmission {
            waits: &[],
            passes: &passes,
            signals: &[(&f, 1)],
        },
    )
    .unwrap();
    assert!(m.wait(&f, 1, 5000), "the clearing pass did not finish");
}

/// The pixels of `texture`, 4 bytes each.
fn pixels(m: &Metal, texture: &<Metal as ResourceBackend>::Texture, width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    m.read_texture(texture, 0, 0, &mut out, width * 4, 0);
    out
}

/// Whether every pixel of `px` is within `slack` of `want` (bytes, in order).
fn all_near(px: &[u8], want: [u8; 4], slack: i32) -> bool {
    px.chunks(4).all(|p| {
        (0..4).all(|c| (i32::from(p[c]) - i32::from(want[c])).abs() <= slack)
    })
}

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];

#[test]
fn a_spatial_metalfx_pass_fills_its_output() {
    let Some(m) = backend() else { return };
    if !metalfx::spatial() {
        println!("MetalFX: no spatial scaler on this system");
        return;
    }
    let (iw, ih, ow, oh) = (16, 16, 32, 32);
    let mut input = TextureDesc::d2(Format::Rgba8Unorm, iw, ih);
    input.render_target = true;
    let color = m.create_texture(&input).unwrap();
    let mut output = TextureDesc::d2(Format::Rgba8Unorm, ow, oh);
    output.render_target = true;
    let out = m.create_texture(&output).unwrap();
    clear(&m, &color, RED);

    let upscale = Pass::MetalFx(MetalFx {
        color: &color,
        depth: None,
        motion: None,
        out: &out,
        jitter: [0.0, 0.0],
        motion_scale: [1.0, 1.0],
        reset: true,
    });
    let q = m.create_queue().unwrap();
    let f = m.create_fence(0).unwrap();
    m.submit_passes(
        &q,
        &PassSubmission {
            waits: &[],
            passes: &[upscale],
            signals: &[(&f, 1)],
        },
    )
    .unwrap();
    assert!(m.wait(&f, 1, 10000), "the upscale did not finish");

    let px = pixels(&m, &out, ow, oh);
    assert!(
        all_near(&px, [255, 0, 0, 255], 8),
        "an upscale of a red frame is red: {:?}",
        &px[..8.min(px.len())]
    );
}

#[test]
fn a_temporal_metalfx_pass_fills_its_output_from_depth_and_motion() {
    let Some(m) = backend() else { return };
    if !metalfx::temporal() {
        println!("MetalFX: no temporal scaler on this system");
        return;
    }
    let (iw, ih, ow, oh) = (16, 16, 32, 32);
    let target = |format: Format, w: usize, h: usize| {
        let mut desc = TextureDesc::d2(format, w, h);
        desc.render_target = true;
        m.create_texture(&desc).unwrap()
    };
    let (color, depth, motion, out) = (
        target(Format::Rgba8Unorm, iw, ih),
        target(Format::D32Float, iw, ih),
        target(Format::Rgba16Float, iw, ih),
        target(Format::Rgba8Unorm, ow, oh),
    );
    // Nothing moves and everything is at the far plane: zero motion vectors, depth 1.
    clear(&m, &color, RED);
    clear(&m, &motion, [0.0, 0.0, 0.0, 1.0]);

    let upscale = Pass::MetalFx(MetalFx {
        color: &color,
        depth: Some(&depth),
        motion: Some(&motion),
        out: &out,
        jitter: [0.5, -0.5],
        motion_scale: [1.0, 1.0],
        reset: true,
    });
    let q = m.create_queue().unwrap();
    let f = m.create_fence(0).unwrap();
    m.submit_passes(
        &q,
        &PassSubmission {
            waits: &[],
            passes: &[upscale],
            signals: &[(&f, 1)],
        },
    )
    .unwrap();
    assert!(m.wait(&f, 1, 10000), "the temporal upscale did not finish");

    let px = pixels(&m, &out, ow, oh);
    assert!(
        all_near(&px, [255, 0, 0, 255], 8),
        "an upscale of a still red frame is red: {:?}",
        &px[..8.min(px.len())]
    );
}

/// A pass into an output MetalFX may not write is refused: the caller is told which pass it
/// lost rather than the frame failing whole.
#[test]
fn a_metalfx_pass_into_a_texture_metalfx_may_not_write_is_refused() {
    let Some(m) = backend() else { return };
    if !metalfx::spatial() {
        return;
    }
    let color = m
        .create_texture(&TextureDesc::d2(Format::Rgba8Unorm, 16, 16))
        .unwrap();
    let out = m
        .create_texture(&TextureDesc::d2(Format::Rgba8Unorm, 32, 32))
        .unwrap();
    let upscale = Pass::MetalFx(MetalFx {
        color: &color,
        depth: None,
        motion: None,
        out: &out,
        jitter: [0.0, 0.0],
        motion_scale: [1.0, 1.0],
        reset: true,
    });
    let q = m.create_queue().unwrap();
    let f = m.create_fence(0).unwrap();
    let failed = m
        .submit_passes(
            &q,
            &PassSubmission {
                waits: &[],
                passes: &[upscale],
                signals: &[(&f, 1)],
            },
        )
        .is_err();
    assert!(failed, "an upscale into a texture MetalFX may not write was encoded");
}

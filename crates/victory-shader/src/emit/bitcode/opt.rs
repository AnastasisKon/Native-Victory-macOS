//! The bits of `emit::shader_opt()` (`VICTORY_SHADER_OPT`): choices of how
//! a console program is written as DXIL that compute the same values, part
//! of the translation and so of the shader cache's key.
//!
//! The bit values are those of fxceless/victory-fork's `VICTORY_SHADER_OPT`
//! (its `emit/bitcode/opt.rs`), so that a bit means the same in both
//! translators. Only [`WIDE`] is read here; the fork's passes over the
//! module (bits 1 to 16) and its first-lane read of paired waves (bit 32)
//! are not part of this translator, and their bits are left free.

/// Not a pass over the module: the lowering reads the dwords of one
/// console load (`s_buffer_load_dwordx2` to `x16` from a raw view, a
/// `global_load_dwordx2` to `x4`) by one call for every four of them, not
/// by a call each (`lower.rs`, `raw_dwords`), as the fork's bit 64 does,
/// where the four's offset is a constant they lie below 2^32 from (a call
/// a dword wraps there; elsewhere it is read a dword a call still).
/// The console answers each dword past a view's end with zero and the
/// dwords before it as they are; Metal (through the Metal Shader
/// Converter's bounds-checked raw views) and D3DMetal answer a four-dword
/// call that reaches past the end the same way, dword by dword (the GPU
/// tests of a load past its view's end in `victory-gpu`'s `native` tests
/// and in `stride_tests`, run with this bit). That is measured only for
/// D3DMetal and the native host's pool views; descriptor-table and heap
/// views on Metal are a guess (`docs/native-host.md`), and the fork saw a
/// host answer such a call with zeros for all of it. So it is off unless
/// asked for.
pub const WIDE: u32 = 64;
/// Every bit this translator reads.
pub const ALL: u32 = WIDE;
/// The bits on when nothing says which: none (a dword a call).
pub const DEFAULT: u32 = 0;

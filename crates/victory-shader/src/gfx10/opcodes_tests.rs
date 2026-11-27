//! Pins for the opcode-name resolution (`name`): each encoding's table, the
//! fixed bases at which VOP3 also encodes the VOPC, VOP2 and VOP1 opcodes, and
//! opcodes outside a table.

use super::{Encoding, name};

#[test]
fn each_encoding_resolves_its_own_table() {
    let cases = [
        (Encoding::Sop2, 0x00E, "s_and_b32"),
        (Encoding::Sopk, 0x000, "s_movk_i32"),
        (Encoding::Sop1, 0x003, "s_mov_b32"),
        (Encoding::Sopc, 0x000, "s_cmp_eq_i32"),
        (Encoding::Sopp, 0x001, "s_endpgm"),
        (Encoding::Smem, 0x000, "s_load_dword"),
        (Encoding::Vop2, 0x003, "v_add_f32"),
        (Encoding::Vop1, 0x001, "v_mov_b32"),
        (Encoding::Vopc, 0x0C4, "v_cmp_gt_u32"),
        (Encoding::Vop3, 0x14B, "v_fma_f32"),
        (Encoding::Vop3p, 0x021, "v_fma_mixlo_f16"),
        (Encoding::Vintrp, 0x002, "v_interp_mov_f32"),
        (Encoding::Ds, 0x036, "ds_read_b32"),
        (Encoding::Mubuf, 0x00C, "buffer_load_dword"),
        (Encoding::Mtbuf, 0x000, "tbuffer_load_format_x"),
        (Encoding::Mimg, 0x020, "image_sample"),
        (Encoding::Flat, 0x00C, "flat_load_dword"),
        (Encoding::Global, 0x00C, "global_load_dword"),
        (Encoding::Scratch, 0x00C, "scratch_load_dword"),
    ];
    for (encoding, opcode, want) in cases {
        assert_eq!(
            name(encoding, opcode),
            Some(want),
            "{encoding:?} {opcode:#x}"
        );
    }
    // EXP has no opcode field; every opcode names it.
    assert_eq!(name(Encoding::Exp, 0), Some("exp"));
    assert_eq!(name(Encoding::Exp, 0xFFFF), Some("exp"));
}

#[test]
fn vop3_encodes_the_vopc_vop2_and_vop1_opcodes_at_fixed_bases() {
    // 0x000-0x0ff are VOPC opcodes, 0x100-0x13f VOP2's (minus 0x100),
    // 0x180-0x1ff VOP1's (minus 0x180); the rest are VOP3's own.
    assert_eq!(name(Encoding::Vop3, 0x0C4), Some("v_cmp_gt_u32"));
    assert_eq!(name(Encoding::Vop3, 0x103), Some("v_add_f32"));
    assert_eq!(name(Encoding::Vop3, 0x181), Some("v_mov_b32"));
    assert_eq!(name(Encoding::Vop3, 0x14B), Some("v_fma_f32"));
    // 0x140-0x17f are in no base range: VOP3's own table decides (0x140 is RDNA2's v_fma_legacy_f32, #321).
    assert_eq!(name(Encoding::Vop3, 0x140), Some("v_fma_legacy_f32"));
}

#[test]
fn an_opcode_outside_the_table_is_unknown() {
    assert_eq!(name(Encoding::Sopc, 0x07F), None); // SOPC ends at 0x013
    assert_eq!(name(Encoding::Smem, 0x0FF), None); // SMEM ends at 0x0ac
    assert_eq!(name(Encoding::Vop3, 0x3FF), None); // VOP3 ends at 0x37f
}

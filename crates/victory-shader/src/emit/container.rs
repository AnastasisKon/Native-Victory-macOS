//! The PC container around emitted DXIL: feature flags (`SFI0`), the
//! signatures (`ISG1`, `OSG1`), pipeline state validation (`PSV0`) and the
//! program (`DXIL`), laid out as validator version 1.6 lays them out, plus
//! the container hash that D3D12 checks.

/// One signature element: one row (D3D12 links stages by semantic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SigElement {
    pub name: String,
    pub index: u32,
    /// DXIL semantic kind (0 arbitrary, 1 vertex id, 3 position, 16 target...).
    pub kind: u8,
    /// DXIL component type (4 i32, 5 u32, 9 f32, 1 i1).
    pub comp: u8,
    /// DXIL interpolation mode (0 for none).
    pub interp: u8,
    /// Start row, if the element has a register.
    pub row: Option<u32>,
    pub col: u8,
    pub cols: u8,
    /// Components read (inputs) or written (outputs), relative to `col`.
    pub usage: u8,
    /// 16-bit on the console (declared 32-bit here).
    pub half: bool,
}

impl SigElement {
    fn system_value(&self) -> u32 {
        match self.kind {
            1 => 6,  // vertex id
            2 => 8,  // instance id
            3 => 1,  // position
            4 => 4,  // render target array index
            5 => 5,  // viewport array index
            6 => 2,  // clip distance
            7 => 3,  // cull distance
            10 => 7, // primitive id
            12 => 10,
            13 => 9, // front face
            14 => 66,
            15 => 70,
            16 => 64, // target
            17 => 65, // depth
            18 => 68,
            19 => 67,
            20 => 69,
            28 => 23, // barycentrics
            30 => 25, // cull primitive
            _ => 0,
        }
    }

    fn sig_comp(&self) -> u32 {
        match self.comp {
            1 | 5 => 1, // bool and u32
            4 => 2,
            9 => 3,
            _ => 0,
        }
    }

    fn mask(&self) -> u8 {
        ((1u8 << self.cols) - 1) << self.col
    }
}

/// `ISG1`/`OSG1`: 32-byte rows, then the names.
/// Gives the elements of a row whose interpolation mode nothing decided
/// (float elements, unless `read` and read) the mode the row's other
/// elements agree on: DXIL wants one mode per row, and the console's
/// signature, packed by the source's modes, had one (an int or a system
/// value beside a float means the float was `nointerpolation`).
pub(crate) fn same_mode_per_row(elements: &mut [SigElement], read: bool) {
    let free = |e: &SigElement| e.kind == 0 && e.comp == 9 && !(read && e.usage != 0);
    let mut rows: Vec<u32> = elements.iter().filter_map(|e| e.row).collect();
    rows.sort_unstable();
    rows.dedup();
    for row in rows {
        let decided = |e: &&SigElement| e.row == Some(row) && !free(e) && e.interp != 0;
        let mut modes = elements.iter().filter(decided).map(|e| e.interp);
        let Some(mode) = modes.next() else { continue };
        if modes.any(|m| m != mode) {
            continue;
        }
        let undecided = |e: &&mut SigElement| e.row == Some(row) && free(e);
        for e in elements.iter_mut().filter(undecided) {
            e.interp = mode;
        }
    }
}

/// Metal's limit on a mesh pipeline's output, in bytes: `Total mesh size
/// (N) exceeds the maximum mesh size allowed (32768)` above it: no
/// pipeline (D3DMetal makes it a no-op).
pub(crate) const METAL_MESH_BYTES: u32 = 32768;

/// Bytes of one output record (vertex or primitive) of a mesh shader as
/// Metal counts them, the record being the struct Apple's Metal Shader
/// Converter (D3DMetal 4.0b3's and the standalone one alike) makes of
/// the signature: the clip and cull distances as a float array, the
/// position, then one field per other element in element order (the
/// distances again among them), each of `cols` 32-bit components and
/// aligned to its own size: 4, 8, 12 (a 3-vector packs at a multiple of
/// 12, not 16) or 16 bytes, with no padding at the end. Every field counts,
/// whether the pixel shader reads it or not. Measured with Metal's own
/// messages for mesh pipelines of chosen output structs
/// (`metal_records_are_the_sizes_metal_reports`).
pub(crate) fn metal_record<'a>(elements: impl IntoIterator<Item = &'a SigElement>) -> u32 {
    let elements: Vec<&SigElement> = elements.into_iter().collect();
    let distances: u32 = elements
        .iter()
        .filter(|e| matches!(e.kind, 6 | 7))
        .map(|e| u32::from(e.cols))
        .sum();
    let (position, rest): (Vec<&SigElement>, Vec<&SigElement>) =
        elements.into_iter().partition(|e| e.kind == 3);
    position.into_iter().chain(rest).fold(4 * distances, |at, e| {
        let size = 4 * u32::from(e.cols.max(1));
        at.div_ceil(size) * size + size
    })
}

/// Metal's size of a mesh shader's output (see [`metal_record`]).
pub(crate) fn metal_mesh_bytes(
    vertices: &[SigElement],
    primitives: &[SigElement],
    max_vertices: u32,
    max_primitives: u32,
) -> u32 {
    let prims = if primitives.is_empty() {
        0
    } else {
        max_primitives * metal_record(primitives)
    };
    max_vertices * metal_record(vertices) + prims
}

/// An order of mesh output elements whose Metal record ([`metal_record`])
/// is as small as it can be: system values first, in their order, then
/// the rest. The elements' rows stay; only their order (their signature
/// IDs) changes, which D3D12 and Metal do not link by (semantics do).
///
/// Only the sizes of the rest matter (4, 8, 12 or 16 bytes, each aligned
/// to its size), and the padding before a field depends only on where the
/// record is modulo 48 (their least common multiple). So the smallest
/// record is found exactly by a search over (how many of each size are
/// left, offset modulo 48), each state remembered: at most 12 offsets
/// times the product of (count + 1) of the four sizes: a few hundred
/// states for a usual mesh, about 700,000 at worst (32 rows of scalars
/// and vectors mixed), and only for a mesh over Metal's limit. Placing the widest field that needs no
/// padding first (the rule before this one) misses layouts: the position,
/// twelve 4-vectors and four 3-vectors take 264 bytes that way but 256
/// with the 3-vectors after the first two 4-vectors. Of the orders as
/// small, it takes the least padding next and then the widest field, the
/// elements of one size in their order.
pub(crate) fn metal_packed_order(elements: &[SigElement]) -> Vec<usize> {
    let (mut order, rest): (Vec<usize>, Vec<usize>) =
        (0..elements.len()).partition(|&i| elements[i].kind != 0);
    // In 4-byte units: sizes 1 to 4, offsets modulo 12.
    let units = |i: usize| usize::from(elements[i].cols.clamp(1, 4));
    let mut by_size: [Vec<usize>; 4] = Default::default();
    for &i in &rest {
        by_size[units(i) - 1].push(i);
    }
    let counts = by_size.each_ref().map(Vec::len);
    let start = metal_record(order.iter().map(|&i| &elements[i])) as usize / 4 % 12;
    let pad = |at: usize, size: usize| (size - at % size) % size;
    // The least padding to place `left` from offset `at`, remembered.
    let states = counts.iter().map(|n| n + 1).product::<usize>() * 12;
    let mut memo = vec![u16::MAX; states];
    fn least(left: [usize; 4], at: usize, counts: &[usize; 4], memo: &mut [u16]) -> u16 {
        let key = left
            .iter()
            .zip(counts)
            .fold(0, |k, (&l, &n)| k * (n + 1) + l)
            * 12
            + at;
        if memo[key] != u16::MAX {
            return memo[key];
        }
        let mut best = if left == [0; 4] { 0 } else { u16::MAX };
        for c in (0..4).filter(|&c| left[c] > 0) {
            let size = c + 1;
            let p = (size - at % size) % size;
            let mut next = left;
            next[c] -= 1;
            let total = p as u16 + least(next, (at + p + size) % 12, counts, memo);
            best = best.min(total);
        }
        memo[key] = best;
        best
    }
    let (mut left, mut at) = (counts, start);
    while left != [0; 4] {
        let c = (0..4)
            .filter(|&c| left[c] > 0)
            .min_by_key(|&c| {
                let p = pad(at, c + 1);
                let mut next = left;
                next[c] -= 1;
                let total = p as u16 + least(next, (at + p + c + 1) % 12, &counts, &mut memo);
                (total, p, std::cmp::Reverse(c))
            })
            .expect("one left");
        order.push(by_size[c][counts[c] - left[c]]);
        at = (at + pad(at, c + 1) + c + 1) % 12;
        left[c] -= 1;
    }
    order
}

/// Mesh output `elements` without the clip and cull distances at the end
/// of their order (clip then cull, by index, as the console's distance
/// vectors carry them) whose channels `idle` names (element, channels):
/// distances that are +0.0 at every vertex, which clip and cull nothing.
/// Only from the end, so that the others keep their places in the vectors;
/// an element left without channels goes. `None` when there are none.
pub(crate) fn without_idle_distances(
    elements: &[SigElement],
    idle: &[(usize, u8)],
) -> Option<Vec<SigElement>> {
    let mut distances: Vec<usize> = (0..elements.len())
        .filter(|&i| matches!(elements[i].kind, 6 | 7))
        .collect();
    distances.sort_by_key(|&i| (elements[i].kind, elements[i].index));
    let mut out = elements.to_vec();
    'elements: for &i in distances.iter().rev() {
        let channels = idle.iter().find(|x| x.0 == i).map_or(0, |x| x.1);
        while out[i].cols > 0 {
            if channels >> (out[i].cols - 1) & 1 == 0 {
                break 'elements;
            }
            out[i].cols -= 1;
        }
    }
    out.retain(|e| e.cols > 0);
    (out != elements).then_some(out)
}

pub(crate) fn signature(elements: &[SigElement], input: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((elements.len() as u32).to_le_bytes());
    out.extend(8u32.to_le_bytes());
    // Validator 1.6 writes one name per element in element order, repeats
    // included, except that system values share theirs (it keys names by
    // the semantic's string, which is one static string per system value);
    // the rows sorted by register (unallocated last).
    let mut offsets: Vec<u32> = Vec::new();
    let mut next = 8 + 32 * elements.len() as u32;
    let mut names = Vec::new();
    for (i, e) in elements.iter().enumerate() {
        let shared = (e.kind != 0)
            .then(|| (0..i).find(|&j| elements[j].kind == e.kind))
            .flatten();
        match shared {
            Some(j) => offsets.push(offsets[j]),
            None => {
                offsets.push(next);
                next += e.name.len() as u32 + 1;
                names.push(&e.name);
            }
        }
    }
    let mut order: Vec<usize> = (0..elements.len()).collect();
    order.sort_by_key(|&i| elements[i].row.unwrap_or(u32::MAX));
    for i in order {
        let (e, offset) = (&elements[i], offsets[i]);
        let used = e.usage << e.col;
        let rw = if input { used } else { !used & 0xf };
        for v in [
            0,
            offset,
            e.index,
            e.system_value(),
            e.sig_comp(),
            e.row.unwrap_or(u32::MAX),
        ] {
            out.extend(v.to_le_bytes());
        }
        out.extend([e.mask(), rw, 0, 0]);
        out.extend(0u32.to_le_bytes());
    }
    for name in names {
        out.extend(name.as_bytes());
        out.push(0);
    }
    out
}

/// A resource bound by register, as `PSV0` lists it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binding {
    /// PSV resource type: 1 sampler, 2 CBV.
    pub kind: u32,
    pub space: u32,
    pub register: u32,
    /// DXIL resource kind (13 cbuffer, 14 sampler).
    pub dxil_kind: u32,
}

pub(crate) struct Psv<'a> {
    /// PSV shader kind: 0 pixel, 1 vertex, 5 compute.
    pub stage: u8,
    /// Vertex: writes `SV_Position`. Pixel: writes depth.
    pub flag: bool,
    pub inputs: &'a [SigElement],
    pub outputs: &'a [SigElement],
    /// Mesh: per-primitive outputs.
    pub prims: &'a [SigElement],
    /// Mesh: its state and the group-shared bytes it declares.
    pub mesh: Option<(super::shader::Mesh, u32)>,
    /// Geometry: its maximum output vertices (triangles in, strips out).
    pub gs: Option<u32>,
    pub bindings: &'a [Binding],
    /// Compute: thread-group size and required wave size.
    pub threads: [u32; 3],
    pub wave_size: Option<u32>,
}

/// `PSV0` with runtime info version 2 (validator 1.6).
pub(crate) fn psv(p: &Psv) -> Vec<u8> {
    let vectors = |els: &[SigElement]| {
        els.iter()
            .filter_map(|e| e.row)
            .map(|r| r + 1)
            .max()
            .unwrap_or(0)
    };
    let (in_vectors, out_vectors) = (vectors(p.inputs), vectors(p.outputs));
    let mut out = Vec::new();
    let u32s = |out: &mut Vec<u8>, vs: &[u32]| {
        for v in vs {
            out.extend(v.to_le_bytes());
        }
    };
    u32s(&mut out, &[48]);
    let mut info = [0u8; 48];
    info[0] = u8::from(p.flag);
    if p.stage == 0 {
        // PSInfo.SampleFrequency: an input interpolated per sample, or the
        // sample index.
        info[1] = u8::from(
            p.inputs
                .iter()
                .any(|e| matches!(e.interp, 6 | 7) || e.kind == 12),
        );
    }
    let (min, max) = p.wave_size.map_or((0, u32::MAX), |n| (n, n));
    info[16..20].copy_from_slice(&min.to_le_bytes());
    info[20..24].copy_from_slice(&max.to_le_bytes());
    info[24] = p.stage;
    for (k, n) in p.threads.into_iter().enumerate() {
        info[36 + 4 * k..40 + 4 * k].copy_from_slice(&n.to_le_bytes());
    }
    info[28] = p.inputs.len() as u8;
    info[29] = p.outputs.len() as u8;
    info[31] = in_vectors as u8;
    info[32] = out_vectors as u8;
    if let Some((mesh, shared)) = p.mesh {
        info[0..4].copy_from_slice(&shared.to_le_bytes());
        info[12..14].copy_from_slice(&(mesh.max_vertices as u16).to_le_bytes());
        info[14..16].copy_from_slice(&(mesh.max_primitives as u16).to_le_bytes());
        info[26] = vectors(p.prims) as u8;
        info[27] = mesh.topology;
        info[30] = p.prims.len() as u8;
    }
    if let Some(max) = p.gs {
        // Input primitive (triangle), output topology (triangle strip),
        // stream mask, then whether it writes a position.
        for (k, v) in [3u32, 5, 1].into_iter().enumerate() {
            info[4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes());
        }
        info[12] = u8::from(p.flag);
        info[26..28].copy_from_slice(&(max as u16).to_le_bytes());
    }
    out.extend(info);
    u32s(&mut out, &[p.bindings.len() as u32]);
    if !p.bindings.is_empty() {
        u32s(&mut out, &[24]);
        for b in p.bindings {
            u32s(
                &mut out,
                &[b.kind, b.space, b.register, b.register, b.dxil_kind, 0],
            );
        }
    }
    // Names of arbitrary semantics (system values use ""), then indices.
    let mut strings = vec![0u8];
    let mut indices: Vec<u32> = Vec::new();
    let mut elements = Vec::new();
    for e in p.inputs.iter().chain(p.outputs).chain(p.prims) {
        let name = if e.kind == 0 {
            let needle: Vec<u8> = e.name.bytes().chain([0]).collect();
            match find(&strings, &needle) {
                Some(at) => at,
                None => {
                    strings.extend(&needle);
                    strings.len() - needle.len()
                }
            }
        } else {
            0
        };
        let index = match indices.iter().position(|&i| i == e.index) {
            Some(at) => at,
            None => {
                indices.push(e.index);
                indices.len() - 1
            }
        };
        let (start, alloc) = match e.row {
            Some(r) => (r as u8, 0x40 | (e.col << 4)),
            None => (0, 0),
        };
        elements.push((name as u32, index as u32, start, e.cols | alloc, e));
    }
    strings.resize(strings.len().next_multiple_of(4), 0);
    u32s(&mut out, &[strings.len() as u32]);
    out.extend(&strings);
    u32s(&mut out, &[indices.len() as u32]);
    u32s(&mut out, &indices);
    if !elements.is_empty() {
        u32s(&mut out, &[16]);
        for (name, index, start, cols, e) in elements {
            u32s(&mut out, &[name, index]);
            out.extend([1, start, cols, e.kind, e.sig_comp() as u8, e.interp, 0, 0]);
        }
    }
    // Input-to-output dependencies, as the view-id state declares them:
    // every output scalar on every input scalar.
    if in_vectors > 0 && out_vectors > 0 {
        let scalars = |els: &[SigElement]| {
            els.iter()
                .filter_map(|e| e.row.map(|r| r * 4 + u32::from(e.col + e.cols)))
                .max()
                .unwrap_or(0)
        };
        let (ni, no) = (scalars(p.inputs), scalars(p.outputs));
        let per = (out_vectors * 4).div_ceil(32);
        for i in 0..in_vectors * 4 {
            for k in 0..per {
                let bits = if i < ni {
                    no.saturating_sub(32 * k).min(32)
                } else {
                    0
                };
                let mask = if bits == 32 {
                    u32::MAX
                } else {
                    (1u32 << bits) - 1
                };
                u32s(&mut out, &[mask]);
            }
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The container: parts in order, hashed.
pub(crate) fn build(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let table = 0x20 + 4 * parts.len();
    let total = table + parts.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend(b"DXBC");
    out.extend([0; 16]);
    out.extend(1u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((total as u32).to_le_bytes());
    out.extend((parts.len() as u32).to_le_bytes());
    let mut offset = table;
    for (_, d) in parts {
        out.extend((offset as u32).to_le_bytes());
        offset += 8 + d.len();
    }
    for (fourcc, d) in parts {
        out.extend(*fourcc);
        out.extend((d.len() as u32).to_le_bytes());
        out.extend(*d);
    }
    let digest = hash(&out[20..]);
    out[4..20].copy_from_slice(&digest);
    out
}

/// The shader hash part (`HASH`): flags (0, no source included) and the
/// MD5 of the program. Apple's D3DMetal keys its shader cache on this hash,
/// so it must differ between shaders: with one shared value, two shaders of
/// the same length got each other's compiled code.
pub(crate) fn shader_hash(dxil: &[u8]) -> [u8; 20] {
    let mut out = [0; 20];
    out[4..].copy_from_slice(&md5(dxil));
    out
}

/// Standard MD5.
pub(crate) fn md5(data: &[u8]) -> [u8; 16] {
    let len = data.len();
    let mut tail = data[len - len % 64..].to_vec();
    tail.push(0x80);
    while tail.len() % 64 != 56 {
        tail.push(0);
    }
    tail.extend(((len as u64) << 3).to_le_bytes());
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in data[..len - len % 64]
        .as_chunks::<64>()
        .0
        .iter()
        .chain(tail.as_chunks::<64>().0)
    {
        md5_block(&mut state, block);
    }
    let mut out = [0; 16];
    for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    out
}

/// The container hash D3D12 checks: MD5 whose last block carries the bit
/// count first and `2 × bytes + 1` last instead of standard padding.
pub(crate) fn hash(data: &[u8]) -> [u8; 16] {
    let len = data.len();
    let left = len % 64;
    let mut tail = Vec::with_capacity(128);
    if left < 56 {
        tail.extend(((len as u32) << 3).to_le_bytes());
        tail.extend(&data[len - left..]);
        tail.push(0x80);
        tail.resize(60, 0);
    } else {
        tail.extend(&data[len - left..]);
        tail.push(0x80);
        tail.resize(64, 0);
        tail.extend(((len as u32) << 3).to_le_bytes());
        tail.resize(124, 0);
    }
    tail.extend(((len as u32) << 1 | 1).to_le_bytes());
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in data[..len - left]
        .as_chunks::<64>()
        .0
        .iter()
        .chain(tail.as_chunks::<64>().0)
    {
        md5_block(&mut state, block);
    }
    let mut out = [0; 16];
    for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    out
}

fn md5_block(state: &mut [u32; 4], block: &[u8; 64]) {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    const K: [u32; 64] = [
        0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee,
        0xf57c_0faf, 0x4787_c62a, 0xa830_4613, 0xfd46_9501,
        0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be,
        0x6b90_1122, 0xfd98_7193, 0xa679_438e, 0x49b4_0821,
        0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa,
        0xd62f_105d, 0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8,
        0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
        0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a,
        0xfffa_3942, 0x8771_f681, 0x6d9d_6122, 0xfde5_380c,
        0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70,
        0x289b_7ec6, 0xeaa1_27fa, 0xd4ef_3085, 0x0488_1d05,
        0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665,
        0xf429_2244, 0x432a_ff97, 0xab94_23a7, 0xfc93_a039,
        0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
        0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1,
        0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb, 0xeb86_d391,
    ];
    let x: [u32; 16] = std::array::from_fn(|i| {
        u32::from_le_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ])
    });
    let [mut a, mut b, mut c, mut d] = *state;
    for (i, &k) in K.iter().enumerate() {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(k)
            .wrapping_add(x[g])
            .rotate_left(S[(i / 16) * 4 + i % 4]);
        (a, b, c, d) = (d, b.wrapping_add(rotated), b, c);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d]) {
        *s = s.wrapping_add(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_core_matches_the_standard() {
        // Standard MD5 of "abc" with standard padding, through md5_block.
        let mut block = b"abc".to_vec();
        block.push(0x80);
        block.resize(56, 0);
        block.extend(24u64.to_le_bytes());
        let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        md5_block(&mut state, block.as_slice().try_into().unwrap());
        let hex: String = state
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn md5_matches_rfc_1321() {
        let hex = |d: [u8; 16]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        // RFC 1321's test suite: empty, one block, 55 and 56 bytes (where
        // the padding spills into a second block), and two full blocks.
        for (input, want) in [
            (&b""[..], "d41d8cd98f00b204e9800998ecf8427e"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(
                hex(md5(input)),
                want,
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
        assert_eq!(hex(md5(&[0x61; 55])), "ef1772b6dff9a122358552954ad0df65");
        assert_eq!(hex(md5(&[0x61; 56])), "3b0c8ac703f828b04c6c197006d17218");
    }

    /// Elements of `cols` 32-bit components, one per row: `(name, kind, cols)`.
    fn outputs(spec: &[(&str, u8, u8)]) -> Vec<SigElement> {
        spec.iter()
            .enumerate()
            .map(|(row, &(name, kind, cols))| SigElement {
                name: name.into(),
                index: row as u32,
                kind,
                comp: 9,
                interp: 2,
                row: Some(row as u32),
                col: 0,
                cols,
                usage: (1 << cols) - 1,
                half: false,
            })
            .collect()
    }

    /// The sizes Metal gave for mesh pipelines (128 vertices, no primitive
    /// data) of chosen output structs: position, 15 float4, then the rest.
    #[test]
    fn metal_records_are_the_sizes_metal_reports() {
        let base: Vec<(&str, u8, u8)> = std::iter::once(("SV_Position", 3, 4))
            .chain(std::iter::repeat_n(("TEXCOORD", 0, 4), 15))
            .collect();
        for (rest, bytes) in [
            (&[4][..], 34816),
            (&[1], 33280),
            (&[2], 33792),
            (&[3], 35328),
            (&[1, 1], 33792),
            (&[1, 4], 36864),
            (&[3, 1], 35840),
            (&[1, 3], 35328),
            (&[3, 3], 36864),
            (&[3, 3, 3, 3], 39936),
            (&[4, 3], 36864),
            (&[3, 2], 36864),
            (&[2, 3], 35328),
            (&[1, 1, 1, 3], 36864),
        ] {
            let mut spec = base.clone();
            spec.extend(rest.iter().map(|&c| ("TEXCOORD", 0, c)));
            let els = outputs(&spec);
            assert_eq!(metal_mesh_bytes(&els, &[], 128, 128), bytes, "{rest:?}");
        }
    }

    /// A synthetic mesh of six groups after the position, each a scalar
    /// and a 3-vector sharing a row, then a 2-vector and a 4-vector: 48
    /// bytes a group as laid out (8 of them padding), 40 packed, which
    /// takes it from over Metal's limit to the sum of its components.
    #[test]
    fn packed_order_fits_a_mesh_under_metals_limit() {
        let mut spec = vec![("SV_Position", 3, 4)];
        for _ in 0..6 {
            spec.extend([1, 3, 2, 4].map(|c| ("TEXCOORD", 0, c)));
        }
        let mut els = outputs(&spec);
        // The scalar and the 3-vector of a group share a row.
        for g in 0..6 {
            let row = els[1 + 4 * g].row;
            (els[2 + 4 * g].row, els[2 + 4 * g].col) = (row, 1);
        }
        assert_eq!(metal_mesh_bytes(&els, &[], 128, 128), 128 * 304);
        let order = metal_packed_order(&els);
        let packed: Vec<SigElement> = order.iter().map(|&i| els[i].clone()).collect();
        assert_eq!(metal_mesh_bytes(&packed, &[], 128, 128), 128 * 256);
        // System values first, then the 4-vectors, a 2-vector (which ends
        // at a multiple of 12), the 3-vectors, the other 2-vectors, the
        // scalars.
        assert_eq!(
            order,
            [
                0, 4, 8, 12, 16, 20, 24, 3, 2, 6, 10, 14, 18, 22, 7, 11, 15, 19, 23, 1, 5, 9, 13,
                17, 21
            ]
        );
        // The position comes first wherever its element is.
        let mut last = els.clone();
        last.rotate_left(1);
        assert_eq!(metal_mesh_bytes(&last, &[], 128, 128), 128 * 304);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..els.len()).collect::<Vec<_>>());
    }

    /// A synthetic mesh's vertex outputs: the position, clip distances of
    /// `distances` channels, `vectors` 4-vectors and, with `scalar`, a
    /// scalar.
    fn distance_mesh(distances: u8, vectors: usize, scalar: bool) -> Vec<SigElement> {
        let mut spec = vec![("SV_Position", 3, 4), ("SV_ClipDistance", 6, distances)];
        spec.extend(std::iter::repeat_n(("TEXCOORD", 0, 4), vectors));
        if scalar {
            spec.push(("TEXCOORD", 0, 1));
        }
        outputs(&spec)
    }

    fn packed_bytes(els: &[SigElement]) -> u32 {
        let packed: Vec<SigElement> = metal_packed_order(els)
            .into_iter()
            .map(|i| els[i].clone())
            .collect();
        metal_mesh_bytes(&packed, &[], 128, 128)
    }

    /// Placing the widest field that needs no padding first misses a
    /// layout that fits: the position, twelve 4-vectors and four 3-vectors
    /// at 128 vertices take 264 bytes a vertex (33,792 in all) that way,
    /// and exactly Metal's 32,768 with the 3-vectors after the first two
    /// 4-vectors.
    #[test]
    fn packed_order_finds_a_fit_widest_first_misses() {
        let mut spec = vec![("SV_Position", 3, 4)];
        spec.extend(std::iter::repeat_n(("TEXCOORD", 0, 4), 12));
        spec.extend(std::iter::repeat_n(("TEXCOORD", 0, 3), 4));
        let els = outputs(&spec);
        assert_eq!(metal_mesh_bytes(&els, &[], 128, 128), 33792);
        assert_eq!(packed_bytes(&els), METAL_MESH_BYTES);
        // The 3-vectors in the reverse order: the same.
        let mut reversed = els.clone();
        reversed[13..].reverse();
        assert_eq!(packed_bytes(&reversed), METAL_MESH_BYTES);
    }

    /// Meshes no order fits: twenty 3-vectors after the position sum to
    /// 256 bytes a vertex, but a 3-vector packs only at a multiple of 12,
    /// so the first pads 8 bytes; one scalar more than the case above is
    /// over by its components alone.
    #[test]
    fn packed_order_cannot_fit_what_no_order_fits() {
        let mut spec = vec![("SV_Position", 3, 4)];
        spec.extend(std::iter::repeat_n(("TEXCOORD", 0, 3), 20));
        assert_eq!(packed_bytes(&outputs(&spec)), 128 * 264);
        let mut spec = vec![("SV_Position", 3, 4)];
        spec.extend(std::iter::repeat_n(("TEXCOORD", 0, 4), 12));
        spec.extend(std::iter::repeat_n(("TEXCOORD", 0, 3), 4));
        spec.push(("TEXCOORD", 0, 1));
        assert_eq!(packed_bytes(&outputs(&spec)), 128 * 260);
    }

    /// Meshes over Metal's limit even packed fit without the clip
    /// distances that are +0.0 at every vertex, or with an output as narrow
    /// as its pixel shader reads it.
    #[test]
    fn meshes_over_metals_limit_packed_fit_without_what_nothing_reads() {
        // Four clip distances, of which the last three are idle: counted
        // twice, as the array and as a field.
        let els = distance_mesh(4, 13, true);
        assert_eq!(packed_bytes(&els), 128 * 260);
        let fewer = without_idle_distances(&els, &[(1, 0b1110)]).unwrap();
        assert_eq!((fewer.len(), fewer[1].cols), (els.len(), 1));
        assert_eq!(packed_bytes(&fewer), 128 * 256);
        // One live distance stays; a pixel shader reading one channel of a
        // 4-vector lets the mesh keep only that channel.
        let mut els = distance_mesh(1, 14, false);
        assert_eq!(packed_bytes(&els), 128 * 272);
        assert_eq!(without_idle_distances(&els, &[(1, 0)]), None);
        (els[2].col, els[2].cols) = (3, 1);
        assert_eq!(packed_bytes(&els), 128 * 256);
    }

    /// Only the distances at the end of the clip-then-cull order go: the
    /// console's vectors carry the others by position.
    #[test]
    fn idle_distances_go_from_the_end_only() {
        let els = outputs(&[
            ("SV_Position", 3, 4),
            ("SV_CullDistance", 7, 2),
            ("TEXCOORD", 0, 4),
            ("SV_ClipDistance", 6, 4),
        ]);
        let cols = |idle: &[(usize, u8)]| {
            without_idle_distances(&els, idle)
                .map(|e| e.iter().map(|e| (e.kind, e.cols)).collect::<Vec<_>>())
        };
        // Clip distances before a live cull distance stay.
        assert_eq!(cols(&[(3, 0b1111)]), None);
        assert_eq!(cols(&[(1, 0b01), (3, 0b1111)]), None);
        assert_eq!(cols(&[(1, 0b10)]), Some(vec![(3, 4), (7, 1), (0, 4), (6, 4)]));
        // An element left without channels goes, and the one before it is
        // then the end.
        assert_eq!(
            cols(&[(1, 0b11), (3, 0b1010)]),
            Some(vec![(3, 4), (0, 4), (6, 3)])
        );
        assert_eq!(cols(&[(1, 0b11), (3, 0b1111)]), Some(vec![(3, 4), (0, 4)]));
    }

    #[test]
    fn shader_hashes_differ_between_programs() {
        let (a, b) = (shader_hash(b"program a"), shader_hash(b"program b"));
        assert_eq!(a[..4], [0; 4], "flags");
        assert_ne!(a, b);
        assert_eq!(a, shader_hash(b"program a"));
    }

    fn el(kind: u8, comp: u8, interp: u8, row: Option<u32>, usage: u8) -> SigElement {
        SigElement {
            name: String::new(),
            index: 0,
            kind,
            comp,
            interp,
            row,
            col: 0,
            cols: 4,
            usage,
            half: false,
        }
    }

    /// A row's undecided float elements take the mode its decided elements
    /// agree on; rows without one decided mode stay as they are. A decided
    /// element is one that is not free (here a system value) with a mode.
    #[test]
    fn a_rows_undecided_floats_take_the_rows_mode() {
        let mut els = vec![
            el(3, 9, 5, Some(0), 0), // position: not free, mode 5
            el(0, 9, 0, Some(0), 0), // free float in row 0
            el(0, 9, 0, Some(1), 0), // free float in a row with no decided mode
            el(0, 9, 0, None, 0),    // no row at all
        ];
        same_mode_per_row(&mut els, false);
        assert_eq!(els[0].interp, 5);
        assert_eq!(els[1].interp, 5);
        assert_eq!(els[2].interp, 0);
        assert_eq!(els[3].interp, 0);
    }

    /// Elements whose modes disagree leave the row's free floats alone.
    #[test]
    fn disagreeing_modes_leave_the_row_alone() {
        let mut els = vec![
            el(3, 9, 3, Some(0), 0),  // position, mode 3
            el(10, 9, 7, Some(0), 0), // primitive id, mode 7
            el(0, 9, 0, Some(0), 0),  // free float
        ];
        same_mode_per_row(&mut els, false);
        assert_eq!(els[2].interp, 0);
    }

    /// An input float that is read (`usage` set) is not free, so it takes no
    /// mode; the same element as an output is free and does.
    #[test]
    fn an_input_float_that_reads_is_not_free() {
        let make = || vec![el(0, 4, 2, Some(0), 0), el(0, 9, 0, Some(0), 1)];
        let mut els = make();
        same_mode_per_row(&mut els, true);
        assert_eq!(els[1].interp, 0, "input with usage set is not free");
        let mut els = make();
        same_mode_per_row(&mut els, false);
        assert_eq!(els[1].interp, 2, "output ignores usage");
    }

    /// Only free elements are overwritten: system values and non-floats
    /// decide a row but keep their own mode.
    #[test]
    fn only_free_elements_are_overwritten() {
        let mut els = vec![
            el(3, 9, 2, Some(0), 0), // position, decided
            el(0, 9, 0, Some(0), 0), // free float
            el(0, 4, 0, Some(0), 0), // i32, not free
        ];
        same_mode_per_row(&mut els, false);
        assert_eq!([els[0].interp, els[1].interp, els[2].interp], [2, 2, 0]);
    }

    /// The semantic-kind, component-type and mask mappings, as the module
    /// documents them.
    #[test]
    fn system_value_component_and_mask_mappings() {
        // DXIL semantic kind -> system value register.
        for (kind, want) in [
            (1, 6),   // vertex id
            (2, 8),   // instance id
            (3, 1),   // position
            (10, 7),  // primitive id
            (13, 9),  // front face
            (16, 64), // target
            (17, 65), // depth
            (30, 25), // cull primitive
            (0, 0),   // arbitrary
            (99, 0),  // unknown
        ] {
            assert_eq!(el(kind, 9, 0, None, 0).system_value(), want, "kind {kind}");
        }
        // DXIL component type.
        for (comp, want) in [(1, 1), (5, 1), (4, 2), (9, 3), (0, 0)] {
            assert_eq!(el(0, comp, 0, None, 0).sig_comp(), want, "comp {comp}");
        }
        // Component mask: `cols` bits from `col`.
        let mut e = el(0, 9, 0, None, 0);
        e.col = 1;
        e.cols = 3;
        assert_eq!(e.mask(), 0b1110);
        e.col = 0;
        e.cols = 4;
        assert_eq!(e.mask(), 0b1111);
    }
}

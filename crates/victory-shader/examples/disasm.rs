//! Disassembles a dumped code piece: `VICTORY_PIECE=<file> [VICTORY_WAVE32=1] cargo run --example disasm`;
//! `VICTORY_CONTAINER=<file>` takes the code of a guessed graphics container (`FSPG` part) instead;
//! `VICTORY_SHADER=<file>` the first program in a dumped shader's `DXIL` part (a sweep corpus file).

fn main() {
    let code = if let Ok(path) = std::env::var("VICTORY_SHADER") {
        let bytes = std::fs::read(path).expect("readable shader");
        let c = victory_shader::Container::parse(&bytes).expect("a container");
        let part = c.part(victory_shader::FourCC::DXIL).expect("a DXIL part");
        victory_shader::native_programs(part.data).expect("native programs")[0].patched_code()
    } else if let Ok(path) = std::env::var("VICTORY_CONTAINER") {
        let bytes = std::fs::read(path).expect("readable container");
        let c = victory_shader::Container::parse(&bytes).expect("a container");
        let part = c.part(victory_shader::FourCC::ACCC).expect("an ACCC part");
        let word = |at: usize| u32::from_le_bytes(part.data[at..at + 4].try_into().unwrap()) as usize;
        part.data[16..16 + word(8)].to_vec()
    } else {
        let path = std::env::var("VICTORY_PIECE").expect("VICTORY_PIECE names a dumped code piece");
        std::fs::read(path).expect("readable piece")
    };
    let wave64 = std::env::var_os("VICTORY_WAVE32").is_none();
    match victory_shader::gfx10::decode(&code) {
        Ok(list) => {
            for i in list {
                println!("{:04x}: {}", i.offset, i.text(wave64));
            }
        }
        Err(e) => println!("decode failed: {e}"),
    }
}

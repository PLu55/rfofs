//! Generates the sine lookup tables used by `src/fastsin.rs` at build time,
//! so they're baked into the binary's .rodata with zero runtime init cost.
//!
//! Two tables are produced:
//! - `SIN_TABLE`: full-cycle table, one entry per `TABLE_BITS`-resolution step.
//! - `QUARTER_TABLE`: covers only `[0, π/2]`; `fast_sin_quarter` reconstructs
//!   the full cycle via quadrant symmetry, giving the same angular resolution
//!   as a full table 4x its size (better cache residency, same accuracy).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

const TABLE_BITS: u32 = 12;
const QUARTER_BITS: u32 = 10; // 1024 entries ≈ same resolution as a 4096-entry full table

fn main() {
    let table_size: usize = 1 << TABLE_BITS;
    let quarter_size: usize = 1 << QUARTER_BITS;

    let mut body = String::new();

    writeln!(body, "pub const TABLE_BITS: u32 = {TABLE_BITS};").unwrap();
    writeln!(body, "pub static SIN_TABLE: [f32; {table_size} + 1] = [").unwrap();
    for i in 0..=table_size {
        let phase = i as f32 / table_size as f32;
        let v = (phase * std::f32::consts::TAU).sin();
        writeln!(body, "    {v}f32,").unwrap();
    }
    body.push_str("];\n");

    writeln!(body, "pub const QUARTER_BITS: u32 = {QUARTER_BITS};").unwrap();
    writeln!(body, "pub static QUARTER_TABLE: [f32; {quarter_size} + 1] = [").unwrap();
    for i in 0..=quarter_size {
        let angle = i as f32 / quarter_size as f32 * std::f32::consts::FRAC_PI_2;
        let v = angle.sin();
        writeln!(body, "    {v}f32,").unwrap();
    }
    body.push_str("];\n");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("sin_table.rs");
    fs::write(&dest_path, body).unwrap();

    println!("cargo:rerun-if-changed=build.rs");
}

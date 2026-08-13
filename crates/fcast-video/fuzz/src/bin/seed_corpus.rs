//! Write `seeds/<target>/` from the decoders' own checked-in fixtures.
//!
//! Not a fuzz target. An ordinary binary, run by hand when a fixture changes.
//! The seeds are checked in and `corpus/` is not, so a fresh clone can replay
//! the interesting inputs (`cargo fuzz run <target> corpus/<target>
//! seeds/<target>`) without having to earn them again.

use std::{fs, path::Path};

use fcast_video::subpic::{dvb, pgs, vobsub};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
    // Both PGS targets take whole display sets as input. The raw one reads
    // them as one packet's bytes, and the structured one's `Raw` segments
    // cover the same ground once the fuzzer has minimised toward them.
    for target in ["pgs_decode", "pgs_decode_structured"] {
        write_seeds(&root, target, pgs::fixtures::seed_corpus());
    }
    // VOBSUB's seeds carry the real `.idx` beside the units, because the
    // target's input is both halves and a corpus with no palette in it would
    // never reach the parser that reads one.
    let mut vobsub_seeds = vobsub::fixtures::seed_corpus();
    vobsub_seeds.push(("sample-idx", vobsub::fixtures::SAMPLE_IDX.to_vec()));
    write_seeds(&root, "vobsub_decode", vobsub_seeds);
    write_seeds(&root, "dvb_decode", dvb::fixtures::seed_corpus());
}

fn write_seeds(root: &Path, target: &str, seeds: Vec<(&'static str, Vec<u8>)>) {
    let directory = root.join(target);
    fs::create_dir_all(&directory).expect("creating the seed directory");
    for (name, bytes) in seeds {
        fs::write(directory.join(format!("{name}.bin")), &bytes).expect("writing a seed");
        println!("{target}/{name}.bin: {} bytes", bytes.len());
    }
}

use std::rc::Rc;

use xshell::Shell;

pub mod android;
pub mod csharp;
pub mod kotlin;
pub mod mdns;
pub mod receiver;
pub mod sender;
pub mod swift;
#[allow(unused_imports)]
pub mod test_corpus;
pub mod workspace;

thread_local! {
    static SH: Rc<Shell> = Rc::new(Shell::new().unwrap())
}

pub fn sh() -> Rc<Shell> {
    SH.with(|sh| sh.clone())
}

#[derive(clap::ValueEnum, Clone)]
pub enum AndroidAbiTarget {
    X64,
    X86,
    Arm64,
    Arm32,
}

impl AndroidAbiTarget {
    pub fn translate(&self) -> &'static str {
        match self {
            Self::X64 => "x86_64-linux-android",
            Self::X86 => "i686-linux-android",
            Self::Arm64 => "aarch64-linux-android",
            Self::Arm32 => "armv7-linux-androideabi",
        }
    }
}

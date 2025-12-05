use std::rc::Rc;

use xshell::Shell;

pub mod android;
pub mod csharp;
pub mod kotlin;
pub mod mdns;
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

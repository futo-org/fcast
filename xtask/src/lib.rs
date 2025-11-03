use std::rc::Rc;

use xshell::Shell;

pub mod csharp;
pub mod android;
pub mod kotlin;
pub mod sender;
pub mod swift;
pub mod workspace;

thread_local! {
    static SH: Rc<Shell> = Rc::new(Shell::new().unwrap())
}

pub fn sh() -> Rc<Shell> {
    SH.with(|sh| sh.clone())
}

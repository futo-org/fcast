use std::rc::Rc;

use xshell::Shell;

pub mod csharp;
pub mod kotlin;
pub mod swift;
pub mod workspace;

thread_local! {
    static SH: Rc<Shell> = Rc::new(Shell::new().unwrap())
}

pub fn sh() -> Rc<Shell> {
    SH.with(|sh| sh.clone())
}

#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#![allow(unnecessary_transmutes)]
// Lints
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::ptr_offset_with_cast)]

extern crate link_cplusplus;

include!(concat!(env!("OUT_DIR"), "/placebo.rs"));

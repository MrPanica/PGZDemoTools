pub mod demo;
pub mod edit;
pub mod voice;
pub mod web;

#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../helper/src/bin/pov_cut_stable.rs"]
pub mod pov_core;

#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../helper/src/bin/pov_cut.rs"]
pub mod source_core;

#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../helper/src/bin/pov_unlock_freecam.rs"]
pub mod freecam_core;

#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../helper/src/main.rs"]
pub mod voice_core;

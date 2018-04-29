#![allow(dead_code, unused_imports)]

extern crate grep2;
extern crate grep_matcher;
#[cfg(test)]
extern crate grep_regex;
#[macro_use]
extern crate log;
#[cfg(feature = "serde1")]
extern crate serde;
#[cfg(feature = "serde1")]
#[macro_use]
extern crate serde_derive;
extern crate termcolor;

pub use color::UserColorSpec;
pub use standard::{Standard, StandardBuilder, StandardSink};
pub use stats::Stats;

#[macro_use]
mod macros;

mod ackmate;
mod color;
mod counter;
#[cfg(feature = "serde1")]
mod json;
mod standard;
mod stats;
mod util;

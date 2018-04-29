/*!
grep, as a library.
*/

#![deny(missing_docs)]

extern crate bytecount;
extern crate grep_matcher;
extern crate memchr;
#[cfg(test)]
extern crate regex;

pub use lines::{LineIter, LineStep};
pub use grep_matcher::Matcher;
pub use searcher::{
    BinaryDetection, ConfigError, MmapChoice,
    Searcher, SearcherBuilder,
};
pub use sink::{
    Sink, SinkError,
    SinkContext, SinkContextKind, SinkFinish, SinkMatch,
};
pub use sink::sinks;

/// Traits and types for the underlying matcher implementation.
pub mod matcher {
    pub use grep_matcher::*;
}

mod line_buffer;
mod lines;
mod searcher;
mod sink;
#[cfg(test)]
mod testutil;

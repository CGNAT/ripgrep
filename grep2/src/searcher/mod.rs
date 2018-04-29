use std::cell::RefCell;
use std::cmp;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};

use grep_matcher::{Match, Matcher};
use line_buffer::{
    self, BufferAllocation, LineBuffer, LineBufferBuilder, LineBufferReader,
    DEFAULT_BUFFER_CAPACITY, alloc_error,
};
use searcher::glue::{ReadByLine, SliceByLine, MultiLine};
use sink::{Sink, SinkError};

mod core;
mod glue;

/// We use this type alias since we want the ergonomics of a matcher's `Match`
/// type, but in practice, we use it for arbitrary ranges, so give it a more
/// accurate name. This is only used in the searcher's internals.
type Range = Match;

/// The behavior of binary detection while searching.
///
/// Binary detection is the process of _heuristically_ identifying whether a
/// given chunk of data is binary or not, and then taking an action based on
/// the result of that heuristic. The motivation behind detecting binary data
/// is that binary data often indicates data that is undesirable to search
/// using textual patterns. Of course, there are many cases in which this isn't
/// true, which is why binary detection is disabled by default.
///
/// Unfortunately, binary detection works differently depending on the type of
/// search being executed:
///
/// 1. When performing a search using a fixed size buffer, binary detection is
///    applied to the buffer's contents as it is filled. Binary detection must
///    be applied to the buffer directly because binary files may not contain
///    line terminators, which could result in exorbitant memory usage.
/// 2. When performing a search using memory maps or by reading data off the
///    heap, then binary detection is only guaranteed to be applied to the
///    parts corresponding to a match. When `Quit` is enabled, then the first
///    few KB of the data are searched for binary data.
#[derive(Clone, Debug, Default)]
pub struct BinaryDetection(line_buffer::BinaryDetection);

impl BinaryDetection {
    /// No binary detection is performed. Data reported by the searcher may
    /// contain arbitrary bytes.
    ///
    /// This is the default.
    pub fn none() -> BinaryDetection {
        BinaryDetection(line_buffer::BinaryDetection::None)
    }

    /// Binary detection is performed by looking for the given byte.
    ///
    /// When searching is performed using a fixed size buffer, then the
    /// contents of that buffer are always searched for the presence of this
    /// byte. If it is found, then the underlying data is considered binary
    /// and the search stops as if it reached EOF.
    ///
    /// When searching is performed with the entire contents mapped into
    /// memory, then binary detection is more conservative. Namely, only a
    /// fixed sized region at the beginning of the contents are detected for
    /// binary data. As a compromise, any subsequent matching (or context)
    /// lines are also searched for binary data. If binary data is detected at
    /// any point, then the search stops as if it reached EOF.
    pub fn quit(binary_byte: u8) -> BinaryDetection {
        BinaryDetection(line_buffer::BinaryDetection::Quit(binary_byte))
    }

    // TODO(burntsushi): Figure out how to make binary conversion work. This
    // permits implementing GNU grep's default behavior, which is to zap NUL
    // bytes but still execute a search (if a match is detected, then GNU grep
    // stops and reports that a match was found but doesn't print the matching
    // line itself).
    //
    // This behavior is pretty simple to implement using the line buffer (and
    // in fact, it is already implemented and tested), since there's a fixed
    // size buffer that we can easily write to. The issue arises when searching
    // a `&[u8]` (whether on the heap or via a memory map), since this isn't
    // something we can easily write to.

    /// The given byte is searched in all contents read by the line buffer. If
    /// it occurs, then it is replaced by the line terminator. The line buffer
    /// guarantees that this byte will never be observable by callers.
    #[allow(dead_code)]
    fn convert(binary_byte: u8) -> BinaryDetection {
        BinaryDetection(line_buffer::BinaryDetection::Convert(binary_byte))
    }
}

/// Controls the strategy used for determining when to use memory maps.
///
/// If a searcher is called in circumstances where it is possible to use memory
/// maps, then it will attempt to do so if it believes it will be advantageous.
#[derive(Clone, Debug)]
pub struct MmapChoice(MmapChoiceImpl);

#[derive(Clone, Debug)]
enum MmapChoiceImpl {
    Auto,
    Never,
}

impl Default for MmapChoice {
    fn default() -> MmapChoice {
        MmapChoice(MmapChoiceImpl::Auto)
    }
}

impl MmapChoice {
    /// Use memory maps when they are believed to be advantageous.
    ///
    /// The heuristics used to determine whether to use a memory map or not
    /// may depend on many things, including but not limited to, file size
    /// and platform.
    ///
    /// If memory maps are unavailable or cannot be used for a specific input,
    /// then normal OS read calls are used instead.
    pub fn auto() -> MmapChoice {
        MmapChoice(MmapChoiceImpl::Auto)
    }

    /// Never use memory maps, no matter what.
    pub fn never() -> MmapChoice {
        MmapChoice(MmapChoiceImpl::Never)
    }

    /// Whether this strategy may employ memory maps or not.
    fn is_enabled(&self) -> bool {
        match self.0 {
            MmapChoiceImpl::Auto => true,
            MmapChoiceImpl::Never => false,
        }
    }
}

/// The internal configuration of a searcher. This is shared among several
/// search related types, but is only ever written to by the SearcherBuilder.
#[derive(Clone, Debug)]
pub struct Config {
    /// The line terminator to use.
    line_term: u8,
    /// Whether to invert matching.
    invert_match: bool,
    /// The number of lines after a match to include.
    after_context: usize,
    /// The number of lines before a match to include.
    before_context: usize,
    /// Whether to count line numbers.
    line_number: bool,
    /// The maximum amount of heap memory to use.
    ///
    /// When not given, no explicit limit is enforced. When set to `0`, then
    /// only the memory map search strategy is available.
    heap_limit: Option<usize>,
    /// The memory map strategy.
    mmap: MmapChoice,
    /// The binary data detection strategy.
    binary: BinaryDetection,
    /// Whether to enable matching across multiple lines.
    multi_line: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            line_term: b'\n',
            invert_match: false,
            after_context: 0,
            before_context: 0,
            line_number: false,
            heap_limit: None,
            mmap: MmapChoice::default(),
            binary: BinaryDetection::default(),
            multi_line: false,
        }
    }
}

impl Config {
    /// Return the maximal amount of lines needed to fulfill this
    /// configuration's context.
    ///
    /// If this returns `0`, then no context is ever needed.
    fn max_context(&self) -> usize {
        cmp::max(self.before_context, self.after_context)
    }

    /// Build a line buffer from this configuration.
    fn line_buffer(&self) -> LineBuffer {
        let mut builder = LineBufferBuilder::new();
        builder
            .line_terminator(self.line_term)
            .binary_detection(self.binary.0);

        if let Some(limit) = self.heap_limit {
            let (capacity, additional) =
                if limit <= DEFAULT_BUFFER_CAPACITY {
                    (limit, 0)
                } else {
                    (DEFAULT_BUFFER_CAPACITY, limit - DEFAULT_BUFFER_CAPACITY)
                };
            builder
                .capacity(capacity)
                .buffer_alloc(BufferAllocation::Error(additional));
        }
        builder.build()
    }
}

/// An error that can occur when building a searcher.
///
/// This error occurs when a non-sensical configuration is present when trying
/// to construct a `Searcher` from a `SearcherBuilder`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// Indicates that the heap limit configuration prevents all possible
    /// search strategies from being used. For example, if the heap limit is
    /// set to 0 and memory map searching is disabled or unavailable.
    SearchUnavailable,
    /// Occurs when a matcher reports a line terminator that is different than
    /// the one configured in the searcher.
    MismatchedLineTerminators {
        /// The matcher's line terminator.
        matcher: u8,
        /// The searcher's line terminator.
        searcher: u8,
    },
    /// Hints that destructuring should not be exhaustive.
    ///
    /// This enum may grow additional variants, so this makes sure clients
    /// don't count on exhaustive matching. (Otherwise, adding a new variant
    /// could break existing code.)
    #[doc(hidden)]
    __Nonexhaustive,
}

impl ::std::error::Error for ConfigError {
    fn description(&self) -> &str { "grep configuration error" }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ConfigError::SearchUnavailable => {
                write!(f, "grep config error: no available searchers")
            }
            ConfigError::MismatchedLineTerminators { matcher, searcher } => {
                write!(
                    f,
                    "grep config error: mismatched line terminators, \
                     matcher has 0x{:02X} but searcher has 0x{:02X}",
                    matcher,
                    searcher
                )
            }
            _ => panic!("BUG: unexpected variant found"),
        }
    }
}

/// A builder for configuring a searcher.
///
/// A search builder permits specifying the configuration of a searcher,
/// including options like whether to invert the search or to enable multi
/// line search.
///
/// Once a searcher has been built, it is beneficial to reuse that searcher
/// for multiple searches, if possible.
#[derive(Clone, Debug)]
pub struct SearcherBuilder {
    config: Config,
}

impl Default for SearcherBuilder {
    fn default() -> SearcherBuilder {
        SearcherBuilder::new()
    }
}

impl SearcherBuilder {
    /// Create a new searcher builder with a default configuration.
    pub fn new() -> SearcherBuilder {
        SearcherBuilder {
            config: Config::default(),
        }
    }

    /// Builder a searcher with the given matcher.
    ///
    /// Building a searcher can fail if the configuration specified is invalid.
    /// For example, if the heap limit is set to `0` and memory maps are
    /// disabled, then most searches will fail. Another example is if the given
    /// matcher has a line terminator set that is inconsistent with the line
    /// terminator set in this builder.
    pub fn build(&self) -> Result<Searcher, ConfigError> {
        if self.config.heap_limit == Some(0)
            && !self.config.mmap.is_enabled()
        {
            return Err(ConfigError::SearchUnavailable);
        // } else if let Some(matcher_line_term) = matcher.line_terminator() {
            // if matcher_line_term != self.config.line_term {
                // return Err(ConfigError::MismatchedLineTerminators {
                    // matcher: matcher_line_term,
                    // searcher: self.config.line_term,
                // });
            // }
        }
        Ok(Searcher {
            config: self.config.clone(),
            // matcher: matcher,
            line_buffer: RefCell::new(self.config.line_buffer()),
            multi_line_buffer: vec![],
        })
    }

    /// Set the line terminator that is used by the searcher.
    ///
    /// When building a searcher, if the matcher provided has a line terminator
    /// set, then it must be the same as this one. If they aren't, building
    /// a searcher will return an error.
    ///
    /// By default, this is set to `b'\n'`.
    pub fn line_terminator(&mut self, line_term: u8) -> &mut SearcherBuilder {
        self.config.line_term = line_term;
        self
    }

    /// Whether to invert matching, whereby lines that don't match are reported
    /// instead of reporting lines that do match.
    ///
    /// By default, this is disabled.
    pub fn invert_match(&mut self, yes: bool) -> &mut SearcherBuilder {
        self.config.invert_match = yes;
        self
    }

    /// Whether to count and include line numbers with matching lines.
    ///
    /// This is disabled by default. In particular, counting line numbers has
    /// a small performance cost, so it's best not to do it unless they are
    /// needed.
    pub fn line_number(&mut self, yes: bool) -> &mut SearcherBuilder {
        self.config.line_number = yes;
        self
    }

    /// Whether to enable multi line search or not.
    ///
    /// When multi line search is enabled, matches *may* match across multiple
    /// lines. Conversely, when multi line search is disabled, it is impossible
    /// for any match to span more than one line.
    ///
    /// **Warning:** multi line search requires having the entire contents to
    /// search mapped in memory at once. When searching files, memory maps
    /// will be used if possible, which avoids using your program's heap.
    /// However, if memory maps cannot be used (e.g., for searching streams
    /// like `stdin`), then the entire contents of the stream are read on to
    /// the heap before starting the search.
    ///
    /// This is disabled by default.
    pub fn multi_line(&mut self, yes: bool) -> &mut SearcherBuilder {
        self.config.multi_line = yes;
        self
    }

    /// Whether to include a fixed number of lines after every match.
    ///
    /// When this is set to a non-zero number, then the searcher will report
    /// `line_count` contextual lines after every match.
    ///
    /// This is set to `0` by default.
    pub fn after_context(
        &mut self,
        line_count: usize,
    ) -> &mut SearcherBuilder {
        self.config.after_context = line_count;
        self
    }

    /// Whether to include a fixed number of lines before every match.
    ///
    /// When this is set to a non-zero number, then the searcher will report
    /// `line_count` contextual lines before every match.
    ///
    /// This is set to `0` by default.
    pub fn before_context(
        &mut self,
        line_count: usize,
    ) -> &mut SearcherBuilder {
        self.config.before_context = line_count;
        self
    }

    /// Set an approximate limit on the amount of heap space used by a
    /// searcher.
    ///
    /// The heap limit is enforced in two scenarios:
    ///
    /// * When searching using a fixed size buffer, the heap limit controls
    ///   how big this buffer is allowed to be. Assuming contexts are disabled,
    ///   the minimum size of this buffer is the length (in bytes) of the
    ///   largest single line in the contents being searched. If any line
    ///   exceeds the heap limit, then an error will be returned.
    /// * When performing a multi line search, a fixed size buffer cannot be
    ///   used. Thus, the only choices are to read the entire contents on to
    ///   the heap, or use memory maps. In the former case, the heap limit set
    ///   here is enforced.
    ///
    /// By default, no limit is set.
    pub fn heap_limit(
        &mut self,
        bytes: Option<usize>,
    ) -> &mut SearcherBuilder {
        self.config.heap_limit = bytes;
        self
    }

    /// Set the strategy to employ use of memory maps.
    ///
    /// Currently, there are only two strategies that can be employed:
    ///
    /// * **Automatic** - A searcher will use heuristics, including but not
    ///   limited to file size and platform, to determine whether to use memory
    ///   maps or not.
    /// * **Never** - Memory maps will never be used. If multi line search is
    ///   enabled, then the entire contents will be read on to the heap before
    ///   searching begins.
    ///
    /// The default behavior is **automatic**. The only reason to disable
    /// memory maps explicitly is if there are concerns using them. For
    /// example, if your process is searching a file backed memory map at the
    /// same time that file is truncated, then it's possible for the process to
    /// terminate with a bus error.
    pub fn memory_map(
        &mut self,
        strategy: MmapChoice,
    ) -> &mut SearcherBuilder {
        self.config.mmap = strategy;
        self
    }

    /// Set the binary detection strategy.
    ///
    /// The binary detection strategy determines not only how the searcher
    /// detects binary data, but how it responds to the presence of binary
    /// data. See the [`BinaryDetection`](struct.BinaryDetection.html) type
    /// for more information.
    ///
    /// By default, binary detection is disabled.
    pub fn binary_detection(
        &mut self,
        detection: BinaryDetection,
    ) -> &mut SearcherBuilder {
        self.config.binary = detection;
        self
    }
}

/// A searcher executes searches over a haystack and writes results to a caller
/// provided sink. Matches are detected via implementations of the `Matcher`
/// trait, which is represented by the `M` type parameter.
///
/// When possible, a single searcher should be reused.
#[derive(Debug)]
pub struct Searcher {
    /// The configuration for this searcher.
    ///
    /// We make most of these settings available to users of `Searcher` via
    /// public API methods, which can be queried in implementations of `Sink`
    /// if necessary.
    config: Config,
    /// A line buffer for use in line oriented searching.
    ///
    /// We wrap it in a RefCell to permit lending out borrows of `Searcher`
    /// to sinks. We still require a mutable borrow to execute a search, so
    /// we statically prevent callers from causing RefCell to panic at runtime
    /// due to a borrowing violation.
    line_buffer: RefCell<LineBuffer>,
    /// A buffer in which to store the contents of a reader when performing a
    /// multi line search. In particular, multi line searches cannot be
    /// performed incrementally, and need the entire haystack in memory at
    /// once.
    ///
    /// (This isn't `RefCell` like `line_buffer` because it is never mutated.)
    multi_line_buffer: Vec<u8>,
}

impl Searcher {
    /// Execute a search over any implementation of `io::Read` and write the
    /// results to the given sink.
    ///
    /// When possible, this implementation will search the reader incrementally
    /// without reading it into memory. In some cases---for example, if multi
    /// line search is enabled---an incremental search isn't possible and the
    /// given reader is consumed completely and placed on the heap before
    /// searching begins. For this reason, when multi line search is enabled,
    /// one should try to use higher level APIs (e.g., searching by file or
    /// file path) so that memory maps can be used if they are available.
    pub fn search_reader<M, R, S>(
        &mut self,
        matcher: M,
        read_from: R,
        write_to: S,
    ) -> Result<(), S::Error>
    where M: Matcher,
          R: io::Read,
          S: Sink,
    {
        if self.config.multi_line {
            self.fill_multi_line_buffer_from_reader::<R, S>(read_from)?;
            MultiLine::new(
                self,
                matcher,
                &self.multi_line_buffer,
                write_to,
            ).run()
        } else {
            let mut line_buffer = self.line_buffer.borrow_mut();
            let rdr = LineBufferReader::new(read_from, &mut *line_buffer);
            ReadByLine::new(self, matcher, rdr, write_to).run()
        }
    }

    /// Execute a search over the given slice and write the results to the
    /// given sink.
    pub fn search_slice<M, S>(
        &mut self,
        matcher: M,
        slice: &[u8],
        write_to: S,
    ) -> Result<(), S::Error>
    where M: Matcher,
          S: Sink,
    {
        if self.config.multi_line {
            MultiLine::new(self, matcher, slice, write_to).run()
        } else {
            SliceByLine::new(self, matcher, slice, write_to).run()
        }
    }
}

impl Searcher {
    /// Returns the line terminator used by this searcher.
    pub fn line_terminator(&self) -> u8 {
        self.config.line_term
    }

    /// Returns true if and only if this searcher is configured to invert its
    /// search results. That is, matching lines are lines that do **not** match
    /// the searcher's matcher.
    pub fn invert_match(&self) -> bool {
        self.config.invert_match
    }

    /// Returns true if and only if this searcher is configured to count line
    /// numbers.
    pub fn line_number(&self) -> bool {
        self.config.line_number
    }

    /// Returns true if and only if this searcher is configured to perform
    /// multi line search.
    pub fn multi_line(&self) -> bool {
        self.config.multi_line
    }

    /// Returns the number of "after" context lines to report. When context
    /// reporting is not enabled, this returns `0`.
    pub fn after_context(&self) -> usize {
        self.config.after_context
    }

    /// Returns the number of "before" context lines to report. When context
    /// reporting is not enabled, this returns `0`.
    pub fn before_context(&self) -> usize {
        self.config.before_context
    }

    /// Fill the buffer for use with multi-line searching from the given file.
    /// This reads from the file until EOF or until an error occurs. If the
    /// contents exceed the configured heap limit, then an error is returned.
    #[allow(dead_code)]
    fn fill_multi_line_buffer_from_file<S: Sink>(
        &mut self,
        mut read_from: &File,
    ) -> Result<(), S::Error> {
        assert!(self.config.multi_line);

        // If we don't have a heap limit, then we can defer to std's
        // read_to_end implementation. fill_multi_line_buffer_from_reader will
        // do this too, but since we have a File, we can be a bit smarter about
        // pre-allocating here.
        if self.config.heap_limit.is_none() {
            let buf = &mut self.multi_line_buffer;
            buf.clear();
            let cap = read_from
                .metadata()
                .map(|m| m.len() as usize + 1)
                .unwrap_or(0);
            buf.reserve(cap);
            read_from.read_to_end(buf).map_err(S::Error::error_io)?;
        }
        self.fill_multi_line_buffer_from_reader::<&File, S>(read_from)
    }

    /// Fill the buffer for use with multi-line searching from the given
    /// reader. This reads from the reader until EOF or until an error occurs.
    /// If the contents exceed the configured heap limit, then an error is
    /// returned.
    fn fill_multi_line_buffer_from_reader<R: io::Read, S: Sink>(
        &mut self,
        mut read_from: R,
    ) -> Result<(), S::Error> {
        assert!(self.config.multi_line);

        let buf = &mut self.multi_line_buffer;
        buf.clear();

        // If we don't have a heap limit, then we can defer to std's
        // read_to_end implementation...
        let heap_limit = match self.config.heap_limit {
            Some(heap_limit) => heap_limit,
            None => {
                read_from.read_to_end(buf).map_err(S::Error::error_io)?;
                return Ok(());
            }
        };
        if heap_limit == 0 {
            return Err(S::Error::error_io(alloc_error(heap_limit)));
        }

        // ... otherwise we need to roll our own. This is likely quite a bit
        // slower than what is optimal, but we avoid `unsafe` until there's a
        // compelling reason to speed this up.
        buf.resize(cmp::min(DEFAULT_BUFFER_CAPACITY, heap_limit), 0);
        let mut pos = 0;
        loop {
            let nread = match read_from.read(&mut buf[pos..]) {
                Ok(nread) => nread,
                Err(ref err) if err.kind() == io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(err) => return Err(S::Error::error_io(err)),
            };
            if nread == 0 {
                buf.resize(pos, 0);
                return Ok(());
            }

            pos += nread;
            if buf[pos..].is_empty() {
                let additional = heap_limit - buf.len();
                if additional == 0 {
                    return Err(S::Error::error_io(alloc_error(heap_limit)));
                }
                let limit = buf.len() + additional;
                let doubled = 2 * buf.len();
                buf.resize(cmp::min(doubled, limit), 0);
            }
        }
    }
}

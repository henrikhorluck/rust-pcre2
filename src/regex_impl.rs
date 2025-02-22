use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::ops::Index;
use std::sync::Arc;

use log::debug;
use pcre2_sys::{
    PCRE2_CASELESS, PCRE2_DOTALL, PCRE2_EXTENDED, PCRE2_MULTILINE, PCRE2_NEVER_UTF,
    PCRE2_NEWLINE_ANYCRLF, PCRE2_NO_UTF_CHECK, PCRE2_UCP, PCRE2_UNSET, PCRE2_UTF,
};
use thread_local::ThreadLocal;

use crate::error::Error;
use crate::ffi::{Code, CodeUnitWidth, CompileContext, MatchConfig, MatchData};

/// Match represents a single match of a regex in a subject string.
///
/// The lifetime parameter `'s` refers to the lifetime of the matched portion
/// of the subject string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Match<'s, W: CodeUnitWidth> {
    subject: &'s [W::SubjectChar],
    start: usize,
    end: usize,
}

impl<'s, W: CodeUnitWidth> Match<'s, W> {
    /// Returns the starting byte offset of the match in the subject.
    #[inline]
    pub fn start(&self) -> usize {
        self.start
    }

    /// Returns the ending byte offset of the match in the subject.
    #[inline]
    pub fn end(&self) -> usize {
        self.end
    }

    /// Returns the matched portion of the subject string.
    #[inline]
    pub fn as_bytes(&self) -> &'s [W::SubjectChar] {
        &self.subject[self.start..self.end]
    }

    /// Creates a new match from the given subject string and byte offsets.
    fn new(subject: &'s [W::SubjectChar], start: usize, end: usize) -> Self {
        Match {
            subject,
            start,
            end,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_pair(&self) -> (usize, usize) {
        (self.start, self.end)
    }
}

#[derive(Clone, Debug)]
struct Config {
    /// PCRE2_CASELESS
    caseless: bool,
    /// PCRE2_DOTALL
    dotall: bool,
    /// PCRE2_EXTENDED
    extended: bool,
    /// PCRE2_MULTILINE
    multi_line: bool,
    /// PCRE2_NEWLINE_ANYCRLF
    crlf: bool,
    /// PCRE2_UCP
    ucp: bool,
    /// PCRE2_UTF
    utf: bool,
    /// PCRE2_NEVER_UTF
    never_utf: bool,
    /// PCRE2_NO_UTF_CHECK
    utf_check: bool,
    /// use pcre2_jit_compile
    jit: JITChoice,
    /// Match-time specific configuration knobs.
    match_config: MatchConfig,
}

#[derive(Clone, Debug)]
enum JITChoice {
    /// Never do JIT compilation.
    Never,
    /// Always do JIT compilation and return an error if it fails.
    Always,
    /// Attempt to do JIT compilation but silently fall back to non-JIT.
    Attempt,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            caseless: false,
            dotall: false,
            extended: false,
            multi_line: false,
            crlf: false,
            ucp: false,
            utf: false,
            never_utf: false,
            utf_check: true,
            jit: JITChoice::Never,
            match_config: MatchConfig::default(),
        }
    }
}

/// A builder for configuring the compilation of a PCRE2 regex.
/// This takes a phantom parameter to aid type inference.
#[derive(Clone, Debug)]
pub struct RegexBuilder<W: CodeUnitWidth> {
    config: Config,
    _phantom: std::marker::PhantomData<W>,
}

impl<W: CodeUnitWidth> RegexBuilder<W> {
    /// Create a new builder with a default configuration.
    pub fn new() -> Self {
        RegexBuilder {
            config: Config::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Compile the given pattern into a PCRE regex using the current
    /// configuration.
    ///
    /// If there was a problem compiling the pattern, then an error is
    /// returned.
    pub fn build<Pat: Into<W::Pattern>>(&self, pattern: Pat) -> Result<Regex<W>, Error> {
        let mut options = 0;
        if self.config.caseless {
            options |= PCRE2_CASELESS;
        }
        if self.config.dotall {
            options |= PCRE2_DOTALL;
        }
        if self.config.extended {
            options |= PCRE2_EXTENDED;
        }
        if self.config.multi_line {
            options |= PCRE2_MULTILINE;
        }
        if self.config.ucp {
            options |= PCRE2_UCP;
            options |= PCRE2_UTF;
        }
        if self.config.utf {
            options |= PCRE2_UTF;
        }
        if self.config.never_utf {
            options |= PCRE2_NEVER_UTF;
        }

        let mut ctx = CompileContext::new();
        if self.config.crlf {
            ctx.set_newline(PCRE2_NEWLINE_ANYCRLF)
                .expect("PCRE2_NEWLINE_ANYCRLF is a legal value");
        }

        let pattern = pattern.into();
        let mut code = Code::new(&pattern, options, ctx)?;
        match self.config.jit {
            JITChoice::Never => {} // fallthrough
            JITChoice::Always => {
                code.jit_compile()?;
            }
            JITChoice::Attempt => {
                if let Err(err) = code.jit_compile() {
                    debug!("JIT compilation failed: {}", err);
                }
            }
        }
        let capture_names = code.capture_names()?;
        let mut idx = HashMap::new();
        for (i, group) in capture_names.iter().enumerate() {
            if let Some(ref name) = *group {
                idx.insert(name.to_string(), i);
            }
        }
        Ok(Regex {
            config: Arc::new(self.config.clone()),
            pattern: pattern,
            code: Arc::new(code),
            capture_names: Arc::new(capture_names),
            capture_names_idx: Arc::new(idx),
            match_data: ThreadLocal::new(),
        })
    }

    /// Enables case insensitive matching.
    ///
    /// If the `utf` option is also set, then Unicode case folding is used
    /// to determine case insensitivity. When the `utf` option is not set,
    /// then only standard ASCII case insensitivity is considered.
    ///
    /// This option corresponds to the `i` flag.
    pub fn caseless(&mut self, yes: bool) -> &mut Self {
        self.config.caseless = yes;
        self
    }

    /// Enables "dot all" matching.
    ///
    /// When enabled, the `.` metacharacter in the pattern matches any
    /// character, include `\n`. When disabled (the default), `.` will match
    /// any character except for `\n`.
    ///
    /// This option corresponds to the `s` flag.
    pub fn dotall(&mut self, yes: bool) -> &mut Self {
        self.config.dotall = yes;
        self
    }

    /// Enable "extended" mode in the pattern, where whitespace is ignored.
    ///
    /// This option corresponds to the `x` flag.
    pub fn extended(&mut self, yes: bool) -> &mut Self {
        self.config.extended = yes;
        self
    }

    /// Enable multiline matching mode.
    ///
    /// When enabled, the `^` and `$` anchors will match both at the beginning
    /// and end of a subject string, in addition to matching at the start of
    /// a line and the end of a line. When disabled, the `^` and `$` anchors
    /// will only match at the beginning and end of a subject string.
    ///
    /// This option corresponds to the `m` flag.
    pub fn multi_line(&mut self, yes: bool) -> &mut Self {
        self.config.multi_line = yes;
        self
    }

    /// Enable matching of CRLF as a line terminator.
    ///
    /// When enabled, anchors such as `^` and `$` will match any of the
    /// following as a line terminator: `\r`, `\n` or `\r\n`.
    ///
    /// This is disabled by default, in which case, only `\n` is recognized as
    /// a line terminator.
    pub fn crlf(&mut self, yes: bool) -> &mut Self {
        self.config.crlf = yes;
        self
    }

    /// Enable Unicode matching mode.
    ///
    /// When enabled, the following patterns become Unicode aware: `\b`, `\B`,
    /// `\d`, `\D`, `\s`, `\S`, `\w`, `\W`.
    ///
    /// When set, this implies UTF matching mode. It is not possible to enable
    /// Unicode matching mode without enabling UTF matching mode.
    ///
    /// This is disabled by default.
    pub fn ucp(&mut self, yes: bool) -> &mut Self {
        self.config.ucp = yes;
        self
    }

    /// Enable UTF matching mode.
    ///
    /// When enabled, characters are treated as sequences of code units that
    /// make up a single codepoint instead of as single bytes. For example,
    /// this will cause `.` to match any single UTF-8 encoded codepoint, where
    /// as when this is disabled, `.` will any single byte (except for `\n` in
    /// both cases, unless "dot all" mode is enabled).
    ///
    /// Note that when UTF matching mode is enabled, every search performed
    /// will do a UTF-8 validation check, which can impact performance. The
    /// UTF-8 check can be disabled via the `disable_utf_check` option, but it
    /// is undefined behavior to enable UTF matching mode and search invalid
    /// UTF-8.
    ///
    /// This is disabled by default.
    pub fn utf(&mut self, yes: bool) -> &mut Self {
        self.config.utf = yes;
        self
    }

    /// Prevent patterns from opting in to UTF matching mode.
    ///
    /// This disables the sequence `(*UTF)` from switching to UTF mode.
    pub fn never_utf(&mut self, yes: bool) -> &mut Self {
        self.config.never_utf = yes;
        self
    }

    /// When UTF matching mode is enabled, this will disable the UTF checking
    /// that PCRE2 will normally perform automatically. If UTF matching mode
    /// is not enabled, then this has no effect.
    ///
    /// UTF checking is enabled by default when UTF matching mode is enabled.
    /// If UTF matching mode is enabled and UTF checking is enabled, then PCRE2
    /// will return an error if you attempt to search a subject string that is
    /// not valid UTF-8.
    ///
    /// # Safety
    ///
    /// It is undefined behavior to disable the UTF check in UTF matching mode
    /// and search a subject string that is not valid UTF-8. When the UTF check
    /// is disabled, callers must guarantee that the subject string is valid
    /// UTF-8.
    pub unsafe fn disable_utf_check(&mut self) -> &mut Self {
        self.config.utf_check = false;
        self
    }

    /// Enable PCRE2's JIT and return an error if it's not available.
    ///
    /// This generally speeds up matching quite a bit. The downside is that it
    /// can increase the time it takes to compile a pattern.
    ///
    /// If the JIT isn't available or if JIT compilation returns an error, then
    /// regex compilation will fail with the corresponding error.
    ///
    /// This is disabled by default, and always overrides `jit_if_available`.
    pub fn jit(&mut self, yes: bool) -> &mut Self {
        if yes {
            self.config.jit = JITChoice::Always;
        } else {
            self.config.jit = JITChoice::Never;
        }
        self
    }

    /// Enable PCRE2's JIT if it's available.
    ///
    /// This generally speeds up matching quite a bit. The downside is that it
    /// can increase the time it takes to compile a pattern.
    ///
    /// If the JIT isn't available or if JIT compilation returns an error,
    /// then a debug message with the error will be emitted and the regex will
    /// otherwise silently fall back to non-JIT matching.
    ///
    /// This is disabled by default, and always overrides `jit`.
    pub fn jit_if_available(&mut self, yes: bool) -> &mut Self {
        if yes {
            self.config.jit = JITChoice::Attempt;
        } else {
            self.config.jit = JITChoice::Never;
        }
        self
    }

    /// Set the maximum size of PCRE2's JIT stack, in bytes. If the JIT is
    /// not enabled, then this has no effect.
    ///
    /// When `None` is given, no custom JIT stack will be created, and instead,
    /// the default JIT stack is used. When the default is used, its maximum
    /// size is 32 KB.
    ///
    /// When this is set, then a new JIT stack will be created with the given
    /// maximum size as its limit.
    ///
    /// Increasing the stack size can be useful for larger regular expressions.
    ///
    /// By default, this is set to `None`.
    pub fn max_jit_stack_size(&mut self, bytes: Option<usize>) -> &mut Self {
        self.config.match_config.max_jit_stack_size = bytes;
        self
    }
}

/// A compiled PCRE2 regular expression.
///
/// This regex is safe to use from multiple threads simultaneously. For top
/// performance, it is better to clone a new regex for each thread.
pub struct Regex<W: CodeUnitWidth> {
    /// The configuration used to build the regex.
    config: Arc<Config>,
    /// The original pattern string.
    pattern: W::Pattern,
    /// The underlying compiled PCRE2 object.
    code: Arc<Code<W>>,
    /// The capture group names for this regex.
    capture_names: Arc<Vec<Option<String>>>,
    /// A map from capture group name to capture group index.
    capture_names_idx: Arc<HashMap<String, usize>>,
    /// Mutable scratch data used by PCRE2 during matching.
    ///
    /// We use the same strategy as Rust's regex crate here, such that each
    /// thread gets its own match data to support using a Regex object from
    /// multiple threads simultaneously. If some match data doesn't exist for
    /// a thread, then a new one is created on demand.
    match_data: ThreadLocal<RefCell<MatchData<W>>>,
}

impl<W: CodeUnitWidth> Clone for Regex<W> {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            pattern: self.pattern.clone(),
            code: Arc::clone(&self.code),
            capture_names: Arc::clone(&self.capture_names),
            capture_names_idx: Arc::clone(&self.capture_names_idx),
            match_data: ThreadLocal::new(),
        }
    }
}

impl<W: CodeUnitWidth> fmt::Debug for Regex<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Regex({:?})", self.pattern)
    }
}

impl<W: CodeUnitWidth> Regex<W> {
    /// Compiles a regular expression using the default configuration.
    ///
    /// Once compiled, it can be used repeatedly to search, split or replace
    /// text in a string.
    ///
    /// If an invalid expression is given, then an error is returned.
    ///
    /// To configure compilation options for the regex, use the
    /// [`RegexBuilder`](struct.RegexBuilder.html).
    pub fn new<Pat: Into<W::Pattern>>(pattern: Pat) -> Result<Self, Error> {
        RegexBuilder::new().build(pattern)
    }

    /// Returns true if and only if the regex matches the subject string given.
    pub fn is_match(&self, subject: &[W::SubjectChar]) -> Result<bool, Error> {
        self.is_match_at(subject, 0)
    }

    /// Returns the start and end byte range of the leftmost-first match in
    /// `subject`. If no match exists, then `None` is returned.
    pub fn find<'s>(&self, subject: &'s [W::SubjectChar]) -> Result<Option<Match<'s, W>>, Error> {
        self.find_at(subject, 0)
    }

    /// Returns an iterator for each successive non-overlapping match in
    /// `subject`, returning the start and end byte indices with respect to
    /// `subject`.
    pub fn find_iter<'r, 's>(&'r self, subject: &'s [W::SubjectChar]) -> Matches<'r, 's, W> {
        Matches {
            re: self,
            match_data: self.match_data(),
            subject: subject,
            last_end: 0,
            last_match: None,
        }
    }

    /// Returns the capture groups corresponding to the leftmost-first
    /// match in `subject`. Capture group `0` always corresponds to the entire
    /// match. If no match is found, then `None` is returned.
    ///
    /// The `0`th capture group is always unnamed, so it must always be
    /// accessed with `get(0)` or `[0]`.
    pub fn captures<'s>(
        &self,
        subject: &'s [W::SubjectChar],
    ) -> Result<Option<Captures<'s, W>>, Error> {
        let mut locs = self.capture_locations();
        Ok(self
            .captures_read(&mut locs, subject)?
            .map(move |_| Captures {
                subject,
                locs: locs,
                idx: Arc::clone(&self.capture_names_idx),
            }))
    }

    /// Returns an iterator over all the non-overlapping capture groups matched
    /// in `subject`. This is operationally the same as `find_iter`, except it
    /// yields information about capturing group matches.
    pub fn captures_iter<'r, 's>(
        &'r self,
        subject: &'s [W::SubjectChar],
    ) -> CaptureMatches<'r, 's, W> {
        CaptureMatches {
            re: self,
            subject: subject,
            last_end: 0,
            last_match: None,
        }
    }

    /// Test helper to access capture name indexes.
    #[cfg(test)]
    pub(crate) fn get_capture_names_idxs(&self) -> &HashMap<String, usize> {
        &self.capture_names_idx
    }
}

/// Advanced or  "lower level" search methods.
impl<W: CodeUnitWidth> Regex<W> {
    /// Returns the same as is_match, but starts the search at the given
    /// offset.
    ///
    /// The significance of the starting point is that it takes the surrounding
    /// context into consideration. For example, the `\A` anchor can only
    /// match when `start == 0`.
    pub fn is_match_at(&self, subject: &[W::SubjectChar], start: usize) -> Result<bool, Error> {
        assert!(
            start <= subject.len(),
            "start ({}) must be <= subject.len() ({})",
            start,
            subject.len()
        );

        let mut options = 0;
        if !self.config.utf_check {
            options |= PCRE2_NO_UTF_CHECK;
        }

        let match_data = self.match_data();
        let mut match_data = match_data.borrow_mut();
        // SAFETY: The only unsafe PCRE2 option we potentially use here is
        // PCRE2_NO_UTF_CHECK, and that only occurs if the caller executes the
        // `disable_utf_check` method, which propagates the safety contract to
        // the caller.
        Ok(unsafe { match_data.find(&self.code, subject, start, options)? })
    }

    /// Returns the same as find, but starts the search at the given
    /// offset.
    ///
    /// The significance of the starting point is that it takes the surrounding
    /// context into consideration. For example, the `\A` anchor can only
    /// match when `start == 0`.
    pub fn find_at<'s>(
        &self,
        subject: &'s [W::SubjectChar],
        start: usize,
    ) -> Result<Option<Match<'s, W>>, Error> {
        self.find_at_with_match_data(self.match_data(), subject, start)
    }

    /// Like find_at, but accepts match data instead of acquiring one itself.
    ///
    /// This is useful for implementing the iterator, which permits avoiding
    /// the synchronization overhead of acquiring the match data.
    #[inline(always)]
    fn find_at_with_match_data<'s>(
        &self,
        match_data: &RefCell<MatchData<W>>,
        subject: &'s [W::SubjectChar],
        start: usize,
    ) -> Result<Option<Match<'s, W>>, Error> {
        assert!(
            start <= subject.len(),
            "start ({}) must be <= subject.len() ({})",
            start,
            subject.len()
        );

        let mut options = 0;
        if !self.config.utf_check {
            options |= PCRE2_NO_UTF_CHECK;
        }

        let mut match_data = match_data.borrow_mut();
        // SAFETY: The only unsafe PCRE2 option we potentially use here is
        // PCRE2_NO_UTF_CHECK, and that only occurs if the caller executes the
        // `disable_utf_check` method, which propagates the safety contract to
        // the caller.
        if unsafe { !match_data.find(&self.code, subject, start, options)? } {
            return Ok(None);
        }
        let ovector = match_data.ovector();
        let (s, e) = (ovector[0], ovector[1]);
        Ok(Some(Match::new(&subject[s..e], s, e)))
    }

    /// This is like `captures`, but uses
    /// [`CaptureLocations`](struct.CaptureLocations.html)
    /// instead of
    /// [`Captures`](struct.Captures.html) in order to amortize allocations.
    ///
    /// To create a `CaptureLocations` value, use the
    /// `Regex::capture_locations` method.
    ///
    /// This returns the overall match if this was successful, which is always
    /// equivalent to the `0`th capture group.
    pub fn captures_read<'s>(
        &self,
        locs: &mut CaptureLocations<W>,
        subject: &'s [W::SubjectChar],
    ) -> Result<Option<Match<'s, W>>, Error> {
        self.captures_read_at(locs, subject, 0)
    }

    /// Returns the same as `captures_read`, but starts the search at the given
    /// offset and populates the capture locations given.
    ///
    /// The significance of the starting point is that it takes the surrounding
    /// context into consideration. For example, the `\A` anchor can only
    /// match when `start == 0`.
    pub fn captures_read_at<'s>(
        &self,
        locs: &mut CaptureLocations<W>,
        subject: &'s [W::SubjectChar],
        start: usize,
    ) -> Result<Option<Match<'s, W>>, Error> {
        assert!(
            start <= subject.len(),
            "start ({}) must be <= subject.len() ({})",
            start,
            subject.len()
        );

        let mut options = 0;
        if !self.config.utf_check {
            options |= PCRE2_NO_UTF_CHECK;
        }
        // SAFETY: The only unsafe PCRE2 option we potentially use here is
        // PCRE2_NO_UTF_CHECK, and that only occurs if the caller executes the
        // `disable_utf_check` method, which propagates the safety contract to
        // the caller.
        if unsafe { !locs.data.find(&self.code, subject, start, options)? } {
            return Ok(None);
        }
        let ovector = locs.data.ovector();
        let (s, e) = (ovector[0], ovector[1]);
        Ok(Some(Match::new(&subject[s..e], s, e)))
    }
}

/// Auxiliary methods.
impl<W: CodeUnitWidth> Regex<W> {
    /// Returns the original pattern string for this regex.
    pub fn as_str(&self) -> &W::Pattern {
        &self.pattern
    }

    /// Returns a sequence of all capturing groups and their names, if present.
    ///
    /// The length of the slice returned is always equal to the result of
    /// `captures_len`, which is the number of capturing groups (including the
    /// capturing group for the entire pattern).
    ///
    /// Each entry in the slice is the name of the corresponding capturing
    /// group, if one exists. The first capturing group (at index `0`) is
    /// always unnamed.
    ///
    /// Capturing groups are indexed by the order of the opening parenthesis.
    pub fn capture_names(&self) -> &[Option<String>] {
        &self.capture_names
    }

    /// Returns the number of capturing groups in the pattern.
    ///
    /// This is always 1 more than the number of syntactic groups in the
    /// pattern, since the first group always corresponds to the entire match.
    pub fn captures_len(&self) -> usize {
        self.code
            .capture_count()
            .expect("a valid capture count from PCRE2")
    }

    /// Returns an empty set of capture locations that can be reused in
    /// multiple calls to `captures_read` or `captures_read_at`.
    pub fn capture_locations(&self) -> CaptureLocations<W> {
        CaptureLocations {
            code: Arc::clone(&self.code),
            data: self.new_match_data(),
        }
    }

    fn match_data(&self) -> &RefCell<MatchData<W>> {
        let create = || RefCell::new(self.new_match_data());
        self.match_data.get_or(create)
    }

    fn new_match_data(&self) -> MatchData<W> {
        MatchData::new(self.config.match_config.clone(), &self.code)
    }
}

/// CaptureLocations is a low level representation of the raw offsets of each
/// submatch.
///
/// Primarily, this type is useful when using `Regex` APIs such as
/// `captures_read`, which permits amortizing the allocation in which capture
/// match locations are stored.
///
/// In order to build a value of this type, you'll need to call the
/// `capture_locations` method on the `Regex` being used to execute the search.
/// The value returned can then be reused in subsequent searches.
pub struct CaptureLocations<W: CodeUnitWidth> {
    code: Arc<Code<W>>,
    data: MatchData<W>,
}

impl<W: CodeUnitWidth> Clone for CaptureLocations<W> {
    fn clone(&self) -> Self {
        CaptureLocations {
            code: Arc::clone(&self.code),
            data: MatchData::new(self.data.config().clone(), &self.code),
        }
    }
}

impl<W: CodeUnitWidth> fmt::Debug for CaptureLocations<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut offsets: Vec<Option<usize>> = vec![];
        for &offset in self.data.ovector() {
            if offset == PCRE2_UNSET {
                offsets.push(None);
            } else {
                offsets.push(Some(offset));
            }
        }
        write!(f, "CaptureLocations(")?;
        f.debug_list().entries(offsets).finish()?;
        write!(f, ")")
    }
}

impl<W: CodeUnitWidth> CaptureLocations<W> {
    /// Returns the start and end positions of the Nth capture group.
    ///
    /// This returns `None` if `i` is not a valid capture group or if the
    /// capture group did not match anything.
    ///
    /// The positions returned are always byte indices with respect to the
    /// original subject string matched.
    #[inline]
    pub fn get(&self, i: usize) -> Option<(usize, usize)> {
        let ovec = self.data.ovector();
        let s = match ovec.get(i * 2) {
            None => return None,
            Some(&s) if s == PCRE2_UNSET => return None,
            Some(&s) => s,
        };
        let e = match ovec.get(i * 2 + 1) {
            None => return None,
            Some(&e) if e == PCRE2_UNSET => return None,
            Some(&e) => e,
        };
        Some((s, e))
    }

    /// Returns the total number of capturing groups.
    ///
    /// This is always at least `1` since every regex has at least `1`
    /// capturing group that corresponds to the entire match.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.ovector().len() / 2
    }
}

/// Captures represents a group of captured byte strings for a single match.
///
/// The 0th capture always corresponds to the entire match. Each subsequent
/// index corresponds to the next capture group in the regex. If a capture
/// group is named, then the matched byte string is *also* available via the
/// `name` method. (Note that the 0th capture is always unnamed and so must be
/// accessed with the `get` method.)
///
/// Positions returned from a capture group are always byte indices.
///
/// `'s` is the lifetime of the matched subject string.
pub struct Captures<'s, W: CodeUnitWidth> {
    subject: &'s [W::SubjectChar],
    locs: CaptureLocations<W>,
    idx: Arc<HashMap<String, usize>>,
}

impl<'s, W: CodeUnitWidth> Captures<'s, W> {
    /// Returns the match associated with the capture group at index `i`. If
    /// `i` does not correspond to a capture group, or if the capture group
    /// did not participate in the match, then `None` is returned.
    pub fn get(&self, i: usize) -> Option<Match<'s, W>> {
        self.locs
            .get(i)
            .map(|(s, e)| Match::new(self.subject, s, e))
    }

    /// Returns the match for the capture group named `name`. If `name` isn't a
    /// valid capture group or didn't match anything, then `None` is returned.
    pub fn name(&self, name: &str) -> Option<Match<'s, W>> {
        self.idx.get(name).and_then(|&i| self.get(i))
    }

    /// Returns the number of captured groups.
    ///
    /// This is always at least `1`, since every regex has at least one capture
    /// group that corresponds to the full match.
    #[inline]
    pub fn len(&self) -> usize {
        self.locs.len()
    }
}

impl<'s, W: CodeUnitWidth> fmt::Debug for Captures<'s, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Captures")
            .field(&CapturesDebug(self))
            .finish()
    }
}

struct CapturesDebug<'c, 's: 'c, W: CodeUnitWidth>(&'c Captures<'s, W>);

impl<'c, 's, W: CodeUnitWidth> fmt::Debug for CapturesDebug<'c, 's, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // We'd like to show something nice here, even if it means an
        // allocation to build a reverse index.
        let slot_to_name: HashMap<&usize, &String> =
            self.0.idx.iter().map(|(a, b)| (b, a)).collect();
        let mut map = f.debug_map();
        for slot in 0..self.0.len() {
            let m = self
                .0
                .locs
                .get(slot)
                .map(|(s, e)| W::escape_subject(&self.0.subject[s..e]));
            if let Some(name) = slot_to_name.get(&slot) {
                map.entry(&name, &m);
            } else {
                map.entry(&slot, &m);
            }
        }
        map.finish()
    }
}

/// Get a group by index.
///
/// `'s` is the lifetime of the matched subject string.
///
/// The subject can't outlive the `Captures` object if this method is
/// used, because of how `Index` is defined (normally `a[i]` is part
/// of `a` and can't outlive it); to do that, use `get()` instead.
///
/// # Panics
///
/// If there is no group at the given index.
impl<'s, W: CodeUnitWidth> Index<usize> for Captures<'s, W> {
    type Output = [W::SubjectChar];

    fn index(&self, i: usize) -> &Self::Output {
        self.get(i)
            .map(|m| m.as_bytes())
            .unwrap_or_else(|| panic!("no group at index '{}'", i))
    }
}

/// Get a group by name.
///
/// `'s` is the lifetime of the matched subject string and `'i` is the lifetime
/// of the group name (the index).
///
/// The text can't outlive the `Captures` object if this method is
/// used, because of how `Index` is defined (normally `a[i]` is part
/// of `a` and can't outlive it); to do that, use `name` instead.
///
/// # Panics
///
/// If there is no group named by the given value.
impl<'s, 'i, W: CodeUnitWidth> Index<&'i str> for Captures<'s, W> {
    type Output = [W::SubjectChar];

    fn index<'a>(&'a self, name: &'i str) -> &'a [W::SubjectChar] {
        self.name(name)
            .map(|m| m.as_bytes())
            .unwrap_or_else(|| panic!("no group named '{}'", name))
    }
}

/// An iterator over all non-overlapping matches for a particular subject
/// string.
///
/// The iterator yields matches (if no error occurred while searching)
/// corresponding to the start and end of the match. The indices are byte
/// offsets. The iterator stops when no more matches can be found.
///
/// `'r` is the lifetime of the compiled regular expression and `'s` is the
/// lifetime of the subject string.
pub struct Matches<'r, 's, W: CodeUnitWidth> {
    re: &'r Regex<W>,
    match_data: &'r RefCell<MatchData<W>>,
    subject: &'s [W::SubjectChar],
    last_end: usize,
    last_match: Option<usize>,
}

impl<'r, 's, W: CodeUnitWidth> Iterator for Matches<'r, 's, W> {
    type Item = Result<Match<'s, W>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.last_end > self.subject.len() {
            return None;
        }
        let res = self
            .re
            .find_at_with_match_data(self.match_data, self.subject, self.last_end);
        let m = match res {
            Err(err) => return Some(Err(err)),
            Ok(None) => return None,
            Ok(Some(m)) => m,
        };
        if m.start() == m.end() {
            // This is an empty match. To ensure we make progress, start
            // the next search at the smallest possible starting position
            // of the next match following this one.
            self.last_end = m.end() + 1;
            // Don't accept empty matches immediately following a match.
            // Just move on to the next match.
            if Some(m.end()) == self.last_match {
                return self.next();
            }
        } else {
            self.last_end = m.end();
        }
        self.last_match = Some(m.end());
        Some(Ok(m))
    }
}

/// An iterator that yields all non-overlapping capture groups matching a
/// particular regular expression.
///
/// The iterator stops when no more matches can be found.
///
/// `'r` is the lifetime of the compiled regular expression and `'s` is the
/// lifetime of the subject string.
pub struct CaptureMatches<'r, 's, W: CodeUnitWidth> {
    re: &'r Regex<W>,
    subject: &'s [W::SubjectChar],
    last_end: usize,
    last_match: Option<usize>,
}

impl<'r, 's, W: CodeUnitWidth> Iterator for CaptureMatches<'r, 's, W> {
    type Item = Result<Captures<'s, W>, Error>;

    fn next(&mut self) -> Option<Result<Captures<'s, W>, Error>> {
        if self.last_end > self.subject.len() {
            return None;
        }
        let mut locs = self.re.capture_locations();
        let res = self
            .re
            .captures_read_at(&mut locs, self.subject, self.last_end);
        let m = match res {
            Err(err) => return Some(Err(err)),
            Ok(None) => return None,
            Ok(Some(m)) => m,
        };
        if m.start() == m.end() {
            // This is an empty match. To ensure we make progress, start
            // the next search at the smallest possible starting position
            // of the next match following this one.
            self.last_end = m.end() + 1;
            // Don't accept empty matches immediately following a match.
            // Just move on to the next match.
            if Some(m.end()) == self.last_match {
                return self.next();
            }
        } else {
            self.last_end = m.end();
        }
        self.last_match = Some(m.end());
        Some(Ok(Captures {
            subject: self.subject,
            locs: locs,
            idx: Arc::clone(&self.re.capture_names_idx),
        }))
    }
}

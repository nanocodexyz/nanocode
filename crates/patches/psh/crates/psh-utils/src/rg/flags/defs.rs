// Copied from ripgrep (https://github.com/BurntSushi/ripgrep, commit 91f5d22),
// crates/core/flags/defs.rs, with the psh patches marked `psh:` (D1133):
// Copyright (c) 2015 Andrew Gallant. Licensed under the MIT License (see NOTICE).

/*!
Defines all of the flags available in ripgrep.

Each flag corresponds to a unit struct with a corresponding implementation
of `Flag`. Note that each implementation of `Flag` might actually have many
possible manifestations of the same "flag." That is, each implementation of
`Flag` can have the following flags available to an end user of ripgrep:

* The long flag name.
* An optional short flag name.
* An optional negated long flag name.
* An arbitrarily long list of aliases.

The idea is that even though there are multiple flags that a user can type,
one implementation of `Flag` corresponds to a single _logical_ flag inside of
ripgrep. For example, `-E`, `--encoding` and `--no-encoding` all manipulate the
same encoding state in ripgrep.
*/

use std::{path::PathBuf, sync::LazyLock};

use crate::rg::anyhow::{self, Context as AnyhowContext};

use crate::rg::flags::{
    Category, Flag, FlagValue,
    lowargs::{
        BinaryMode, BoundaryMode, BufferMode, CaseMode, ColorChoice,
        ContextMode, EncodingMode, EngineChoice, GenerateMode, LoggingMode,
        LowArgs, MmapMode, Mode, PatternSource, SearchMode, SortMode,
        SortModeKind, SpecialMode, TypeChange,
    },
};


use super::CompletionType;

/// A list of all flags in ripgrep via implementations of `Flag`.
///
/// The order of these flags matter. It determines the order of the flags in
/// the generated documentation (`-h`, `--help` and the man page) within each
/// category. (This is why the deprecated flags are last.)
pub(super) const FLAGS: &[&dyn Flag] = &[
    // -e/--regexp and -f/--file should come before anything else in the
    // same category.
    &Regexp,
    &File,
    &AfterContext,
    &BeforeContext,
    &Binary,
    &BlockBuffered,
    &ByteOffset,
    &CaseSensitive,
    &Color,
    &Colors,
    &Column,
    &Context,
    &ContextSeparator,
    &Count,
    &CountMatches,
    &Crlf,
    &Debug,
    &DfaSizeLimit,
    &Encoding,
    &Engine,
    &FieldContextSeparator,
    &FieldMatchSeparator,
    &Files,
    &FilesWithMatches,
    &FilesWithoutMatch,
    &FixedStrings,
    &Follow,
    &Generate,
    &Glob,
    &GlobCaseInsensitive,
    &Heading,
    &Help,
    &Hidden,
    &HostnameBin,
    &HyperlinkFormat,
    &IGlob,
    &IgnoreCase,
    &IgnoreFile,
    &IgnoreFileCaseInsensitive,
    &IncludeZero,
    &InvertMatch,
    &JSON,
    &LineBuffered,
    &LineNumber,
    &LineNumberNo,
    &LineRegexp,
    &MaxColumns,
    &MaxColumnsPreview,
    &MaxCount,
    &MaxDepth,
    &MaxFilesize,
    &Mmap,
    &Multiline,
    &MultilineDotall,
    &NoConfig,
    &NoIgnore,
    &NoIgnoreDot,
    &NoIgnoreExclude,
    &NoIgnoreFiles,
    &NoIgnoreGlobal,
    &NoIgnoreMessages,
    &NoIgnoreParent,
    &NoIgnoreVcs,
    &NoMessages,
    &NoRequireGit,
    &NoUnicode,
    &Null,
    &NullData,
    &OneFileSystem,
    &OnlyMatching,
    &PathSeparator,
    &Passthru,
    &PCRE2,
    &PCRE2Version,
    &Pre,
    &PreGlob,
    &Pretty,
    &Quiet,
    &RegexSizeLimit,
    &Replace,
    &SearchZip,
    &SmartCase,
    &Sort,
    &Sortr,
    &Stats,
    &StopOnNonmatch,
    &Text,
    &Threads,
    &Trace,
    &Trim,
    &Type,
    &TypeNot,
    &TypeAdd,
    &TypeClear,
    &TypeList,
    &Unrestricted,
    &Version,
    &Vimgrep,
    &WithFilename,
    &WithFilenameNo,
    &WordRegexp,
    // DEPRECATED (make them show up last in their respective categories)
    &AutoHybridRegex,
    &NoPcre2Unicode,
    &SortFiles,
];

/// -A/--after-context
#[derive(Debug)]
struct AfterContext;

impl Flag for AfterContext {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'A')
    }
    fn name_long(&self) -> &'static str {
        "after-context"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        "Show NUM lines after each match."
    }
    fn doc_long(&self) -> &'static str {
        r"
Show \fINUM\fP lines after each match.
.sp
This overrides the \flag{passthru} flag and partially overrides the
\flag{context} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.context.set_after(convert::usize(&v.unwrap_value())?);
        Ok(())
    }
}


/// --auto-hybrid-regex
#[derive(Debug)]
struct AutoHybridRegex;

impl Flag for AutoHybridRegex {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "auto-hybrid-regex"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-auto-hybrid-regex")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        "(DEPRECATED) Use PCRE2 if appropriate."
    }
    fn doc_long(&self) -> &'static str {
        r"
DEPRECATED. Use \flag{engine} instead.
.sp
When this flag is used, ripgrep will dynamically choose between supported regex
engines depending on the features used in a pattern. When ripgrep chooses a
regex engine, it applies that choice for every regex provided to ripgrep (e.g.,
via multiple \flag{regexp} or \flag{file} flags).
.sp
As an example of how this flag might behave, ripgrep will attempt to use
its default finite automata based regex engine whenever the pattern can be
successfully compiled with that regex engine. If PCRE2 is enabled and if the
pattern given could not be compiled with the default regex engine, then PCRE2
will be automatically used for searching. If PCRE2 isn't available, then this
flag has no effect because there is only one regex engine to choose from.
.sp
In the future, ripgrep may adjust its heuristics for how it decides which
regex engine to use. In general, the heuristics will be limited to a static
analysis of the patterns, and not to any specific runtime behavior observed
while searching files.
.sp
The primary downside of using this flag is that it may not always be obvious
which regex engine ripgrep uses, and thus, the match semantics or performance
profile of ripgrep may subtly and unexpectedly change. However, in many cases,
all regex engines will agree on what constitutes a match and it can be nice
to transparently support more advanced regex features like look-around and
backreferences without explicitly needing to enable them.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let mode = if v.unwrap_switch() {
            EngineChoice::Auto
        } else {
            EngineChoice::Default
        };
        args.engine = mode;
        Ok(())
    }
}


/// -B/--before-context
#[derive(Debug)]
struct BeforeContext;

impl Flag for BeforeContext {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'B')
    }
    fn name_long(&self) -> &'static str {
        "before-context"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        "Show NUM lines before each match."
    }
    fn doc_long(&self) -> &'static str {
        r"
Show \fINUM\fP lines before each match.
.sp
This overrides the \flag{passthru} flag and partially overrides the
\flag{context} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.context.set_before(convert::usize(&v.unwrap_value())?);
        Ok(())
    }
}


/// --binary
#[derive(Debug)]
struct Binary;

impl Flag for Binary {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "binary"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-binary")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        "Search binary files."
    }
    fn doc_long(&self) -> &'static str {
        r"
Enabling this flag will cause ripgrep to search binary files. By default,
ripgrep attempts to automatically skip binary files in order to improve the
relevance of results and make the search faster.
.sp
Binary files are heuristically detected based on whether they contain a
\fBNUL\fP byte or not. By default (without this flag set), once a \fBNUL\fP
byte is seen, ripgrep will stop searching the file. Usually, \fBNUL\fP bytes
occur in the beginning of most binary files. If a \fBNUL\fP byte occurs after
a match, then ripgrep will not print the match, stop searching that file, and
emit a warning that some matches are being suppressed.
.sp
In contrast, when this flag is provided, ripgrep will continue searching a
file even if a \fBNUL\fP byte is found. In particular, if a \fBNUL\fP byte is
found then ripgrep will continue searching until either a match is found or
the end of the file is reached, whichever comes sooner. If a match is found,
then ripgrep will stop and print a warning saying that the search stopped
prematurely.
.sp
If you want ripgrep to search a file without any special \fBNUL\fP byte
handling at all (and potentially print binary data to stdout), then you should
use the \flag{text} flag.
.sp
The \flag{binary} flag is a flag for controlling ripgrep's automatic filtering
mechanism. As such, it does not need to be used when searching a file
explicitly or when searching stdin. That is, it is only applicable when
recursively searching a directory.
.sp
When the \flag{unrestricted} flag is provided for a third time, then this flag
is automatically enabled.
.sp
This flag overrides the \flag{text} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.binary = if v.unwrap_switch() {
            BinaryMode::SearchAndSuppress
        } else {
            BinaryMode::Auto
        };
        Ok(())
    }
}


/// --block-buffered
#[derive(Debug)]
struct BlockBuffered;

impl Flag for BlockBuffered {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "block-buffered"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-block-buffered")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        "Force block buffering."
    }
    fn doc_long(&self) -> &'static str {
        r"
When enabled, ripgrep will use block buffering. That is, whenever a matching
line is found, it will be written to an in-memory buffer and will not be
written to stdout until the buffer reaches a certain size. This is the default
when ripgrep's stdout is redirected to a pipeline or a file. When ripgrep's
stdout is connected to a tty, line buffering will be used by default. Forcing
block buffering can be useful when dumping a large amount of contents to a tty.
.sp
This overrides the \flag{line-buffered} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.buffer = if v.unwrap_switch() {
            BufferMode::Block
        } else {
            BufferMode::Auto
        };
        Ok(())
    }
}


/// --byte-offset
#[derive(Debug)]
struct ByteOffset;

impl Flag for ByteOffset {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'b')
    }
    fn name_long(&self) -> &'static str {
        "byte-offset"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-byte-offset")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        "Print the byte offset for each matching line."
    }
    fn doc_long(&self) -> &'static str {
        r"
Print the 0-based byte offset within the input file before each line of output.
If \flag{only-matching} is specified, print the offset of the matched text
itself.
.sp
If ripgrep does transcoding, then the byte offset is in terms of the result
of transcoding and not the original data. This applies similarly to other
transformations on the data, such as decompression or a \flag{pre} filter.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.byte_offset = v.unwrap_switch();
        Ok(())
    }
}


/// -s/--case-sensitive
#[derive(Debug)]
struct CaseSensitive;

impl Flag for CaseSensitive {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b's')
    }
    fn name_long(&self) -> &'static str {
        "case-sensitive"
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Search case sensitively (default)."
    }
    fn doc_long(&self) -> &'static str {
        r"
Execute the search case sensitively. This is the default mode.
.sp
This is a global option that applies to all patterns given to ripgrep.
Individual patterns can still be matched case insensitively by using inline
regex flags. For example, \fB(?i)abc\fP will match \fBabc\fP case insensitively
even when this flag is used.
.sp
This flag overrides the \flag{ignore-case} and \flag{smart-case} flags.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "flag has no negation");
        args.case = CaseMode::Sensitive;
        Ok(())
    }
}


/// --color
#[derive(Debug)]
struct Color;

impl Flag for Color {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "color"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("WHEN")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        "When to use color."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag controls when to use colors. The default setting is \fBauto\fP, which
means ripgrep will try to guess when to use colors. For example, if ripgrep is
printing to a tty, then it will use colors, but if it is redirected to a file
or a pipe, then it will suppress color output.
.sp
ripgrep will suppress color output by default in some other circumstances as
well. These include, but are not limited to:
.sp
.IP \(bu 3n
When the \fBTERM\fP environment variable is not set or set to \fBdumb\fP.
.sp
.IP \(bu 3n
When the \fBNO_COLOR\fP environment variable is set (regardless of value).
.sp
.IP \(bu 3n
When flags that imply no use for colors are given. For example,
\flag{vimgrep} and \flag{json}.
.
.PP
The possible values for this flag are:
.sp
.IP \fBnever\fP 10n
Colors will never be used.
.sp
.IP \fBauto\fP 10n
The default. ripgrep tries to be smart.
.sp
.IP \fBalways\fP 10n
Colors will always be used regardless of where output is sent.
.sp
.IP \fBansi\fP 10n
Like 'always', but emits ANSI escapes (even in a Windows console).
.
.PP
This flag also controls whether hyperlinks are emitted. For example, when
a hyperlink format is specified, hyperlinks won't be used when color is
suppressed. If one wants to emit hyperlinks but no colors, then one must use
the \flag{colors} flag to manually set all color styles to \fBnone\fP:
.sp
.EX
    \-\-colors 'path:none' \\
    \-\-colors 'line:none' \\
    \-\-colors 'column:none' \\
    \-\-colors 'match:none' \\
    \-\-colors 'highlight:none'
.EE
.sp
"
    }
    fn doc_choices(&self) -> &'static [&'static str] {
        &["never", "auto", "always", "ansi"]
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.color = match convert::str(&v.unwrap_value())? {
            "never" => ColorChoice::Never,
            "auto" => ColorChoice::Auto,
            "always" => ColorChoice::Always,
            "ansi" => ColorChoice::Ansi,
            unk => anyhow::bail!("choice '{unk}' is unrecognized"),
        };
        Ok(())
    }
}


/// --colors
#[derive(Debug)]
struct Colors;

impl Flag for Colors {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "colors"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("COLOR_SPEC")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        "Configure color settings and styles."
    }
    fn doc_long(&self) -> &'static str {
        r#"
This flag specifies color settings for use in the output. This flag may be
provided multiple times. Settings are applied iteratively. Pre-existing color
labels are limited to one of eight choices: \fBred\fP, \fBblue\fP, \fBgreen\fP,
\fBcyan\fP, \fBmagenta\fP, \fByellow\fP, \fBwhite\fP and \fBblack\fP. Styles
are limited to \fBnobold\fP, \fBbold\fP, \fBnointense\fP, \fBintense\fP,
\fBnounderline\fP, \fBunderline\fP, \fBnoitalic\fP or \fBitalic\fP.
.sp
The format of the flag is
\fB{\fP\fItype\fP\fB}:{\fP\fIattribute\fP\fB}:{\fP\fIvalue\fP\fB}\fP.
\fItype\fP should be one of \fBpath\fP, \fBline\fP, \fBcolumn\fP,
\fBhighlight\fP or \fBmatch\fP. \fIattribute\fP can be \fBfg\fP, \fBbg\fP or
\fBstyle\fP. \fIvalue\fP is either a color (for \fBfg\fP and \fBbg\fP) or a
text style. A special format, \fB{\fP\fItype\fP\fB}:none\fP, will clear all
color settings for \fItype\fP.
.sp
For example, the following command will change the match color to magenta and
the background color for line numbers to yellow:
.sp
.EX
    rg \-\-colors 'match:fg:magenta' \-\-colors 'line:bg:yellow'
.EE
.sp
Another example, the following command will "highlight" the non-matching text
in matching lines:
.sp
.EX
    rg \-\-colors 'highlight:bg:yellow' \-\-colors 'highlight:fg:black'
.EE
.sp
The "highlight" color type is particularly useful for contrasting matching
lines with surrounding context printed by the \flag{before-context},
\flag{after-context}, \flag{context} or \flag{passthru} flags.
.sp
Extended colors can be used for \fIvalue\fP when the tty supports ANSI color
sequences. These are specified as either \fIx\fP (256-color) or
.IB x , x , x
(24-bit truecolor) where \fIx\fP is a number between \fB0\fP and \fB255\fP
inclusive. \fIx\fP may be given as a normal decimal number or a hexadecimal
number, which is prefixed by \fB0x\fP.
.sp
For example, the following command will change the match background color to
that represented by the rgb value (0,128,255):
.sp
.EX
    rg \-\-colors 'match:bg:0,128,255'
.EE
.sp
or, equivalently,
.sp
.EX
    rg \-\-colors 'match:bg:0x0,0x80,0xFF'
.EE
.sp
Note that the \fBintense\fP and \fBnointense\fP styles will have no effect when
used alongside these extended color codes.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let v = v.unwrap_value();
        let v = convert::str(&v)?;
        args.colors.push(v.parse()?);
        Ok(())
    }
}


/// --column
#[derive(Debug)]
struct Column;

impl Flag for Column {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "column"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-column")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        "Show column numbers."
    }
    fn doc_long(&self) -> &'static str {
        r"
Show column numbers (1-based). This only shows the column numbers for the first
match on each line. This does not try to account for Unicode. One byte is equal
to one column. This implies \flag{line-number}.
.sp
When \flag{only-matching} is used, then the column numbers written correspond
to the start of each match.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.column = Some(v.unwrap_switch());
        Ok(())
    }
}


/// -C/--context
#[derive(Debug)]
struct Context;

impl Flag for Context {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'C')
    }
    fn name_long(&self) -> &'static str {
        "context"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Show NUM lines before and after each match."
    }
    fn doc_long(&self) -> &'static str {
        r"
Show \fINUM\fP lines before and after each match. This is equivalent to
providing both the \flag{before-context} and \flag{after-context} flags with
the same value.
.sp
This overrides the \flag{passthru} flag. The \flag{after-context} and
\flag{before-context} flags both partially override this flag, regardless of
the order. For example, \fB\-A2 \-C1\fP is equivalent to \fB\-A2 \-B1\fP.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.context.set_both(convert::usize(&v.unwrap_value())?);
        Ok(())
    }
}


/// --context-separator
#[derive(Debug)]
struct ContextSeparator;

impl Flag for ContextSeparator {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "context-separator"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-context-separator")
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("SEPARATOR")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Set the separator for contextual chunks."
    }
    fn doc_long(&self) -> &'static str {
        r"
The string used to separate non-contiguous context lines in the output. This is
only used when one of the context flags is used (that is, \flag{after-context},
\flag{before-context} or \flag{context}). Escape sequences like \fB\\x7F\fP or
\fB\\t\fP may be used. The default value is \fB\-\-\fP.
.sp
When the context separator is set to an empty string, then a line break
is still inserted. To completely disable context separators, use the
\flag-negate{context-separator} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        use crate::rg::flags::lowargs::ContextSeparator as Separator;

        args.context_separator = match v {
            FlagValue::Switch(true) => {
                unreachable!("flag can only be disabled")
            }
            FlagValue::Switch(false) => Separator::disabled(),
            FlagValue::Value(v) => Separator::new(&v)?,
        };
        Ok(())
    }
}


/// -c/--count
#[derive(Debug)]
struct Count;

impl Flag for Count {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'c')
    }
    fn name_long(&self) -> &'static str {
        "count"
    }
    fn doc_category(&self) -> Category {
        Category::OutputModes
    }
    fn doc_short(&self) -> &'static str {
        r"Show count of matching lines for each file."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag suppresses normal output and shows the number of lines that match
the given patterns for each file searched. Each file containing a match has
its path and count printed on each line. Note that unless \flag{multiline} is
enabled and the pattern(s) given can match over multiple lines, this reports
the number of lines that match and not the total number of matches. When
multiline mode is enabled and the pattern(s) given can match over multiple
lines, \flag{count} is equivalent to \flag{count-matches}.
.sp
If only one file is given to ripgrep, then only the count is printed if there
is a match. The \flag{with-filename} flag can be used to force printing the
file path in this case. If you need a count to be printed regardless of whether
there is a match, then use \flag{include-zero}.
.sp
Note that it is possible for this flag to have results inconsistent with
the output of \flag{files-with-matches}. Notably, by default, ripgrep tries
to avoid searching files with binary data. With this flag, ripgrep needs to
search the entire content of files, which may include binary data. But with
\flag{files-with-matches}, ripgrep can stop as soon as a match is observed,
which may come well before any binary data. To avoid this inconsistency without
disabling binary detection, use the \flag{binary} flag.
.sp
This overrides the \flag{count-matches} flag. Note that when \flag{count}
is combined with \flag{only-matching}, then ripgrep behaves as if
\flag{count-matches} was given.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--count can only be enabled");
        args.mode.update(Mode::Search(SearchMode::Count));
        Ok(())
    }
}


/// --count-matches
#[derive(Debug)]
struct CountMatches;

impl Flag for CountMatches {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "count-matches"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        None
    }
    fn doc_category(&self) -> Category {
        Category::OutputModes
    }
    fn doc_short(&self) -> &'static str {
        r"Show count of every match for each file."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag suppresses normal output and shows the number of individual matches
of the given patterns for each file searched. Each file containing matches has
its path and match count printed on each line. Note that this reports the total
number of individual matches and not the number of lines that match.
.sp
If only one file is given to ripgrep, then only the count is printed if there
is a match. The \flag{with-filename} flag can be used to force printing the
file path in this case.
.sp
This overrides the \flag{count} flag. Note that when \flag{count} is combined
with \flag{only-matching}, then ripgrep behaves as if \flag{count-matches} was
given.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--count-matches can only be enabled");
        args.mode.update(Mode::Search(SearchMode::CountMatches));
        Ok(())
    }
}


/// --crlf
#[derive(Debug)]
struct Crlf;

impl Flag for Crlf {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "crlf"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-crlf")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Use CRLF line terminators (nice for Windows)."
    }
    fn doc_long(&self) -> &'static str {
        r"
When enabled, ripgrep will treat CRLF (\fB\\r\\n\fP) as a line terminator
instead of just \fB\\n\fP.
.sp
Principally, this permits the line anchor assertions \fB^\fP and \fB$\fP in
regex patterns to treat CRLF, CR or LF as line terminators instead of just LF.
Note that they will never match between a CR and a LF. CRLF is treated as one
single line terminator.
.sp
When using the default regex engine, CRLF support can also be enabled inside
the pattern with the \fBR\fP flag. For example, \fB(?R:$)\fP will match just
before either CR or LF, but never between CR and LF.
.sp
This flag overrides \flag{null-data}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.crlf = v.unwrap_switch();
        args.crlf_explicit = true;
        if args.crlf {
            args.null_data = false;
        }
        Ok(())
    }
}


/// --debug
#[derive(Debug)]
struct Debug;

impl Flag for Debug {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "debug"
    }
    fn doc_category(&self) -> Category {
        Category::Logging
    }
    fn doc_short(&self) -> &'static str {
        r"Show debug messages."
    }
    fn doc_long(&self) -> &'static str {
        r"
Show debug messages. Please use this when filing a bug report.
.sp
The \flag{debug} flag is generally useful for figuring out why ripgrep skipped
searching a particular file. The debug messages should mention all files
skipped and why they were skipped.
.sp
To get even more debug output, use the \flag{trace} flag, which implies
\flag{debug} along with additional trace data.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--debug can only be enabled");
        args.logging = Some(LoggingMode::Debug);
        Ok(())
    }
}


/// --dfa-size-limit
#[derive(Debug)]
struct DfaSizeLimit;

impl Flag for DfaSizeLimit {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "dfa-size-limit"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM+SUFFIX?")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"The upper size limit of the regex DFA."
    }
    fn doc_long(&self) -> &'static str {
        r"
The upper size limit of the regex DFA. The default limit is something generous
for any single pattern or for many smallish patterns. This should only be
changed on very large regex inputs where the (slower) fallback regex engine may
otherwise be used if the limit is reached.
.sp
The input format accepts suffixes of \fBK\fP, \fBM\fP or \fBG\fP which
correspond to kilobytes, megabytes and gigabytes, respectively. If no suffix is
provided the input is treated as bytes.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let v = v.unwrap_value();
        args.dfa_size_limit = Some(convert::human_readable_usize(&v)?);
        Ok(())
    }
}


/// -E/--encoding
#[derive(Debug)]
struct Encoding;

impl Flag for Encoding {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'E')
    }
    fn name_long(&self) -> &'static str {
        "encoding"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-encoding")
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("ENCODING")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Specify the text encoding of files to search."
    }
    fn doc_long(&self) -> &'static str {
        r"
Specify the text encoding that ripgrep will use on all files searched. The
default value is \fBauto\fP, which will cause ripgrep to do a best effort
automatic detection of encoding on a per-file basis. Automatic detection in
this case only applies to files that begin with a UTF-8 or UTF-16 byte-order
mark (BOM). No other automatic detection is performed. One can also specify
\fBnone\fP which will then completely disable BOM sniffing and always result
in searching the raw bytes, including a BOM if it's present, regardless of its
encoding.
.sp
Other supported values can be found in the list of labels here:
\fIhttps://encoding.spec.whatwg.org/#concept-encoding-get\fP.
.sp
For more details on encoding and how ripgrep deals with it, see \fBGUIDE.md\fP.
.sp
The encoding detection that ripgrep uses can be reverted to its automatic mode
via the \flag-negate{encoding} flag.
"
    }
    fn completion_type(&self) -> CompletionType {
        CompletionType::Encoding
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let value = match v {
            FlagValue::Value(v) => v,
            FlagValue::Switch(true) => {
                unreachable!("--encoding must accept a value")
            }
            FlagValue::Switch(false) => {
                args.encoding = EncodingMode::Auto;
                return Ok(());
            }
        };
        let label = convert::str(&value)?;
        args.encoding = match label {
            "auto" => EncodingMode::Auto,
            "none" => EncodingMode::Disabled,
            _ => EncodingMode::Some(crate::searcher::Encoding::new(label)?),
        };
        Ok(())
    }
}


/// --engine
#[derive(Debug)]
struct Engine;

impl Flag for Engine {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "engine"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("ENGINE")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Specify which regex engine to use."
    }
    fn doc_long(&self) -> &'static str {
        r"
Specify which regular expression engine to use. When you choose a regex engine,
it applies that choice for every regex provided to ripgrep (e.g., via multiple
\flag{regexp} or \flag{file} flags).
.sp
Accepted values are \fBdefault\fP, \fBpcre2\fP, or \fBauto\fP.
.sp
The default value is \fBdefault\fP, which is usually the fastest and should be
good for most use cases. The \fBpcre2\fP engine is generally useful when you
want to use features such as look-around or backreferences. \fBauto\fP will
dynamically choose between supported regex engines depending on the features
used in a pattern on a best effort basis.
.sp
Note that the \fBpcre2\fP engine is an optional ripgrep feature. If PCRE2
wasn't included in your build of ripgrep, then using this flag will result in
ripgrep printing an error message and exiting.
.sp
This overrides previous uses of the \flag{pcre2} and \flag{auto-hybrid-regex}
flags.
"
    }
    fn doc_choices(&self) -> &'static [&'static str] {
        &["default", "pcre2", "auto"]
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let v = v.unwrap_value();
        let string = convert::str(&v)?;
        args.engine = match string {
            "default" => EngineChoice::Default,
            "pcre2" => EngineChoice::PCRE2,
            "auto" => EngineChoice::Auto,
            _ => anyhow::bail!("unrecognized regex engine '{string}'"),
        };
        Ok(())
    }
}


/// --field-context-separator
#[derive(Debug)]
struct FieldContextSeparator;

impl Flag for FieldContextSeparator {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "field-context-separator"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("SEPARATOR")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Set the field context separator."
    }
    fn doc_long(&self) -> &'static str {
        r"
Set the field context separator. This separator is only used when printing
contextual lines. It is used to delimit file paths, line numbers, columns and
the contextual line itself. The separator may be any number of bytes, including
zero. Escape sequences like \fB\\x7F\fP or \fB\\t\fP may be used.
.sp
The \fB-\fP character is the default value.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        use crate::rg::flags::lowargs::FieldContextSeparator as Separator;

        args.field_context_separator = Separator::new(&v.unwrap_value())?;
        Ok(())
    }
}


/// --field-match-separator
#[derive(Debug)]
struct FieldMatchSeparator;

impl Flag for FieldMatchSeparator {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "field-match-separator"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("SEPARATOR")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Set the field match separator."
    }
    fn doc_long(&self) -> &'static str {
        r"
Set the field match separator. This separator is only used when printing
matching lines. It is used to delimit file paths, line numbers, columns and the
matching line itself. The separator may be any number of bytes, including zero.
Escape sequences like \fB\\x7F\fP or \fB\\t\fP may be used.
.sp
The \fB:\fP character is the default value.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        use crate::rg::flags::lowargs::FieldMatchSeparator as Separator;

        args.field_match_separator = Separator::new(&v.unwrap_value())?;
        Ok(())
    }
}


/// -f/--file
#[derive(Debug)]
struct File;

impl Flag for File {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'f')
    }
    fn name_long(&self) -> &'static str {
        "file"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("PATTERNFILE")
    }
    fn doc_category(&self) -> Category {
        Category::Input
    }
    fn doc_short(&self) -> &'static str {
        r"Search for patterns from the given file."
    }
    fn doc_long(&self) -> &'static str {
        r"
Search for patterns from the given file, with one pattern per line. When this
flag is used multiple times or in combination with the \flag{regexp} flag, then
all patterns provided are searched. Empty pattern lines will match all input
lines, and the newline is not counted as part of the pattern.
.sp
A line is printed if and only if it matches at least one of the patterns.
.sp
When \fIPATTERNFILE\fP is \fB-\fP, then \fBstdin\fP will be read for the
patterns.
.sp
When \flag{file} or \flag{regexp} is used, then ripgrep treats all positional
arguments as files or directories to search.
"
    }
    fn completion_type(&self) -> CompletionType {
        CompletionType::Filename
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let path = PathBuf::from(v.unwrap_value());
        args.patterns.push(PatternSource::File(path));
        Ok(())
    }
}


/// --files
#[derive(Debug)]
struct Files;

impl Flag for Files {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "files"
    }
    fn doc_category(&self) -> Category {
        Category::OtherBehaviors
    }
    fn doc_short(&self) -> &'static str {
        r"Print each file that would be searched."
    }
    fn doc_long(&self) -> &'static str {
        r"
Print each file that would be searched without actually performing the search.
This is useful to determine whether a particular file is being searched or not.
.sp
This overrides \flag{type-list}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch());
        args.mode.update(Mode::Files);
        Ok(())
    }
}


/// -l/--files-with-matches
#[derive(Debug)]
struct FilesWithMatches;

impl Flag for FilesWithMatches {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'l')
    }
    fn name_long(&self) -> &'static str {
        "files-with-matches"
    }
    fn doc_category(&self) -> Category {
        Category::OutputModes
    }
    fn doc_short(&self) -> &'static str {
        r"Print the paths with at least one match."
    }
    fn doc_long(&self) -> &'static str {
        r"
Print only the paths with at least one match and suppress match contents.
.sp
Note that it is possible for this flag to have results inconsistent with the
output of \flag{count}. Notably, by default, ripgrep tries to avoid searching
files with binary data. With this flag, ripgrep might stop searching before
the binary data is observed. But with \flag{count}, ripgrep has to search the
entire contents to determine the match count, which means it might see binary
data that causes it to skip searching that file. To avoid this inconsistency
without disabling binary detection, use the \flag{binary} flag.
.sp
This overrides \flag{files-without-match}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--files-with-matches can only be enabled");
        args.mode.update(Mode::Search(SearchMode::FilesWithMatches));
        Ok(())
    }
}


/// -l/--files-without-match
#[derive(Debug)]
struct FilesWithoutMatch;

impl Flag for FilesWithoutMatch {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "files-without-match"
    }
    fn doc_category(&self) -> Category {
        Category::OutputModes
    }
    fn doc_short(&self) -> &'static str {
        r"Print the paths that contain zero matches."
    }
    fn doc_long(&self) -> &'static str {
        r"
Print the paths that contain zero matches and suppress match contents.
.sp
This overrides \flag{files-with-matches}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(
            v.unwrap_switch(),
            "--files-without-match can only be enabled"
        );
        args.mode.update(Mode::Search(SearchMode::FilesWithoutMatch));
        Ok(())
    }
}


/// -F/--fixed-strings
#[derive(Debug)]
struct FixedStrings;

impl Flag for FixedStrings {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'F')
    }
    fn name_long(&self) -> &'static str {
        "fixed-strings"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-fixed-strings")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Treat all patterns as literals."
    }
    fn doc_long(&self) -> &'static str {
        r"
Treat all patterns as literals instead of as regular expressions. When this
flag is used, special regular expression meta characters such as \fB.(){}*+\fP
should not need be escaped.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.fixed_strings = v.unwrap_switch();
        Ok(())
    }
}


/// -L/--follow
#[derive(Debug)]
struct Follow;

impl Flag for Follow {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'L')
    }
    fn name_long(&self) -> &'static str {
        "follow"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-follow")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Follow symbolic links."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag instructs ripgrep to follow symbolic links while traversing
directories. This behavior is disabled by default. Note that ripgrep will
check for symbolic link loops and report errors if it finds one. ripgrep will
also report errors for broken links. To suppress error messages, use the
\flag{no-messages} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.follow = v.unwrap_switch();
        Ok(())
    }
}


/// --generate
#[derive(Debug)]
struct Generate;

impl Flag for Generate {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "generate"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("KIND")
    }
    fn doc_category(&self) -> Category {
        Category::OtherBehaviors
    }
    fn doc_short(&self) -> &'static str {
        r"Generate man pages and completion scripts."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag instructs ripgrep to generate some special kind of output identified
by \fIKIND\fP and then quit without searching. \fIKIND\fP can be one of the
following values:
.sp
.TP 15
\fBman\fP
Generates a manual page for ripgrep in the \fBroff\fP format.
.TP 15
\fBcomplete\-bash\fP
Generates a completion script for the \fBbash\fP shell.
.TP 15
\fBcomplete\-zsh\fP
Generates a completion script for the \fBzsh\fP shell.
.TP 15
\fBcomplete\-fish\fP
Generates a completion script for the \fBfish\fP shell.
.TP 15
\fBcomplete\-powershell\fP
Generates a completion script for PowerShell.
.PP
The output is written to \fBstdout\fP. The list above may expand over time.
"
    }
    fn doc_choices(&self) -> &'static [&'static str] {
        &[
            "man",
            "complete-bash",
            "complete-zsh",
            "complete-fish",
            "complete-powershell",
        ]
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let genmode = match convert::str(&v.unwrap_value())? {
            "man" => GenerateMode::Man,
            "complete-bash" => GenerateMode::CompleteBash,
            "complete-zsh" => GenerateMode::CompleteZsh,
            "complete-fish" => GenerateMode::CompleteFish,
            "complete-powershell" => GenerateMode::CompletePowerShell,
            unk => anyhow::bail!("choice '{unk}' is unrecognized"),
        };
        args.mode.update(Mode::Generate(genmode));
        Ok(())
    }
}


/// -g/--glob
#[derive(Debug)]
struct Glob;

impl Flag for Glob {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'g')
    }
    fn name_long(&self) -> &'static str {
        "glob"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("GLOB")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Include or exclude file paths."
    }
    fn doc_long(&self) -> &'static str {
        r#"
Include or exclude files and directories for searching that match the given
glob. This always overrides any other ignore logic. Multiple glob flags may
be used. Globbing rules match \fB.gitignore\fP globs. Precede a glob with a
\fB!\fP to exclude it. If multiple globs match a file or directory, the glob
given later in the command line takes precedence.
.sp
As an extension, globs support specifying alternatives:
.BI "\-g '" ab{c,d}* '
is equivalent to
.BI "\-g " "abc " "\-g " abd.
Empty alternatives like
.BI "\-g '" ab{,c} '
are not currently supported. Note that this syntax extension is also currently
enabled in \fBgitignore\fP files, even though this syntax isn't supported by
git itself. ripgrep may disable this syntax extension in gitignore files, but
it will always remain available via the \flag{glob} flag.
.sp
When this flag is set, every file and directory is applied to it to test for
a match. For example, if you only want to search in a particular directory
\fIfoo\fP, then
.BI "\-g " foo
is incorrect because \fIfoo/bar\fP does not match
the glob \fIfoo\fP. Instead, you should use
.BI "\-g '" foo/** '.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let glob = convert::string(v.unwrap_value())?;
        args.globs.push(glob);
        Ok(())
    }
}


/// --glob-case-insensitive
#[derive(Debug)]
struct GlobCaseInsensitive;

impl Flag for GlobCaseInsensitive {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "glob-case-insensitive"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-glob-case-insensitive")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Process all glob patterns case insensitively."
    }
    fn doc_long(&self) -> &'static str {
        r"
Process all glob patterns given with the \flag{glob} flag case insensitively.
This effectively treats \flag{glob} as \flag{iglob}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.glob_case_insensitive = v.unwrap_switch();
        Ok(())
    }
}


/// --heading
#[derive(Debug)]
struct Heading;

impl Flag for Heading {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "heading"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-heading")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Print matches grouped by each file."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag prints the file path above clusters of matches from each file instead
of printing the file path as a prefix for each matched line.
.sp
This is the default mode when printing to a tty.
.sp
When \fBstdout\fP is not a tty, then ripgrep will default to the standard
grep-like format. One can force this format in Unix-like environments by
piping the output of ripgrep to \fBcat\fP. For example, \fBrg\fP \fIfoo\fP \fB|
cat\fP.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.heading = Some(v.unwrap_switch());
        Ok(())
    }
}


/// -h/--help
#[derive(Debug)]
struct Help;

impl Flag for Help {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "help"
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'h')
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Show help output."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag prints the help output for ripgrep.
.sp
Unlike most other flags, the behavior of the short flag, \fB\-h\fP, and the
long flag, \fB\-\-help\fP, is different. The short flag will show a condensed
help output while the long flag will show a verbose help output. The verbose
help output has complete documentation, where as the condensed help output will
show only a single line for every flag.
"
    }

    fn update(&self, v: FlagValue, _: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--help has no negation");
        // Since this flag has different semantics for -h and --help and the
        // Flag trait doesn't support encoding this sort of thing, we handle it
        // as a special case in the parser.
        Ok(())
    }
}


/// -./--hidden
#[derive(Debug)]
struct Hidden;

impl Flag for Hidden {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'.')
    }
    fn name_long(&self) -> &'static str {
        "hidden"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-hidden")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Search hidden files and directories."
    }
    fn doc_long(&self) -> &'static str {
        r#"
Search hidden files and directories. By default, hidden files and directories
are skipped. Note that if a hidden file or a directory is whitelisted in
an ignore file, then it will be searched even if this flag isn't provided.
Similarly if a hidden file or directory is given explicitly as an argument to
ripgrep.
.sp
A file or directory is considered hidden if its base name starts with a dot
character (\fB.\fP). On operating systems which support a "hidden" file
attribute, like Windows, files with this attribute are also considered hidden.
.sp
Note that \flag{hidden} will include files and folders like \fB.git\fP
regardless of \flag{no-ignore-vcs}. To exclude such paths when using
\flag{hidden}, you must explicitly ignore them using another flag or ignore
file.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.hidden = v.unwrap_switch();
        Ok(())
    }
}


/// --hostname-bin
#[derive(Debug)]
struct HostnameBin;

impl Flag for HostnameBin {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "hostname-bin"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("COMMAND")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Run a program to get this system's hostname."
    }
    fn doc_long(&self) -> &'static str {
        r#"
This flag controls how ripgrep determines this system's hostname. The flag's
value should correspond to an executable (either a path or something that can
be found via your system's \fBPATH\fP environment variable). When set, ripgrep
will run this executable, with no arguments, and treat its output (with leading
and trailing whitespace stripped) as your system's hostname.
.sp
When not set (the default, or the empty string), ripgrep will try to
automatically detect your system's hostname. On Unix, this corresponds
to calling \fBgethostname\fP. On Windows, this corresponds to calling
\fBGetComputerNameExW\fP to fetch the system's "physical DNS hostname."
.sp
ripgrep uses your system's hostname for producing hyperlinks.
"#
    }
    fn completion_type(&self) -> CompletionType {
        CompletionType::Executable
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let path = PathBuf::from(v.unwrap_value());
        args.hostname_bin =
            if path.as_os_str().is_empty() { None } else { Some(path) };
        Ok(())
    }
}


/// --hyperlink-format
#[derive(Debug)]
struct HyperlinkFormat;

impl Flag for HyperlinkFormat {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "hyperlink-format"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("FORMAT")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Set the format of hyperlinks."
    }
    fn doc_long(&self) -> &'static str {
        static DOC: LazyLock<String> = LazyLock::new(|| {
            let mut doc = String::new();
            doc.push_str(
                r#"
Set the format of hyperlinks to use when printing results. Hyperlinks make
certain elements of ripgrep's output, such as file paths, clickable. This
generally only works in terminal emulators that support OSC-8 hyperlinks. For
example, the format \fBfile://{host}{path}\fP will emit an RFC 8089 hyperlink.
To see the format that ripgrep is using, pass the \flag{debug} flag.
.sp
Alternatively, a format string may correspond to one of the following aliases:
"#,
            );

            let mut aliases = crate::printer::hyperlink_aliases();
            aliases.sort_by_key(|alias| {
                alias.display_priority().unwrap_or(i16::MAX)
            });
            for (i, alias) in aliases.iter().enumerate() {
                doc.push_str(r"\fB");
                doc.push_str(alias.name());
                doc.push_str(r"\fP");
                doc.push_str(if i < aliases.len() - 1 { ", " } else { "." });
            }
            doc.push_str(
                r#"
The alias will be replaced with a format string that is intended to work for
the corresponding application.
.sp
The following variables are available in the format string:
.sp
.TP 12
\fB{path}\fP
Required. This is replaced with a path to a matching file. The path is
guaranteed to be absolute and percent encoded such that it is valid to put into
a URI. Note that a path is guaranteed to start with a /.
.TP 12
\fB{host}\fP
Optional. This is replaced with your system's hostname. On Unix, this
corresponds to calling \fBgethostname\fP. On Windows, this corresponds to
calling \fBGetComputerNameExW\fP to fetch the system's "physical DNS hostname."
Alternatively, if \flag{hostname-bin} was provided, then the hostname returned
from the output of that program will be returned. If no hostname could be
found, then this variable is replaced with the empty string.
.TP 12
\fB{line}\fP
Optional. If appropriate, this is replaced with the line number of a match. If
no line number is available (for example, if \fB\-\-no\-line\-number\fP was
given), then it is automatically replaced with the value 1.
.TP 12
\fB{column}\fP
Optional, but requires the presence of \fB{line}\fP. If appropriate, this is
replaced with the column number of a match. If no column number is available
(for example, if \fB\-\-no\-column\fP was given), then it is automatically
replaced with the value 1.
.TP 12
\fB{wslprefix}\fP
Optional. This is a special value that is set to
\fBwsl$/\fP\fIWSL_DISTRO_NAME\fP, where \fIWSL_DISTRO_NAME\fP corresponds to
the value of the equivalent environment variable. If the system is not Unix
or if the \fIWSL_DISTRO_NAME\fP environment variable is not set, then this is
replaced with the empty string.
.PP
A format string may be empty. An empty format string is equivalent to the
\fBnone\fP alias. In this case, hyperlinks will be disabled.
.sp
At present, ripgrep does not enable hyperlinks by default. Users must opt into
them. If you aren't sure what format to use, try \fBdefault\fP.
.sp
Like colors, when ripgrep detects that stdout is not connected to a tty, then
hyperlinks are automatically disabled, regardless of the value of this flag.
Users can pass \fB\-\-color=always\fP to forcefully emit hyperlinks.
.sp
Note that hyperlinks are only written when a path is also in the output
and colors are enabled. To write hyperlinks without colors, you'll need to
configure ripgrep to not colorize anything without actually disabling all ANSI
escape codes completely:
.sp
.EX
    \-\-colors 'path:none' \\
    \-\-colors 'line:none' \\
    \-\-colors 'column:none' \\
    \-\-colors 'match:none'
.EE
.sp
ripgrep works this way because it treats the \flag{color} flag as a proxy for
whether ANSI escape codes should be used at all. This means that environment
variables like \fBNO_COLOR=1\fP and \fBTERM=dumb\fP not only disable colors,
but hyperlinks as well. Similarly, colors and hyperlinks are disabled when
ripgrep is not writing to a tty. (Unless one forces the issue by setting
\fB\-\-color=always\fP.)
.sp
If you're searching a file directly, for example:
.sp
.EX
    rg foo path/to/file
.EE
.sp
then hyperlinks will not be emitted since the path given does not appear
in the output. To make the path appear, and thus also a hyperlink, use the
\flag{with-filename} flag.
.sp
For more information on hyperlinks in terminal emulators, see:
https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda
"#,
            );
            doc
        });
        &DOC
    }

    fn doc_choices(&self) -> &'static [&'static str] {
        static CHOICES: LazyLock<Vec<String>> = LazyLock::new(|| {
            let mut aliases = crate::printer::hyperlink_aliases();
            aliases.sort_by_key(|alias| {
                alias.display_priority().unwrap_or(i16::MAX)
            });
            aliases.iter().map(|alias| alias.name().to_string()).collect()
        });
        static BORROWED: LazyLock<Vec<&'static str>> =
            LazyLock::new(|| CHOICES.iter().map(|name| &**name).collect());
        &*BORROWED
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let v = v.unwrap_value();
        let string = convert::str(&v)?;
        let format = string.parse().context("invalid hyperlink format")?;
        args.hyperlink_format = format;
        Ok(())
    }
}


/// --iglob
#[derive(Debug)]
struct IGlob;

impl Flag for IGlob {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "iglob"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("GLOB")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Include/exclude paths case insensitively."
    }
    fn doc_long(&self) -> &'static str {
        r"
Include or exclude files and directories for searching that match the given
glob. This always overrides any other ignore logic. Multiple glob flags may
be used. Globbing rules match \fB.gitignore\fP globs. Precede a glob with a
\fB!\fP to exclude it. If multiple globs match a file or directory, the glob
given later in the command line takes precedence. Globs used via this flag are
matched case insensitively.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let glob = convert::string(v.unwrap_value())?;
        args.iglobs.push(glob);
        Ok(())
    }
}


/// -i/--ignore-case
#[derive(Debug)]
struct IgnoreCase;

impl Flag for IgnoreCase {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'i')
    }
    fn name_long(&self) -> &'static str {
        "ignore-case"
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Case insensitive search."
    }
    fn doc_long(&self) -> &'static str {
        r#"
When this flag is provided, all patterns will be searched case insensitively.
The case insensitivity rules used by ripgrep's default regex engine conform to
Unicode's "simple" case folding rules.
.sp
This is a global option that applies to all patterns given to ripgrep.
Individual patterns can still be matched case sensitively by using
inline regex flags. For example, \fB(?\-i)abc\fP will match \fBabc\fP
case sensitively even when this flag is used.
.sp
This flag overrides \flag{case-sensitive} and \flag{smart-case}.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "flag has no negation");
        args.case = CaseMode::Insensitive;
        Ok(())
    }
}


/// --ignore-file
#[derive(Debug)]
struct IgnoreFile;

impl Flag for IgnoreFile {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "ignore-file"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("PATH")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Specify additional ignore files."
    }
    fn doc_long(&self) -> &'static str {
        r"
Specifies a path to one or more \fBgitignore\fP formatted rules files.
These patterns are applied after the patterns found in \fB.gitignore\fP,
\fB.rgignore\fP and \fB.ignore\fP are applied and are matched relative to the
current working directory. That is, files specified via this flag have lower
precedence than files automatically found in the directory tree. Multiple
additional ignore files can be specified by using this flag repeatedly. When
specifying multiple ignore files, earlier files have lower precedence than
later files.
.sp
If you are looking for a way to include or exclude files and directories
directly on the command line, then use \flag{glob} instead.
"
    }
    fn completion_type(&self) -> CompletionType {
        CompletionType::Filename
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let path = PathBuf::from(v.unwrap_value());
        args.ignore_file.push(path);
        Ok(())
    }
}


/// --ignore-file-case-insensitive
#[derive(Debug)]
struct IgnoreFileCaseInsensitive;

impl Flag for IgnoreFileCaseInsensitive {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "ignore-file-case-insensitive"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-ignore-file-case-insensitive")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Process ignore files case insensitively."
    }
    fn doc_long(&self) -> &'static str {
        r"
Process ignore files (\fB.gitignore\fP, \fB.ignore\fP, etc.) case
insensitively. Note that this comes with a performance penalty and is most
useful on case insensitive file systems (such as Windows).
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.ignore_file_case_insensitive = v.unwrap_switch();
        Ok(())
    }
}


/// --include-zero
#[derive(Debug)]
struct IncludeZero;

impl Flag for IncludeZero {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "include-zero"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-include-zero")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Include zero matches in summary output."
    }
    fn doc_long(&self) -> &'static str {
        r"
When used with \flag{count} or \flag{count-matches}, this causes ripgrep to
print the number of matches for each file even if there were zero matches. This
is disabled by default but can be enabled to make ripgrep behave more like
grep.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.include_zero = v.unwrap_switch();
        Ok(())
    }
}


/// -v/--invert-match
#[derive(Debug)]
struct InvertMatch;

impl Flag for InvertMatch {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'v')
    }
    fn name_long(&self) -> &'static str {
        "invert-match"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-invert-match")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Invert matching."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag inverts matching. That is, instead of printing lines that match,
ripgrep will print lines that don't match.
.sp
Note that this only inverts line-by-line matching. For example, combining this
flag with \flag{files-with-matches} will emit files that contain any lines
that do not match the patterns given. That's not the same as, for example,
\flag{files-without-match}, which will emit files that do not contain any
matching lines.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.invert_match = v.unwrap_switch();
        Ok(())
    }
}


/// --json
#[derive(Debug)]
struct JSON;

impl Flag for JSON {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "json"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-json")
    }
    fn doc_category(&self) -> Category {
        Category::OutputModes
    }
    fn doc_short(&self) -> &'static str {
        r"Show search results in a JSON Lines format."
    }
    fn doc_long(&self) -> &'static str {
        r"
Enable printing results in a JSON Lines format.
.sp
When this flag is provided, ripgrep will emit a sequence of messages, each
encoded as a JSON object, where there are five different message types:
.sp
.TP 12
\fBbegin\fP
A message that indicates a file is being searched and contains at least one
match.
.TP 12
\fBend\fP
A message the indicates a file is done being searched. This message also
include summary statistics about the search for a particular file.
.TP 12
\fBmatch\fP
A message that indicates a match was found. This includes the text and offsets
of the match.
.TP 12
\fBcontext\fP
A message that indicates a contextual line was found. This includes the text of
the line, along with any match information if the search was inverted.
.TP 12
\fBsummary\fP
The final message emitted by ripgrep that contains summary statistics about the
search across all files.
.PP
Since file paths or the contents of files are not guaranteed to be valid
UTF-8 and JSON itself must be representable by a Unicode encoding, ripgrep
will emit all data elements as objects with one of two keys: \fBtext\fP or
\fBbytes\fP. \fBtext\fP is a normal JSON string when the data is valid UTF-8
while \fBbytes\fP is the base64 encoded contents of the data.
.sp
The JSON Lines format is only supported for showing search results. It cannot
be used with other flags that emit other types of output, such as \flag{files},
\flag{files-with-matches}, \flag{files-without-match}, \flag{count} or
\flag{count-matches}. ripgrep will report an error if any of the aforementioned
flags are used in concert with \flag{json}.
.sp
Other flags that control aspects of the standard output such as
\flag{only-matching}, \flag{heading}, \flag{replace}, \flag{max-columns}, etc.,
have no effect when \flag{json} is set. However, enabling JSON output will
always implicitly and unconditionally enable \flag{stats}.
.sp
A more complete description of the JSON format used can be found here:
\fIhttps://docs.rs/grep-printer/*/grep_printer/struct.JSON.html\fP.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        if v.unwrap_switch() {
            args.mode.update(Mode::Search(SearchMode::JSON));
        } else if matches!(args.mode, Mode::Search(SearchMode::JSON)) {
            // --no-json only reverts to the default mode if the mode is
            // JSON, otherwise it's a no-op.
            args.mode.update(Mode::Search(SearchMode::Standard));
        }
        Ok(())
    }
}


/// --line-buffered
#[derive(Debug)]
struct LineBuffered;

impl Flag for LineBuffered {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "line-buffered"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-line-buffered")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Force line buffering."
    }
    fn doc_long(&self) -> &'static str {
        r"
When enabled, ripgrep will always use line buffering. That is, whenever a
matching line is found, it will be flushed to stdout immediately. This is the
default when ripgrep's stdout is connected to a tty, but otherwise, ripgrep
will use block buffering, which is typically faster. This flag forces ripgrep
to use line buffering even if it would otherwise use block buffering. This is
typically useful in shell pipelines, for example:
.sp
.EX
    tail -f something.log | rg foo --line-buffered | rg bar
.EE
.sp
This overrides the \flag{block-buffered} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.buffer = if v.unwrap_switch() {
            BufferMode::Line
        } else {
            BufferMode::Auto
        };
        Ok(())
    }
}


/// -n/--line-number
#[derive(Debug)]
struct LineNumber;

impl Flag for LineNumber {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'n')
    }
    fn name_long(&self) -> &'static str {
        "line-number"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Show line numbers."
    }
    fn doc_long(&self) -> &'static str {
        r"
Show line numbers (1-based).
.sp
This is enabled by default when stdout is connected to a tty.
.sp
This flag can be disabled by \flag{no-line-number}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--line-number has no automatic negation");
        args.line_number = Some(true);
        Ok(())
    }
}


/// -N/--no-line-number
#[derive(Debug)]
struct LineNumberNo;

impl Flag for LineNumberNo {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'N')
    }
    fn name_long(&self) -> &'static str {
        "no-line-number"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Suppress line numbers."
    }
    fn doc_long(&self) -> &'static str {
        r"
Suppress line numbers.
.sp
Line numbers are off by default when stdout is not connected to a tty.
.sp
Line numbers can be forcefully turned on by \flag{line-number}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(
            v.unwrap_switch(),
            "--no-line-number has no automatic negation"
        );
        args.line_number = Some(false);
        Ok(())
    }
}


/// -x/--line-regexp
#[derive(Debug)]
struct LineRegexp;

impl Flag for LineRegexp {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'x')
    }
    fn name_long(&self) -> &'static str {
        "line-regexp"
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Show matches surrounded by line boundaries."
    }
    fn doc_long(&self) -> &'static str {
        r"
When enabled, ripgrep will only show matches surrounded by line boundaries.
This is equivalent to surrounding every pattern with \fB^\fP and \fB$\fP. In
other words, this only prints lines where the entire line participates in a
match.
.sp
This overrides the \flag{word-regexp} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--line-regexp has no negation");
        args.boundary = Some(BoundaryMode::Line);
        Ok(())
    }
}


/// -M/--max-columns
#[derive(Debug)]
struct MaxColumns;

impl Flag for MaxColumns {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'M')
    }
    fn name_long(&self) -> &'static str {
        "max-columns"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Omit lines longer than this limit."
    }
    fn doc_long(&self) -> &'static str {
        r"
When given, ripgrep will omit lines longer than this limit in bytes. Instead of
printing long lines, only the number of matches in that line is printed.
.sp
When this flag is omitted or is set to \fB0\fP, then it has no effect.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let max = convert::u64(&v.unwrap_value())?;
        args.max_columns = if max == 0 { None } else { Some(max) };
        Ok(())
    }
}


/// --max-columns-preview
#[derive(Debug)]
struct MaxColumnsPreview;

impl Flag for MaxColumnsPreview {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "max-columns-preview"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-max-columns-preview")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Show preview for lines exceeding the limit."
    }
    fn doc_long(&self) -> &'static str {
        r"
Prints a preview for lines exceeding the configured max column limit.
.sp
When the \flag{max-columns} flag is used, ripgrep will by default completely
replace any line that is too long with a message indicating that a matching
line was removed. When this flag is combined with \flag{max-columns}, a preview
of the line (corresponding to the limit size) is shown instead, where the part
of the line exceeding the limit is not shown.
.sp
If the \flag{max-columns} flag is not set, then this has no effect.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.max_columns_preview = v.unwrap_switch();
        Ok(())
    }
}


/// -m/--max-count
#[derive(Debug)]
struct MaxCount;

impl Flag for MaxCount {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'm')
    }
    fn name_long(&self) -> &'static str {
        "max-count"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Limit the number of matching lines."
    }
    fn doc_long(&self) -> &'static str {
        r"
Limit the number of matching lines per file searched to \fINUM\fP.
.sp
When \flag{multiline} is used, a single match that spans multiple lines is only
counted once for the purposes of this limit. Multiple matches in a single line
are counted only once, as they would be in non-multiline mode.
.sp
When combined with \flag{after-context} or \flag{context}, it's possible for
more matches than the maximum to be printed if contextual lines contain a
match.
.sp
Note that \fB0\fP is a legal value but not likely to be useful. When used,
ripgrep won't search anything.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.max_count = Some(convert::u64(&v.unwrap_value())?);
        Ok(())
    }
}


/// --max-depth
#[derive(Debug)]
struct MaxDepth;

impl Flag for MaxDepth {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'd')
    }
    fn name_long(&self) -> &'static str {
        "max-depth"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["maxdepth"]
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Descend at most NUM directories."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag limits the depth of directory traversal to \fINUM\fP levels beyond
the paths given. A value of \fB0\fP only searches the explicitly given paths
themselves.
.sp
For example, \fBrg --max-depth 0 \fP\fIdir/\fP is a no-op because \fIdir/\fP
will not be descended into. \fBrg --max-depth 1 \fP\fIdir/\fP will search only
the direct children of \fIdir\fP.
.sp
An alternative spelling for this flag is \fB\-\-maxdepth\fP.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.max_depth = Some(convert::usize(&v.unwrap_value())?);
        Ok(())
    }
}


/// --max-filesize
#[derive(Debug)]
struct MaxFilesize;

impl Flag for MaxFilesize {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "max-filesize"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM+SUFFIX?")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Ignore files larger than NUM in size."
    }
    fn doc_long(&self) -> &'static str {
        r"
Ignore files larger than \fINUM\fP in size. This does not apply to directories.
.sp
The input format accepts suffixes of \fBK\fP, \fBM\fP or \fBG\fP which
correspond to kilobytes, megabytes and gigabytes, respectively. If no suffix is
provided the input is treated as bytes.
.sp
Examples: \fB\-\-max-filesize 50K\fP or \fB\-\-max\-filesize 80M\fP.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let v = v.unwrap_value();
        args.max_filesize = Some(convert::human_readable_u64(&v)?);
        Ok(())
    }
}


/// --mmap
#[derive(Debug)]
struct Mmap;

impl Flag for Mmap {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "mmap"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-mmap")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Search with memory maps when possible."
    }
    fn doc_long(&self) -> &'static str {
        r"
When enabled, ripgrep will search using memory maps when possible. This is
enabled by default when ripgrep thinks it will be faster.
.sp
Memory map searching cannot be used in all circumstances. For example, when
searching virtual files or streams likes \fBstdin\fP. In such cases, memory
maps will not be used even when this flag is enabled.
.sp
Note that ripgrep may abort unexpectedly when memory maps are used if it
searches a file that is simultaneously truncated. Users can opt out of this
possibility by disabling memory maps.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.mmap = if v.unwrap_switch() {
            MmapMode::AlwaysTryMmap
        } else {
            MmapMode::Never
        };
        Ok(())
    }
}


/// -U/--multiline
#[derive(Debug)]
struct Multiline;

impl Flag for Multiline {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'U')
    }
    fn name_long(&self) -> &'static str {
        "multiline"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-multiline")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Enable searching across multiple lines."
    }
    fn doc_long(&self) -> &'static str {
        r#"
This flag enables searching across multiple lines.
.sp
When multiline mode is enabled, ripgrep will lift the restriction that a
match cannot include a line terminator. For example, when multiline mode
is not enabled (the default), then the regex \fB\\p{any}\fP will match any
Unicode codepoint other than \fB\\n\fP. Similarly, the regex \fB\\n\fP is
explicitly forbidden, and if you try to use it, ripgrep will return an error.
However, when multiline mode is enabled, \fB\\p{any}\fP will match any Unicode
codepoint, including \fB\\n\fP, and regexes like \fB\\n\fP are permitted.
.sp
An important caveat is that multiline mode does not change the match semantics
of \fB.\fP. Namely, in most regex matchers, a \fB.\fP will by default match any
character other than \fB\\n\fP, and this is true in ripgrep as well. In order
to make \fB.\fP match \fB\\n\fP, you must enable the "dot all" flag inside the
regex. For example, both \fB(?s).\fP and \fB(?s:.)\fP have the same semantics,
where \fB.\fP will match any character, including \fB\\n\fP. Alternatively, the
\flag{multiline-dotall} flag may be passed to make the "dot all" behavior the
default. This flag only applies when multiline search is enabled.
.sp
There is no limit on the number of the lines that a single match can span.
.sp
\fBWARNING\fP: Because of how the underlying regex engine works, multiline
searches may be slower than normal line-oriented searches, and they may also
use more memory. In particular, when multiline mode is enabled, ripgrep
requires that each file it searches is laid out contiguously in memory (either
by reading it onto the heap or by memory-mapping it). Things that cannot be
memory-mapped (such as \fBstdin\fP) will be consumed until EOF before searching
can begin. In general, ripgrep will only do these things when necessary.
Specifically, if the \flag{multiline} flag is provided but the regex does
not contain patterns that would match \fB\\n\fP characters, then ripgrep
will automatically avoid reading each file into memory before searching it.
Nevertheless, if you only care about matches spanning at most one line, then it
is always better to disable multiline mode.
.sp
This overrides the \flag{stop-on-nonmatch} flag.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.multiline = v.unwrap_switch();
        if args.multiline {
            args.stop_on_nonmatch = false;
        }
        Ok(())
    }
}


/// --multiline-dotall
#[derive(Debug)]
struct MultilineDotall;

impl Flag for MultilineDotall {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "multiline-dotall"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-multiline-dotall")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Make '.' match line terminators."
    }
    fn doc_long(&self) -> &'static str {
        r#"
This flag enables "dot all" mode in all regex patterns. This causes \fB.\fP to
match line terminators when multiline searching is enabled. This flag has no
effect if multiline searching isn't enabled with the \flag{multiline} flag.
.sp
Normally, a \fB.\fP will match any character except line terminators. While
this behavior typically isn't relevant for line-oriented matching (since
matches can span at most one line), this can be useful when searching with the
\flag{multiline} flag. By default, multiline mode runs without "dot all" mode
enabled.
.sp
This flag is generally intended to be used in an alias or your ripgrep config
file if you prefer "dot all" semantics by default. Note that regardless of
whether this flag is used, "dot all" semantics can still be controlled via
inline flags in the regex pattern itself, e.g., \fB(?s:.)\fP always enables
"dot all" whereas \fB(?-s:.)\fP always disables "dot all". Moreover, you
can use character classes like \fB\\p{any}\fP to match any Unicode codepoint
regardless of whether "dot all" mode is enabled or not.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.multiline_dotall = v.unwrap_switch();
        Ok(())
    }
}


/// --no-config
#[derive(Debug)]
struct NoConfig;

impl Flag for NoConfig {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-config"
    }
    fn doc_category(&self) -> Category {
        Category::OtherBehaviors
    }
    fn doc_short(&self) -> &'static str {
        r"Never read configuration files."
    }
    fn doc_long(&self) -> &'static str {
        r"
When set, ripgrep will never read configuration files. When this flag is
present, ripgrep will not respect the \fBRIPGREP_CONFIG_PATH\fP environment
variable.
.sp
If ripgrep ever grows a feature to automatically read configuration files in
pre-defined locations, then this flag will also disable that behavior as well.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--no-config has no negation");
        args.no_config = true;
        Ok(())
    }
}


/// --no-ignore
#[derive(Debug)]
struct NoIgnore;

impl Flag for NoIgnore {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-ignore"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("ignore")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Don't use ignore files."
    }
    fn doc_long(&self) -> &'static str {
        r"
When set, ignore files such as \fB.gitignore\fP, \fB.ignore\fP and
\fB.rgignore\fP will not be respected. This implies \flag{no-ignore-dot},
\flag{no-ignore-exclude}, \flag{no-ignore-global}, \flag{no-ignore-parent} and
\flag{no-ignore-vcs}.
.sp
This does not imply \flag{no-ignore-files}, since \flag{ignore-file} is
specified explicitly as a command line argument.
.sp
When given only once, the \flag{unrestricted} flag is identical in
behavior to this flag and can be considered an alias. However, subsequent
\flag{unrestricted} flags have additional effects.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let yes = v.unwrap_switch();
        args.no_ignore_dot = yes;
        args.no_ignore_exclude = yes;
        args.no_ignore_global = yes;
        args.no_ignore_parent = yes;
        args.no_ignore_vcs = yes;
        Ok(())
    }
}


/// --no-ignore-dot
#[derive(Debug)]
struct NoIgnoreDot;

impl Flag for NoIgnoreDot {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-ignore-dot"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("ignore-dot")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Don't use .ignore or .rgignore files."
    }
    fn doc_long(&self) -> &'static str {
        r"
Don't respect filter rules from \fB.ignore\fP or \fB.rgignore\fP files.
.sp
This does not impact whether ripgrep will ignore files and directories whose
names begin with a dot. For that, see the \flag{hidden} flag. This flag also
does not impact whether filter rules from \fB.gitignore\fP files are respected.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_ignore_dot = v.unwrap_switch();
        Ok(())
    }
}


/// --no-ignore-exclude
#[derive(Debug)]
struct NoIgnoreExclude;

impl Flag for NoIgnoreExclude {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-ignore-exclude"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("ignore-exclude")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Don't use local exclusion files."
    }
    fn doc_long(&self) -> &'static str {
        r"
Don't respect filter rules from files that are manually configured for the repository.
For example, this includes \fBgit\fP's \fB.git/info/exclude\fP.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_ignore_exclude = v.unwrap_switch();
        Ok(())
    }
}


/// --no-ignore-files
#[derive(Debug)]
struct NoIgnoreFiles;

impl Flag for NoIgnoreFiles {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-ignore-files"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("ignore-files")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Don't use --ignore-file arguments."
    }
    fn doc_long(&self) -> &'static str {
        r"
When set, any \flag{ignore-file} flags, even ones that come after this flag,
are ignored.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_ignore_files = v.unwrap_switch();
        Ok(())
    }
}


/// --no-ignore-global
#[derive(Debug)]
struct NoIgnoreGlobal;

impl Flag for NoIgnoreGlobal {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-ignore-global"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("ignore-global")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Don't use global ignore files."
    }
    fn doc_long(&self) -> &'static str {
        r#"
Don't respect filter rules from ignore files that come from "global" sources
such as \fBgit\fP's \fBcore.excludesFile\fP configuration option (which
defaults to \fB$HOME/.config/git/ignore\fP).
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_ignore_global = v.unwrap_switch();
        Ok(())
    }
}


/// --no-ignore-messages
#[derive(Debug)]
struct NoIgnoreMessages;

impl Flag for NoIgnoreMessages {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-ignore-messages"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("ignore-messages")
    }
    fn doc_category(&self) -> Category {
        Category::Logging
    }
    fn doc_short(&self) -> &'static str {
        r"Suppress gitignore parse error messages."
    }
    fn doc_long(&self) -> &'static str {
        r"
When this flag is enabled, all error messages related to parsing ignore files
are suppressed. By default, error messages are printed to stderr. In cases
where these errors are expected, this flag can be used to avoid seeing the
noise produced by the messages.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_ignore_messages = v.unwrap_switch();
        Ok(())
    }
}


/// --no-ignore-parent
#[derive(Debug)]
struct NoIgnoreParent;

impl Flag for NoIgnoreParent {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-ignore-parent"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("ignore-parent")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Don't use ignore files in parent directories."
    }
    fn doc_long(&self) -> &'static str {
        r"
When this flag is set, filter rules from ignore files found in parent
directories are not respected. By default, ripgrep will ascend the parent
directories of the current working directory to look for any applicable ignore
files that should be applied. In some cases this may not be desirable.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_ignore_parent = v.unwrap_switch();
        Ok(())
    }
}


/// --no-ignore-vcs
#[derive(Debug)]
struct NoIgnoreVcs;

impl Flag for NoIgnoreVcs {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-ignore-vcs"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("ignore-vcs")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Don't use ignore files from source control."
    }
    fn doc_long(&self) -> &'static str {
        r"
When given, filter rules from source control ignore files (e.g.,
\fB.gitignore\fP) are not respected. By default, ripgrep respects \fBgit\fP's
ignore rules for automatic filtering. In some cases, it may not be desirable
to respect the source control's ignore rules and instead only respect rules in
\fB.ignore\fP or \fB.rgignore\fP.
.sp
Note that this flag does not directly affect the filtering of source control
files or folders that start with a dot (\fB.\fP), like \fB.git\fP. These are
affected by \flag{hidden} and its related flags instead.
.sp
This flag implies \flag{no-ignore-parent} for source control ignore files as
well.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_ignore_vcs = v.unwrap_switch();
        Ok(())
    }
}


/// --no-messages
#[derive(Debug)]
struct NoMessages;

impl Flag for NoMessages {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-messages"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("messages")
    }
    fn doc_category(&self) -> Category {
        Category::Logging
    }
    fn doc_short(&self) -> &'static str {
        r"Suppress some error messages."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag suppresses some error messages. Specifically, messages related to
the failed opening and reading of files. Error messages related to the syntax
of the pattern are still shown.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_messages = v.unwrap_switch();
        Ok(())
    }
}


/// --no-pcre2-unicode
#[derive(Debug)]
struct NoPcre2Unicode;

impl Flag for NoPcre2Unicode {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-pcre2-unicode"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("pcre2-unicode")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"(DEPRECATED) Disable Unicode mode for PCRE2."
    }
    fn doc_long(&self) -> &'static str {
        r"
DEPRECATED. Use \flag{no-unicode} instead.
.sp
Note that Unicode mode is enabled by default.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_unicode = v.unwrap_switch();
        Ok(())
    }
}


/// --no-require-git
#[derive(Debug)]
struct NoRequireGit;

impl Flag for NoRequireGit {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-require-git"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("require-git")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Use .gitignore outside of git repositories."
    }
    fn doc_long(&self) -> &'static str {
        r"
When this flag is given, source control ignore files such as \fB.gitignore\fP
are respected even if no \fBgit\fP repository is present.
.sp
By default, ripgrep will only respect filter rules from source control ignore
files when ripgrep detects that the search is executed inside a source control
repository. For example, when a \fB.git\fP directory is observed.
.sp
This flag relaxes the default restriction. For example, it might be useful when
the contents of a \fBgit\fP repository are stored or copied somewhere, but
where the repository state is absent.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_require_git = v.unwrap_switch();
        Ok(())
    }
}


/// --no-unicode
#[derive(Debug)]
struct NoUnicode;

impl Flag for NoUnicode {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "no-unicode"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("unicode")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Disable Unicode mode."
    }
    fn doc_long(&self) -> &'static str {
        r#"
This flag disables Unicode mode for all patterns given to ripgrep.
.sp
By default, ripgrep will enable "Unicode mode" in all of its regexes. This has
a number of consequences:
.sp
.IP \(bu 3n
\fB.\fP will only match valid UTF-8 encoded Unicode scalar values.
.sp
.IP \(bu 3n
Classes like \fB\\w\fP, \fB\\s\fP, \fB\\d\fP are all Unicode aware and much
bigger than their ASCII only versions.
.sp
.IP \(bu 3n
Case insensitive matching will use Unicode case folding.
.sp
.IP \(bu 3n
A large array of classes like \fB\\p{Emoji}\fP are available. (Although the
specific set of classes available varies based on the regex engine. In general,
the default regex engine has more classes available to it.)
.sp
.IP \(bu 3n
Word boundaries (\fB\\b\fP and \fB\\B\fP) use the Unicode definition of a word
character.
.PP
In some cases it can be desirable to turn these things off. This flag will do
exactly that. For example, Unicode mode can sometimes have a negative impact
on performance, especially when things like \fB\\w\fP are used frequently
(including via bounded repetitions like \fB\\w{100}\fP) when only their ASCII
interpretation is needed.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.no_unicode = v.unwrap_switch();
        Ok(())
    }
}


/// -0/--null
#[derive(Debug)]
struct Null;

impl Flag for Null {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'0')
    }
    fn name_long(&self) -> &'static str {
        "null"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Print a NUL byte after file paths."
    }
    fn doc_long(&self) -> &'static str {
        r"
Whenever a file path is printed, follow it with a \fBNUL\fP byte. This includes
printing file paths before matches, and when printing a list of matching files
such as with \flag{count}, \flag{files-with-matches} and \flag{files}. This
option is useful for use with \fBxargs\fP.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--null has no negation");
        args.null = true;
        Ok(())
    }
}


/// --null-data
#[derive(Debug)]
struct NullData;

impl Flag for NullData {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "null-data"
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Use NUL as a line terminator."
    }
    fn doc_long(&self) -> &'static str {
        r"
Enabling this flag causes ripgrep to use \fBNUL\fP as a line terminator instead
of the default of \fP\\n\fP.
.sp
This is useful when searching large binary files that would otherwise have
very long lines if \fB\\n\fP were used as the line terminator. In particular,
ripgrep requires that, at a minimum, each line must fit into memory. Using
\fBNUL\fP instead can be a useful stopgap to keep memory requirements low and
avoid OOM (out of memory) conditions.
.sp
This is also useful for processing NUL delimited data, such as that emitted
when using ripgrep's \flag{null} flag or \fBfind\fP's \fB\-\-print0\fP flag.
.sp
Using this flag implies \flag{text}. It also overrides \flag{crlf}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--null-data has no negation");
        args.crlf = false;
        args.null_data = true;
        Ok(())
    }
}


/// --one-file-system
#[derive(Debug)]
struct OneFileSystem;

impl Flag for OneFileSystem {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "one-file-system"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-one-file-system")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Skip directories on other file systems."
    }
    fn doc_long(&self) -> &'static str {
        r"
When enabled, ripgrep will not cross file system boundaries relative to where
the search started from.
.sp
Note that this applies to each path argument given to ripgrep. For example, in
the command
.sp
.EX
    rg \-\-one\-file\-system /foo/bar /quux/baz
.EE
.sp
ripgrep will search both \fI/foo/bar\fP and \fI/quux/baz\fP even if they are
on different file systems, but will not cross a file system boundary when
traversing each path's directory tree.
.sp
This is similar to \fBfind\fP's \fB\-xdev\fP or \fB\-mount\fP flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.one_file_system = v.unwrap_switch();
        Ok(())
    }
}


/// -o/--only-matching
#[derive(Debug)]
struct OnlyMatching;

impl Flag for OnlyMatching {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'o')
    }
    fn name_long(&self) -> &'static str {
        "only-matching"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Print only matched parts of a line."
    }
    fn doc_long(&self) -> &'static str {
        r"
Print only the matched (non-empty) parts of a matching line, with each such
part on a separate output line.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--only-matching does not have a negation");
        args.only_matching = true;
        Ok(())
    }
}


/// --path-separator
#[derive(Debug)]
struct PathSeparator;

impl Flag for PathSeparator {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "path-separator"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("SEPARATOR")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Set the path separator for printing paths."
    }
    fn doc_long(&self) -> &'static str {
        r"
Set the path separator to use when printing file paths. This defaults to your
platform's path separator, which is \fB/\fP on Unix and \fB\\\fP on Windows.
This flag is intended for overriding the default when the environment demands
it (e.g., cygwin). A path separator is limited to a single byte.
.sp
Setting this flag to an empty string reverts it to its default behavior. That
is, the path separator is automatically chosen based on the environment.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let s = convert::string(v.unwrap_value())?;
        let raw = crate::rg::bytes::unescape(&s);
        args.path_separator = if raw.is_empty() {
            None
        } else if raw.len() == 1 {
            Some(raw[0])
        } else {
            anyhow::bail!(
                "A path separator must be exactly one byte, but \
                 the given separator is {len} bytes: {sep}\n\
                 In some shells on Windows '/' is automatically \
                 expanded. Use '//' instead.",
                len = raw.len(),
                sep = s,
            )
        };
        Ok(())
    }
}


/// --passthru
#[derive(Debug)]
struct Passthru;

impl Flag for Passthru {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "passthru"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["passthrough"]
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Print both matching and non-matching lines."
    }
    fn doc_long(&self) -> &'static str {
        r#"
Print both matching and non-matching lines.
.sp
Another way to achieve a similar effect is by modifying your pattern to match
the empty string. For example, if you are searching using \fBrg\fP \fIfoo\fP,
then using \fBrg\fP \fB'^|\fP\fIfoo\fP\fB'\fP instead will emit every line in
every file searched, but only occurrences of \fIfoo\fP will be highlighted.
This flag enables the same behavior without needing to modify the pattern.
.sp
An alternative spelling for this flag is \fB\-\-passthrough\fP.
.sp
This overrides the \flag{context}, \flag{after-context} and
\flag{before-context} flags.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--passthru has no negation");
        args.context = ContextMode::Passthru;
        Ok(())
    }
}


/// -P/--pcre2
#[derive(Debug)]
struct PCRE2;

impl Flag for PCRE2 {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'P')
    }
    fn name_long(&self) -> &'static str {
        "pcre2"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-pcre2")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Enable PCRE2 matching."
    }
    fn doc_long(&self) -> &'static str {
        r"
When this flag is present, ripgrep will use the PCRE2 regex engine instead of
its default regex engine.
.sp
This is generally useful when you want to use features such as look-around
or backreferences.
.sp
Using this flag is the same as passing \fB\-\-engine=pcre2\fP. Users may
instead elect to use \fB\-\-engine=auto\fP to ask ripgrep to automatically
select the right regex engine based on the patterns given. This flag and the
\flag{engine} flag override one another.
.sp
Note that PCRE2 is an optional ripgrep feature. If PCRE2 wasn't included in
your build of ripgrep, then using this flag will result in ripgrep printing
an error message and exiting. PCRE2 may also have worse user experience in
some cases, since it has fewer introspection APIs than ripgrep's default
regex engine. For example, if you use a \fB\\n\fP in a PCRE2 regex without
the \flag{multiline} flag, then ripgrep will silently fail to match anything
instead of reporting an error immediately (like it does with the default regex
engine).
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.engine = if v.unwrap_switch() {
            EngineChoice::PCRE2
        } else {
            EngineChoice::Default
        };
        Ok(())
    }
}


/// --pcre2-version
#[derive(Debug)]
struct PCRE2Version;

impl Flag for PCRE2Version {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "pcre2-version"
    }
    fn doc_category(&self) -> Category {
        Category::OtherBehaviors
    }
    fn doc_short(&self) -> &'static str {
        r"Print the version of PCRE2 that ripgrep uses."
    }
    fn doc_long(&self) -> &'static str {
        r"
When this flag is present, ripgrep will print the version of PCRE2 in use,
along with other information, and then exit. If PCRE2 is not available, then
ripgrep will print an error message and exit with an error code.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--pcre2-version has no negation");
        args.special = Some(SpecialMode::VersionPCRE2);
        Ok(())
    }
}


/// --pre
#[derive(Debug)]
struct Pre;

impl Flag for Pre {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "pre"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-pre")
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("COMMAND")
    }
    fn doc_category(&self) -> Category {
        Category::Input
    }
    fn doc_short(&self) -> &'static str {
        r"Search output of COMMAND for each PATH."
    }
    fn doc_long(&self) -> &'static str {
        r#"
For each input \fIPATH\fP, this flag causes ripgrep to search the standard
output of \fICOMMAND\fP \fIPATH\fP instead of the contents of \fIPATH\fP.
This option expects the \fICOMMAND\fP program to either be a path or to be
available in your \fBPATH\fP. Either an empty string \fICOMMAND\fP or the
\fB\-\-no\-pre\fP flag will disable this behavior.
.sp
.TP 12
\fBWARNING\fP
When this flag is set, ripgrep will unconditionally spawn a process for every
file that is searched. Therefore, this can incur an unnecessarily large
performance penalty if you don't otherwise need the flexibility offered by this
flag. One possible mitigation to this is to use the \flag{pre-glob} flag to
limit which files a preprocessor is run with.
.PP
A preprocessor is not run when ripgrep is searching stdin.
.sp
When searching over sets of files that may require one of several
preprocessors, \fICOMMAND\fP should be a wrapper program which first classifies
\fIPATH\fP based on magic numbers/content or based on the \fIPATH\fP name and
then dispatches to an appropriate preprocessor. Each \fICOMMAND\fP also has its
standard input connected to \fIPATH\fP for convenience.
.sp
For example, a shell script for \fICOMMAND\fP might look like:
.sp
.EX
    case "$1" in
    *.pdf)
        exec pdftotext "$1" -
        ;;
    *)
        case $(file "$1") in
        *Zstandard*)
            exec pzstd -cdq
            ;;
        *)
            exec cat
            ;;
        esac
        ;;
    esac
.EE
.sp
The above script uses \fBpdftotext\fP to convert a PDF file to plain text. For
all other files, the script uses the \fBfile\fP utility to sniff the type of
the file based on its contents. If it is a compressed file in the Zstandard
format, then \fBpzstd\fP is used to decompress the contents to stdout.
.sp
This overrides the \flag{search-zip} flag.
"#
    }
    fn completion_type(&self) -> CompletionType {
        CompletionType::Executable
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let path = match v {
            FlagValue::Value(v) => PathBuf::from(v),
            FlagValue::Switch(yes) => {
                assert!(!yes, "there is no affirmative switch for --pre");
                args.pre = None;
                return Ok(());
            }
        };
        args.pre = if path.as_os_str().is_empty() { None } else { Some(path) };
        if args.pre.is_some() {
            args.search_zip = false;
        }
        Ok(())
    }
}


/// --pre-glob
#[derive(Debug)]
struct PreGlob;

impl Flag for PreGlob {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "pre-glob"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("GLOB")
    }
    fn doc_category(&self) -> Category {
        Category::Input
    }
    fn doc_short(&self) -> &'static str {
        r"Include or exclude files from a preprocessor."
    }
    fn doc_long(&self) -> &'static str {
        r#"
This flag works in conjunction with the \flag{pre} flag. Namely, when one or
more \flag{pre-glob} flags are given, then only files that match the given set
of globs will be handed to the command specified by the \flag{pre} flag. Any
non-matching files will be searched without using the preprocessor command.
.sp
This flag is useful when searching many files with the \flag{pre} flag.
Namely, it provides the ability to avoid process overhead for files that
don't need preprocessing. For example, given the following shell script,
\fIpre-pdftotext\fP:
.sp
.EX
    #!/bin/sh
    pdftotext "$1" -
.EE
.sp
then it is possible to use \fB\-\-pre\fP \fIpre-pdftotext\fP
\fB\-\-pre\-glob\fP '\fI*.pdf\fP' to make it so ripgrep only executes
the \fIpre-pdftotext\fP command on files with a \fI.pdf\fP extension.
.sp
Multiple \flag{pre-glob} flags may be used. Globbing rules match
\fBgitignore\fP globs. Precede a glob with a \fB!\fP to exclude it.
.sp
This flag has no effect if the \flag{pre} flag is not used.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let glob = convert::string(v.unwrap_value())?;
        args.pre_glob.push(glob);
        Ok(())
    }
}


/// -p/--pretty
#[derive(Debug)]
struct Pretty;

impl Flag for Pretty {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'p')
    }
    fn name_long(&self) -> &'static str {
        "pretty"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Alias for colors, headings and line numbers."
    }
    fn doc_long(&self) -> &'static str {
        r"
This is a convenience alias for \fB\-\-color=always \-\-heading
\-\-line\-number\fP. This flag is useful when you still want pretty output even
if you're piping ripgrep to another program or file. For example: \fBrg -p
\fP\fIfoo\fP \fB| less -R\fP.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--pretty has no negation");
        args.color = ColorChoice::Always;
        args.heading = Some(true);
        args.line_number = Some(true);
        Ok(())
    }
}


/// -q/--quiet
#[derive(Debug)]
struct Quiet;

impl Flag for Quiet {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'q')
    }
    fn name_long(&self) -> &'static str {
        "quiet"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Do not print anything to stdout."
    }
    fn doc_long(&self) -> &'static str {
        r"
Do not print anything to stdout. If a match is found in a file, then ripgrep
will stop searching. This is useful when ripgrep is used only for its exit code
(which will be an error code if no matches are found).
.sp
When \flag{files} is used, ripgrep will stop finding files after finding the
first file that does not match any ignore rules.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--quiet has no negation");
        args.quiet = true;
        Ok(())
    }
}


/// --regex-size-limit
#[derive(Debug)]
struct RegexSizeLimit;

impl Flag for RegexSizeLimit {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "regex-size-limit"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM+SUFFIX?")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"The size limit of the compiled regex."
    }
    fn doc_long(&self) -> &'static str {
        r"
The size limit of the compiled regex, where the compiled regex generally
corresponds to a single object in memory that can match all of the patterns
provided to ripgrep. The default limit is generous enough that most reasonable
patterns (or even a small number of them) should fit.
.sp
This useful to change when you explicitly want to let ripgrep spend potentially
much more time and/or memory building a regex matcher.
.sp
The input format accepts suffixes of \fBK\fP, \fBM\fP or \fBG\fP which
correspond to kilobytes, megabytes and gigabytes, respectively. If no suffix is
provided the input is treated as bytes.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let v = v.unwrap_value();
        args.regex_size_limit = Some(convert::human_readable_usize(&v)?);
        Ok(())
    }
}


/// -e/--regexp
#[derive(Debug)]
struct Regexp;

impl Flag for Regexp {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'e')
    }
    fn name_long(&self) -> &'static str {
        "regexp"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("PATTERN")
    }
    fn doc_category(&self) -> Category {
        Category::Input
    }
    fn doc_short(&self) -> &'static str {
        r"A pattern to search for."
    }
    fn doc_long(&self) -> &'static str {
        r"
A pattern to search for. This option can be provided multiple times, where
all patterns given are searched, in addition to any patterns provided by
\flag{file}. Lines matching at least one of the provided patterns are printed.
This flag can also be used when searching for patterns that start with a dash.
.sp
For example, to search for the literal \fB\-foo\fP:
.sp
.EX
    rg \-e \-foo
.EE
.sp
You can also use the special \fB\-\-\fP delimiter to indicate that no more
flags will be provided. Namely, the following is equivalent to the above:
.sp
.EX
    rg \-\- \-foo
.EE
.sp
When \flag{file} or \flag{regexp} is used, then ripgrep treats all positional
arguments as files or directories to search.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let regexp = convert::string(v.unwrap_value())?;
        args.patterns.push(PatternSource::Regexp(regexp));
        Ok(())
    }
}


/// -r/--replace
#[derive(Debug)]
struct Replace;

impl Flag for Replace {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'r')
    }
    fn name_long(&self) -> &'static str {
        "replace"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("REPLACEMENT")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Replace matches with the given text."
    }
    fn doc_long(&self) -> &'static str {
        r#"
Replaces every match with the text given when printing results. Neither this
flag nor any other ripgrep flag will modify your files.
.sp
Capture group indices (e.g., \fB$\fP\fI5\fP) and names (e.g., \fB$\fP\fIfoo\fP)
are supported in the replacement string. Capture group indices are numbered
based on the position of the opening parenthesis of the group, where the
leftmost such group is \fB$\fP\fI1\fP. The special \fB$\fP\fI0\fP group
corresponds to the entire match.
.sp
The name of a group is formed by taking the longest string of letters, numbers
and underscores (i.e. \fB[_0-9A-Za-z]\fP) after the \fB$\fP. For example,
\fB$\fP\fI1a\fP will be replaced with the group named \fI1a\fP, not the
group at index \fI1\fP. If the group's name contains characters that aren't
letters, numbers or underscores, or you want to immediately follow the group
with another string, the name should be put inside braces. For example,
\fB${\fP\fI1\fP\fB}\fP\fIa\fP will take the content of the group at index
\fI1\fP and append \fIa\fP to the end of it.
.sp
If an index or name does not refer to a valid capture group, it will be
replaced with an empty string.
.sp
In shells such as Bash and zsh, you should wrap the pattern in single quotes
instead of double quotes. Otherwise, capture group indices will be replaced by
expanded shell variables which will most likely be empty.
.sp
To write a literal \fB$\fP, use \fB$$\fP.
.sp
Note that the replacement by default replaces each match, and not the entire
line. To replace the entire line, you should match the entire line.
.sp
This flag can be used with the \flag{only-matching} flag.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.replace = Some(convert::string(v.unwrap_value())?.into());
        Ok(())
    }
}


/// -z/--search-zip
#[derive(Debug)]
struct SearchZip;

impl Flag for SearchZip {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'z')
    }
    fn name_long(&self) -> &'static str {
        "search-zip"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-search-zip")
    }
    fn doc_category(&self) -> Category {
        Category::Input
    }
    fn doc_short(&self) -> &'static str {
        r"Search in compressed files."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag instructs ripgrep to search in compressed files. Currently gzip,
bzip2, xz, LZ4, LZMA, Brotli and Zstd files are supported. This option expects
the decompression binaries (such as \fBgzip\fP) to be available in your
\fBPATH\fP. If the required binaries are not found, then ripgrep will not
emit an error messages by default. Use the \flag{debug} flag to see more
information.
.sp
Note that this flag does not make ripgrep search archive formats as directory
trees. It only makes ripgrep detect compressed files and then decompress them
before searching their contents as it would any other file.
.sp
This overrides the \flag{pre} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.search_zip = if v.unwrap_switch() {
            args.pre = None;
            true
        } else {
            false
        };
        Ok(())
    }
}


/// -S/--smart-case
#[derive(Debug)]
struct SmartCase;

impl Flag for SmartCase {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'S')
    }
    fn name_long(&self) -> &'static str {
        "smart-case"
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Smart case search."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag instructs ripgrep to searches case insensitively if the pattern is
all lowercase. Otherwise, ripgrep will search case sensitively.
.sp
A pattern is considered all lowercase if both of the following rules hold:
.sp
.IP \(bu 3n
First, the pattern contains at least one literal character. For example,
\fBa\\w\fP contains a literal (\fBa\fP) but just \fB\\w\fP does not.
.sp
.IP \(bu 3n
Second, of the literals in the pattern, none of them are considered to be
uppercase according to Unicode. For example, \fBfoo\\pL\fP has no uppercase
literals but \fBFoo\\pL\fP does.
.PP
This overrides the \flag{case-sensitive} and \flag{ignore-case} flags.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--smart-case flag has no negation");
        args.case = CaseMode::Smart;
        Ok(())
    }
}


/// --sort-files
#[derive(Debug)]
struct SortFiles;

impl Flag for SortFiles {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "sort-files"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-sort-files")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"(DEPRECATED) Sort results by file path."
    }
    fn doc_long(&self) -> &'static str {
        r"
DEPRECATED. Use \fB\-\-sort=path\fP instead.
.sp
This flag instructs ripgrep to sort search results by file path
lexicographically in ascending order. Note that this currently disables all
parallelism and runs search in a single thread.
.sp
This flag overrides \flag{sort} and \flag{sortr}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.sort = if v.unwrap_switch() {
            Some(SortMode { reverse: false, kind: SortModeKind::Path })
        } else {
            None
        };
        Ok(())
    }
}


/// --sort
#[derive(Debug)]
struct Sort;

impl Flag for Sort {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "sort"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("SORTBY")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Sort results in ascending order."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag enables sorting of results in ascending order. The possible values
for this flag are:
.sp
.TP 12
\fBnone\fP
(Default) Do not sort results. Fastest. Can be multi-threaded.
.TP 12
\fBpath\fP
Sort by file path. Always single-threaded. The order is determined by sorting
files in each directory entry during traversal. This means that given the files
\fBa/b\fP and \fBa+\fP, the latter will sort after the former even though
\fB+\fP would normally sort before \fB/\fP.
.TP 12
\fBmodified\fP
Sort by the last modified time on a file. Always single-threaded.
.TP 12
\fBaccessed\fP
Sort by the last accessed time on a file. Always single-threaded.
.TP 12
\fBcreated\fP
Sort by the creation time on a file. Always single-threaded.
.PP
If the chosen (manually or by-default) sorting criteria isn't available on your
system (for example, creation time is not available on ext4 file systems), then
ripgrep will attempt to detect this, print an error and exit without searching.
.sp
To sort results in reverse or descending order, use the \flag{sortr} flag.
Also, this flag overrides \flag{sortr}.
.sp
Note that sorting results currently always forces ripgrep to abandon
parallelism and run in a single thread.
"
    }
    fn doc_choices(&self) -> &'static [&'static str] {
        &["none", "path", "modified", "accessed", "created"]
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let kind = match convert::str(&v.unwrap_value())? {
            "none" => {
                args.sort = None;
                return Ok(());
            }
            "path" => SortModeKind::Path,
            "modified" => SortModeKind::LastModified,
            "accessed" => SortModeKind::LastAccessed,
            "created" => SortModeKind::Created,
            unk => anyhow::bail!("choice '{unk}' is unrecognized"),
        };
        args.sort = Some(SortMode { reverse: false, kind });
        Ok(())
    }
}


/// --sortr
#[derive(Debug)]
struct Sortr;

impl Flag for Sortr {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "sortr"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("SORTBY")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Sort results in descending order."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag enables sorting of results in descending order. The possible values
for this flag are:
.sp
.TP 12
\fBnone\fP
(Default) Do not sort results. Fastest. Can be multi-threaded.
.TP 12
\fBpath\fP
Sort by file path. Always single-threaded. The order is determined by sorting
files in each directory entry during traversal. This means that given the files
\fBa/b\fP and \fBa+\fP, the latter will sort before the former even though
\fB+\fP would normally sort after \fB/\fP when doing a reverse lexicographic
sort.
.TP 12
\fBmodified\fP
Sort by the last modified time on a file. Always single-threaded.
.TP 12
\fBaccessed\fP
Sort by the last accessed time on a file. Always single-threaded.
.TP 12
\fBcreated\fP
Sort by the creation time on a file. Always single-threaded.
.PP
If the chosen (manually or by-default) sorting criteria isn't available on your
system (for example, creation time is not available on ext4 file systems), then
ripgrep will attempt to detect this, print an error and exit without searching.
.sp
To sort results in ascending order, use the \flag{sort} flag. Also, this flag
overrides \flag{sort}.
.sp
Note that sorting results currently always forces ripgrep to abandon
parallelism and run in a single thread.
"
    }
    fn doc_choices(&self) -> &'static [&'static str] {
        &["none", "path", "modified", "accessed", "created"]
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let kind = match convert::str(&v.unwrap_value())? {
            "none" => {
                args.sort = None;
                return Ok(());
            }
            "path" => SortModeKind::Path,
            "modified" => SortModeKind::LastModified,
            "accessed" => SortModeKind::LastAccessed,
            "created" => SortModeKind::Created,
            unk => anyhow::bail!("choice '{unk}' is unrecognized"),
        };
        args.sort = Some(SortMode { reverse: true, kind });
        Ok(())
    }
}


/// --stats
#[derive(Debug)]
struct Stats;

impl Flag for Stats {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "stats"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-stats")
    }
    fn doc_category(&self) -> Category {
        Category::Logging
    }
    fn doc_short(&self) -> &'static str {
        r"Print statistics about the search."
    }
    fn doc_long(&self) -> &'static str {
        r"
When enabled, ripgrep will print aggregate statistics about the search. When
this flag is present, ripgrep will print at least the following stats to
stdout at the end of the search: number of matched lines, number of files with
matches, number of files searched, and the time taken for the entire search to
complete.
.sp
This set of aggregate statistics may expand over time.
.sp
This flag is always and implicitly enabled when \flag{json} is used.
.sp
Note that this flag has no effect if \flag{files}, \flag{files-with-matches} or
\flag{files-without-match} is passed.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.stats = v.unwrap_switch();
        Ok(())
    }
}


/// --stop-on-nonmatch
#[derive(Debug)]
struct StopOnNonmatch;

impl Flag for StopOnNonmatch {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "stop-on-nonmatch"
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Stop searching after a non-match."
    }
    fn doc_long(&self) -> &'static str {
        r"
Enabling this option will cause ripgrep to stop reading a file once it
encounters a non-matching line after it has encountered a matching line.
This is useful if it is expected that all matches in a given file will be on
sequential lines, for example due to the lines being sorted.
.sp
This overrides the \flag{multiline} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--stop-on-nonmatch has no negation");
        args.stop_on_nonmatch = true;
        args.multiline = false;
        Ok(())
    }
}


/// -a/--text
#[derive(Debug)]
struct Text;

impl Flag for Text {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'a')
    }
    fn name_long(&self) -> &'static str {
        "text"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-text")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Search binary files as if they were text."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag instructs ripgrep to search binary files as if they were text. When
this flag is present, ripgrep's binary file detection is disabled. This means
that when a binary file is searched, its contents may be printed if there is
a match. This may cause escape codes to be printed that alter the behavior of
your terminal.
.sp
When binary file detection is enabled, it is imperfect. In general, it uses
a simple heuristic. If a \fBNUL\fP byte is seen during search, then the file
is considered binary and searching stops (unless this flag is present).
Alternatively, if the \flag{binary} flag is used, then ripgrep will only quit
when it sees a \fBNUL\fP byte after it sees a match (or searches the entire
file).
.sp
This flag overrides the \flag{binary} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.binary = if v.unwrap_switch() {
            BinaryMode::AsText
        } else {
            BinaryMode::Auto
        };
        Ok(())
    }
}


/// -j/--threads
#[derive(Debug)]
struct Threads;

impl Flag for Threads {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'j')
    }
    fn name_long(&self) -> &'static str {
        "threads"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("NUM")
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Set the approximate number of threads to use."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag sets the approximate number of threads to use. A value of \fB0\fP
(which is the default) causes ripgrep to choose the thread count using
heuristics.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        let threads = convert::usize(&v.unwrap_value())?;
        args.threads = if threads == 0 { None } else { Some(threads) };
        Ok(())
    }
}


/// --trace
#[derive(Debug)]
struct Trace;

impl Flag for Trace {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "trace"
    }
    fn doc_category(&self) -> Category {
        Category::Logging
    }
    fn doc_short(&self) -> &'static str {
        r"Show trace messages."
    }
    fn doc_long(&self) -> &'static str {
        r"
Show trace messages. This shows even more detail than the \flag{debug}
flag. Generally, one should only use this if \flag{debug} doesn't emit the
information you're looking for.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--trace can only be enabled");
        args.logging = Some(LoggingMode::Trace);
        Ok(())
    }
}


/// --trim
#[derive(Debug)]
struct Trim;

impl Flag for Trim {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "trim"
    }
    fn name_negated(&self) -> Option<&'static str> {
        Some("no-trim")
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Trim prefix whitespace from matches."
    }
    fn doc_long(&self) -> &'static str {
        r"
When set, all ASCII whitespace at the beginning of each line printed will be
removed.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.trim = v.unwrap_switch();
        Ok(())
    }
}


/// -t/--type
#[derive(Debug)]
struct Type;

impl Flag for Type {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b't')
    }
    fn name_long(&self) -> &'static str {
        "type"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("TYPE")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Only search files matching TYPE."
    }
    fn doc_long(&self) -> &'static str {
        r#"
This flag limits ripgrep to searching files matching \fITYPE\fP. Multiple
\flag{type} flags may be provided.
.sp
This flag supports the special value \fBall\fP, which will behave as if
\flag{type} was provided for every file type supported by ripgrep (including
any custom file types). The end result is that \fB\-\-type=all\fP causes
ripgrep to search in "whitelist" mode, where it will only search files it
recognizes via its type definitions.
.sp
Note that this flag has lower precedence than both the \flag{glob} flag and
any rules found in ignore files.
.sp
To see the list of available file types, use the \flag{type-list} flag.
"#
    }
    fn completion_type(&self) -> CompletionType {
        CompletionType::Filetype
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.type_changes.push(TypeChange::Select {
            name: convert::string(v.unwrap_value())?,
        });
        Ok(())
    }
}


/// --type-add
#[derive(Debug)]
struct TypeAdd;

impl Flag for TypeAdd {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "type-add"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("TYPESPEC")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Add a new glob for a file type."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag adds a new glob for a particular file type. Only one glob can be
added at a time. Multiple \flag{type-add} flags can be provided. Unless
\flag{type-clear} is used, globs are added to any existing globs defined inside
of ripgrep.
.sp
Note that this must be passed to every invocation of ripgrep. Type settings are
not persisted. See \fBCONFIGURATION FILES\fP for a workaround.
.sp
Example:
.sp
.EX
    rg \-\-type\-add 'foo:*.foo' -tfoo \fIPATTERN\fP
.EE
.sp
This flag can also be used to include rules from other types with the special
include directive. The include directive permits specifying one or more other
type names (separated by a comma) that have been defined and its rules will
automatically be imported into the type specified. For example, to create a
type called src that matches C++, Python and Markdown files, one can use:
.sp
.EX
    \-\-type\-add 'src:include:cpp,py,md'
.EE
.sp
Additional glob rules can still be added to the src type by using this flag
again:
.sp
.EX
    \-\-type\-add 'src:include:cpp,py,md' \-\-type\-add 'src:*.foo'
.EE
.sp
Note that type names must consist only of Unicode letters or numbers.
Punctuation characters are not allowed.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.type_changes
            .push(TypeChange::Add { def: convert::string(v.unwrap_value())? });
        Ok(())
    }
}


/// --type-clear
#[derive(Debug)]
struct TypeClear;

impl Flag for TypeClear {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_long(&self) -> &'static str {
        "type-clear"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("TYPE")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Clear globs for a file type."
    }
    fn doc_long(&self) -> &'static str {
        r"
Clear the file type globs previously defined for \fITYPE\fP. This clears any
previously defined globs for the \fITYPE\fP, but globs can be added after this
flag.
.sp
Note that this must be passed to every invocation of ripgrep. Type settings are
not persisted. See \fBCONFIGURATION FILES\fP for a workaround.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.type_changes.push(TypeChange::Clear {
            name: convert::string(v.unwrap_value())?,
        });
        Ok(())
    }
}


/// --type-not
#[derive(Debug)]
struct TypeNot;

impl Flag for TypeNot {
    fn is_switch(&self) -> bool {
        false
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'T')
    }
    fn name_long(&self) -> &'static str {
        "type-not"
    }
    fn doc_variable(&self) -> Option<&'static str> {
        Some("TYPE")
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r"Do not search files matching TYPE."
    }
    fn doc_long(&self) -> &'static str {
        r#"
Do not search files matching \fITYPE\fP. Multiple \flag{type-not} flags may be
provided. Use the \flag{type-list} flag to list all available types.
.sp
This flag supports the special value \fBall\fP, which will behave
as if \flag{type-not} was provided for every file type supported by
ripgrep (including any custom file types). The end result is that
\fB\-\-type\-not=all\fP causes ripgrep to search in "blacklist" mode, where it
will only search files that are unrecognized by its type definitions.
.sp
To see the list of available file types, use the \flag{type-list} flag.
"#
    }
    fn completion_type(&self) -> CompletionType {
        CompletionType::Filetype
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        args.type_changes.push(TypeChange::Negate {
            name: convert::string(v.unwrap_value())?,
        });
        Ok(())
    }
}


/// --type-list
#[derive(Debug)]
struct TypeList;

impl Flag for TypeList {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "type-list"
    }
    fn doc_category(&self) -> Category {
        Category::OtherBehaviors
    }
    fn doc_short(&self) -> &'static str {
        r"Show all supported file types."
    }
    fn doc_long(&self) -> &'static str {
        r"
Show all supported file types and their corresponding globs. This takes any
\flag{type-add} and \flag{type-clear} flags given into account. Each type is
printed on its own line, followed by a \fB:\fP and then a comma-delimited list
of globs for that type on the same line.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--type-list has no negation");
        args.mode.update(Mode::Types);
        Ok(())
    }
}


/// -u/--unrestricted
#[derive(Debug)]
struct Unrestricted;

impl Flag for Unrestricted {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'u')
    }
    fn name_long(&self) -> &'static str {
        "unrestricted"
    }
    fn doc_category(&self) -> Category {
        Category::Filter
    }
    fn doc_short(&self) -> &'static str {
        r#"Reduce the level of "smart" filtering."#
    }
    fn doc_long(&self) -> &'static str {
        r#"
This flag reduces the level of "smart" filtering. Repeated uses (up to 3) reduces
the filtering even more. When repeated three times, ripgrep will search every
file in a directory tree.
.sp
A single \flag{unrestricted} flag is equivalent to \flag{no-ignore}. Two
\flag{unrestricted} flags is equivalent to \flag{no-ignore} \flag{hidden}.
Three \flag{unrestricted} flags is equivalent to \flag{no-ignore} \flag{hidden}
\flag{binary}.
.sp
The only filtering ripgrep still does when \fB-uuu\fP is given is to skip
symbolic links and to avoid printing matches from binary files. Symbolic links
can be followed via the \flag{follow} flag, and binary files can be treated as
text files via the \flag{text} flag.
"#
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--unrestricted has no negation");
        args.unrestricted = args.unrestricted.saturating_add(1);
        anyhow::ensure!(
            args.unrestricted <= 3,
            "flag can only be repeated up to 3 times"
        );
        if args.unrestricted == 1 {
            NoIgnore.update(FlagValue::Switch(true), args)?;
        } else if args.unrestricted == 2 {
            Hidden.update(FlagValue::Switch(true), args)?;
        } else {
            assert_eq!(args.unrestricted, 3);
            Binary.update(FlagValue::Switch(true), args)?;
        }
        Ok(())
    }
}


/// --version
#[derive(Debug)]
struct Version;

impl Flag for Version {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'V')
    }
    fn name_long(&self) -> &'static str {
        "version"
    }
    fn doc_category(&self) -> Category {
        Category::OtherBehaviors
    }
    fn doc_short(&self) -> &'static str {
        r"Print ripgrep's version."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag prints ripgrep's version. This also may print other relevant
information, such as the presence of target specific optimizations and the
\fBgit\fP revision that this build of ripgrep was compiled from.
"
    }

    fn update(&self, v: FlagValue, _: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--version has no negation");
        // Since this flag has different semantics for -V and --version and the
        // Flag trait doesn't support encoding this sort of thing, we handle it
        // as a special case in the parser.
        Ok(())
    }
}


/// --vimgrep
#[derive(Debug)]
struct Vimgrep;

impl Flag for Vimgrep {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_long(&self) -> &'static str {
        "vimgrep"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Print results in a vim compatible format."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag instructs ripgrep to print results with every match on its own line,
including line numbers and column numbers.
.sp
With this option, a line with more than one match will be printed in its
entirety more than once. For that reason, the total amount of output as a
result of this flag can be quadratic in the size of the input. For example,
if the pattern matches every byte in an input file, then each line will be
repeated for every byte matched. For this reason, users should only use this
flag when there is no other choice. Editor integrations should prefer some
other way of reading results from ripgrep, such as via the \flag{json} flag.
One alternative to avoiding exorbitant memory usage is to force ripgrep into
single threaded mode with the \flag{threads} flag. Note though that this will
not impact the total size of the output, just the heap memory that ripgrep will
use.
"
    }
    fn doc_choices(&self) -> &'static [&'static str] {
        &[]
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--vimgrep has no negation");
        args.vimgrep = true;
        Ok(())
    }
}


/// --with-filename
#[derive(Debug)]
struct WithFilename;

impl Flag for WithFilename {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'H')
    }
    fn name_long(&self) -> &'static str {
        "with-filename"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Print the file path with each matching line."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag instructs ripgrep to print the file path for each matching line.
This is the default when more than one file is searched. If \flag{heading} is
enabled (the default when printing to a tty), the file path will be shown above
clusters of matches from each file; otherwise, the file name will be shown as a
prefix for each matched line.
.sp
This flag overrides \flag{no-filename}.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--with-filename has no defined negation");
        args.with_filename = Some(true);
        Ok(())
    }
}


/// --no-filename
#[derive(Debug)]
struct WithFilenameNo;

impl Flag for WithFilenameNo {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'I')
    }
    fn name_long(&self) -> &'static str {
        "no-filename"
    }
    fn doc_category(&self) -> Category {
        Category::Output
    }
    fn doc_short(&self) -> &'static str {
        r"Never print the path with each matching line."
    }
    fn doc_long(&self) -> &'static str {
        r"
This flag instructs ripgrep to never print the file path with each matching
line. This is the default when ripgrep is explicitly instructed to search one
file or stdin.
.sp
This flag overrides \flag{with-filename}.
"
    }
    fn doc_choices(&self) -> &'static [&'static str] {
        &[]
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--no-filename has no defined negation");
        args.with_filename = Some(false);
        Ok(())
    }
}


/// -w/--word-regexp
#[derive(Debug)]
struct WordRegexp;

impl Flag for WordRegexp {
    fn is_switch(&self) -> bool {
        true
    }
    fn name_short(&self) -> Option<u8> {
        Some(b'w')
    }
    fn name_long(&self) -> &'static str {
        "word-regexp"
    }
    fn doc_category(&self) -> Category {
        Category::Search
    }
    fn doc_short(&self) -> &'static str {
        r"Show matches surrounded by word boundaries."
    }
    fn doc_long(&self) -> &'static str {
        r"
When enabled, ripgrep will only show matches surrounded by word boundaries.
This is equivalent to surrounding every pattern with \fB\\b{start-half}\fP and
\fB\\b{end-half}\fP. These are a custom syntax from ripgrep's default regex
engine that, unlike \fB\\b\fP, doesn't require matching a word character on one
side. That is, \fB\\b{start-half}\fP corresponds to matching \fB\\W|\\A\fP on
the left and \fB\\b{end-half}\fP corresponds to matching \fB\\W|\\z\fP on the
right.
.sp
This overrides the \flag{line-regexp} flag.
"
    }

    fn update(&self, v: FlagValue, args: &mut LowArgs) -> anyhow::Result<()> {
        assert!(v.unwrap_switch(), "--word-regexp has no negation");
        args.boundary = Some(BoundaryMode::Word);
        Ok(())
    }
}


mod convert {
    use std::ffi::{OsStr, OsString};

    use crate::rg::anyhow::{self, Context};

    pub(super) fn str(v: &OsStr) -> anyhow::Result<&str> {
        let Some(s) = v.to_str() else {
            anyhow::bail!("value is not valid UTF-8")
        };
        Ok(s)
    }

    pub(super) fn string(v: OsString) -> anyhow::Result<String> {
        let Ok(s) = v.into_string() else {
            anyhow::bail!("value is not valid UTF-8")
        };
        Ok(s)
    }

    pub(super) fn usize(v: &OsStr) -> anyhow::Result<usize> {
        str(v)?.parse().context("value is not a valid number")
    }

    pub(super) fn u64(v: &OsStr) -> anyhow::Result<u64> {
        str(v)?.parse().context("value is not a valid number")
    }

    pub(super) fn human_readable_u64(v: &OsStr) -> anyhow::Result<u64> {
        crate::rg::cli::parse_human_readable_size(str(v)?)
            .context("invalid size")
    }

    pub(super) fn human_readable_usize(v: &OsStr) -> anyhow::Result<usize> {
        let size = human_readable_u64(v)?;
        let Ok(size) = usize::try_from(size) else {
            anyhow::bail!("size is too big")
        };
        Ok(size)
    }
}

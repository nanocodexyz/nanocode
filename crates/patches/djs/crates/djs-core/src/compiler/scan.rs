//! Token-level lookahead scans (docs/compiler.md 3.1, 3.4). Scans only move a copy of
//! the lexer; they never emit.
use crate::PushOut;

use alloc::vec::Vec;

use crate::lexer::{Kw, Lexer, Token, TokenKind};

/// The previous-token heuristic for scans: may a `/` after `prev` start a regex
/// (QuickJS's `is_regexp_allowed`)? The parser itself always knows.
pub(super) fn regex_allowed(prev: &Token) -> bool {
    use TokenKind::*;
    match prev.kind {
        Ident => matches!(prev.kw, Kw::Yield | Kw::Await | Kw::Of) && !prev.has_escape(),
        Number | BigInt | String | NoSubstTemplate | TemplateTail | Regex | PrivateName
        | RParen | RBracket | RBrace | This | Super | True | False | Null | PlusPlus
        | MinusMinus => false,
        _ => true,
    }
}

/// A lexer copy that handles regexes and template continuations like the parser
/// would, for balanced-bracket scans.
struct Scanner<'a> {
    lx: Lexer<'a>,
    prev: Token,
    /// Open brackets: `(`, `[`, `{`, or `` ` `` for a template substitution; [`BLOCK`]
    /// and [`HEAD`] for the statement forms of `{` and `(`.
    stack: Brackets,
    /// `prev` closed a [`BLOCK`] or [`HEAD`]: a `/` after it starts a regex, which
    /// [`regex_allowed`] cannot tell from `prev` alone (`if (a) {} /x/`, 0217).
    after_stmt: bool,
    /// The stack depth at a `class` keyword: the next `{` at that depth is its body.
    class_at: Option<usize>,
}

/// A `{` that opens a statement block, function or class body rather than an object
/// literal or pattern.
const BLOCK: u8 = b'B';
/// The `(` of an `if`, `for`, `while` or `with` head.
const HEAD: u8 = b'H';

const INLINE_DEPTH: usize = 32;

struct Brackets {
    inline: [u8; INLINE_DEPTH],
    len: usize,
    deeper: Vec<u8>,
}

impl Brackets {
    fn new(open: u8) -> Self {
        let mut b = Brackets {
            inline: [0; INLINE_DEPTH],
            len: 0,
            deeper: Vec::new(),
        };
        b.push_out(open);
        b
    }

    fn push_out(&mut self, x: u8) {
        if self.len < INLINE_DEPTH {
            self.inline[self.len] = x;
        } else {
            self.deeper.push_out(x);
        }
        self.len += 1;
    }

    fn pop(&mut self) -> Option<u8> {
        self.len = self.len.checked_sub(1)?;
        if self.len < INLINE_DEPTH {
            Some(self.inline[self.len])
        } else {
            self.deeper.pop()
        }
    }

    fn last(&self) -> Option<&u8> {
        let top = self.len.checked_sub(1)?;
        if top < INLINE_DEPTH {
            self.inline.get(top)
        } else {
            self.deeper.last()
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<'a> Scanner<'a> {
    fn new(lx: Lexer<'a>, prev: Token, open: u8) -> Self {
        Scanner {
            lx,
            prev,
            stack: Brackets::new(open),
            after_stmt: false,
            class_at: None,
        }
    }

    /// Does a `{` after `prev` open a block? A guess like [`regex_allowed`]: a `{`
    /// after `)` or `=>` is a body, one after `:` is a block inside a block (a label
    /// or `case`), else an object; a function or class expression followed by `/`
    /// is taken for a declaration, which division after it would make no sense for.
    fn opens_block(&self) -> bool {
        use TokenKind::*;
        match self.prev.kind {
            RParen | Arrow | Semicolon | LBrace | RBrace | Else | Try | Catch | Finally | Do => {
                true
            }
            Colon => self.stack.last() == Some(&BLOCK),
            _ => self.class_at == Some(self.stack.len()),
        }
    }

    fn next(&mut self) -> Option<Token> {
        use TokenKind::*;
        let mut tok = self.lx.next_token().ok()?;
        let mut after_stmt = false;
        match tok.kind {
            Slash | SlashAssign if self.after_stmt || regex_allowed(&self.prev) => {
                tok = self.lx.rescan_as_regex(tok).ok()?;
            }
            RBrace if self.stack.last() == Some(&b'`') => {
                tok = self.lx.rescan_template_continuation(tok).ok()?;
                if tok.kind == TemplateTail {
                    self.stack.pop();
                }
            }
            TemplateHead => self.stack.push_out(b'`'),
            LParen => {
                let head = matches!(self.prev.kind, If | For | While | With);
                self.stack.push_out(if head { HEAD } else { b'(' });
            }
            LBracket => self.stack.push_out(b'['),
            LBrace => {
                let open = if self.opens_block() { BLOCK } else { b'{' };
                if open == BLOCK && self.class_at == Some(self.stack.len()) {
                    self.class_at = None;
                }
                self.stack.push_out(open);
            }
            Class => self.class_at = Some(self.stack.len()),
            RParen | RBracket | RBrace => {
                let top = self.stack.pop()?;
                let fits = match tok.kind {
                    RParen => top == b'(' || top == HEAD,
                    RBracket => top == b'[',
                    _ => top == b'{' || top == BLOCK,
                };
                if !fits {
                    return None;
                }
                after_stmt = top == BLOCK || top == HEAD;
            }
            Eof => return None,
            _ => {}
        }
        self.prev = tok;
        self.after_stmt = after_stmt;
        Some(tok)
    }
}

/// With `open` the current `(` token and `lx` positioned after it: is this an arrow
/// function head, `( params ) =>` with no newline before the arrow? Bails out at the
/// first token that cannot be part of a parameter list, so parenthesized expressions
/// cost a token or two (docs/compiler.md 3.1).
pub(super) fn is_arrow_head(lx: Lexer<'_>, open: Token) -> bool {
    #[derive(PartialEq)]
    enum State {
        Param,
        AfterParam,
        Default,
    }
    let mut s = Scanner::new(lx, open, b'(');
    let mut state = State::Param;
    loop {
        let depth_before = s.stack.len();
        let Some(tok) = s.next() else { return false };
        let depth = s.stack.len();
        if depth_before == 1 {
            // A token directly inside the parentheses (or the closing one).
            match (&state, tok.kind) {
                (_, TokenKind::RParen) => break,
                (State::Param, TokenKind::Ident) => state = State::AfterParam,
                (State::Param, TokenKind::Ellipsis) => {}
                // A destructuring pattern: skipped as balanced, then AfterParam.
                (State::Param, TokenKind::LBrace | TokenKind::LBracket) => {}
                (State::Param, _) => return false,
                (State::AfterParam, TokenKind::Comma) => state = State::Param,
                (State::AfterParam, TokenKind::Assign) => state = State::Default,
                (State::AfterParam, _) => return false,
                (State::Default, TokenKind::Comma) => state = State::Param,
                (State::Default, _) => {}
            }
        } else if depth == 1 && state == State::Param {
            // A pattern parameter closed.
            state = State::AfterParam;
        }
    }
    matches!(s.lx.next_token(), Ok(t) if t.kind == TokenKind::Arrow && !t.newline_before())
}

/// What follows the closing bracket of a `{...}` or `[...]` (docs/compiler.md 3.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Follow {
    /// `=`: a destructuring assignment pattern.
    Assign,
    /// `,`, `]` or `}`: inside a pattern, a nested pattern.
    ElemEnd,
    /// `of` or `in`: in a for-in/of head, a pattern.
    InOf,
    Other,
}

fn classify(t: &Token) -> Follow {
    match t.kind {
        TokenKind::Assign => Follow::Assign,
        TokenKind::Comma | TokenKind::RBracket | TokenKind::RBrace => Follow::ElemEnd,
        TokenKind::In => Follow::InOf,
        _ if t.is_contextual(Kw::Of) => Follow::InOf,
        _ => Follow::Other,
    }
}

/// The memoized bracket scan (docs/compiler.md 3.3): one scan of a literal records what
/// follows every `{`/`[` group closed inside it, so nested literals are not rescanned.
#[derive(Default)]
pub(super) struct PatternScan {
    start: u32,
    end: u32,
    /// `(start offset of the bracket, what follows its closer)`, sorted.
    entries: Vec<(u32, Follow)>,
    opens: Vec<Option<u32>>,
}

impl PatternScan {
    pub fn reusing(mut entries: Vec<(u32, Follow)>, opens: Vec<Option<u32>>) -> Self {
        entries.clear();
        PatternScan {
            entries,
            opens,
            ..Default::default()
        }
    }

    pub fn take_buffers(&mut self) -> (Vec<(u32, Follow)>, Vec<Option<u32>>) {
        (
            core::mem::take(&mut self.entries),
            core::mem::take(&mut self.opens),
        )
    }

    /// With `open` the current `{` or `[` token and `lx` positioned after it.
    pub fn follow(&mut self, lx: Lexer<'_>, open: Token) -> Follow {
        if self.start <= open.start
            && open.start < self.end
            && let Ok(i) = self.entries.binary_search_by_key(&open.start, |e| e.0)
        {
            return self.entries[i].1;
        }
        self.scan(lx, open)
    }

    fn scan(&mut self, lx: Lexer<'_>, open: Token) -> Follow {
        self.start = 0;
        self.end = 0;
        self.entries.clear();
        let kind = if open.kind == TokenKind::LBrace {
            b'{'
        } else {
            b'['
        };
        let mut s = Scanner::new(lx, open, kind);
        // Parallel to `s.stack`: the start of each open `{`/`[`.
        let mut opens = core::mem::take(&mut self.opens);
        opens.clear();
        opens.push_out(Some(open.start));
        let mut pending: Option<u32> = None;
        let mut end = open.end;
        while !s.stack.is_empty() {
            let before = s.stack.len();
            let Some(tok) = s.next() else {
                self.entries.clear();
                self.opens = opens;
                return Follow::Other;
            };
            if let Some(p) = pending.take() {
                self.entries.push_out((p, classify(&tok)));
            }
            if s.stack.len() > before {
                let bracket = matches!(tok.kind, TokenKind::LBrace | TokenKind::LBracket);
                opens.push_out(bracket.then_some(tok.start));
            } else if s.stack.len() < before {
                pending = opens.pop().flatten();
            }
            end = tok.end;
        }
        let follow = match s.lx.next_token() {
            Ok(t) => classify(&t),
            Err(_) => Follow::Other,
        };
        self.entries.push_out((open.start, follow));
        crate::sort::sort_by(&mut self.entries, |a, b| a.0 < b.0);
        self.start = open.start;
        self.end = end;
        self.opens = opens;
        follow
    }
}

/// With `open` the current `{` or `[` and `lx` after it: the lexer after the matching
/// closer and the closer's end offset, or `None` if the group does not scan.
pub(super) fn skip_group<'a>(lx: Lexer<'a>, open: Token) -> Option<(Lexer<'a>, u32)> {
    let kind = if open.kind == TokenKind::LBrace {
        b'{'
    } else {
        b'['
    };
    let mut s = Scanner::new(lx, open, kind);
    let mut end = open.end;
    while !s.stack.is_empty() {
        end = s.next()?.end;
    }
    Some((s.lx, end))
}

/// With `open` a `{` and `lx` after it: does the object pattern have a rest property
/// (`...` directly inside)?
pub(super) fn object_has_rest(lx: Lexer<'_>, open: Token) -> bool {
    let mut s = Scanner::new(lx, open, b'{');
    while !s.stack.is_empty() {
        let Some(t) = s.next() else { return false };
        if t.kind == TokenKind::Ellipsis && s.stack.len() == 1 {
            return true;
        }
    }
    false
}

/// With `open` the `{` of a try block and `lx` after it: does the statement have a
/// `finally` clause? (`break`/`return` inside the try block must know before it is
/// parsed, D1080.)
pub(super) fn try_has_finally(lx: Lexer<'_>, open: Token) -> bool {
    let mut s = Scanner::new(lx, open, BLOCK);
    let skip_group = |s: &mut Scanner<'_>| -> Option<()> {
        while !s.stack.is_empty() {
            s.next()?;
        }
        Some(())
    };
    if skip_group(&mut s).is_none() {
        return false;
    }
    let Some(mut t) = s.next() else { return false };
    if t.kind == TokenKind::Catch {
        let Some(n) = s.next() else { return false };
        if n.kind == TokenKind::LParen && skip_group(&mut s).is_none() {
            return false;
        }
        if n.kind == TokenKind::LParen {
            match s.next() {
                Some(b) if b.kind == TokenKind::LBrace => {}
                _ => return false,
            }
        } else if n.kind != TokenKind::LBrace {
            return false;
        }
        if skip_group(&mut s).is_none() {
            return false;
        }
        match s.lx.next_token() {
            Ok(n) => t = n,
            Err(_) => return false,
        }
    }
    t.kind == TokenKind::Finally
}

/// With `open` the `(` after `for` and `lx` positioned after it: is this a C-style
/// `for` head, with a `;` directly inside the parentheses (docs/compiler.md 3.8,
/// QuickJS does the same)? Otherwise it is for-in/of, also for a head that does not
/// scan (unbalanced, end of input): its parser reports the error, as in QuickJS.
pub(super) fn for_head_is_c_style(lx: Lexer<'_>, open: Token) -> bool {
    let mut s = Scanner::new(lx, open, b'(');
    loop {
        let Some(t) = s.next() else { return false };
        if s.stack.is_empty() {
            return false;
        }
        if t.kind == TokenKind::Semicolon && s.stack.len() == 1 {
            return true;
        }
    }
}

/// With `first` the first token of an expression for-in/of head and `lx` after it:
/// does the left side end at `of` (rather than `in`)? Looks for the first `of` or `in`
/// directly inside the head's parentheses; an `of` after `.` or `?.` is a property
/// name.
pub(super) fn for_lhs_ends_with_of(lx: Lexer<'_>, first: Token) -> bool {
    let mut s = Scanner::new(lx, first, b'(');
    match first.kind {
        TokenKind::LParen => s.stack.push_out(b'('),
        TokenKind::LBracket => s.stack.push_out(b'['),
        TokenKind::LBrace => s.stack.push_out(b'{'),
        TokenKind::TemplateHead => s.stack.push_out(b'`'),
        _ => {}
    }
    loop {
        let prev = s.prev.kind;
        let Some(t) = s.next() else { return false };
        if s.stack.len() != 1 {
            if s.stack.is_empty() {
                return false;
            }
            continue;
        }
        if t.kind == TokenKind::In {
            return false;
        }
        if t.is_contextual(Kw::Of) && !matches!(prev, TokenKind::Dot | TokenKind::QuestionDot) {
            return true;
        }
    }
}

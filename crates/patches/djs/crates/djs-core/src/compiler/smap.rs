//! Suspension maps (D2563, docs/continuation-dumps.md "Suspension maps"): for every
//! `await` and `yield` of a function, what a frame suspended there holds, described by
//! source names rather than by this build's slots and pcs, so that a later
//! build can rebuild the frame from a dump and an exporter can name what it finds.
//!
//! Computed on demand by compiling again with recording on
//! ([`suspension_maps`]); nothing is stored in templates and a normal compile records
//! nothing. Without the `dump` feature the parser's hooks compile to nothing.
//!
//! The values a construct keeps on the operand stack while a nested expression is
//! parsed (a call's callee and `this`, a binary operator's left operand) are named by
//! the construct and the source offset of what keeps them (`call.callee@41`, D2721):
//! the parser pushes their names when it emits them and pops them when it consumes
//! them, so at a site the name stack is the operand stack below the awaited value.
//! Statement-level state lives in temporaries (locals of its own, D2732, D2733), so
//! only expressions keep values there. Resolution checks the names' count against the
//! stack depth of the final code: a mismatch (a compiler bug), or a suspension inside a
//! class definition (whose half-made class no dump can write), makes the site
//! unsupported instead of misnamed.

/// What kind of suspension point a site is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SiteKind {
    /// `await x`.
    Await,
    /// `yield x` of a generator.
    Yield,
    /// An async generator's `yield x`: the await of `x`, the yield itself, and the await
    /// of the value a `return(v)` resumes it with.
    AsyncYieldAwait,
    AsyncYield,
    AsyncYieldReturn,
    /// An async generator's `return x`: the await of `x`.
    AsyncReturn,
    /// `for await`: the await of each `next()` result, and of `return()` when the loop
    /// is left early.
    ForAwaitNext,
    ForAwaitClose,
    /// `await using`: the await of a disposal.
    AwaitUsing,
    /// `yield*`: its yields (a sync generator passes the inner result through, an async
    /// one yields its value) and, in async generators, the awaits of the inner results
    /// and of a `return(v)` value.
    YieldStar,
    YieldStarAwait,
}

impl SiteKind {
    /// The name's prefix: `await#1`, `yield#2`, `for-await.next#1`, …
    #[allow(dead_code)]
    pub fn name(self) -> &'static str {
        match self {
            SiteKind::Await => "await",
            SiteKind::Yield => "yield",
            SiteKind::AsyncYieldAwait => "yield.await",
            SiteKind::AsyncYield => "yield",
            SiteKind::AsyncYieldReturn => "yield.return",
            SiteKind::AsyncReturn => "return.await",
            SiteKind::ForAwaitNext => "for-await.next",
            SiteKind::ForAwaitClose => "for-await.close",
            SiteKind::AwaitUsing => "await-using",
            SiteKind::YieldStar => "yield*",
            SiteKind::YieldStarAwait => "yield*.await",
        }
    }
}

// ---- The parser's hooks ----------------------------------------------------------------

impl super::parser::Parser<'_, '_> {
    #[inline(always)]
    pub(super) fn names_push(&mut self, _at: u32, _cs: &[&'static str]) {}
    #[inline(always)]
    pub(super) fn names_pop(&mut self, _n: usize) {}
    #[inline(always)]
    pub(super) fn names_replace(&mut self, _n: usize, _at: u32, _cs: &[&'static str]) {}
    #[inline(always)]
    pub(super) fn unsupported_begin(&mut self, _construct: &'static str) {}
    #[inline(always)]
    pub(super) fn unsupported_end(&mut self) {}
    #[inline(always)]
    pub(super) fn site(&mut self, _kind: SiteKind, _pos: u32) {}
}

//! Construct temporaries (D2732): the state a construct keeps while its body runs (a
//! loop's iterator record, a switch's discriminant, a destructuring pattern's source, a
//! `using` list's resource Record) lives in a local of its own, not on the operand
//! stack, so a suspended frame holds it in a named slot (D2720, D2721).
//!
//! Each is a binding of kind `VarKind::Temp` with an internal name (the integer atom of
//! the temporary's number in its function, which no source name can be and which needs
//! no interning), reached with
//! `name_get`/`name_put` like any local. Its slot name is `construct@offset`
//! (`for-of.iterator@18`), kept in `FuncState::temps`. A suspension map lists a
//! temporary at a site only where it holds a value the code still reads: [`live_at`]
//! computes that from the final phase-1 code, so a construct's code order (a for-of's
//! target runs after its right side) cannot make the map wrong.

use super::func::VarKind;
use super::parser::Parser;
use crate::heap::Atom;

impl Parser<'_, '_> {
    /// A new temporary of `construct` (`"for-of.iterator"`) at source offset `at`
    /// (absolute; its name is relative to the function's start), declared in the
    /// current scope.
    #[inline(never)]
    pub(super) fn temp(&mut self, construct: &'static str, at: u32) -> Atom {
        // An integer atom: no source name is one, and it needs no interning.
        let n = self.f.temps.len() as u32;
        let atom = Atom::int(n).unwrap_or(Atom::NULL);
        let v = self.f.add_var(atom, VarKind::Temp, self.scope);
        self.f
            .temps
            .push((v, construct, at.saturating_sub(self.f.src_start)));
        atom
    }

    /// Pushes temporary `t`.
    pub(super) fn temp_get(&mut self, t: Atom) {
        self.temp_op(t, false);
    }

    /// Pops into temporary `t`.
    pub(super) fn temp_put(&mut self, t: Atom) {
        self.temp_op(t, true);
    }

    #[inline(never)]
    fn temp_op(&mut self, t: Atom, put: bool) {
        let r = self.name_ref(t, 0);
        if put {
            self.code().name_put(r);
        } else {
            self.code().name_get(r);
        }
    }
}

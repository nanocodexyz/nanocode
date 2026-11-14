mod _abc;
#[cfg(feature = "ast")]
pub(crate) mod _ast;
mod _codecs;
mod _collections;
pub(crate) use _collections::ordered_dict::ordered_dict::PyOrderedDictItems;
mod _functools;
mod _imp;
pub mod _io;
mod _operator;
mod _sre;
mod _stat;
mod _string;
#[cfg(feature = "compiler")]
mod _symtable;
mod _sysconfig;
mod _sysconfigdata;
pub(crate) mod _testinternalcapi;
mod _types;
pub mod _typing;
pub mod _warnings;
mod _weakref;
pub mod atexit;
pub mod builtins;
pub mod errno;
mod gc;
mod itertools;
mod marshal;
pub mod time;
mod typevar;

pub(crate) mod _interpchannels;
pub(crate) mod _interpqueues;
pub(crate) mod _interpreters;
pub mod sys;

use crate::{Context, builtins::PyModuleDef};

/// Returns module definitions for multi-phase init modules.
///
/// These modules use multi-phase initialization pattern:
/// 1. Create module from def and add to sys.modules
/// 2. Call exec slot (can safely import other modules without circular import issues)
pub fn builtin_module_defs(ctx: &Context) -> Vec<&'static PyModuleDef> {
    vec![
        _abc::module_def(ctx),
        _types::module_def(ctx),
        #[cfg(feature = "ast")]
        _ast::module_def(ctx),
        atexit::module_def(ctx),
        _codecs::module_def(ctx),
        _collections::module_def(ctx),
        errno::module_def(ctx),
        _functools::module_def(ctx),
        gc::module_def(ctx),
        _imp::module_def(ctx),
        _io::module_def(ctx),
        itertools::module_def(ctx),
        marshal::module_def(ctx),
        _operator::module_def(ctx),
        _sre::module_def(ctx),
        _stat::module_def(ctx),
        _string::module_def(ctx),
        _testinternalcapi::module_def(ctx),
        #[cfg(feature = "compiler")]
        _symtable::module_def(ctx),
        _sysconfigdata::module_def(ctx),
        _sysconfig::module_def(ctx),
        _interpchannels::module_def(ctx),
        _interpqueues::module_def(ctx),
        _interpreters::module_def(ctx),
        time::module_def(ctx),
        _typing::module_def(ctx),
        _warnings::module_def(ctx),
        _weakref::module_def(ctx),
    ]
}

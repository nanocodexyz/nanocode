//!
//! Take an AST and transform it into bytecode
//!
//! Inspirational code:
//!   <https://github.com/python/cpython/blob/main/Python/compile.c>
//!   <https://github.com/micropython/micropython/blob/master/py/compile.c>

// spell-checker:ignore starunpack subscripter

#![deny(clippy::cast_possible_truncation)]

use crate::{
    IndexMap, IndexSet, ToPythonName, ast_constant_value_to_constant_data,
    error::{CodegenError, CodegenErrorType, InternalError},
    interpolated_string_literal_value, interpolation_debug_text,
    ir::{self, Block, BlockIdx, Blocks},
    preprocess, string_literal_part_value, string_literal_value,
    symboltable::{self, CompilerScope, Symbol, SymbolFlags, SymbolScope, SymbolTable},
    unparse::UnparseExpr,
};
use alloc::borrow::Cow;
use core::{mem, slice};
use nanocode_numeric::BigInt;
use nanocode_numeric::Complex;
use nanocode_numeric::{Num, ToPrimitive, Zero};
use ruff_python_ast::{self as ast, name::Name};
use ruff_text_size::{Ranged, TextRange, TextSize, TextSlice};
use rustpython_compiler_core::{
    Mode, OneIndexed, PositionEncoding, SourceFile, SourceLocation,
    bytecode::{
        self, AnyInstruction, AnyOpcode, Arg as OpArgMarker, BinaryOperator, BuildSliceArgCount,
        CodeObject, ComparisonOperator, ConstantData, ConvertValueOparg, Instruction,
        IntrinsicFunction1, Invert, LoadAttr, LoadSuperAttr, OpArg, OpArgType, PseudoInstruction,
        SpecialMethod, UnpackExArgs, oparg,
    },
};
use rustpython_literal::{
    complex as literal_complex,
    escape::{AsciiEscape, UnicodeEscape},
    float as literal_float,
};
use rustpython_wtf8::Wtf8Buf;

/// Extension trait for `ast::Expr` to add constant checking methods
trait ExprExt {
    /// Returns true if the expression is a constant literal with no side effects.
    fn is_constant(&self) -> bool;
}

impl ExprExt for ast::Expr {
    fn is_constant(&self) -> bool {
        matches!(
            self,
            Self::NumberLiteral(_)
                | Self::StringLiteral(_)
                | Self::BytesLiteral(_)
                | Self::Constant(_)
                | Self::NoneLiteral(_)
                | Self::BooleanLiteral(_)
                | Self::EllipsisLiteral(_)
        )
    }
}

const CO_MAXBLOCKS: usize = 21;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FBlockType {
    WhileLoop,
    ForLoop,
    TryExcept,
    FinallyTry,
    FinallyEnd,
    With,
    AsyncWith,
    HandlerCleanup,
    PopValue,
    ExceptionHandler,
    ExceptionGroupHandler,
    AsyncComprehensionGenerator,
    StopIteration,
}

/// Stores additional data for fblock unwinding
// fb_datum
#[derive(Debug, Clone)]
pub enum FBlockDatum {
    None,
    /// For FinallyTry: stores the finally body statements to compile during unwind
    FinallyBody(Vec<ast::Stmt>),
    /// For HandlerCleanup: stores the exception variable name (e.g., "e" in "except X as e")
    ExceptionName(String),
}

/// Type of super() call optimization detected by can_optimize_super_call()
#[derive(Debug, Clone)]
enum SuperCallType<'a> {
    /// super(class, self) - explicit 2-argument form
    TwoArg {
        class_arg: &'a ast::Expr,
        self_arg: &'a ast::Expr,
    },
    /// super() - implicit 0-argument form (uses __class__ cell)
    ZeroArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinGeneratorCallKind {
    Tuple,
    All,
    Any,
}

#[derive(Debug, Clone)]
pub struct FBlockInfo {
    pub fb_type: FBlockType,
    // CPython _PyCompile_FBlockInfo stores jump_target_label values here.
    pub(crate) fb_block: ir::InstructionSequenceLabel,
    // CPython's optional type-specific exit or cleanup jump_target_label.
    pub(crate) fb_exit: ir::InstructionSequenceLabel,
    pub fb_range: TextRange,
    // additional data for fblock unwinding
    pub fb_datum: FBlockDatum,
}

pub(crate) type InternalResult<T> = Result<T, InternalError>;
type CompileResult<T> = Result<T, CodegenError>;
pub type SyntaxWarningHandler<'a> =
    dyn FnMut(SourceLocation, String) -> Result<(), CodegenError> + 'a;

fn warn_ast_preprocess_syntax(
    source_file: &SourceFile,
    handler: &mut SyntaxWarningHandler<'_>,
    range: TextRange,
    message: String,
) -> CompileResult<()> {
    let location = source_file
        .to_source_code()
        .source_location(range.start(), PositionEncoding::Utf8);
    handler(location, message)
}

fn checked_future_features(
    ast: &ruff_python_ast::Mod,
    source_file: &SourceFile,
) -> CompileResult<bytecode::CodeFlags> {
    preprocess::checked_future_features(ast).map_err(|err| {
        let source_code = source_file.to_source_code();
        let location = source_code.source_location(err.range.start(), PositionEncoding::Utf8);
        let end_location = source_code.source_location(err.range.end(), PositionEncoding::Utf8);
        let error = match err.kind {
            preprocess::FutureFeatureErrorKind::InvalidFeature(feature) => {
                CodegenErrorType::InvalidFutureFeature(feature)
            }
            preprocess::FutureFeatureErrorKind::InvalidBraces => {
                CodegenErrorType::InvalidFutureBraces
            }
        };
        CodegenError {
            location: Some(location),
            end_location: Some(end_location),
            error,
            source_path: source_file.name().to_owned(),
        }
    })
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum NameUsage {
    Load,
    Store,
    Delete,
}
/// Main structure holding the state of compilation.
struct Compiler<'a> {
    code_stack: Vec<ir::CodeInfo>,
    symbol_table_stack: Vec<SymbolTable>,
    source_file: SourceFile,
    // current_source_location: SourceLocation,
    current_source_range: TextRange,
    future_features: bytecode::CodeFlags,
    future_annotations: bool,
    ctx: CompileContext,
    opts: CompileOpts,
    in_annotation: bool,
    /// True when compiling in "single" (interactive) mode.
    /// Expression statements at module scope emit CALL_INTRINSIC_1(Print).
    interactive: bool,
    /// Counter for dead-code elimination during constant folding.
    /// When > 0, the compiler walks AST (consuming sub_tables) but emits no bytecode.
    /// Mirrors CPython's `c_do_not_emit_bytecode`.
    do_not_emit_bytecode: u32,
    /// Mirrors `c_disable_warning` while compiling FINALLY_END copies.
    disable_warning: u32,
    syntax_warning_handler: Option<&'a mut SyntaxWarningHandler<'a>>,
    /// If true, keep unoptimized nested instruction sequences on the parent.
    save_nested_seqs: bool,
}

/// A Python `__future__` feature flag imported via `from __future__ import <feature>`.
///
/// # See Also
///
/// - [Python documentation on `__future__`](https://docs.python.org/3.14/library/__future__.html)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FutureFeature {
    /// ```py
    /// from __future__ import absolute_import
    /// ```
    AbsoluteImport,

    /// ```py
    /// from __future__ import annotations
    /// ```
    Annotations,

    /// ```py
    /// from __future__ import barry_as_FLUFL
    /// ```
    BarryAsFLUFL,

    /// ```py
    /// from __future__ import braces
    /// ```
    Braces,

    /// ```py
    /// from __future__ import division
    /// ```
    Division,

    /// ```py
    /// from __future__ import generator_stop
    /// ```
    GeneratorStop,

    /// ```py
    /// from __future__ import generators
    /// ```
    Generators,

    /// ```py
    /// from __future__ import nested_scopes
    /// ```
    NestedScopes,

    /// ```py
    /// from __future__ import print_function
    /// ```
    PrintFunction,

    /// ```py
    /// from __future__ import unicode_literals
    /// ```
    UnicodeLiterals,

    /// ```py
    /// from __future__ import with_statement
    /// ```
    WithStatement,
}

impl TryFrom<&str> for FutureFeature {
    type Error = String;

    fn try_from(name: &str) -> Result<Self, Self::Error> {
        Ok(match name {
            "absolute_import" => Self::AbsoluteImport,
            "annotations" => Self::Annotations,
            "barry_as_FLUFL" => Self::BarryAsFLUFL,
            "braces" => Self::Braces,
            "division" => Self::Division,
            "generator_stop" => Self::GeneratorStop,
            "generators" => Self::Generators,
            "nested_scopes" => Self::NestedScopes,
            "print_function" => Self::PrintFunction,
            "unicode_literals" => Self::UnicodeLiterals,
            "with_statement" => Self::WithStatement,
            _ => return Err(name.into()),
        })
    }
}

#[derive(Clone, Copy)]
enum ComprehensionSymbolSource {
    Child,
    Inlined,
}

#[derive(Clone, Copy)]
struct SymbolTableCursors {
    sub_table: usize,
    hidden_annotation_block: usize,
    inlined_comprehension_block: usize,
}

#[derive(Clone, Debug)]
pub struct CompileOpts {
    /// How optimized the bytecode output should be; any optimize > 0 does
    /// not emit assert statements
    pub optimize: u8,
    /// Include column info in bytecode (-X no_debug_ranges disables)
    pub debug_ranges: bool,
    /// Maximum decimal integer literal digits, matching sys.int_info/default.
    pub int_max_str_digits: usize,
    /// Allow module-level await/async-for/async-with, matching PyCF_ALLOW_TOP_LEVEL_AWAIT.
    pub allow_top_level_await: bool,
    /// Future compiler flags passed explicitly to compile(), matching cf_flags merge.
    pub future_features: bytecode::CodeFlags,
    /// Keep single-input blocks incomplete until a terminating newline is seen.
    pub dont_imply_dedent: bool,
    /// Recursion limit used by compiler tree walks, matching Py_EnterRecursiveCall.
    pub recursion_limit: usize,
}

impl Default for CompileOpts {
    fn default() -> Self {
        Self {
            optimize: 0,
            debug_ranges: true,
            int_max_str_digits: 4300,
            allow_top_level_await: false,
            future_features: bytecode::CodeFlags::empty(),
            dont_imply_dedent: false,
            recursion_limit: 1000,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CompileContext {
    in_class: bool,
    func: FunctionContext,
    /// True if we're anywhere inside an async function (even inside nested comprehensions)
    in_async_scope: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FunctionContext {
    NoFunction,
    Function,
    AsyncFunction,
}

impl CompileContext {
    fn in_func(self) -> bool {
        self.func != FunctionContext::NoFunction
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ComprehensionType {
    Generator,
    List,
    Set,
    Dict,
}

#[derive(Debug, Clone, Copy)]
enum ComprehensionLoopControl {
    Iteration {
        loop_block: BlockIdx,
        if_cleanup_block: BlockIdx,
        after_block: BlockIdx,
        backedge_range: TextRange,
        is_async: bool,
        end_async_for_target: BlockIdx,
    },
    IfCleanupOnly {
        if_cleanup_block: BlockIdx,
    },
}

fn validate_duplicate_params(params: &ast::Parameters) -> Result<(), CodegenErrorType> {
    let mut seen_params = IndexSet::default();
    for param in params {
        let param_name = param.name().as_str();
        if !seen_params.insert(param_name) {
            return Err(CodegenErrorType::SyntaxError(format!(
                r#"Duplicate parameter "{param_name}""#
            )));
        }
    }

    Ok(())
}

/// Compile an Mod produced from ruff parser
pub fn compile_top(
    ast: ruff_python_ast::Mod,
    source_file: SourceFile,
    mode: Mode,
    opts: CompileOpts,
) -> CompileResult<CodeObject> {
    compile_top_with_syntax_warning_handler(ast, source_file, mode, opts, None)
}

pub fn compile_top_with_syntax_warning_handler<'a>(
    mut ast: ruff_python_ast::Mod,
    source_file: SourceFile,
    mode: Mode,
    mut opts: CompileOpts,
    mut syntax_warning_handler: Option<&'a mut SyntaxWarningHandler<'a>>,
) -> CompileResult<CodeObject> {
    opts.future_features |= checked_future_features(&ast, &source_file)?;
    let future_annotations = opts
        .future_features
        .contains(bytecode::CodeFlags::FUTURE_ANNOTATIONS);
    if let Some(handler) = syntax_warning_handler.as_deref_mut() {
        preprocess::warn_control_flow_in_finally(&ast, |range, message| {
            warn_ast_preprocess_syntax(&source_file, handler, range, message)
        })?;
    }
    if matches!(mode, Mode::Single)
        && let ruff_python_ast::Mod::Module(module) = &mut ast
    {
        preprocess::preprocess_statements(
            &mut module.body,
            opts.optimize,
            future_annotations,
            false,
        );
    } else {
        preprocess::preprocess_mod(&mut ast, opts.optimize, future_annotations, false);
    }
    match ast {
        ruff_python_ast::Mod::Module(module) => match mode {
            Mode::Exec | Mode::Eval => compile_program_with_syntax_warning_handler(
                &module,
                source_file,
                opts,
                syntax_warning_handler,
            ),
            Mode::Single => compile_program_single_with_syntax_warning_handler(
                &module,
                source_file,
                opts,
                syntax_warning_handler,
            ),
            Mode::BlockExpr => compile_block_expression_with_syntax_warning_handler(
                &module,
                source_file,
                opts,
                syntax_warning_handler,
            ),
        },
        ruff_python_ast::Mod::Expression(expr) => compile_expression_with_syntax_warning_handler(
            &expr,
            source_file,
            opts,
            syntax_warning_handler,
        ),
    }
}

/// Unoptimized instruction sequence and the metadata needed by optimize_cfg.
pub struct CodegenOutput {
    pub seq: ir::InstructionSequence,
    pub argcount: u32,
    pub posonlyargcount: u32,
    pub kwonlyargcount: u32,
    pub consts: Vec<ConstantData>,
}

/// compile.c _PyCompile_CodeGen
pub fn compile_codegen(
    ast: ruff_python_ast::Mod,
    source_file: SourceFile,
    mode: Mode,
    opts: CompileOpts,
) -> CompileResult<CodegenOutput> {
    compile_codegen_with_syntax_warning_handler(ast, source_file, mode, opts, None)
}

fn compile_codegen_with_syntax_warning_handler<'a>(
    mut ast: ruff_python_ast::Mod,
    source_file: SourceFile,
    mode: Mode,
    mut opts: CompileOpts,
    mut syntax_warning_handler: Option<&'a mut SyntaxWarningHandler<'a>>,
) -> CompileResult<CodegenOutput> {
    opts.future_features |= checked_future_features(&ast, &source_file)?;
    let future_annotations = opts
        .future_features
        .contains(bytecode::CodeFlags::FUTURE_ANNOTATIONS);
    if let Some(handler) = syntax_warning_handler.as_deref_mut() {
        preprocess::warn_control_flow_in_finally(&ast, |range, message| {
            warn_ast_preprocess_syntax(&source_file, handler, range, message)
        })?;
    }
    if matches!(mode, Mode::Single)
        && let ruff_python_ast::Mod::Module(module) = &mut ast
    {
        preprocess::preprocess_statements(
            &mut module.body,
            opts.optimize,
            future_annotations,
            false,
        );
    } else {
        preprocess::preprocess_mod(&mut ast, opts.optimize, future_annotations, false);
    }

    let mut compiler = Compiler::new_with_syntax_warning_handler(
        opts,
        source_file.clone(),
        "<module>",
        syntax_warning_handler,
    );
    compiler.save_nested_seqs = true;
    match ast {
        ruff_python_ast::Mod::Module(module) => match mode {
            Mode::Exec | Mode::Eval => {
                let symbol_table = scan_module_symbols(&module, &source_file, &compiler.opts)?;
                compiler.compile_program(&module, symbol_table)?;
            }
            Mode::Single => {
                let symbol_table = scan_module_symbols(&module, &source_file, &compiler.opts)?;
                compiler.compile_program_single(&module.body, symbol_table)?;
            }
            Mode::BlockExpr => {
                let symbol_table = scan_module_symbols(&module, &source_file, &compiler.opts)?;
                compiler.compile_block_expr(&module.body, symbol_table)?;
            }
        },
        ruff_python_ast::Mod::Expression(expr) => {
            let symbol_table = scan_expr_symbols(&expr, &source_file, &compiler.opts)?;
            compiler.compile_eval(&expr, symbol_table)?;
        }
    }

    let mut unit = compiler
        .code_stack
        .pop()
        .expect("codegen leaves the top-level unit on the stack");
    unit.instr_sequence.apply_label_map();
    Ok(CodegenOutput {
        seq: unit.instr_sequence,
        argcount: unit.metadata.argcount,
        posonlyargcount: unit.metadata.posonlyargcount,
        kwonlyargcount: unit.metadata.kwonlyargcount,
        consts: unit.metadata.consts.into_vec(),
    })
}

/// Compile a standard Python program to bytecode
pub fn compile_program(
    ast: &ast::ModModule,
    source_file: SourceFile,
    opts: CompileOpts,
) -> CompileResult<CodeObject> {
    compile_program_with_syntax_warning_handler(ast, source_file, opts, None)
}

fn scan_module_symbols(
    ast: &ast::ModModule,
    source_file: &SourceFile,
    opts: &CompileOpts,
) -> CompileResult<SymbolTable> {
    SymbolTable::scan_program_with_options(
        ast,
        source_file.clone(),
        opts.allow_top_level_await,
        opts.future_features
            .contains(bytecode::CodeFlags::FUTURE_ANNOTATIONS),
        opts.recursion_limit,
    )
    .map_err(|e| e.into_codegen_error(source_file.name().to_owned()))
}

fn scan_expr_symbols(
    ast: &ast::ModExpression,
    source_file: &SourceFile,
    opts: &CompileOpts,
) -> CompileResult<SymbolTable> {
    SymbolTable::scan_expr_with_options(
        ast,
        source_file.clone(),
        opts.allow_top_level_await,
        opts.future_features
            .contains(bytecode::CodeFlags::FUTURE_ANNOTATIONS),
        opts.recursion_limit,
    )
    .map_err(|e| e.into_codegen_error(source_file.name().to_owned()))
}

fn compile_program_with_syntax_warning_handler<'a>(
    ast: &ast::ModModule,
    source_file: SourceFile,
    opts: CompileOpts,
    syntax_warning_handler: Option<&'a mut SyntaxWarningHandler<'a>>,
) -> CompileResult<CodeObject> {
    let symbol_table = scan_module_symbols(ast, &source_file, &opts)?;
    let mut compiler = Compiler::new_with_syntax_warning_handler(
        opts,
        source_file,
        "<module>",
        syntax_warning_handler,
    );
    compiler.compile_program(ast, symbol_table)?;
    let code = compiler.exit_scope();
    nanocode_trace::trace!("Compilation completed: {code:?}");
    Ok(code)
}

/// Compile a Python program to bytecode for the context of a REPL
pub fn compile_program_single(
    ast: &ast::ModModule,
    source_file: SourceFile,
    opts: CompileOpts,
) -> CompileResult<CodeObject> {
    compile_program_single_with_syntax_warning_handler(ast, source_file, opts, None)
}

fn compile_program_single_with_syntax_warning_handler<'a>(
    ast: &ast::ModModule,
    source_file: SourceFile,
    opts: CompileOpts,
    syntax_warning_handler: Option<&'a mut SyntaxWarningHandler<'a>>,
) -> CompileResult<CodeObject> {
    let symbol_table = scan_module_symbols(ast, &source_file, &opts)?;
    let mut compiler = Compiler::new_with_syntax_warning_handler(
        opts,
        source_file,
        "<module>",
        syntax_warning_handler,
    );
    compiler.compile_program_single(&ast.body, symbol_table)?;
    let code = compiler.exit_scope();
    nanocode_trace::trace!("Compilation completed: {code:?}");
    Ok(code)
}

pub fn compile_block_expression(
    ast: &ast::ModModule,
    source_file: SourceFile,
    opts: CompileOpts,
) -> CompileResult<CodeObject> {
    compile_block_expression_with_syntax_warning_handler(ast, source_file, opts, None)
}

fn compile_block_expression_with_syntax_warning_handler<'a>(
    ast: &ast::ModModule,
    source_file: SourceFile,
    opts: CompileOpts,
    syntax_warning_handler: Option<&'a mut SyntaxWarningHandler<'a>>,
) -> CompileResult<CodeObject> {
    let symbol_table = scan_module_symbols(ast, &source_file, &opts)?;
    let mut compiler = Compiler::new_with_syntax_warning_handler(
        opts,
        source_file,
        "<module>",
        syntax_warning_handler,
    );
    compiler.compile_block_expr(&ast.body, symbol_table)?;
    let code = compiler.exit_scope();
    nanocode_trace::trace!("Compilation completed: {code:?}");
    Ok(code)
}

pub fn compile_expression(
    ast: &ast::ModExpression,
    source_file: SourceFile,
    opts: CompileOpts,
) -> CompileResult<CodeObject> {
    compile_expression_with_syntax_warning_handler(ast, source_file, opts, None)
}

fn compile_expression_with_syntax_warning_handler<'a>(
    ast: &ast::ModExpression,
    source_file: SourceFile,
    opts: CompileOpts,
    syntax_warning_handler: Option<&'a mut SyntaxWarningHandler<'a>>,
) -> CompileResult<CodeObject> {
    let symbol_table = scan_expr_symbols(ast, &source_file, &opts)?;
    let mut compiler = Compiler::new_with_syntax_warning_handler(
        opts,
        source_file,
        "<module>",
        syntax_warning_handler,
    );
    compiler.compile_eval(ast, symbol_table)?;
    let code = compiler.exit_scope();
    Ok(code)
}

macro_rules! emit {
    // Struct variant with single identifier (e.g., Foo::A { arg })
    ($c:expr, $enum:ident :: $op:ident { $arg:ident $(,)? } $(,)?) => {
        $c.emit_arg($arg, |x| $enum::$op { $arg: x })
    };

    // Struct variant with explicit value (e.g., Foo::A { arg: 42 })
    ($c:expr, $enum:ident :: $op:ident { $arg:ident : $arg_val:expr $(,)? } $(,)?) => {
        $c.emit_arg($arg_val, |x| $enum::$op { $arg: x })
    };

    // Tuple variant (e.g., Foo::B(42)). Should never be reached, here for validation.
    ($c:expr, $enum:ident :: $op:ident($arg_val:expr $(,)? ) $(,)?) => {
        panic!("No instruction should be defined as `Instruction::Foo(value)` use `Instruction::Foo { x: value }` instead")
    };

    // No-arg variant (e.g., Foo::C)
    ($c:expr, $enum:ident :: $op:ident $(,)?) => {
        $c.emit_no_arg($enum::$op)
    };
}

fn eprint_location(zelf: &Compiler<'_>) {
    let start = zelf
        .source_file
        .to_source_code()
        .source_location(zelf.current_source_range.start(), PositionEncoding::Utf8);
    let end = zelf
        .source_file
        .to_source_code()
        .source_location(zelf.current_source_range.end(), PositionEncoding::Utf8);
    eprintln!(
        "LOCATION: {} from {}:{} to {}:{}",
        zelf.source_file.name(),
        start.line,
        start.character_offset,
        end.line,
        end.character_offset
    );
}

/// Better traceback for internal error
#[track_caller]
fn unwrap_internal<T>(zelf: &Compiler<'_>, r: InternalResult<T>) -> T {
    if let Err(ref r_err) = r {
        eprintln!("=== CODEGEN PANIC INFO ===");
        eprintln!("This IS an internal error: {r_err}");
        eprint_location(zelf);
        eprintln!("=== END PANIC INFO ===");
    }
    r.unwrap()
}

fn compiler_unwrap_option<T>(zelf: &Compiler<'_>, o: Option<T>) -> T {
    if o.is_none() {
        eprintln!("=== CODEGEN PANIC INFO ===");
        eprintln!("This IS an internal error, an option was unwrapped during codegen");
        eprint_location(zelf);
        eprintln!("=== END PANIC INFO ===");
    }
    o.unwrap()
}

// fn compiler_result_unwrap<T, E: core::fmt::Debug>(zelf: &Compiler, result: Result<T, E>) -> T {
//     if result.is_err() {
//         eprintln!("=== CODEGEN PANIC INFO ===");
//         eprintln!("This IS an internal error, an result was unwrapped during codegen");
//         eprint_location(zelf);
//         eprintln!("=== END PANIC INFO ===");
//     }
//     result.unwrap()
// }

/// The pattern context holds information about captured names and jump targets.
#[derive(Clone)]
pub struct PatternContext {
    /// A list of names captured by the pattern.
    pub stores: Vec<Name>,
    /// If false, then any name captures against our subject will raise.
    pub allow_irrefutable: bool,
    /// A list of jump target labels used on pattern failure.
    pub fail_pop: Vec<BlockIdx>,
    /// The number of items on top of the stack that should remain.
    pub on_top: usize,
}

impl Default for PatternContext {
    fn default() -> Self {
        Self::new()
    }
}

impl PatternContext {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stores: Vec::new(),
            allow_irrefutable: false,
            fail_pop: Vec::new(),
            on_top: 0,
        }
    }

    #[must_use]
    pub fn fail_pop_size(&self) -> usize {
        self.fail_pop.len()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum JumpOp {
    Jump,
    PopJumpIfFalse,
}

/// Type of collection to build in starunpack_helper
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CollectionType {
    Tuple,
    List,
    Set,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum InferredType {
    Tuple,
    List,
    Dict,
    Set,
    FrozenSet,
    Generator,
    Function,
    Template,
    Str,
    Bytes,
    Int,
    Float,
    Complex,
    Bool,
    NoneType,
    Ellipsis,
    Slice,
}

impl InferredType {
    const fn name(self) -> &'static str {
        match self {
            Self::Tuple => "tuple",
            Self::List => "list",
            Self::Dict => "dict",
            Self::Set => "set",
            Self::FrozenSet => "frozenset",
            Self::Generator => "generator",
            Self::Function => "function",
            Self::Template => "string.templatelib.Template",
            Self::Str => "str",
            Self::Bytes => "bytes",
            Self::Int => "int",
            Self::Float => "float",
            Self::Complex => "complex",
            Self::Bool => "bool",
            Self::NoneType => "NoneType",
            Self::Ellipsis => "ellipsis",
            Self::Slice => "slice",
        }
    }

    const fn is_long_subclass(self) -> bool {
        matches!(self, Self::Int | Self::Bool)
    }
}

const STACK_USE_GUIDELINE: u32 = 30;

impl<'warnings> Compiler<'warnings> {
    fn constant_truthiness(constant: &ConstantData) -> bool {
        match constant {
            ConstantData::Tuple { elements } | ConstantData::Frozenset { elements } => {
                !elements.is_empty()
            }
            ConstantData::Integer { value } => !value.is_zero(),
            ConstantData::Float { value } => *value != 0.0,
            ConstantData::Complex { value } => value.re != 0.0 || value.im != 0.0,
            ConstantData::Boolean { value } => *value,
            ConstantData::Str { value } => !value.is_empty(),
            ConstantData::Bytes { value } => !value.is_empty(),
            ConstantData::Code { .. } | ConstantData::Slice { .. } | ConstantData::Ellipsis => true,
            ConstantData::None => false,
        }
    }

    fn infer_type_constant(constant: &ConstantData) -> Option<InferredType> {
        match constant {
            ConstantData::Tuple { .. } => Some(InferredType::Tuple),
            ConstantData::Frozenset { .. } => Some(InferredType::FrozenSet),
            ConstantData::Integer { .. } => Some(InferredType::Int),
            ConstantData::Float { .. } => Some(InferredType::Float),
            ConstantData::Complex { .. } => Some(InferredType::Complex),
            ConstantData::Boolean { .. } => Some(InferredType::Bool),
            ConstantData::Str { .. } => Some(InferredType::Str),
            ConstantData::Bytes { .. } => Some(InferredType::Bytes),
            ConstantData::None => Some(InferredType::NoneType),
            ConstantData::Ellipsis => Some(InferredType::Ellipsis),
            ConstantData::Slice { .. } => Some(InferredType::Slice),
            ConstantData::Code { .. } => None,
        }
    }

    fn infer_type(&self, expr: &ast::Expr) -> Option<InferredType> {
        if let Some(constant) = self.ast_constant_value(expr) {
            return Self::infer_type_constant(&constant);
        }
        match expr {
            ast::Expr::Tuple(_) => Some(InferredType::Tuple),
            ast::Expr::List(_) | ast::Expr::ListComp(_) => Some(InferredType::List),
            ast::Expr::Dict(_) | ast::Expr::DictComp(_) => Some(InferredType::Dict),
            ast::Expr::Set(_) | ast::Expr::SetComp(_) => Some(InferredType::Set),
            ast::Expr::Generator(_) => Some(InferredType::Generator),
            ast::Expr::Lambda(_) => Some(InferredType::Function),
            ast::Expr::TString(_) => Some(InferredType::Template),
            ast::Expr::FString(_) | ast::Expr::StringLiteral(_) => Some(InferredType::Str),
            ast::Expr::BytesLiteral(_) => Some(InferredType::Bytes),
            ast::Expr::NumberLiteral(number) => match number.value {
                ast::Number::Int(_) => Some(InferredType::Int),
                ast::Number::Float(_) => Some(InferredType::Float),
                ast::Number::Complex { .. } => Some(InferredType::Complex),
            },
            ast::Expr::BooleanLiteral(_) => Some(InferredType::Bool),
            ast::Expr::NoneLiteral(_) => Some(InferredType::NoneType),
            ast::Expr::EllipsisLiteral(_) => Some(InferredType::Ellipsis),
            ast::Expr::Slice(_) => Some(InferredType::Slice),
            _ => None,
        }
    }

    fn is_constant_expr(&self, expr: &ast::Expr) -> bool {
        if self.ast_constant_value(expr).is_some() {
            return true;
        }
        matches!(
            expr,
            ast::Expr::StringLiteral(_)
                | ast::Expr::BytesLiteral(_)
                | ast::Expr::NumberLiteral(_)
                | ast::Expr::BooleanLiteral(_)
                | ast::Expr::NoneLiteral(_)
                | ast::Expr::EllipsisLiteral(_)
        )
    }

    fn is_constant_slice(&self, slice: &ast::Expr) -> bool {
        match slice {
            ast::Expr::Slice(s) => {
                let lower_const = s.lower.is_none()
                    || s.lower.as_deref().is_some_and(|e| self.is_constant_expr(e));
                let upper_const = s.upper.is_none()
                    || s.upper.as_deref().is_some_and(|e| self.is_constant_expr(e));
                let step_const =
                    s.step.is_none() || s.step.as_deref().is_some_and(|e| self.is_constant_expr(e));
                lower_const && upper_const && step_const
            }
            _ => false,
        }
    }

    fn should_apply_two_element_slice_optimization(&self, slice: &ast::Expr) -> bool {
        !self.is_constant_slice(slice) && matches!(slice, ast::Expr::Slice(s) if s.step.is_none())
    }

    fn check_is_arg(&self, expr: &ast::Expr) -> bool {
        if let Some(constant) = self.ast_constant_value(expr) {
            return matches!(
                constant,
                ConstantData::None | ConstantData::Boolean { .. } | ConstantData::Ellipsis
            );
        }
        if let ast::Expr::Tuple(tuple) = expr {
            return !tuple.elts.iter().all(|expr| self.is_constant_expr(expr));
        }
        if !self.is_constant_expr(expr) {
            return true;
        }
        matches!(
            expr,
            ast::Expr::NoneLiteral(_)
                | ast::Expr::BooleanLiteral(_)
                | ast::Expr::EllipsisLiteral(_)
        )
    }

    fn warn_syntax(&mut self, range: TextRange, message: String) -> CompileResult<()> {
        if self.disable_warning > 0 {
            return Ok(());
        }
        let Some(handler) = self.syntax_warning_handler.as_deref_mut() else {
            return Ok(());
        };
        let location = self
            .source_file
            .to_source_code()
            .source_location(range.start(), PositionEncoding::Utf8);
        handler(location, message)
    }

    fn check_caller(&mut self, func: &ast::Expr) -> CompileResult<()> {
        let warns = self.ast_constant_value(func).is_some()
            || matches!(
                func,
                ast::Expr::StringLiteral(_)
                    | ast::Expr::BytesLiteral(_)
                    | ast::Expr::NumberLiteral(_)
                    | ast::Expr::BooleanLiteral(_)
                    | ast::Expr::NoneLiteral(_)
                    | ast::Expr::EllipsisLiteral(_)
                    | ast::Expr::Tuple(_)
                    | ast::Expr::List(_)
                    | ast::Expr::ListComp(_)
                    | ast::Expr::Dict(_)
                    | ast::Expr::DictComp(_)
                    | ast::Expr::Set(_)
                    | ast::Expr::SetComp(_)
                    | ast::Expr::Generator(_)
                    | ast::Expr::FString(_)
                    | ast::Expr::TString(_)
            );
        if warns && let Some(inferred) = self.infer_type(func) {
            self.warn_syntax(
                func.range(),
                format!(
                    "'{}' object is not callable; perhaps you missed a comma?",
                    inferred.name()
                ),
            )?;
        }
        Ok(())
    }

    fn check_compare(
        &mut self,
        range: TextRange,
        left: &ast::Expr,
        ops: &[ast::CmpOp],
        comparators: &[ast::Expr],
    ) -> CompileResult<()> {
        let mut left_is_arg = self.check_is_arg(left);
        let mut left_expr = left;
        for (op, right_expr) in ops.iter().zip(comparators.iter()) {
            let right_is_arg = self.check_is_arg(right_expr);
            if matches!(op, ast::CmpOp::Is | ast::CmpOp::IsNot) && (!right_is_arg || !left_is_arg) {
                let literal = if !left_is_arg { left_expr } else { right_expr };
                if let Some(inferred) = self.infer_type(literal) {
                    let is_op = matches!(op, ast::CmpOp::Is);
                    let op = if is_op { "\"is\"" } else { "\"is not\"" };
                    let replacement = if is_op { "==" } else { "!=" };
                    self.warn_syntax(
                        range,
                        format!(
                            "{op} with '{}' literal. Did you mean \"{replacement}\"?",
                            inferred.name()
                        ),
                    )?;
                    return Ok(());
                }
            }
            left_is_arg = right_is_arg;
            left_expr = right_expr;
        }
        Ok(())
    }

    fn constant_warns_as_subscripter(constant: &ConstantData) -> bool {
        matches!(
            constant,
            ConstantData::None
                | ConstantData::Ellipsis
                | ConstantData::Integer { .. }
                | ConstantData::Float { .. }
                | ConstantData::Complex { .. }
                | ConstantData::Boolean { .. }
                | ConstantData::Frozenset { .. }
        )
    }

    fn check_subscripter(&mut self, value: &ast::Expr) -> CompileResult<()> {
        let warns = self
            .ast_constant_value(value)
            .is_some_and(|constant| Self::constant_warns_as_subscripter(&constant))
            || matches!(
                value,
                ast::Expr::NoneLiteral(_)
                    | ast::Expr::EllipsisLiteral(_)
                    | ast::Expr::NumberLiteral(_)
                    | ast::Expr::BooleanLiteral(_)
                    | ast::Expr::Set(_)
                    | ast::Expr::SetComp(_)
                    | ast::Expr::Generator(_)
                    | ast::Expr::TString(_)
                    | ast::Expr::Lambda(_)
            );
        if warns && let Some(inferred) = self.infer_type(value) {
            self.warn_syntax(
                value.range(),
                format!(
                    "'{}' object is not subscriptable; perhaps you missed a comma?",
                    inferred.name()
                ),
            )?;
        }
        Ok(())
    }

    fn check_index(&mut self, value: &ast::Expr, slice: &ast::Expr) -> CompileResult<()> {
        let Some(index_type) = self.infer_type(slice) else {
            return Ok(());
        };
        if index_type.is_long_subclass() || index_type == InferredType::Slice {
            return Ok(());
        }

        let constant_warns = self.ast_constant_value(value).is_some_and(|constant| {
            matches!(
                constant,
                ConstantData::Str { .. } | ConstantData::Bytes { .. } | ConstantData::Tuple { .. }
            )
        });
        let warns = constant_warns
            || matches!(
                value,
                ast::Expr::StringLiteral(_)
                    | ast::Expr::BytesLiteral(_)
                    | ast::Expr::Tuple(_)
                    | ast::Expr::List(_)
                    | ast::Expr::ListComp(_)
                    | ast::Expr::FString(_)
            );
        if warns && let Some(value_type) = self.infer_type(value) {
            self.warn_syntax(
                value.range(),
                format!(
                    "{} indices must be integers or slices, not {}; perhaps you missed a comma?",
                    value_type.name(),
                    index_type.name()
                ),
            )?;
        }
        Ok(())
    }

    fn check_assert(&mut self, assert_stmt: &ast::StmtAssert) -> CompileResult<()> {
        let warns = match &*assert_stmt.test {
            ast::Expr::Tuple(tuple) => !tuple.elts.is_empty(),
            _ => matches!(
                self.ast_constant_value(&assert_stmt.test),
                Some(ConstantData::Tuple { ref elements }) if !elements.is_empty()
            ),
        };
        if warns {
            self.warn_syntax(
                assert_stmt.range,
                "assertion is always true, perhaps remove parentheses?".to_owned(),
            )?;
        }
        Ok(())
    }

    fn new_with_syntax_warning_handler(
        opts: CompileOpts,
        source_file: SourceFile,
        code_name: &str,
        syntax_warning_handler: Option<&'warnings mut SyntaxWarningHandler<'warnings>>,
    ) -> Self {
        let module_code = ir::CodeInfo {
            // CPython convention: top-level module / interactive /
            // expression code does not carry CO_NEWLOCALS or CO_OPTIMIZED.
            // (See `Python/compile.c compiler_enter_scope` for module
            // scope.)  This matches the per-scope mapping at
            // enter_scope::CompilerScope::Module below, which also returns
            // empty flags.  frame.rs:725-731 then binds locals to globals
            // for module/REPL frames whose `scope.locals` is None - the
            // correct semantics for `exec(code, globals)` and module init.
            flags: bytecode::CodeFlags::empty(),
            source_path: source_file.name().to_owned(),
            private: None,
            blocks: Blocks::from([Block::default()]),
            current_block: BlockIdx::new(0),
            instr_sequence: ir::InstructionSequence::new(),
            instr_sequence_label_map: ir::InstructionSequenceLabelMap::new(),
            annotations_instr_sequence: None,
            metadata: ir::CodeUnitMetadata {
                name: code_name.to_string(),
                qualname: Some(code_name.to_string()),
                consts: Default::default(),
                names: IndexSet::default(),
                varnames: IndexSet::default(),
                cellvars: IndexSet::default(),
                freevars: IndexSet::default(),
                fast_hidden: IndexMap::default(),
                fast_hidden_final: IndexSet::default(),
                argcount: 0,
                posonlyargcount: 0,
                kwonlyargcount: 0,
                firstlineno: OneIndexed::MIN,
            },
            static_attributes: None,
            in_inlined_comp: false,
            fblock: Vec::with_capacity(CO_MAXBLOCKS),
            symbol_table_index: 0, // Module is always the first symbol table
            nparams: 0,
            in_conditional_block: 0,
            next_conditional_annotation_index: 0,
        };
        Self {
            code_stack: vec![module_code],
            symbol_table_stack: Vec::new(),
            source_file,
            // current_source_location: SourceLocation::default(),
            current_source_range: TextRange::default(),
            future_features: opts.future_features,
            future_annotations: false,
            ctx: CompileContext {
                in_class: false,
                func: FunctionContext::NoFunction,
                in_async_scope: false,
            },
            opts,
            in_annotation: false,
            interactive: false,
            do_not_emit_bytecode: 0,
            disable_warning: 0,
            syntax_warning_handler,
            save_nested_seqs: false,
        }
    }

    fn compile_module_annotation_setup_sequence(
        &mut self,
        body: &[ast::Stmt],
        loc: TextRange,
    ) -> CompileResult<()> {
        let (
            saved_blocks,
            saved_current_block,
            saved_instr_sequence,
            saved_instr_sequence_label_map,
            saved_annotations_instr_sequence,
        ) = {
            let code = self.current_code_info();

            (
                mem::replace(&mut code.blocks, Blocks::from([Block::default()])),
                mem::replace(&mut code.current_block, BlockIdx::new(0)),
                mem::replace(&mut code.instr_sequence, ir::InstructionSequence::new()),
                mem::replace(
                    &mut code.instr_sequence_label_map,
                    ir::InstructionSequenceLabelMap::new(),
                ),
                code.annotations_instr_sequence.take(),
            )
        };

        let result = self.compile_module_annotate(body, Some(loc));

        {
            let code = self.current_code_info();
            code.blocks = saved_blocks;
            let annotations_instr_sequence =
                mem::replace(&mut code.instr_sequence, saved_instr_sequence);
            code.current_block = saved_current_block;
            code.instr_sequence_label_map = saved_instr_sequence_label_map;
            debug_assert!(saved_annotations_instr_sequence.is_none());
            if matches!(result, Ok(true)) {
                code.annotations_instr_sequence = Some(annotations_instr_sequence);
            } else {
                code.annotations_instr_sequence = saved_annotations_instr_sequence;
            }
        };

        result.map(|_| ())
    }

    /// Compile just start and stop of a slice (for BINARY_SLICE/STORE_SLICE)
    // = codegen_slice_two_parts
    fn compile_slice_two_parts(&mut self, s: &ast::ExprSlice) -> CompileResult<()> {
        // Compile lower (or None)
        if let Some(lower) = &s.lower {
            self.compile_expression(lower)?;
        } else {
            self.set_source_range(s.range);
            self.emit_load_const(ConstantData::None);
        }

        // Compile upper (or None)
        if let Some(upper) = &s.upper {
            self.compile_expression(upper)?;
        } else {
            self.set_source_range(s.range);
            self.emit_load_const(ConstantData::None);
        }

        Ok(())
    }
    /// Compile a subscript expression
    // = compiler_subscript
    fn compile_subscript(
        &mut self,
        value: &ast::Expr,
        slice: &ast::Expr,
        ctx: ast::ExprContext,
    ) -> CompileResult<()> {
        // Save full subscript expression range (set by compile_expression before this call)
        let subscript_range = self.current_source_range;
        if matches!(ctx, ast::ExprContext::Load) {
            self.check_subscripter(value)?;
            self.check_index(value, slice)?;
        }

        // VISIT(c, expr, e->v.Subscript.value)
        self.compile_expression(value)?;

        // Handle two-element non-constant slice with BINARY_SLICE/STORE_SLICE
        let use_slice_opt = matches!(ctx, ast::ExprContext::Load | ast::ExprContext::Store)
            && self.should_apply_two_element_slice_optimization(slice);
        if use_slice_opt {
            match slice {
                ast::Expr::Slice(s) => self.compile_slice_two_parts(s)?,
                _ => unreachable!(
                    "should_use_slice_optimization should only return true for ast::Expr::Slice"
                ),
            };
        } else {
            // VISIT(c, expr, e->v.Subscript.slice)
            self.compile_expression(slice)?;
        }

        // Restore full subscript expression range before emitting
        self.set_source_range(subscript_range);

        match (use_slice_opt, ctx) {
            (true, ast::ExprContext::Load) => emit!(self, Instruction::BinarySlice),
            (true, ast::ExprContext::Store) => emit!(self, Instruction::StoreSlice),
            (true, _) => unreachable!(),
            (false, ast::ExprContext::Load) => emit!(
                self,
                Instruction::BinaryOp {
                    op: BinaryOperator::Subscr
                }
            ),
            (false, ast::ExprContext::Store) => emit!(self, Instruction::StoreSubscr),
            (false, ast::ExprContext::Del) => emit!(self, Instruction::DeleteSubscr),
            (false, ast::ExprContext::Invalid) => {
                return Err(self.error(CodegenErrorType::SyntaxError(
                    "Invalid expression context".to_owned(),
                )));
            }
        }

        Ok(())
    }

    /// Helper function for compiling tuples/lists/sets with starred expressions
    ///
    /// ast::Parameters:
    /// - elts: The elements to compile
    /// - pushed: Number of items already on the stack
    /// - collection_type: What type of collection to build (tuple, list, set)
    ///
    // = starunpack_helper in compile.c
    fn starunpack_helper_impl(
        &mut self,
        elts: &[ast::Expr],
        injected_arg: Option<&Name>,
        pushed: u32,
        collection_type: CollectionType,
    ) -> CompileResult<()> {
        let collection_range = self.current_source_range;
        let n = elts.len().to_u32();
        let seen_star = elts.iter().any(|e| matches!(e, ast::Expr::Starred(_)));

        let injected_count = u32::from(injected_arg.is_some());
        let big = n + pushed + injected_count > STACK_USE_GUIDELINE;

        // Constant collections are not folded here: the late flowgraph
        // optimization passes introduce tuple-backed constants after their
        // operands have first been emitted, matching the constant ordering.

        // If no stars and not too big, compile all elements and build once
        if !seen_star && !big {
            for elt in elts {
                self.compile_expression(elt)?;
            }
            if let Some(injected_arg) = injected_arg {
                self.set_source_range(collection_range);
                self.load_name(injected_arg)?;
            }
            let total_size = n + injected_count + pushed;
            self.set_source_range(collection_range);
            match collection_type {
                CollectionType::List => {
                    emit!(self, Instruction::BuildList { count: total_size });
                }
                CollectionType::Set => {
                    emit!(self, Instruction::BuildSet { count: total_size });
                }
                CollectionType::Tuple => {
                    emit!(self, Instruction::BuildTuple { count: total_size });
                }
            }
            return Ok(());
        }

        // Has stars or too big: use streaming approach.
        let mut sequence_built = false;
        let mut i = 0u32;

        if big {
            self.set_source_range(collection_range);
            match collection_type {
                CollectionType::List => {
                    emit!(self, Instruction::BuildList { count: pushed });
                    sequence_built = true;
                }
                CollectionType::Set => {
                    emit!(self, Instruction::BuildSet { count: pushed });
                    sequence_built = true;
                }
                CollectionType::Tuple => {
                    emit!(self, Instruction::BuildList { count: pushed });
                    sequence_built = true;
                }
            }
        }

        for elt in elts {
            if let ast::Expr::Starred(ast::ExprStarred { value, .. }) = elt {
                // When we hit first star, build sequence with elements so far
                if !sequence_built {
                    self.set_source_range(collection_range);
                    match collection_type {
                        CollectionType::List => {
                            emit!(self, Instruction::BuildList { count: i + pushed });
                        }
                        CollectionType::Set => {
                            emit!(self, Instruction::BuildSet { count: i + pushed });
                        }
                        CollectionType::Tuple => {
                            emit!(self, Instruction::BuildList { count: i + pushed });
                        }
                    }
                    sequence_built = true;
                }

                // Compile the starred expression and extend
                self.compile_expression(value)?;
                self.set_source_range(collection_range);
                match collection_type {
                    CollectionType::List => {
                        emit!(self, Instruction::ListExtend { i: 1 });
                    }
                    CollectionType::Set => {
                        emit!(self, Instruction::SetUpdate { i: 1 });
                    }
                    CollectionType::Tuple => {
                        emit!(self, Instruction::ListExtend { i: 1 });
                    }
                }
            } else {
                // Non-starred element
                self.compile_expression(elt)?;

                if sequence_built {
                    // Sequence already exists, append to it
                    self.set_source_range(collection_range);
                    match collection_type {
                        CollectionType::List => {
                            emit!(self, Instruction::ListAppend { i: 1 });
                        }
                        CollectionType::Set => {
                            emit!(self, Instruction::SetAdd { i: 1 });
                        }
                        CollectionType::Tuple => {
                            emit!(self, Instruction::ListAppend { i: 1 });
                        }
                    }
                } else {
                    // Still collecting elements before first star
                    i += 1;
                }
            }
        }

        debug_assert!(sequence_built);
        if let Some(injected_arg) = injected_arg {
            self.set_source_range(collection_range);
            self.load_name(injected_arg)?;
            self.set_source_range(collection_range);
            match collection_type {
                CollectionType::List | CollectionType::Tuple => {
                    emit!(self, Instruction::ListAppend { i: 1 });
                }
                CollectionType::Set => {
                    emit!(self, Instruction::SetAdd { i: 1 });
                }
            }
        }

        if collection_type == CollectionType::Tuple {
            self.set_source_range(collection_range);
            emit!(
                self,
                Instruction::CallIntrinsic1 {
                    func: IntrinsicFunction1::ListToTuple
                }
            );
        }

        Ok(())
    }

    fn starunpack_helper(
        &mut self,
        elts: &[ast::Expr],
        pushed: u32,
        collection_type: CollectionType,
    ) -> CompileResult<()> {
        self.starunpack_helper_impl(elts, None, pushed, collection_type)
    }

    fn error(&mut self, error: CodegenErrorType) -> CodegenError {
        self.error_ranged(error, self.current_source_range)
    }

    fn error_ranged(&mut self, error: CodegenErrorType, range: TextRange) -> CodegenError {
        let source_code = self.source_file.to_source_code();
        let location = source_code.source_location(range.start(), PositionEncoding::Utf8);
        let end_location = source_code.source_location(range.end(), PositionEncoding::Utf8);
        CodegenError {
            error,
            location: Some(location),
            end_location: Some(end_location),
            source_path: self.source_file.name().to_owned(),
        }
    }

    fn error_optional_range(
        &mut self,
        error: CodegenErrorType,
        range: Option<TextRange>,
    ) -> CodegenError {
        match range {
            Some(range) => self.error_ranged(error, range),
            None => CodegenError {
                error,
                location: None,
                end_location: None,
                source_path: self.source_file.name().to_owned(),
            },
        }
    }

    /// Get the SymbolTable for the current scope.
    fn current_symbol_table(&self) -> &SymbolTable {
        self.symbol_table_stack
            .last()
            .expect("symbol_table_stack is empty! This is a compiler bug.")
    }

    fn has_enclosing_non_module_code_scope(&self) -> bool {
        self.code_stack.len() > 1
    }

    /// Match CPython's `is_import_originated()`: only imports recorded in the
    /// module-level symbol table suppress method-call optimization.
    fn is_name_imported(&self, name: &str) -> bool {
        self.symbol_table_stack
            .first()
            .and_then(|table| table.symbols.get(name))
            .is_some_and(|sym| sym.flags.contains(SymbolFlags::DEF_IMPORT))
    }

    /// Get the cell-relative index of a free variable.
    /// Returns ncells + freevar_idx. Fixed up to localsplus index during finalize.
    fn get_free_var_index(&mut self, name: &str) -> oparg::VarNum {
        let info = self.code_stack.last_mut().unwrap();
        let idx = info
            .metadata
            .freevars
            .get_index_of(name)
            .unwrap_or_else(|| info.metadata.freevars.insert_full(name.to_owned()).0);
        (idx + info.metadata.cellvars.len()).to_u32().into()
    }

    /// Get the cell-relative index of a cell variable.
    /// Returns cellvar_idx. Fixed up to localsplus index during finalize.
    fn get_cell_var_index(&mut self, name: &str) -> oparg::VarNum {
        let info = self.code_stack.last_mut().unwrap();
        let idx = info
            .metadata
            .cellvars
            .get_index_of(name)
            .unwrap_or_else(|| info.metadata.cellvars.insert_full(name.to_owned()).0);
        idx.to_u32().into()
    }

    /// Get the index of a local variable.
    fn get_local_var_index(&mut self, name: &str) -> oparg::VarNum {
        let info = self.code_stack.last_mut().unwrap();
        let idx = info
            .metadata
            .varnames
            .get_index_of(name)
            .unwrap_or_else(|| info.metadata.varnames.insert_full(name.to_owned()).0);
        idx.to_u32().into()
    }

    /// Get the index of a global name.
    fn get_global_name_index(&mut self, name: &str) -> u32 {
        let info = self.code_stack.last_mut().unwrap();
        let idx = info
            .metadata
            .names
            .get_index_of(name)
            .unwrap_or_else(|| info.metadata.names.insert_full(name.to_owned()).0);
        idx.to_u32()
    }

    /// Push the next symbol table on to the stack
    fn push_symbol_table(&mut self) -> CompileResult<&SymbolTable> {
        // Look up the next table contained in the scope of the current table
        let current_table = self
            .symbol_table_stack
            .last_mut()
            .expect("no current symbol table");

        if current_table.next_sub_table >= current_table.sub_tables.len() {
            let name = current_table.name.clone();
            let typ = current_table.typ;
            return Err(self.error(CodegenErrorType::SyntaxError(format!(
                "no symbol table available in {name} (type: {typ:?})"
            ))));
        }

        while current_table.next_sub_table < current_table.sub_tables.len()
            && current_table.sub_tables[current_table.next_sub_table].typ
                == CompilerScope::Annotation
        {
            current_table.next_sub_table += 1;
        }
        if current_table.next_sub_table >= current_table.sub_tables.len() {
            let name = current_table.name.clone();
            let typ = current_table.typ;
            return Err(self.error(CodegenErrorType::SyntaxError(format!(
                "no symbol table available in {name} (type: {typ:?})"
            ))));
        }

        let idx = current_table.next_sub_table;
        current_table.next_sub_table += 1;
        let table = current_table.sub_tables[idx].clone();

        // Push the next table onto the stack
        self.symbol_table_stack.push(table);
        Ok(self.current_symbol_table())
    }

    fn push_symbol_table_matching(
        &mut self,
        typ: CompilerScope,
        table_name: &str,
    ) -> CompileResult<&SymbolTable> {
        let current_table = self
            .symbol_table_stack
            .last_mut()
            .expect("no current symbol table");

        while current_table.next_sub_table < current_table.sub_tables.len()
            && current_table.sub_tables[current_table.next_sub_table].typ
                == CompilerScope::Annotation
        {
            current_table.next_sub_table += 1;
        }

        let start = current_table.next_sub_table;
        let Some(idx) = current_table.sub_tables[start..]
            .iter()
            .position(|table| table.typ == typ && table.name == table_name)
            .map(|idx| start + idx)
        else {
            let name = current_table.name.clone();
            let current_typ = current_table.typ;
            return Err(self.error(CodegenErrorType::SyntaxError(format!(
                "no matching symbol table {table_name} ({typ:?}) available in {name} (type: {current_typ:?})"
            ))));
        };

        let table = current_table.sub_tables[idx].clone();
        current_table.next_sub_table = idx + 1;
        self.symbol_table_stack.push(table);
        Ok(self.current_symbol_table())
    }

    /// Push the function annotation symbol table.
    /// Signature annotation blocks are stored in st_blocks keyed by the
    /// arguments AST node. Without future annotations they are also children;
    /// with future annotations they are hidden from children and consumed here.
    fn push_annotation_symbol_table(&mut self) -> bool {
        let Some(annotation_table) = ({
            let current_table = self
                .symbol_table_stack
                .last_mut()
                .expect("no current symbol table");

            let next_idx = current_table.next_sub_table;
            if next_idx < current_table.sub_tables.len()
                && current_table.sub_tables[next_idx].typ == CompilerScope::Annotation
            {
                let next_table = current_table.sub_tables[next_idx].clone();
                current_table.next_sub_table += 1;
                Some(next_table)
            } else if current_table.next_hidden_annotation_block
                < current_table.hidden_annotation_blocks.len()
            {
                let idx = current_table.next_hidden_annotation_block;
                current_table.next_hidden_annotation_block += 1;
                Some(current_table.hidden_annotation_blocks[idx].clone())
            } else {
                None
            }
        }) else {
            return false;
        };

        self.symbol_table_stack.push(annotation_table);
        true
    }

    fn next_function_annotation_symbol_table_uses_annotations(&self) -> bool {
        let current_table = self
            .symbol_table_stack
            .last()
            .expect("no current symbol table");
        let next_idx = current_table.next_sub_table;
        if next_idx < current_table.sub_tables.len()
            && current_table.sub_tables[next_idx].typ == CompilerScope::Annotation
        {
            return current_table.sub_tables[next_idx].annotations_used;
        }

        let hidden_idx = current_table.next_hidden_annotation_block;
        current_table
            .hidden_annotation_blocks
            .get(hidden_idx)
            .is_some_and(|table| table.annotations_used)
    }

    /// Push the annotation symbol table for module/class level annotations
    /// This takes annotation_block from the current symbol table (not sub_tables)
    fn push_current_annotation_symbol_table(&mut self) -> bool {
        let current_table = self
            .symbol_table_stack
            .last_mut()
            .expect("no current symbol table");

        // For modules/classes, annotation_block is directly in the current table
        if let Some(annotation_block) = current_table.annotation_block.take() {
            self.symbol_table_stack.push(*annotation_block);
            true
        } else {
            false
        }
    }

    /// Pop the annotation symbol table.
    fn pop_annotation_symbol_table(&mut self) {
        self.symbol_table_stack.pop().expect("compiler bug");
    }

    /// Pop the current symbol table off the stack
    fn pop_symbol_table(&mut self) -> SymbolTable {
        self.symbol_table_stack.pop().expect("compiler bug")
    }

    /// Check if a super() call can be optimized
    /// Returns Some(SuperCallType) if optimization is possible, None otherwise
    fn can_optimize_super_call<'a>(
        &self,
        value: &'a ast::Expr,
        attr: &str,
    ) -> Option<SuperCallType<'a>> {
        // 1. value must be a Call expression
        let ast::Expr::Call(ast::ExprCall {
            func, arguments, ..
        }) = value
        else {
            return None;
        };

        // 2. func must be Name("super")
        let ast::Expr::Name(ast::ExprName { id, .. }) = func.as_ref() else {
            return None;
        };
        if id.as_str() != "super" {
            return None;
        }

        // 3. attr must not be "__class__"
        if attr == "__class__" {
            return None;
        }

        // 4. No keyword arguments
        if !arguments.keywords.is_empty() {
            return None;
        }

        // 5. "super" must be GlobalImplicit in the current scope.
        let table = self.current_symbol_table();
        if let Some(symbol) = table.lookup(&"super".into())
            && symbol.scope != SymbolScope::GlobalImplicit
        {
            return None;
        }
        // Then check the top-level scope and reject any statically
        // visible symbol for "super", not just local bindings.
        if let Some(top_table) = self.symbol_table_stack.first()
            && top_table.lookup(&"super".into()).is_some()
        {
            return None;
        }

        // 6. Check argument pattern
        let args = &arguments.args;

        // No starred expressions allowed
        if args.iter().any(|arg| matches!(arg, ast::Expr::Starred(_))) {
            return None;
        }

        match args.len() {
            2 => {
                // 2-arg: super(class, self)
                Some(SuperCallType::TwoArg {
                    class_arg: &args[0],
                    self_arg: &args[1],
                })
            }
            0 => {
                // 0-arg: super() - need __class__ cell and first parameter
                // Enclosing function should have at least one positional argument
                let info = self.code_stack.last()?;
                if info.metadata.argcount == 0 && info.metadata.posonlyargcount == 0 {
                    return None;
                }

                // Check if __class__ is available as a cell/free variable
                // The scope must be Free (from enclosing class) or have DEF_FREE_CLASS flag
                {
                    let symbol = table.lookup(&"__class__".into())?;
                    if symbol.scope != SymbolScope::Free
                        && !symbol.flags.contains(SymbolFlags::DEF_FREE_CLASS)
                    {
                        return None;
                    }
                }

                Some(SuperCallType::ZeroArg)
            }
            _ => None, // 1 or 3+ args - not optimizable
        }
    }

    /// Load arguments for super() optimization onto the stack
    /// Stack result: [global_super, class, self]
    fn load_args_for_super(
        &mut self,
        super_type: &SuperCallType<'_>,
        super_name_range: TextRange,
        super_call_range: TextRange,
    ) -> CompileResult<()> {
        // 1. Load global super
        self.set_source_range(super_name_range);
        self.compile_name(&"super".into(), NameUsage::Load)?;

        match super_type {
            SuperCallType::TwoArg {
                class_arg,
                self_arg,
            } => {
                // 2-arg: load provided arguments
                self.compile_expression(class_arg)?;
                self.compile_expression(self_arg)?;
            }
            SuperCallType::ZeroArg => {
                // 0-arg: load __class__ cell and first parameter
                // Load __class__ from cell/free variable
                self.set_source_range(super_call_range);
                let scope = self
                    .get_ref_type(&"__class__".into())
                    .map_err(|e| self.error(e))?;
                let idx = match scope {
                    SymbolScope::Cell => self.get_cell_var_index("__class__"),
                    SymbolScope::Free => self.get_free_var_index("__class__"),
                    _ => {
                        return Err(self.error(CodegenErrorType::SyntaxError(
                            "super(): __class__ cell not found".to_owned(),
                        )));
                    }
                };
                emit!(self, Instruction::LoadDeref { i: idx });

                // Load first parameter (typically 'self').
                // Safety: can_optimize_super_call() ensures argcount > 0, and
                // parameters are always added to varnames first (see symboltable.rs).
                let first_param = {
                    let info = self.code_stack.last().unwrap();
                    info.metadata.varnames.first().cloned()
                };
                let first_param = first_param.ok_or_else(|| {
                    self.error(CodegenErrorType::SyntaxError(
                        "super(): no arguments and no first parameter".to_owned(),
                    ))
                })?;
                self.set_source_range(super_call_range);
                self.compile_name(&first_param.into(), NameUsage::Load)?;
            }
        }
        Ok(())
    }

    /// Check if this is an inlined comprehension context (PEP 709).
    /// Generator expressions are never inlined.
    fn is_inlined_comprehension_context(
        &self,
        comprehension_type: ComprehensionType,
        comp_table: &SymbolTable,
    ) -> bool {
        if comprehension_type == ComprehensionType::Generator {
            return false;
        }
        comp_table.comp_inlined
    }

    /// Enter a new scope
    // = compiler_enter_scope
    fn enter_scope(
        &mut self,
        name: &str,
        scope_type: CompilerScope,
        key: usize, // Symbol table stack index used like CPython's scope key.
        lineno: u32,
    ) -> CompileResult<()> {
        // Allocate a new compiler unit

        // In Rust, we'll create the structure directly
        let source_path = self.source_file.name().to_owned();

        // Lookup symbol table entry using key (_PySymtable_Lookup)
        let ste = match self.symbol_table_stack.get(key) {
            Some(v) => v,
            None => {
                return Err(self.error(CodegenErrorType::SyntaxError(
                    "unknown symbol table entry".to_owned(),
                )));
            }
        };

        // Use varnames from symbol table (already collected in definition order)
        let varname_cache: IndexSet<Name> = ste.varnames.iter().cloned().collect();
        let nparams = ste.varnames.len();

        // Build cellvars using dictbytype (CELL scope or COMP_CELL flag, sorted)
        let mut cellvar_cache = IndexSet::default();
        let mut cell_names: Vec<_> = ste
            .symbols
            .iter()
            .filter(|(_, s)| {
                s.scope == SymbolScope::Cell || s.flags.contains(SymbolFlags::DEF_COMP_CELL)
            })
            .map(|(name, _)| name.clone())
            .collect();
        cell_names.sort();
        for name in cell_names {
            cellvar_cache.insert(name.into());
        }

        // Handle implicit __class__ cell if needed
        if ste.needs_class_closure {
            // Cook up an implicit __class__ cell
            debug_assert_eq!(scope_type, CompilerScope::Class);
            cellvar_cache.insert("__class__".to_string());
        }

        // Handle implicit __classdict__ cell if needed
        if ste.needs_classdict {
            // Cook up an implicit __classdict__ cell
            debug_assert_eq!(scope_type, CompilerScope::Class);
            cellvar_cache.insert("__classdict__".to_string());
        }

        // Handle implicit __conditional_annotations__ cell if needed.
        if Self::scope_needs_conditional_annotations_cell(ste) {
            cellvar_cache.insert("__conditional_annotations__".to_string());
        }

        // Build freevars using dictbytype (FREE scope, offset by cellvars size)
        let mut freevar_cache = IndexSet::default();
        let annotation_free_names: IndexSet<Name> = ste
            .annotation_block
            .as_ref()
            .map(|annotation| {
                annotation
                    .symbols
                    .iter()
                    .filter(|(_, s)| s.scope == SymbolScope::Free)
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut free_names: Vec<_> = ste
            .symbols
            .iter()
            .filter(|(_, s)| {
                s.scope == SymbolScope::Free
                    || (scope_type != CompilerScope::Class
                        && s.flags.contains(SymbolFlags::DEF_FREE_CLASS))
                    || (scope_type == CompilerScope::Class
                        && s.flags.contains(SymbolFlags::DEF_FREE_CLASS)
                        && self.has_enclosing_non_module_code_scope())
            })
            .filter(|(name, symbol)| {
                if !matches!(
                    scope_type,
                    CompilerScope::Function | CompilerScope::AsyncFunction | CompilerScope::Lambda
                ) {
                    return true;
                }
                !(annotation_free_names.contains(*name) && symbol.flags.is_empty())
            })
            .map(|(name, _)| name.clone())
            .collect();
        free_names.sort();
        for name in free_names {
            freevar_cache.insert(name.into());
        }

        // The `__conditional_annotations__` cell an annotation scope reads is
        // cooked up here rather than carried by the symbol table, so it lands
        // after the names the symbol table did supply.
        if scope_type == CompilerScope::Annotation && ste.has_conditional_annotations {
            freevar_cache.insert("__conditional_annotations__".to_string());
        }

        // Initialize u_metadata fields
        let (mut flags, posonlyarg_count, arg_count, kwonlyarg_count) = match scope_type {
            CompilerScope::Module => (bytecode::CodeFlags::empty(), 0, 0, 0),
            CompilerScope::Class => (bytecode::CodeFlags::empty(), 0, 0, 0),
            CompilerScope::Function | CompilerScope::AsyncFunction | CompilerScope::Lambda => (
                bytecode::CodeFlags::NEWLOCALS | bytecode::CodeFlags::OPTIMIZED,
                0, // Will be set later in enter_function
                0, // Will be set later in enter_function
                0, // Will be set later in enter_function
            ),
            CompilerScope::Comprehension => (
                bytecode::CodeFlags::NEWLOCALS | bytecode::CodeFlags::OPTIMIZED,
                0,
                1, // comprehensions take one argument (.0)
                0,
            ),
            CompilerScope::TypeParams => (
                bytecode::CodeFlags::NEWLOCALS | bytecode::CodeFlags::OPTIMIZED,
                0,
                0,
                0,
            ),
            CompilerScope::Annotation => (
                bytecode::CodeFlags::NEWLOCALS | bytecode::CodeFlags::OPTIMIZED,
                1, // format is positional-only
                0,
                0,
            ),
            CompilerScope::TypeAlias | CompilerScope::TypeVariable => (
                bytecode::CodeFlags::NEWLOCALS | bytecode::CodeFlags::OPTIMIZED,
                1, // format is positional-only
                0,
                0,
            ),
        };

        if ste.is_method {
            flags |= bytecode::CodeFlags::METHOD;
        }

        // CPython sets CO_NESTED from symtable's ste_nested, not merely
        // from lexical depth: module-level class methods are CO_METHOD but
        // not CO_NESTED.
        let mut flags = if ste.is_nested
            && matches!(
                scope_type,
                CompilerScope::Function
                    | CompilerScope::AsyncFunction
                    | CompilerScope::Lambda
                    | CompilerScope::Comprehension
                    | CompilerScope::Annotation
                    | CompilerScope::TypeAlias
                    | CompilerScope::TypeVariable
                    | CompilerScope::TypeParams
            ) {
            flags | bytecode::CodeFlags::NESTED
        } else {
            flags
        };
        flags |= self.future_features;

        // Get private name from parent scope
        let private = if !self.code_stack.is_empty() {
            self.code_stack.last().unwrap().private.clone()
        } else {
            None
        };

        // Create the new compilation unit
        let code_info = ir::CodeInfo {
            flags,
            source_path,
            private,
            blocks: Blocks::from([Block::default()]),
            current_block: BlockIdx::new(0),
            instr_sequence: ir::InstructionSequence::new(),
            instr_sequence_label_map: ir::InstructionSequenceLabelMap::new(),
            annotations_instr_sequence: None,
            metadata: ir::CodeUnitMetadata {
                name: name.to_owned(),
                qualname: None, // Will be set below
                consts: Default::default(),
                names: IndexSet::default(),
                varnames: varname_cache.into_iter().map(Into::into).collect(),
                cellvars: cellvar_cache,
                freevars: freevar_cache,
                fast_hidden: IndexMap::default(),
                fast_hidden_final: IndexSet::default(),
                argcount: arg_count,
                posonlyargcount: posonlyarg_count,
                kwonlyargcount: kwonlyarg_count,
                firstlineno: OneIndexed::new(lineno as usize).unwrap_or(OneIndexed::MIN),
            },
            static_attributes: if scope_type == CompilerScope::Class {
                Some(IndexSet::default())
            } else {
                None
            },
            in_inlined_comp: false,
            fblock: Vec::with_capacity(CO_MAXBLOCKS),
            symbol_table_index: key,
            nparams,
            in_conditional_block: 0,
            next_conditional_annotation_index: 0,
        };

        // Push the old compiler unit on the stack (like PyCapsule)
        // This happens before setting qualname
        self.code_stack.push(code_info);

        // Set qualname after pushing (uses compiler_set_qualname logic)
        if scope_type != CompilerScope::Module {
            self.set_qualname();
        }

        // Emit RESUME (handles async preamble and module lineno 0)
        // CPython: LOCATION(lineno, lineno, 0, 0), then loc.lineno = 0 for module
        self.emit_resume_for_scope(scope_type, lineno);

        Ok(())
    }

    /// Emit RESUME instruction with proper handling for module lineno.
    /// codegen_enter_scope equivalent for RESUME emission.
    fn emit_resume_for_scope(&mut self, scope_type: CompilerScope, lineno: u32) {
        // CPython: LOCATION(lineno, lineno, 0, 0)
        // Module scope: loc.lineno = 0 (before the first line)
        let lineno_override = if scope_type == CompilerScope::Module {
            Some(0)
        } else {
            None
        };

        // Use lineno for location (col = 0 as in CPython)
        let location = SourceLocation {
            line: OneIndexed::new(lineno as usize).unwrap_or(OneIndexed::MIN),
            character_offset: OneIndexed::MIN, // col = 0
        };
        let end_location = location; // end_lineno = lineno, end_col = 0
        let except_handler = None;

        self.cpython_cfg_builder_addop(ir::InstructionInfo {
            instr: Instruction::Resume {
                context: OpArgMarker::marker(),
            }
            .into(),
            arg: OpArg::new(oparg::ResumeLocation::AtFuncStart.into()),
            target: BlockIdx::NULL,
            location,
            end_location,
            except_handler,
            lineno_override,
        });
    }

    fn push_output(
        &mut self,
        flags: bytecode::CodeFlags,
        posonlyarg_count: u32,
        arg_count: u32,
        kwonlyarg_count: u32,
        obj_name: &str,
    ) -> CompileResult<()> {
        // First push the symbol table
        let table = self.push_symbol_table()?;
        let scope_type = table.typ;

        // The key is the current position in the symbol table stack
        let key = self.symbol_table_stack.len() - 1;

        // Get the line number
        let lineno = self.get_source_line_number().get();

        // Call enter_scope which does most of the work
        self.enter_scope(obj_name, scope_type, key, lineno.to_u32())?;

        // Override the values that push_output sets explicitly
        // enter_scope sets default values based on scope_type, but push_output
        // allows callers to specify exact values
        if let Some(info) = self.code_stack.last_mut() {
            // Preserve flags computed from the symbol-table context.
            info.flags = flags
                | (info.flags
                    & (bytecode::CodeFlags::NESTED
                        | bytecode::CodeFlags::METHOD
                        | bytecode::CodeFlags::FUTURE_DIVISION
                        | bytecode::CodeFlags::FUTURE_ABSOLUTE_IMPORT
                        | bytecode::CodeFlags::FUTURE_WITH_STATEMENT
                        | bytecode::CodeFlags::FUTURE_PRINT_FUNCTION
                        | bytecode::CodeFlags::FUTURE_UNICODE_LITERALS
                        | bytecode::CodeFlags::FUTURE_BARRY_AS_BDFL
                        | bytecode::CodeFlags::FUTURE_GENERATOR_STOP
                        | bytecode::CodeFlags::FUTURE_ANNOTATIONS));
            info.metadata.argcount = arg_count;
            info.metadata.posonlyargcount = posonlyarg_count;
            info.metadata.kwonlyargcount = kwonlyarg_count;
        }
        Ok(())
    }

    // compiler_exit_scope
    fn exit_scope(&mut self) -> CodeObject {
        self.pop_symbol_table();
        // Various scopes can have sub_tables:
        // - ast::TypeParams scope can have sub_tables (the function body's symbol table)
        // - Module scope can have sub_tables (for TypeAlias scopes, nested functions, classes)
        // - Function scope can have sub_tables (for nested functions, classes)
        // - Class scope can have sub_tables (for nested classes, methods)

        let pop = self.code_stack.pop();
        let stack_top = compiler_unwrap_option(self, pop);
        let nested = self
            .save_nested_seqs
            .then(|| stack_top.instr_sequence.clone());
        let code = unwrap_internal(self, stack_top.finalize_code(&self.opts));
        if let (Some(nested), Some(parent)) = (nested, self.code_stack.last_mut()) {
            parent.instr_sequence.add_nested(nested);
        }
        code
    }

    fn expose_annotation_format_parameter(code: &mut CodeObject) {
        if let Some(first) = code.varnames.first_mut() {
            *first = String::from("format");
        }
    }

    fn configure_annotation_format_parameter(&mut self) {
        let info = self.current_code_info();
        info.metadata.varnames.insert(".format".to_owned());
        info.nparams = 1;
    }

    /// Exit a function signature annotation scope.
    fn exit_annotation_scope(&mut self, saved_ctx: CompileContext) -> CodeObject {
        self.pop_annotation_symbol_table();
        self.ctx = saved_ctx;
        let pop = self.code_stack.pop();
        let stack_top = compiler_unwrap_option(self, pop);
        let nested = self
            .save_nested_seqs
            .then(|| stack_top.instr_sequence.clone());
        let mut code = unwrap_internal(self, stack_top.finalize_code(&self.opts));
        if let (Some(nested), Some(parent)) = (nested, self.code_stack.last_mut()) {
            parent.instr_sequence.add_nested(nested);
        }
        Self::expose_annotation_format_parameter(&mut code);
        code
    }

    /// Enter a function signature annotation scope.
    /// Returns None if no matching annotation symbol table exists.
    /// On success, returns the saved CompileContext to pass to exit_annotation_scope.
    fn enter_annotation_scope(
        &mut self,
        func_name: &str,
        loc: TextRange,
    ) -> CompileResult<Option<CompileContext>> {
        if !self.push_annotation_symbol_table() {
            return Ok(None);
        }

        // Annotation scopes are never async (even inside async functions)
        let saved_ctx = self.ctx;
        self.ctx = CompileContext {
            in_class: saved_ctx.in_class,
            func: FunctionContext::Function,
            in_async_scope: false,
        };

        self.set_source_range(loc);
        let key = self.symbol_table_stack.len() - 1;
        let lineno = self.get_source_line_number().get();
        self.enter_scope(
            "__annotate__",
            CompilerScope::Annotation,
            key,
            lineno.to_u32(),
        )?;

        // enter_scope() qualified the scope by the enclosing scope only; redo it
        // now that the annotated function is known. Only signature annotations
        // get this treatment - deferred class and module annotations are
        // compiled inside the scope they belong to and are already qualified.
        self.set_annotation_qualname(func_name);

        // Keep the internal ".format" name; exit_annotation_scope()
        // renames it to "format" on the final code object.
        self.configure_annotation_format_parameter();

        // Emit format validation: if format > VALUE_WITH_FAKE_GLOBALS: raise NotImplementedError
        // VALUE_WITH_FAKE_GLOBALS = 2 (from annotationlib.Format)
        self.emit_format_validation();

        Ok(Some(saved_ctx))
    }

    /// Emit format parameter validation for annotation scope
    /// if format > VALUE_WITH_FAKE_GLOBALS (2): raise NotImplementedError
    fn emit_format_validation(&mut self) {
        // Load format parameter (first local variable, index 0)
        emit!(
            self,
            Instruction::LoadFast {
                var_num: oparg::VarNum::from_u32(0)
            }
        );

        // Load VALUE_WITH_FAKE_GLOBALS constant (2)
        self.emit_load_const(ConstantData::Integer { value: 2.into() });

        // Compare: format > 2
        emit!(
            self,
            Instruction::CompareOp {
                opname: ComparisonOperator::Greater
            }
        );

        // Jump to body if format <= 2 (comparison is false)
        let body_block = self.new_block();
        emit!(self, Instruction::PopJumpIfFalse { delta: body_block });

        // Raise NotImplementedError
        emit!(
            self,
            Instruction::LoadCommonConstant {
                idx: bytecode::CommonConstant::NotImplementedError
            }
        );
        emit!(
            self,
            Instruction::RaiseVarargs {
                argc: bytecode::RaiseKind::Raise
            }
        );

        // Body label - continue with annotation evaluation
        self.use_cpython_label_block(body_block);
    }

    /// CPython `_PyCompile_PushFBlock()`: store the active label targets on the
    /// fblock stack.
    fn push_fblock_labels(
        &mut self,
        fb_type: FBlockType,
        fb_block: ir::InstructionSequenceLabel,
        fb_exit: ir::InstructionSequenceLabel,
        fb_datum: FBlockDatum,
    ) -> CompileResult<()> {
        let fb_range = self.current_source_range;
        if self.current_code_info().fblock.len() >= CO_MAXBLOCKS {
            return Err(self.error(CodegenErrorType::SyntaxError(
                "too many statically nested blocks".to_owned(),
            )));
        }
        if matches!(fb_type, FBlockType::FinallyEnd) {
            self.disable_warning += 1;
        }
        let code = self.current_code_info();
        code.fblock.push(FBlockInfo {
            fb_type,
            fb_block,
            fb_exit,
            fb_range,
            fb_datum,
        });
        Ok(())
    }

    /// CPython `_PyCompile_PopFBlock()`: assert the popped type and label.
    fn pop_fblock_label(
        &mut self,
        expected_type: FBlockType,
        expected_block: ir::InstructionSequenceLabel,
    ) -> FBlockInfo {
        let fblock = {
            let code = self.current_code_info();
            code.fblock.pop().expect("fblock stack underflow")
        };
        debug_assert_eq!(fblock.fb_type, expected_type);
        debug_assert_eq!(
            fblock.fb_block, expected_block,
            "CPython _PyCompile_PopFBlock asserts the popped fb_block label"
        );
        if matches!(expected_type, FBlockType::FinallyEnd) {
            self.disable_warning -= 1;
        }
        fblock
    }

    /// `_PyCompile_PushFBlock()` call used by
    /// `codegen_unwind_fblock_stack()` to restore the copied fblock after
    /// recursive unwinding.
    fn restore_fblock_info(&mut self, fblock: FBlockInfo) -> CompileResult<()> {
        let FBlockInfo {
            fb_type,
            fb_block,
            fb_exit,
            fb_range,
            fb_datum,
        } = fblock;
        let code = self.current_code_info();
        if code.fblock.len() >= CO_MAXBLOCKS {
            return Err(self.error_ranged(
                CodegenErrorType::SyntaxError("too many statically nested blocks".to_owned()),
                fb_range,
            ));
        }
        code.fblock.push(FBlockInfo {
            fb_type,
            fb_block,
            fb_exit,
            fb_range,
            fb_datum,
        });
        Ok(())
    }

    fn set_unwind_source_range(&mut self, loc: Option<TextRange>) {
        if let Some(range) = loc {
            self.set_source_range(range);
        }
    }

    fn mark_unwind_no_location(&mut self, loc: Option<TextRange>) {
        if loc.is_none() {
            self.set_no_location();
        }
    }

    /// Unwind a single fblock, emitting cleanup code
    /// preserve_tos: if true, preserve the top of stack (e.g., return value)
    fn unwind_fblock(
        &mut self,
        info: &FBlockInfo,
        preserve_tos: bool,
        loc: &mut Option<TextRange>,
    ) -> CompileResult<()> {
        match info.fb_type {
            FBlockType::WhileLoop
            | FBlockType::ExceptionHandler
            | FBlockType::ExceptionGroupHandler
            | FBlockType::AsyncComprehensionGenerator
            | FBlockType::StopIteration => {
                // No cleanup needed
            }

            FBlockType::ForLoop => {
                // When returning from a for-loop, CPython swaps the preserved
                // value with the iterator and uses POP_TOP for loop cleanup.
                if preserve_tos {
                    self.set_unwind_source_range(*loc);
                    emit!(self, Instruction::Swap { i: 2 });
                    self.mark_unwind_no_location(*loc);
                }
                self.set_unwind_source_range(*loc);
                emit!(self, Instruction::PopTop);
                self.mark_unwind_no_location(*loc);
            }

            FBlockType::TryExcept => {
                self.set_unwind_source_range(*loc);
                emit!(self, PseudoInstruction::PopBlock);
                self.mark_unwind_no_location(*loc);
            }

            FBlockType::FinallyTry => {
                // codegen_unwind_fblock(FINALLY_TRY)
                self.set_unwind_source_range(*loc);
                emit!(self, PseudoInstruction::PopBlock);
                self.mark_unwind_no_location(*loc);

                if preserve_tos {
                    self.push_fblock_labels(
                        FBlockType::PopValue,
                        ir::InstructionSequenceLabel::NO_LABEL,
                        ir::InstructionSequenceLabel::NO_LABEL,
                        FBlockDatum::None,
                    )?;
                }

                if let FBlockDatum::FinallyBody(ref body) = info.fb_datum {
                    // This is an extra copy of the finally body, emitted for the
                    // path that leaves the try block early. Nested scopes are
                    // handed out by position, and the cursors are still inside
                    // the try block's own run of scopes here, so seek them to the
                    // ones this body opened before compiling it. The try
                    // statement emits its own copies from the same place
                    // afterwards, so put the cursors back when the copy is done.
                    let symbol_table_cursors = self.current_symbol_table_cursors();
                    if let Some(first) = body.first() {
                        let line = self
                            .source_file
                            .to_source_code()
                            .line_index(first.range().start())
                            .get()
                            .to_u32();
                        self.seek_symbol_table_cursors_to_line(line);
                    }
                    self.compile_statements(body)?;
                    self.set_symbol_table_cursors(symbol_table_cursors);
                }

                if preserve_tos {
                    self.pop_fblock_label(
                        FBlockType::PopValue,
                        ir::InstructionSequenceLabel::NO_LABEL,
                    );
                }

                *loc = None;
            }

            FBlockType::FinallyEnd => {
                // codegen_unwind_fblock(FINALLY_END)
                if preserve_tos {
                    self.set_unwind_source_range(*loc);
                    emit!(self, Instruction::Swap { i: 2 });
                    self.mark_unwind_no_location(*loc);
                }
                self.set_unwind_source_range(*loc);
                emit!(self, Instruction::PopTop); // exc_value
                self.mark_unwind_no_location(*loc);
                if preserve_tos {
                    self.set_unwind_source_range(*loc);
                    emit!(self, Instruction::Swap { i: 2 });
                    self.mark_unwind_no_location(*loc);
                }
                self.set_unwind_source_range(*loc);
                emit!(self, PseudoInstruction::PopBlock);
                self.mark_unwind_no_location(*loc);
                self.set_unwind_source_range(*loc);
                emit!(self, Instruction::PopExcept);
                self.mark_unwind_no_location(*loc);
            }

            FBlockType::With | FBlockType::AsyncWith => {
                // Stack: [..., exit_func, self_exit, return_value (if preserve_tos)]
                // CPython codegen_unwind_fblock() assigns *ploc = info->fb_loc
                // for WITH/ASYNC_WITH cleanup and then makes following unwind
                // instructions artificial with *ploc = NO_LOCATION.
                *loc = Some(info.fb_range);
                self.set_unwind_source_range(*loc);
                emit!(self, PseudoInstruction::PopBlock);

                if preserve_tos {
                    // Rotate return value below the exit pair
                    // [exit_func, self_exit, value] → [value, exit_func, self_exit]
                    self.set_unwind_source_range(*loc);
                    emit!(self, Instruction::Swap { i: 3 }); // [value, self_exit, exit_func]
                    self.set_unwind_source_range(*loc);
                    emit!(self, Instruction::Swap { i: 2 }); // [value, exit_func, self_exit]
                }

                self.set_unwind_source_range(*loc);
                self.compile_call_exit_with_nones();

                // For async with, await the result
                if matches!(info.fb_type, FBlockType::AsyncWith) {
                    self.set_unwind_source_range(*loc);
                    emit!(self, Instruction::GetAwaitable { r#where: 2 });
                    self.set_unwind_source_range(*loc);
                    self.emit_load_const(ConstantData::None);
                    let _ = self.compile_yield_from_sequence(true);
                }

                // Pop the __exit__ result
                self.set_unwind_source_range(*loc);
                emit!(self, Instruction::PopTop);
                *loc = None;
            }

            FBlockType::HandlerCleanup => {
                // codegen_unwind_fblock(HANDLER_CLEANUP)
                if let FBlockDatum::ExceptionName(_) = info.fb_datum {
                    // Named handler: PopBlock for inner SETUP_CLEANUP
                    self.set_unwind_source_range(*loc);
                    emit!(self, PseudoInstruction::PopBlock);
                    self.mark_unwind_no_location(*loc);
                }
                if preserve_tos {
                    self.set_unwind_source_range(*loc);
                    emit!(self, Instruction::Swap { i: 2 });
                    self.mark_unwind_no_location(*loc);
                }
                // PopBlock for outer SETUP_CLEANUP (ExceptionHandler)
                self.set_unwind_source_range(*loc);
                emit!(self, PseudoInstruction::PopBlock);
                self.mark_unwind_no_location(*loc);
                self.set_unwind_source_range(*loc);
                emit!(self, Instruction::PopExcept);
                self.mark_unwind_no_location(*loc);

                // If there's an exception name, clean it up
                if let FBlockDatum::ExceptionName(ref name) = info.fb_datum {
                    self.set_unwind_source_range(*loc);
                    self.emit_load_const(ConstantData::None);
                    self.mark_unwind_no_location(*loc);
                    self.set_unwind_source_range(*loc);
                    self.store_name(&name.into())?;
                    self.mark_unwind_no_location(*loc);
                    self.set_unwind_source_range(*loc);
                    self.compile_name(&name.into(), NameUsage::Delete)?;
                    self.mark_unwind_no_location(*loc);
                }
            }

            FBlockType::PopValue => {
                if preserve_tos {
                    self.set_unwind_source_range(*loc);
                    emit!(self, Instruction::Swap { i: 2 });
                    self.mark_unwind_no_location(*loc);
                }
                self.set_unwind_source_range(*loc);
                emit!(self, Instruction::PopTop);
                self.mark_unwind_no_location(*loc);
            }
        }
        Ok(())
    }

    /// Unwind the fblock stack, emitting cleanup code for each block
    /// preserve_tos: if true, preserve the top of stack (e.g., return value)
    /// stop_at_loop: if true, stop when encountering a loop (for break/continue)
    fn unwind_fblock_stack(
        &mut self,
        preserve_tos: bool,
        stop_at_loop: bool,
    ) -> CompileResult<Option<TextRange>> {
        let (unwind_loc, _loop_fblock) =
            self.unwind_fblock_stack_with_loop(preserve_tos, stop_at_loop)?;
        Ok(unwind_loc)
    }

    /// CPython `codegen_unwind_fblock_stack()`: unwind frame blocks and, when
    /// requested by break/continue codegen, return the first loop fblock instead
    /// of unwinding it.
    fn unwind_fblock_stack_with_loop(
        &mut self,
        preserve_tos: bool,
        stop_at_loop: bool,
    ) -> CompileResult<(Option<TextRange>, Option<FBlockInfo>)> {
        let mut unwind_loc = Some(self.current_source_range);
        let loop_fblock =
            self.unwind_fblock_stack_inner(preserve_tos, stop_at_loop, &mut unwind_loc)?;
        Ok((unwind_loc, loop_fblock))
    }

    fn unwind_fblock_stack_inner(
        &mut self,
        preserve_tos: bool,
        stop_at_loop: bool,
        unwind_loc: &mut Option<TextRange>,
    ) -> CompileResult<Option<FBlockInfo>> {
        let Some(top) = self.current_code_info().fblock.last().cloned() else {
            return Ok(None);
        };
        if matches!(top.fb_type, FBlockType::ExceptionGroupHandler) {
            return Err(self.error_optional_range(
                CodegenErrorType::BreakContinueReturnInExceptStar,
                *unwind_loc,
            ));
        }
        if stop_at_loop && matches!(top.fb_type, FBlockType::WhileLoop | FBlockType::ForLoop) {
            return Ok(Some(top));
        }

        let copy = self
            .current_code_info()
            .fblock
            .pop()
            .expect("fblock stack underflow");
        self.unwind_fblock(&copy, preserve_tos, unwind_loc)?;
        let loop_fblock = self.unwind_fblock_stack_inner(preserve_tos, stop_at_loop, unwind_loc)?;
        self.restore_fblock_info(copy)?;
        Ok(loop_fblock)
    }

    // could take impl Into<Cow<str>>, but everything is borrowed from ast structs; we never
    // actually have a `String` to pass
    fn name(&mut self, name: &str) -> bytecode::NameIdx {
        self._name_inner(name, |i| &mut i.metadata.names)
    }

    fn varname(&mut self, name: &str) -> oparg::VarNum {
        // NOTE: __debug__ checks are now handled in symboltable phase
        oparg::VarNum::from_u32(self._name_inner(name, |i| &mut i.metadata.varnames))
    }

    fn _name_inner(
        &mut self,
        name: &str,
        cache: impl FnOnce(&mut ir::CodeInfo) -> &mut IndexSet<String>,
    ) -> u32 {
        let target = name.into();
        let name = self.mangle(&target);
        let cache = cache(self.current_code_info());
        cache
            .get_index_of(name.as_str())
            .unwrap_or_else(|| cache.insert_full(name.to_string()).0)
            .to_u32()
    }

    /// Set the qualified name for the current code object
    // = compiler_set_qualname
    fn set_qualname(&mut self) -> String {
        self.set_qualname_for_function(None)
    }

    /// Set the qualname of an annotation scope, qualified by the function whose
    /// signature it annotates. The annotation block's symbol table entry records
    /// that name (`ste_function_name`) and folds it into the qualname, so `f`'s
    /// annotation scope is named `f.__annotate__`.
    fn set_annotation_qualname(&mut self, function_name: &str) {
        self.set_qualname_for_function(Some(function_name));
    }

    fn set_qualname_for_function(&mut self, function_name: Option<&str>) -> String {
        let qualname = self.make_qualname(function_name);
        self.current_code_info().metadata.qualname = Some(qualname.clone());
        qualname
    }

    fn make_qualname(&mut self, function_name: Option<&str>) -> String {
        let stack_size = self.code_stack.len();
        assert!(stack_size >= 1);

        let current_obj_name = self.current_code_info().metadata.name.clone();

        // If we're at the module level (stack_size == 1), qualname is just the name
        if stack_size <= 1 {
            return current_obj_name;
        }

        // Check parent scope
        let mut parent_idx = stack_size - 2;
        let mut parent = &self.code_stack[parent_idx];

        let mut parent_scope = self
            .symbol_table_stack
            .get(parent_idx)
            .map(|table| table.typ);

        // CPython skips both generic-parameter scopes and annotation scopes
        // when building qualnames for the contained function/class code object.
        if matches!(
            parent_scope,
            Some(
                CompilerScope::TypeParams
                    | CompilerScope::Annotation
                    | CompilerScope::TypeAlias
                    | CompilerScope::TypeVariable,
            )
        ) || parent.metadata.name.starts_with("<generic parameters of ")
        {
            if stack_size == 2 {
                // If we're immediately within the module, qualname is just the name
                return current_obj_name;
            }
            // Use grandparent
            parent_idx = stack_size - 3;
            parent = &self.code_stack[parent_idx];
            parent_scope = self
                .symbol_table_stack
                .get(parent_idx)
                .map(|table| table.typ);
        }

        // Check if this is a global class/function.
        // CPython compiler_set_qualname() only applies this GLOBAL_EXPLICIT
        // shortcut to function, async-function, and class scopes. Annotation
        // scopes, including type-alias value scopes, still inherit the parent
        // function's .<locals> qualname.
        let mut force_global = false;
        let current_scope = self
            .code_stack
            .last()
            .map(|code| self.symbol_table_stack[code.symbol_table_index].typ);
        if matches!(
            current_scope,
            Some(CompilerScope::Function | CompilerScope::AsyncFunction | CompilerScope::Class)
        ) {
            if stack_size > self.symbol_table_stack.len() {
                // We might be in a situation where symbol table isn't pushed yet
                // In this case, check the parent symbol table
                if let Some(parent_table) = self.symbol_table_stack.last()
                    && let Some(symbol) = parent_table.lookup(&current_obj_name.clone().into())
                    && symbol.scope == SymbolScope::GlobalExplicit
                {
                    force_global = true;
                }
            } else if let Some(_current_table) = self.symbol_table_stack.last() {
                // Mangle the name if necessary (for private names in classes)
                let target = &current_obj_name.clone().into();
                let mangled_name = self.mangle(target);

                // Look up in parent symbol table to check scope
                if self.symbol_table_stack.len() >= 2 {
                    let parent_table = &self.symbol_table_stack[self.symbol_table_stack.len() - 2];
                    if let Some(symbol) = parent_table.lookup(&mangled_name)
                        && symbol.scope == SymbolScope::GlobalExplicit
                    {
                        force_global = true;
                    }
                }
            }
        }

        // Build the prefix the current name is qualified by, if any
        let base = if force_global {
            // For global symbols, qualname is just the name
            None
        } else {
            // Check parent scope type
            let parent_obj_name = &parent.metadata.name;

            // Determine if parent is a function-like scope
            let is_function_parent = matches!(
                parent_scope,
                Some(
                    CompilerScope::Function | CompilerScope::AsyncFunction | CompilerScope::Lambda
                )
            );

            // Use parent's qualname if available, otherwise use parent_obj_name
            let parent_qualname = parent.metadata.qualname.as_ref().unwrap_or(parent_obj_name);

            if is_function_parent {
                // For functions, append .<locals> to parent qualname
                Some(format!("{parent_qualname}.<locals>"))
            } else if parent_qualname == "<module>" {
                // Module level, nothing to qualify by
                None
            } else {
                // For classes and other scopes, use parent's qualname directly
                Some(parent_qualname.clone())
            }
        };

        // An annotation scope is compiled in the scope enclosing the function it
        // annotates, so the function itself is missing from the prefix above.
        let base = match (base, function_name) {
            (Some(base), Some(function_name)) => Some(format!("{base}.{function_name}")),
            (None, Some(function_name)) => Some(function_name.to_owned()),
            (base, None) => base,
        };

        match base {
            Some(base) => format!("{base}.{current_obj_name}"),
            None => current_obj_name,
        }
    }

    fn compile_program(
        &mut self,
        body: &ast::ModModule,
        symbol_table: SymbolTable,
    ) -> CompileResult<()> {
        let size_before = self.code_stack.len();
        // Set future_annotations from symbol table (detected during symbol table scan)
        self.future_annotations = symbol_table.future_annotations;
        let future_features = self.future_features;
        self.current_code_info().flags |= future_features;
        if self.future_annotations {
            self.future_features
                .insert(bytecode::CodeFlags::FUTURE_ANNOTATIONS);
            self.current_code_info()
                .flags
                .insert(bytecode::CodeFlags::FUTURE_ANNOTATIONS);
        }
        if symbol_table.is_coroutine {
            self.current_code_info()
                .flags
                .insert(bytecode::CodeFlags::COROUTINE);
        }

        // Module-level __conditional_annotations__ cell
        let has_module_cond_ann = Self::scope_needs_conditional_annotations_cell(&symbol_table);
        if has_module_cond_ann {
            self.current_code_info()
                .metadata
                .cellvars
                .insert("__conditional_annotations__".to_string());
        }

        self.symbol_table_stack.push(symbol_table);

        self.emit_resume_for_scope(CompilerScope::Module, 1);
        emit!(self, PseudoInstruction::AnnotationsPlaceholder);

        let (doc, statements) = split_doc_with_range(&body.body, &self.opts);
        let module_start_loc = self.module_start_location(&body.body);
        let annotations_used = self.current_symbol_table().annotations_used;
        // Handle annotation bookkeeping before the docstring assignment, as
        // codegen_body() does after _PyCodegen_Module() inserts the prefix set.
        if Self::scope_needs_conditional_annotations_cell(self.current_symbol_table()) {
            self.set_source_range(module_start_loc);
            emit!(self, Instruction::BuildSet { count: 0 });
            self.store_name(&"__conditional_annotations__".into())?;
        }

        if self.future_annotations && annotations_used {
            self.set_source_range(module_start_loc);
            emit!(self, Instruction::SetupAnnotations);
        }

        if let Some((value, range)) = doc {
            let saved_range = self.current_source_range;
            self.set_source_range(range);
            self.emit_load_const(ConstantData::Str {
                value: value.into(),
            });
            let doc = self.name("__doc__");
            emit!(self, Instruction::StoreName { namei: doc });
            self.set_no_location();
            self.set_source_range(saved_range);
        }

        // Compile all statements
        self.compile_statements(statements)?;

        if annotations_used && !self.future_annotations {
            self.compile_module_annotation_setup_sequence(statements, module_start_loc)?;
        }

        assert_eq!(self.code_stack.len(), size_before);

        // Match _PyCodegen_AddReturnAtEnd(): implicit scope epilogues start
        // without a source location and receive one later via CFG line
        // propagation.
        self.emit_return_const_no_location(ConstantData::None);
        Ok(())
    }

    fn compile_program_single(
        &mut self,
        body: &[ast::Stmt],
        symbol_table: SymbolTable,
    ) -> CompileResult<()> {
        self.interactive = true;
        // Set future_annotations from symbol table (detected during symbol table scan)
        self.future_annotations = symbol_table.future_annotations;
        let future_features = self.future_features;
        self.current_code_info().flags |= future_features;
        if self.future_annotations {
            self.future_features
                .insert(bytecode::CodeFlags::FUTURE_ANNOTATIONS);
            self.current_code_info()
                .flags
                .insert(bytecode::CodeFlags::FUTURE_ANNOTATIONS);
        }
        if symbol_table.is_coroutine {
            self.current_code_info()
                .flags
                .insert(bytecode::CodeFlags::COROUTINE);
        }

        // Module-level __conditional_annotations__ cell
        if Self::scope_needs_conditional_annotations_cell(&symbol_table) {
            self.current_code_info()
                .metadata
                .cellvars
                .insert("__conditional_annotations__".to_string());
        }

        self.symbol_table_stack.push(symbol_table);
        let module_start_loc = self.module_start_location(body);

        self.emit_resume_for_scope(CompilerScope::Module, 1);
        emit!(self, PseudoInstruction::AnnotationsPlaceholder);

        let annotations_used = self.current_symbol_table().annotations_used;
        // Handle annotations based on future_annotations flag
        if self.current_symbol_table().has_conditional_annotations {
            self.set_source_range(module_start_loc);
            emit!(self, Instruction::BuildSet { count: 0 });
            self.store_name(&"__conditional_annotations__".into())?;
        }

        if self.future_annotations && annotations_used {
            self.set_source_range(module_start_loc);
            // PEP 563: Initialize __annotations__ dict
            emit!(self, Instruction::SetupAnnotations);
        }

        self.compile_statements(body)?;

        if annotations_used && !self.future_annotations {
            self.compile_module_annotation_setup_sequence(body, module_start_loc)?;
        }

        self.emit_return_const_no_location(ConstantData::None);
        Ok(())
    }

    fn compile_block_expr(
        &mut self,
        body: &[ast::Stmt],
        symbol_table: SymbolTable,
    ) -> CompileResult<()> {
        let future_features = self.future_features;
        self.current_code_info().flags |= future_features;
        if symbol_table.is_coroutine {
            self.current_code_info()
                .flags
                .insert(bytecode::CodeFlags::COROUTINE);
        }
        self.symbol_table_stack.push(symbol_table);
        self.emit_resume_for_scope(CompilerScope::Module, 1);

        if let Some((last_statement, statements)) = body.split_last() {
            self.compile_statements(statements)?;
            match last_statement {
                ast::Stmt::Expr(ast::StmtExpr { value, .. }) => {
                    self.compile_expression(value)?;
                }
                ast::Stmt::FunctionDef(ast::StmtFunctionDef {
                    name,
                    parameters,
                    body,
                    decorator_list,
                    returns,
                    type_params,
                    is_async,
                    ..
                }) => {
                    validate_duplicate_params(parameters).map_err(|e| self.error(e))?;
                    self.compile_function_def(
                        name.id(),
                        parameters,
                        body,
                        decorator_list,
                        returns.as_deref(),
                        *is_async,
                        type_params.as_deref(),
                        true,
                    )?;
                }
                ast::Stmt::ClassDef(ast::StmtClassDef {
                    name,
                    body,
                    decorator_list,
                    type_params,
                    arguments,
                    ..
                }) => {
                    self.compile_class_def(
                        name.id(),
                        body,
                        decorator_list,
                        type_params.as_deref(),
                        arguments.as_deref(),
                        true,
                    )?;
                }
                _ => {
                    self.compile_statement(last_statement)?;
                    self.emit_load_const(ConstantData::None);
                }
            }
        } else {
            self.emit_load_const(ConstantData::None);
        }
        self.emit_return_value();

        Ok(())
    }

    // Compile statement in eval mode:
    fn compile_eval(
        &mut self,
        expression: &ast::ModExpression,
        symbol_table: SymbolTable,
    ) -> CompileResult<()> {
        let future_features = self.future_features;
        self.current_code_info().flags |= future_features;
        if symbol_table.is_coroutine {
            self.current_code_info()
                .flags
                .insert(bytecode::CodeFlags::COROUTINE);
        }
        self.symbol_table_stack.push(symbol_table);
        self.emit_resume_for_scope(CompilerScope::Module, 1);

        self.compile_expression(&expression.body)?;
        self.emit_return_value();
        // The return belongs to no expression, so the exit block can take its
        // location from whichever path reaches it.
        self.set_no_location();
        Ok(())
    }

    fn compile_statements(&mut self, statements: &[ast::Stmt]) -> CompileResult<()> {
        for statement in statements {
            self.compile_statement(statement)?;
        }
        Ok(())
    }

    fn compile_with_body_statements(&mut self, statements: &[ast::Stmt]) -> CompileResult<()> {
        for statement in statements {
            self.compile_statement(statement)?;
        }
        Ok(())
    }

    /// CPython `codegen_call_exit_with_nones()`.
    fn compile_call_exit_with_nones(&mut self) {
        self.emit_load_const(ConstantData::None);
        self.emit_load_const(ConstantData::None);
        self.emit_load_const(ConstantData::None);
        emit!(self, Instruction::Call { argc: 3 });
    }

    /// CPython `codegen_with_except_finish()`.
    fn compile_with_except_finish(&mut self, cleanup_block: BlockIdx) {
        let suppress_block = self.new_block();

        emit!(self, Instruction::ToBool);
        self.set_no_location();
        emit!(
            self,
            Instruction::PopJumpIfTrue {
                delta: suppress_block
            }
        );
        self.set_no_location();
        emit!(self, Instruction::Reraise { depth: 2 });
        self.set_no_location();

        self.use_cpython_label_block(suppress_block);
        emit!(self, Instruction::PopTop);
        self.set_no_location();
        emit!(self, PseudoInstruction::PopBlock);
        self.set_no_location();
        emit!(self, Instruction::PopExcept);
        self.set_no_location();
        emit!(self, Instruction::PopTop);
        self.set_no_location();
        emit!(self, Instruction::PopTop);
        self.set_no_location();
        emit!(self, Instruction::PopTop);
        self.set_no_location();
        let exit_block = self.new_block();
        emit!(
            self,
            PseudoInstruction::JumpNoInterrupt { delta: exit_block }
        );
        self.set_no_location();

        self.use_cpython_label_block(cleanup_block);
        emit!(self, Instruction::Copy { i: 3 });
        self.set_no_location();
        emit!(self, Instruction::PopExcept);
        self.set_no_location();
        emit!(self, Instruction::Reraise { depth: 1 });
        self.set_no_location();

        self.use_cpython_label_block(exit_block);
    }

    fn compile_loop_body_statements(&mut self, statements: &[ast::Stmt]) -> CompileResult<()> {
        self.compile_statements(statements)
    }

    fn scope_needs_conditional_annotations_cell(symbol_table: &SymbolTable) -> bool {
        match symbol_table.typ {
            CompilerScope::Module | CompilerScope::Class => {
                symbol_table.has_conditional_annotations
            }
            _ => false,
        }
    }

    fn load_name(&mut self, name: &Name) -> CompileResult<()> {
        self.compile_name(name, NameUsage::Load)
    }

    fn store_name(&mut self, name: &Name) -> CompileResult<()> {
        self.compile_name(name, NameUsage::Store)
    }

    fn emit_no_location_exception_name_cleanup(&mut self, name: &Name) -> CompileResult<()> {
        // CPython codegen_try_except() emits `name = None; del name`
        // with NO_LOCATION for `except ... as name` cleanup.
        self.emit_load_const(ConstantData::None);
        self.set_no_location();
        self.store_name(name)?;
        self.set_no_location();
        self.compile_name(name, NameUsage::Delete)?;
        self.set_no_location();
        Ok(())
    }

    fn mangle<'a>(&self, name: &'a Name) -> Cow<'a, Name> {
        // Use private from current code unit for name mangling
        let private = self
            .code_stack
            .last()
            .and_then(|info| info.private.as_ref());
        let mangled_names = self.current_symbol_table().mangled_names.as_ref();
        symboltable::maybe_mangle_name(private.map(Name::from).as_ref(), mangled_names, name)
    }

    fn module_name_declared_global_in_nested_scope(table: &SymbolTable, name: &Name) -> bool {
        table.sub_tables.iter().any(|subtable| {
            (!subtable.comp_inlined
                && subtable
                    .lookup(name)
                    .is_some_and(|symbol| symbol.scope == SymbolScope::GlobalExplicit))
                || Self::module_name_declared_global_in_nested_scope(subtable, name)
        })
    }

    // = compiler_nameop
    fn compile_name(&mut self, name: &Name, usage: NameUsage) -> CompileResult<()> {
        enum NameOp {
            Fast,
            Global,
            Deref,
            Name,
            DictOrGlobals, // PEP 649: can_see_class_scope
        }

        let name = self.mangle(name);

        // Special handling for __debug__
        if NameUsage::Load == usage && name.as_str() == "__debug__" {
            self.emit_load_const(ConstantData::Boolean {
                value: self.opts.optimize == 0,
            });
            return Ok(());
        }

        // Determine the operation type based on symbol scope
        let is_function_like = self.ctx.in_func();

        // Look up the symbol, handling ast::TypeParams and Annotation scopes specially
        let (symbol_scope, can_see_class_scope, class_declared_global) = {
            let current_idx = self.symbol_table_stack.len() - 1;
            let current_table = &self.symbol_table_stack[current_idx];
            let is_typeparams = current_table.typ == CompilerScope::TypeParams;
            let is_annotation = matches!(
                current_table.typ,
                CompilerScope::Annotation | CompilerScope::TypeAlias | CompilerScope::TypeVariable
            );
            let can_see_class = current_table.can_see_class_scope;

            // First try to find in current table
            let symbol = current_table.lookup(&name);

            // If not found and we're in ast::TypeParams or Annotation scope, try parent scope
            let symbol = if symbol.is_none() && (is_typeparams || is_annotation) {
                self.symbol_table_stack
                    .get(self.symbol_table_stack.len() - 2) // Try to get parent index
                    .expect("Symbol has no parent! This is a compiler bug.")
                    .lookup(&name)
            } else {
                symbol
            };
            let class_declared_global = can_see_class
                && self.symbol_table_stack[..current_idx]
                    .iter()
                    .rev()
                    .find(|table| table.typ == CompilerScope::Class)
                    .and_then(|table| table.lookup(&name))
                    .is_some_and(|symbol| symbol.flags.contains(SymbolFlags::DEF_GLOBAL));

            (
                symbol.map(|s| s.scope),
                can_see_class,
                class_declared_global,
            )
        };

        // Special handling for class scope implicit cell variables.
        // __classdict__: only LOAD uses Cell (stores go to class namespace)
        // __conditional_annotations__: both LOAD and STORE use Cell (it's a mutable set
        // that the annotation scope accesses through the closure)
        let symbol_scope = {
            let current_table = self.current_symbol_table();
            if current_table.typ == CompilerScope::Class
                && !self.current_code_info().in_inlined_comp
                && ((usage == NameUsage::Load
                    && (matches!(
                        name.as_str(),
                        "__classdict__" | "__conditional_annotations__"
                    )))
                    || (name.as_str() == "__conditional_annotations__"
                        && usage == NameUsage::Store))
            {
                Some(SymbolScope::Cell)
            } else {
                symbol_scope
            }
        };

        // In annotation or type params scope, missing symbols are treated as global implicit
        // This allows referencing global names like Union, Optional, etc. that are imported
        // at module level but not explicitly bound in the function scope
        let actual_scope = match symbol_scope {
            Some(scope) => scope,
            None => {
                let current_table = self.current_symbol_table();
                if matches!(
                    current_table.typ,
                    CompilerScope::Annotation
                        | CompilerScope::TypeAlias
                        | CompilerScope::TypeVariable
                        | CompilerScope::TypeParams
                ) {
                    SymbolScope::GlobalImplicit
                } else if matches!(
                    name.as_str(),
                    "__name__"
                        | "__module__"
                        | "__qualname__"
                        | "__firstlineno__"
                        | "__doc__"
                        | "__static_attributes__"
                        | "__annotate__"
                        | "__annotate_func__"
                        | "__classdictcell__"
                        | "__classcell__"
                ) {
                    SymbolScope::Unknown
                } else {
                    return Err(self.error(CodegenErrorType::SyntaxError(format!(
                        "the symbol '{name}' must be present in the symbol table"
                    ))));
                }
            }
        };

        let module_global_from_nested_scope = {
            let current_table = self.current_symbol_table();
            current_table.typ == CompilerScope::Module
                && Self::module_name_declared_global_in_nested_scope(current_table, name.as_ref())
        };

        // Determine operation type based on scope
        let op_type = match actual_scope {
            SymbolScope::Free => NameOp::Deref,
            SymbolScope::Cell => NameOp::Deref,
            SymbolScope::Local => {
                if module_global_from_nested_scope {
                    NameOp::Global
                } else if is_function_like
                    || self
                        .current_code_info()
                        .metadata
                        .fast_hidden
                        .get(name.as_str())
                        .is_some_and(|&hidden| hidden)
                {
                    NameOp::Fast
                } else {
                    NameOp::Name
                }
            }
            SymbolScope::GlobalImplicit => {
                // PEP 649: In annotation scope with class visibility, use DictOrGlobals
                // to check classdict first before globals
                if class_declared_global {
                    NameOp::Global
                } else if can_see_class_scope && usage == NameUsage::Load {
                    NameOp::DictOrGlobals
                } else if is_function_like {
                    NameOp::Global
                } else {
                    NameOp::Name
                }
            }
            SymbolScope::GlobalExplicit => {
                // A global declared in the owning class body must bypass the
                // classdict, but an explicit global inherited from an outer
                // function still participates in DictOrGlobals lookup.
                if can_see_class_scope && !class_declared_global && usage == NameUsage::Load {
                    NameOp::DictOrGlobals
                } else {
                    NameOp::Global
                }
            }
            SymbolScope::Unknown => {
                if module_global_from_nested_scope {
                    NameOp::Global
                } else {
                    NameOp::Name
                }
            }
        };

        // Generate appropriate instructions based on operation type
        match op_type {
            NameOp::Deref => {
                let i = match actual_scope {
                    SymbolScope::Free => self.get_free_var_index(&name),
                    SymbolScope::Cell => self.get_cell_var_index(&name),
                    _ => unreachable!("Invalid scope for Deref operation"),
                };

                match usage {
                    NameUsage::Load => {
                        // ClassBlock (not inlined comp): LOAD_LOCALS first, then LOAD_FROM_DICT_OR_DEREF
                        if self.ctx.in_class
                            && !self.ctx.in_func()
                            && !self.current_code_info().in_inlined_comp
                        {
                            emit!(self, Instruction::LoadLocals);
                            emit!(self, Instruction::LoadFromDictOrDeref { i });
                        // can_see_class_scope: LOAD_DEREF(__classdict__) first
                        } else if can_see_class_scope {
                            let classdict_idx = self.get_free_var_index("__classdict__");
                            emit!(self, Instruction::LoadDeref { i: classdict_idx });
                            emit!(self, Instruction::LoadFromDictOrDeref { i });
                        } else {
                            emit!(self, Instruction::LoadDeref { i });
                        }
                    }
                    NameUsage::Store => emit!(self, Instruction::StoreDeref { i }),
                    NameUsage::Delete => emit!(self, Instruction::DeleteDeref { i }),
                };
            }
            NameOp::Fast => {
                let var_num = self.get_local_var_index(&name);
                match usage {
                    NameUsage::Load => emit!(self, Instruction::LoadFast { var_num }),
                    NameUsage::Store => emit!(self, Instruction::StoreFast { var_num }),
                    NameUsage::Delete => emit!(self, Instruction::DeleteFast { var_num }),
                };
            }
            NameOp::Global => {
                let namei = self.get_global_name_index(&name);
                match usage {
                    NameUsage::Load => {
                        self.emit_load_global(namei, false);
                        return Ok(());
                    }
                    NameUsage::Store => emit!(self, Instruction::StoreGlobal { namei }),
                    NameUsage::Delete => emit!(self, Instruction::DeleteGlobal { namei }),
                };
            }
            NameOp::Name => {
                let namei = self.get_global_name_index(&name);
                match usage {
                    NameUsage::Load => {
                        if self.current_symbol_table().typ == CompilerScope::Class
                            && self.current_code_info().in_inlined_comp
                        {
                            self.emit_load_global(namei, false);
                        } else {
                            emit!(self, Instruction::LoadName { namei });
                        }
                    }
                    NameUsage::Store => emit!(self, Instruction::StoreName { namei }),
                    NameUsage::Delete => emit!(self, Instruction::DeleteName { namei }),
                };
            }
            NameOp::DictOrGlobals => {
                // PEP 649: First check classdict (from __classdict__ freevar), then globals
                let idx = self.get_global_name_index(&name);
                debug_assert!(usage == NameUsage::Load);
                let classdict_idx = self.get_free_var_index("__classdict__");
                emit!(self, Instruction::LoadDeref { i: classdict_idx });
                emit!(self, Instruction::LoadFromDictOrGlobals { i: idx });
            }
        }

        Ok(())
    }

    fn compile_statement(&mut self, statement: &ast::Stmt) -> CompileResult<()> {
        nanocode_trace::trace!("Compiling {statement:?}");
        let prev_source_range = self.current_source_range;
        self.set_source_range(statement.range());

        // `from __future__` still executes that `from` statement at runtime, so the
        // ImportFrom is compiled down below as well.
        if let ast::Stmt::ImportFrom(ast::StmtImportFrom {
            module,
            names,
            level,
            ..
        }) = &statement
            && *level == 0
            && module.as_ref().map(|id| id.as_str()) == Some("__future__")
        {
            self.compile_future_features(names)?;
        }

        match &statement {
            ast::Stmt::Import(ast::StmtImport { names, .. }) => {
                // import a, b, c as d
                for name in names {
                    let name = &name;
                    self.emit_load_const(ConstantData::Integer {
                        value: nanocode_numeric::Zero::zero(),
                    });
                    self.emit_load_const(ConstantData::None);
                    let namei = self.name(&name.name);
                    emit!(self, Instruction::ImportName { namei });
                    if let Some(alias) = &name.asname {
                        let parts: Vec<&str> = name.name.split('.').skip(1).collect();
                        for (i, part) in parts.iter().enumerate() {
                            let namei = self.name(part);
                            emit!(self, Instruction::ImportFrom { namei });
                            if i < parts.len() - 1 {
                                emit!(self, Instruction::Swap { i: 2 });
                                emit!(self, Instruction::PopTop);
                            }
                        }
                        self.store_name(alias.id())?;
                        if !parts.is_empty() {
                            emit!(self, Instruction::PopTop);
                        }
                    } else {
                        self.store_name(&name.name.split('.').next().unwrap().into())?
                    }
                }
            }
            ast::Stmt::ImportFrom(ast::StmtImportFrom {
                level,
                module,
                names,
                ..
            }) => {
                let import_star = names.first().is_some_and(|n| &n.name == "*");

                let from_list = names
                    .iter()
                    .map(|n| ConstantData::Str {
                        value: n.name.as_str().into(),
                    })
                    .collect();

                // from .... import (*fromlist)
                self.emit_load_const(ConstantData::Integer {
                    value: (*level).into(),
                });
                self.emit_load_const(ConstantData::Tuple {
                    elements: from_list,
                });

                let module_name = module.as_ref().map_or("", |s| s.as_str());
                let module_idx = self.name(module_name);
                emit!(self, Instruction::ImportName { namei: module_idx });

                if import_star {
                    // from .... import *
                    emit!(
                        self,
                        Instruction::CallIntrinsic1 {
                            func: bytecode::IntrinsicFunction1::ImportStar
                        }
                    );
                    emit!(self, Instruction::PopTop);
                    self.set_no_location();
                } else {
                    // from mod import a, b as c

                    for name in names {
                        let name = &name;
                        let idx = self.name(name.name.as_str());
                        // import symbol from module:
                        emit!(self, Instruction::ImportFrom { namei: idx });

                        // Store module under proper name:
                        if let Some(alias) = &name.asname {
                            self.store_name(alias.id())?
                        } else {
                            self.store_name(name.name.id())?
                        }
                    }

                    // Pop module from stack:
                    emit!(self, Instruction::PopTop);
                }
            }
            ast::Stmt::Expr(ast::StmtExpr { value, .. }) => {
                // Optimize away constant expressions with no side effects.
                // In interactive mode, always compile (to print the result).
                let dominated_by_interactive =
                    self.interactive && !self.ctx.in_func() && !self.ctx.in_class;
                if !dominated_by_interactive && value.is_constant() {
                    emit!(self, Instruction::Nop);
                } else {
                    let statement_range = self.current_source_range;
                    self.compile_expression(value)?;

                    if dominated_by_interactive {
                        // The printing belongs to the statement, not to whatever
                        // the expression left behind.
                        self.set_source_range(statement_range);
                        emit!(
                            self,
                            Instruction::CallIntrinsic1 {
                                func: bytecode::IntrinsicFunction1::Print
                            }
                        );
                    }

                    emit!(self, Instruction::PopTop);
                    self.set_no_location();
                }
            }
            ast::Stmt::Global(_) | ast::Stmt::Nonlocal(_) => {
                // Handled during symbol table construction.
            }
            ast::Stmt::If(ast::StmtIf {
                test,
                body,
                elif_else_clauses,
                ..
            }) => {
                self.enter_conditional_block();
                self.compile_if(test, body, elif_else_clauses, statement.range())?;
                self.leave_conditional_block();
                self.set_source_range(statement.range());
            }
            ast::Stmt::While(ast::StmtWhile {
                test,
                body,
                orelse,
                range,
                ..
            }) => self.compile_while(test, body, orelse, *range)?,
            ast::Stmt::With(ast::StmtWith {
                items,
                body,
                is_async,
                ..
            }) => self.compile_with(items, body, *is_async)?,
            ast::Stmt::For(ast::StmtFor {
                target,
                iter,
                body,
                orelse,
                is_async,
                range,
                ..
            }) => self.compile_for(target, iter, body, orelse, *is_async, *range)?,
            ast::Stmt::Match(ast::StmtMatch { subject, cases, .. }) => {
                self.compile_match(subject, cases)?
            }
            ast::Stmt::Raise(ast::StmtRaise {
                exc, cause, range, ..
            }) => {
                let kind = match exc {
                    Some(value) => {
                        self.compile_expression(value)?;
                        match cause {
                            Some(cause) => {
                                self.compile_expression(cause)?;
                                bytecode::RaiseKind::RaiseCause
                            }
                            None => bytecode::RaiseKind::Raise,
                        }
                    }
                    None => bytecode::RaiseKind::BareRaise,
                };
                self.set_source_range(*range);
                emit!(self, Instruction::RaiseVarargs { argc: kind });
            }
            ast::Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                is_star,
                ..
            }) => {
                self.enter_conditional_block();
                if *is_star {
                    self.compile_try_star_statement(body, handlers, orelse, finalbody)?
                } else {
                    self.compile_try_statement(body, handlers, orelse, finalbody)?
                }
                self.leave_conditional_block();
            }
            ast::Stmt::FunctionDef(ast::StmtFunctionDef {
                name,
                parameters,
                body,
                decorator_list,
                returns,
                type_params,
                is_async,
                ..
            }) => {
                validate_duplicate_params(parameters).map_err(|e| self.error(e))?;

                self.compile_function_def(
                    name.id(),
                    parameters,
                    body,
                    decorator_list,
                    returns.as_deref(),
                    *is_async,
                    type_params.as_deref(),
                    false,
                )?
            }
            ast::Stmt::ClassDef(ast::StmtClassDef {
                name,
                body,
                decorator_list,
                type_params,
                arguments,
                ..
            }) => self.compile_class_def(
                name.id(),
                body,
                decorator_list,
                type_params.as_deref(),
                arguments.as_deref(),
                false,
            )?,
            ast::Stmt::Assert(assert_stmt) => {
                let ast::StmtAssert {
                    test, msg, range, ..
                } = assert_stmt;
                self.check_assert(assert_stmt)?;
                // if some flag, ignore all assert statements!
                if self.opts.optimize == 0 {
                    let after_block = self.new_block();
                    self.compile_jump_if_inner(test, true, after_block, Some(*range))?;
                    self.set_source_range(*range);
                    emit!(
                        self,
                        Instruction::LoadCommonConstant {
                            idx: bytecode::CommonConstant::AssertionError
                        }
                    );
                    if let Some(e) = msg {
                        self.compile_expression(e)?;
                        self.set_source_range(*range);
                        emit!(self, Instruction::Call { argc: 0 });
                    }
                    self.set_source_range(test.range());
                    emit!(
                        self,
                        Instruction::RaiseVarargs {
                            argc: bytecode::RaiseKind::Raise,
                        }
                    );
                    self.use_cpython_label_block(after_block);
                } else {
                    // Optimized-out asserts still need to consume any nested
                    // scope symbol tables they contain so later nested scopes
                    // stay aligned with AST traversal order.
                    self.consume_skipped_nested_scopes_in_expr(test)?;
                    if let Some(expr) = msg {
                        self.consume_skipped_nested_scopes_in_expr(expr)?;
                    }
                }
            }
            ast::Stmt::Break(_) => {
                // Unwind fblock stack until we find a loop, emitting cleanup for each fblock
                self.compile_break_continue(statement.range(), true)?;
            }
            ast::Stmt::Continue(_) => {
                // Unwind fblock stack until we find a loop, emitting cleanup for each fblock
                self.compile_break_continue(statement.range(), false)?;
            }
            ast::Stmt::Return(ast::StmtReturn { value, .. }) => {
                if !self.ctx.in_func() {
                    return Err(
                        self.error_ranged(CodegenErrorType::InvalidReturn, statement.range())
                    );
                }

                let prev_source_range = self.current_source_range;
                let stmt_range = statement.range();
                match value {
                    Some(v) => {
                        if self.ctx.func == FunctionContext::AsyncFunction
                            && self.current_symbol_table().is_generator
                        {
                            return Err(self.error_ranged(
                                CodegenErrorType::AsyncReturnValue,
                                statement.range(),
                            ));
                        }
                        let debug_constant = matches!(
                            &**v,
                            ast::Expr::Name(ast::ExprName { id, ctx, .. })
                                if matches!(ctx, ast::ExprContext::Load)
                                    && id.as_str() == "__debug__"
                        );
                        let folded_constant = if self.is_constant_expr(v) || debug_constant {
                            self.try_fold_constant_expr(v)?
                        } else {
                            None
                        };
                        let preserve_tos = folded_constant.is_none();
                        let mut return_range = stmt_range;
                        if preserve_tos {
                            self.compile_expression(v)?;
                        } else {
                            return_range = v.range();
                            self.set_source_range(v.range());
                            emit!(self, Instruction::Nop);
                        }

                        let source = self.source_file.to_source_code();
                        if source.line_index(v.range().start())
                            != source.line_index(stmt_range.start())
                        {
                            return_range = stmt_range;
                            self.set_source_range(stmt_range);
                            emit!(self, Instruction::Nop);
                        }
                        self.set_source_range(return_range);
                        let unwind_loc = self.unwind_fblock_stack(preserve_tos, false)?;
                        if let Some(loc) = unwind_loc {
                            self.set_source_range(loc);
                            match folded_constant {
                                Some(constant) => self.emit_return_const(constant),
                                None => {
                                    self.emit_return_value();
                                }
                            }
                        } else {
                            match folded_constant {
                                Some(constant) => self.emit_return_const_no_location(constant),
                                None => {
                                    self.emit_return_value();
                                    self.set_no_location();
                                }
                            }
                        }
                    }
                    None => {
                        self.set_source_range(stmt_range);
                        emit!(self, Instruction::Nop);
                        // Unwind fblock stack with preserve_tos=false (no value to preserve)
                        let unwind_loc = self.unwind_fblock_stack(false, false)?;
                        if let Some(loc) = unwind_loc {
                            self.set_source_range(loc);
                            self.emit_return_const(ConstantData::None);
                        } else {
                            self.emit_return_const_no_location(ConstantData::None);
                        }
                    }
                }
                self.set_source_range(prev_source_range);
            }
            ast::Stmt::Assign(ast::StmtAssign {
                targets,
                value,
                range,
                ..
            }) => {
                self.compile_expression(value)?;

                for (i, target) in targets.iter().enumerate() {
                    if i + 1 != targets.len() {
                        self.set_source_range(*range);
                        emit!(self, Instruction::Copy { i: 1 });
                    }
                    self.compile_store(target)?;
                }
            }
            ast::Stmt::AugAssign(ast::StmtAugAssign {
                target, op, value, ..
            }) => self.compile_augassign(target, *op, value)?,
            ast::Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value,
                simple,
                range,
                ..
            }) => {
                self.compile_annotated_assign(
                    target,
                    annotation,
                    value.as_deref(),
                    *simple,
                    *range,
                )?;
                // Bare annotations in function scope emit no code; restore
                // source range so subsequent instructions keep the correct line.
                if value.is_none() && self.ctx.in_func() {
                    self.set_source_range(prev_source_range);
                }
            }
            ast::Stmt::Delete(ast::StmtDelete { targets, .. }) => {
                for target in targets {
                    self.compile_delete(target)?;
                }
            }
            ast::Stmt::Pass(_) => {
                emit!(self, Instruction::Nop); // NOP for line tracing
            }
            ast::Stmt::TypeAlias(ast::StmtTypeAlias {
                name,
                type_params,
                value,
                range,
                ..
            }) => {
                let Some(name) = name.as_name_expr() else {
                    return Err(self.error(CodegenErrorType::SyntaxError(
                        "type alias expect name".to_owned(),
                    )));
                };
                let name_string = name.id();

                if let Some(type_params) = type_params {
                    self.set_source_range(*range);
                    self.push_symbol_table()?;
                    let key = self.symbol_table_stack.len() - 1;
                    let lineno = self.get_source_line_number().get().to_u32();
                    let scope_name = format!("<generic parameters of {name_string}>");
                    self.enter_scope(&scope_name, CompilerScope::TypeParams, key, lineno)?;

                    // TypeParams scope is function-like
                    let prev_ctx = self.ctx;
                    self.ctx = CompileContext {
                        in_class: prev_ctx.in_class,
                        func: FunctionContext::Function,
                        in_async_scope: false,
                    };

                    self.set_source_range(*range);
                    self.emit_load_const(ConstantData::Str {
                        value: name_string.as_str().into(),
                    });
                    self.compile_type_params(type_params)?;
                    self.compile_typealias_value_closure(name_string, value, *range)?;
                    self.set_source_range(*range);
                    emit!(self, Instruction::BuildTuple { count: 3 });
                    emit!(
                        self,
                        Instruction::CallIntrinsic1 {
                            func: bytecode::IntrinsicFunction1::TypeAlias
                        }
                    );
                    emit!(self, Instruction::ReturnValue);

                    let code = self.exit_scope();
                    self.ctx = prev_ctx;
                    self.set_source_range(*range);
                    self.make_closure(code, bytecode::MakeFunctionFlags::new())?;
                    self.set_source_range(*range);
                    emit!(self, Instruction::PushNull);
                    emit!(self, Instruction::Call { argc: 0 });
                } else {
                    self.set_source_range(*range);
                    self.emit_load_const(ConstantData::Str {
                        value: name_string.as_str().into(),
                    });
                    self.emit_load_const(ConstantData::None);
                    self.compile_typealias_value_closure(name_string, value, *range)?;
                    self.set_source_range(*range);
                    emit!(self, Instruction::BuildTuple { count: 3 });
                    emit!(
                        self,
                        Instruction::CallIntrinsic1 {
                            func: bytecode::IntrinsicFunction1::TypeAlias
                        }
                    );
                }

                self.set_source_range(*range);
                self.store_name(name_string)?;
            }
            ast::Stmt::IpyEscapeCommand(stmt) => {
                return Err(self.error_ranged(
                    CodegenErrorType::SyntaxError("invalid syntax".to_owned()),
                    stmt.range,
                ));
            }
        }
        Ok(())
    }

    fn compile_delete(&mut self, expression: &ast::Expr) -> CompileResult<()> {
        let prev_source_range = self.current_source_range;
        self.set_source_range(expression.range());
        let result = (|| -> CompileResult<()> {
            match &expression {
                ast::Expr::Name(ast::ExprName { id, .. }) => {
                    self.compile_name(id, NameUsage::Delete)?
                }
                ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                    self.compile_expression(value)?;
                    let namei = self.name(attr.as_str());
                    self.set_source_range(self.update_start_location_to_match_attr(
                        expression.range(),
                        expression.range(),
                        attr.as_str(),
                    ));
                    emit!(self, Instruction::DeleteAttr { namei });
                }
                ast::Expr::Subscript(ast::ExprSubscript {
                    value, slice, ctx, ..
                }) => {
                    self.compile_subscript(value, slice, *ctx)?;
                }
                ast::Expr::Tuple(ast::ExprTuple { elts, .. })
                | ast::Expr::List(ast::ExprList { elts, .. }) => {
                    for element in elts {
                        self.compile_delete(element)?;
                    }
                }
                ast::Expr::BinOp(_) | ast::Expr::UnaryOp(_) => {
                    return Err(self.error(CodegenErrorType::Delete("expression")));
                }
                _ => return Err(self.error(CodegenErrorType::Delete(expression.python_name()))),
            }
            Ok(())
        })();
        self.set_source_range(prev_source_range);
        result
    }

    fn enter_function(&mut self, name: &str, parameters: &ast::Parameters) -> CompileResult<()> {
        self.push_output(
            bytecode::CodeFlags::NEWLOCALS | bytecode::CodeFlags::OPTIMIZED,
            parameters.posonlyargs.len().to_u32(),
            parameters.args.len().to_u32(),
            parameters.kwonlyargs.len().to_u32(),
            name,
        )?;

        let args_iter = core::iter::empty()
            .chain(&parameters.posonlyargs)
            .chain(&parameters.args)
            .map(|arg| &arg.parameter)
            .chain(parameters.kwonlyargs.iter().map(|arg| &arg.parameter));
        for name in args_iter {
            self.varname(name.name.as_str());
        }

        if let Some(name) = parameters.vararg.as_deref() {
            self.current_code_info().flags |= bytecode::CodeFlags::VARARGS;
            self.varname(name.name.as_str());
        }
        if let Some(name) = parameters.kwarg.as_deref() {
            self.current_code_info().flags |= bytecode::CodeFlags::VARKEYWORDS;
            self.varname(name.name.as_str());
        }

        Ok(())
    }

    /// Push decorators onto the stack in source order.
    /// For @dec1 @dec2 def foo(): stack becomes [dec1, dec2]
    fn prepare_decorators(&mut self, decorator_list: &[ast::Decorator]) -> CompileResult<()> {
        for decorator in decorator_list {
            self.compile_expression(&decorator.expression)?;
        }
        Ok(())
    }

    /// Apply decorators: each decorator calls the function below it.
    /// Stack: [dec1, dec2, func] → CALL 0 → [dec1, dec2(func)] → CALL 0 → [dec1(dec2(func))]
    fn apply_decorators(&mut self, decorator_list: &[ast::Decorator]) {
        for decorator in decorator_list.iter().rev() {
            self.set_source_range(decorator.expression.range());
            emit!(self, Instruction::Call { argc: 0 });
        }
    }

    /// Compile type parameter bound or default in a separate scope and return closure
    fn compile_type_param_bound_or_default(
        &mut self,
        expr: &ast::Expr,
        name: &str,
        allow_starred: bool,
    ) -> CompileResult<()> {
        let expr_range = expr.range();
        self.set_source_range(expr_range);
        self.emit_load_const(ConstantData::Tuple {
            elements: vec![ConstantData::Integer { value: 1.into() }],
        });

        // Push the next symbol table onto the stack
        self.push_symbol_table()?;

        // Get the current symbol table
        let key = self.symbol_table_stack.len() - 1;
        let lineno = self.get_source_line_number().get().to_u32();

        // Enter scope with the type parameter name
        self.enter_scope(name, CompilerScope::TypeVariable, key, lineno)?;

        self.configure_annotation_format_parameter();

        self.emit_format_validation();

        // TypeParams scope is function-like
        let prev_ctx = self.ctx;
        self.ctx = CompileContext {
            in_class: prev_ctx.in_class,
            func: FunctionContext::Function,
            in_async_scope: false,
        };

        // Compile the expression
        if allow_starred && matches!(expr, ast::Expr::Starred(_)) {
            if let ast::Expr::Starred(starred) = expr {
                self.compile_expression(&starred.value)?;
                self.set_source_range(expr_range);
                emit!(self, Instruction::UnpackSequence { count: 1 });
            }
        } else {
            self.compile_expression(expr)?;
        }

        // Return value
        self.set_source_range(expr_range);
        emit!(self, Instruction::ReturnValue);
        self.emit_return_const_no_location(ConstantData::None);

        // Exit scope and create closure
        let code = self.exit_scope();
        self.ctx = prev_ctx;

        self.set_source_range(expr_range);
        self.make_closure(
            code,
            bytecode::MakeFunctionFlags::from([bytecode::MakeFunctionFlag::Defaults]),
        )?;

        Ok(())
    }

    fn compile_typealias_value_closure(
        &mut self,
        alias_name: &str,
        value: &ast::Expr,
        alias_range: TextRange,
    ) -> CompileResult<()> {
        self.set_source_range(alias_range);
        self.emit_load_const(ConstantData::Tuple {
            elements: vec![ConstantData::Integer { value: 1.into() }],
        });

        self.push_symbol_table()?;
        let key = self.symbol_table_stack.len() - 1;
        let lineno = self.get_source_line_number().get().to_u32();
        self.enter_scope(alias_name, CompilerScope::TypeAlias, key, lineno)?;
        self.configure_annotation_format_parameter();
        self.emit_format_validation();

        let prev_ctx = self.ctx;
        self.ctx = CompileContext {
            in_class: prev_ctx.in_class,
            func: FunctionContext::Function,
            in_async_scope: false,
        };

        self.compile_expression(value)?;
        self.set_source_range(alias_range);
        emit!(self, Instruction::ReturnValue);

        let code = self.exit_scope();
        self.ctx = prev_ctx;
        self.set_source_range(alias_range);
        self.make_closure(
            code,
            bytecode::MakeFunctionFlags::from([bytecode::MakeFunctionFlag::Defaults]),
        )?;

        Ok(())
    }

    /// Store each type parameter so it is accessible to the current scope, and leave a tuple of
    /// all the type parameters on the stack. Handles default values per PEP 695.
    fn compile_type_params(&mut self, type_params: &ast::TypeParams) -> CompileResult<()> {
        let mut seen_default = false;
        // First, compile each type parameter and store it
        for type_param in &type_params.type_params {
            match type_param {
                ast::TypeParam::TypeVar(ast::TypeParamTypeVar {
                    name,
                    bound,
                    default,
                    range,
                    ..
                }) => {
                    self.set_source_range(*range);
                    self.emit_load_const(ConstantData::Str {
                        value: name.as_str().into(),
                    });

                    if let Some(expr) = &bound {
                        self.compile_type_param_bound_or_default(expr, name.as_str(), false)?;

                        self.set_source_range(*range);
                        let intrinsic = if expr.is_tuple_expr() {
                            bytecode::IntrinsicFunction2::TypeVarWithConstraint
                        } else {
                            bytecode::IntrinsicFunction2::TypeVarWithBound
                        };
                        emit!(self, Instruction::CallIntrinsic2 { func: intrinsic });
                    } else {
                        emit!(
                            self,
                            Instruction::CallIntrinsic1 {
                                func: bytecode::IntrinsicFunction1::TypeVar
                            }
                        );
                    }

                    if let Some(default_expr) = default {
                        seen_default = true;
                        self.compile_type_param_bound_or_default(
                            default_expr,
                            name.as_str(),
                            false,
                        )?;
                        self.set_source_range(*range);
                        emit!(
                            self,
                            Instruction::CallIntrinsic2 {
                                func: bytecode::IntrinsicFunction2::SetTypeparamDefault
                            }
                        );
                    } else if seen_default {
                        return Err(self.error_ranged(
                            CodegenErrorType::SyntaxError(format!(
                                "non-default type parameter '{name}' follows default type parameter"
                            )),
                            *range,
                        ));
                    }

                    self.set_source_range(*range);
                    emit!(self, Instruction::Copy { i: 1 });
                    self.store_name(name.id())?;
                }
                ast::TypeParam::ParamSpec(ast::TypeParamParamSpec {
                    name,
                    default,
                    range,
                    ..
                }) => {
                    self.set_source_range(*range);
                    self.emit_load_const(ConstantData::Str {
                        value: name.as_str().into(),
                    });
                    emit!(
                        self,
                        Instruction::CallIntrinsic1 {
                            func: bytecode::IntrinsicFunction1::ParamSpec
                        }
                    );

                    if let Some(default_expr) = default {
                        seen_default = true;
                        self.compile_type_param_bound_or_default(
                            default_expr,
                            name.as_str(),
                            false,
                        )?;
                        self.set_source_range(*range);
                        emit!(
                            self,
                            Instruction::CallIntrinsic2 {
                                func: bytecode::IntrinsicFunction2::SetTypeparamDefault
                            }
                        );
                    } else if seen_default {
                        return Err(self.error_ranged(
                            CodegenErrorType::SyntaxError(format!(
                                "non-default type parameter '{name}' follows default type parameter"
                            )),
                            *range,
                        ));
                    }

                    self.set_source_range(*range);
                    emit!(self, Instruction::Copy { i: 1 });
                    self.store_name(name.id())?;
                }
                ast::TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple {
                    name,
                    default,
                    range,
                    ..
                }) => {
                    self.set_source_range(*range);
                    self.emit_load_const(ConstantData::Str {
                        value: name.as_str().into(),
                    });
                    emit!(
                        self,
                        Instruction::CallIntrinsic1 {
                            func: bytecode::IntrinsicFunction1::TypeVarTuple
                        }
                    );

                    if let Some(default_expr) = default {
                        // TypeVarTuple allows starred expressions
                        self.compile_type_param_bound_or_default(
                            default_expr,
                            name.as_str(),
                            true,
                        )?;
                        self.set_source_range(*range);
                        emit!(
                            self,
                            Instruction::CallIntrinsic2 {
                                func: bytecode::IntrinsicFunction2::SetTypeparamDefault
                            }
                        );
                        seen_default = true;
                    } else if seen_default {
                        return Err(self.error_ranged(
                            CodegenErrorType::SyntaxError(format!(
                                "non-default type parameter '{name}' follows default type parameter"
                            )),
                            *range,
                        ));
                    }

                    self.set_source_range(*range);
                    emit!(self, Instruction::Copy { i: 1 });
                    self.store_name(name.id())?;
                }
            };
        }
        if let Some(first) = type_params.type_params.first() {
            self.set_source_range(first.range());
        }
        emit!(
            self,
            Instruction::BuildTuple {
                count: u32::try_from(type_params.len()).unwrap(),
            }
        );
        Ok(())
    }

    fn compile_try_statement(
        &mut self,
        body: &[ast::Stmt],
        handlers: &[ast::ExceptHandler],
        orelse: &[ast::Stmt],
        finalbody: &[ast::Stmt],
    ) -> CompileResult<()> {
        if finalbody.is_empty() {
            return self.compile_try_except_no_finally(body, handlers, orelse);
        }

        let body_block = self.new_block();
        let finally_except_block = self.new_block();
        let exit_block = self.new_block();
        let finally_cleanup_block = self.new_block();

        emit!(
            self,
            PseudoInstruction::SetupFinally {
                delta: finally_except_block
            }
        );
        self.use_cpython_label_block(body_block);
        let body_label = self.instr_sequence_label_for_block(body_block);
        let finally_except_label = self.instr_sequence_label_for_block(finally_except_block);
        self.push_fblock_labels(
            FBlockType::FinallyTry,
            body_label,
            finally_except_label,
            FBlockDatum::FinallyBody(finalbody.to_vec()),
        )?;

        if handlers.is_empty() {
            self.compile_statements(body)?;
        } else {
            self.compile_try_except_no_finally(body, handlers, orelse)?;
        }

        emit!(self, PseudoInstruction::PopBlock);
        self.set_no_location();
        self.pop_fblock_label(FBlockType::FinallyTry, body_label);

        let symbol_table_cursors = self.current_symbol_table_cursors();

        self.compile_statements(finalbody)?;
        emit!(
            self,
            PseudoInstruction::JumpNoInterrupt { delta: exit_block }
        );
        self.set_no_location();

        self.set_symbol_table_cursors(symbol_table_cursors);

        self.use_cpython_label_block(finally_except_block);
        emit!(
            self,
            PseudoInstruction::SetupCleanup {
                delta: finally_cleanup_block
            }
        );
        self.set_no_location();
        emit!(self, Instruction::PushExcInfo);
        self.set_no_location();
        self.push_fblock_labels(
            FBlockType::FinallyEnd,
            finally_except_label,
            ir::InstructionSequenceLabel::NO_LABEL,
            FBlockDatum::None,
        )?;
        self.compile_statements(finalbody)?;
        self.pop_fblock_label(FBlockType::FinallyEnd, finally_except_label);
        emit!(self, Instruction::Reraise { depth: 0 });
        self.set_no_location();

        self.use_cpython_label_block(finally_cleanup_block);
        emit!(self, Instruction::Copy { i: 3 });
        self.set_no_location();
        emit!(self, Instruction::PopExcept);
        self.set_no_location();
        emit!(self, Instruction::Reraise { depth: 1 });
        self.set_no_location();

        self.use_cpython_label_block(exit_block);

        Ok(())
    }

    fn compile_try_except_no_finally(
        &mut self,
        body: &[ast::Stmt],
        handlers: &[ast::ExceptHandler],
        orelse: &[ast::Stmt],
    ) -> CompileResult<()> {
        let body_block = self.new_block();
        let handler_block = self.new_block();
        let end_block = self.new_block();
        let cleanup_block = self.new_block();
        emit!(
            self,
            PseudoInstruction::SetupFinally {
                delta: handler_block
            }
        );
        self.use_cpython_label_block(body_block);
        let body_label = self.instr_sequence_label_for_block(body_block);
        self.push_fblock_labels(
            FBlockType::TryExcept,
            body_label,
            ir::InstructionSequenceLabel::NO_LABEL,
            FBlockDatum::None,
        )?;
        self.compile_statements(body)?;
        self.pop_fblock_label(FBlockType::TryExcept, body_label);
        emit!(self, PseudoInstruction::PopBlock);
        self.set_no_location();

        // The symtable stores child scopes in AST visit order
        // (body, handlers, orelse), while codegen_try_except() emits orelse
        // before the exception handlers. Keep the symbol table in symtable order
        // and only move the codegen cursor while compiling orelse.
        let handler_symbol_table_cursors = self.current_symbol_table_cursors();
        self.consume_skipped_nested_scopes_in_except_handlers(handlers)?;
        self.compile_statements(orelse)?;
        let after_orelse_symbol_table_cursors = self.current_symbol_table_cursors();
        self.set_symbol_table_cursors(handler_symbol_table_cursors);
        emit!(
            self,
            PseudoInstruction::JumpNoInterrupt { delta: end_block }
        );
        self.set_no_location();

        self.use_cpython_label_block(handler_block);
        emit!(
            self,
            PseudoInstruction::SetupCleanup {
                delta: cleanup_block
            }
        );
        self.set_no_location();
        emit!(self, Instruction::PushExcInfo);
        self.set_no_location();
        self.push_fblock_labels(
            FBlockType::ExceptionHandler,
            ir::InstructionSequenceLabel::NO_LABEL,
            ir::InstructionSequenceLabel::NO_LABEL,
            FBlockDatum::None,
        )?;

        for (i, handler) in handlers.iter().enumerate() {
            let ast::ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                type_,
                name,
                body,
                range: handler_range,
                ..
            }) = handler;
            self.set_source_range(*handler_range);
            if type_.is_none() && i < handlers.len() - 1 {
                return Err(self.error(CodegenErrorType::SyntaxError(
                    "default 'except:' must be last".to_owned(),
                )));
            }
            let next_handler = self.new_block();

            if let Some(exc_type) = type_ {
                self.compile_expression(exc_type)?;
                self.set_source_range(*handler_range);
                emit!(self, Instruction::CheckExcMatch);
                emit!(
                    self,
                    Instruction::PopJumpIfFalse {
                        delta: next_handler
                    }
                );
            }

            if let Some(alias) = name {
                let cleanup_end = self.new_block();
                let cleanup_body = self.new_block();

                self.store_name(alias.id())?;

                emit!(self, PseudoInstruction::SetupCleanup { delta: cleanup_end });
                self.use_cpython_label_block(cleanup_body);
                let cleanup_body_label = self.instr_sequence_label_for_block(cleanup_body);
                self.push_fblock_labels(
                    FBlockType::HandlerCleanup,
                    cleanup_body_label,
                    ir::InstructionSequenceLabel::NO_LABEL,
                    FBlockDatum::ExceptionName(alias.as_str().to_owned()),
                )?;

                self.compile_statements(body)?;

                self.pop_fblock_label(FBlockType::HandlerCleanup, cleanup_body_label);
                emit!(self, PseudoInstruction::PopBlock);
                self.set_no_location();
                emit!(self, PseudoInstruction::PopBlock);
                self.set_no_location();
                emit!(self, Instruction::PopExcept);
                self.set_no_location();

                self.emit_no_location_exception_name_cleanup(alias.id())?;

                emit!(
                    self,
                    PseudoInstruction::JumpNoInterrupt { delta: end_block }
                );
                self.set_no_location();

                self.use_cpython_label_block(cleanup_end);
                self.emit_no_location_exception_name_cleanup(alias.id())?;
                emit!(self, Instruction::Reraise { depth: 1 });
                self.set_no_location();
            } else {
                let cleanup_body = self.new_block();

                emit!(self, Instruction::PopTop);
                self.use_cpython_label_block(cleanup_body);
                let cleanup_body_label = self.instr_sequence_label_for_block(cleanup_body);
                self.push_fblock_labels(
                    FBlockType::HandlerCleanup,
                    cleanup_body_label,
                    ir::InstructionSequenceLabel::NO_LABEL,
                    FBlockDatum::None,
                )?;

                self.compile_statements(body)?;

                self.pop_fblock_label(FBlockType::HandlerCleanup, cleanup_body_label);
                emit!(self, PseudoInstruction::PopBlock);
                self.set_no_location();
                emit!(self, Instruction::PopExcept);
                self.set_no_location();
                emit!(
                    self,
                    PseudoInstruction::JumpNoInterrupt { delta: end_block }
                );
                self.set_no_location();
            }

            self.use_cpython_label_block(next_handler);
        }
        self.set_symbol_table_cursors(after_orelse_symbol_table_cursors);

        self.pop_fblock_label(
            FBlockType::ExceptionHandler,
            ir::InstructionSequenceLabel::NO_LABEL,
        );
        emit!(self, Instruction::Reraise { depth: 0 });
        self.set_no_location();

        self.use_cpython_label_block(cleanup_block);
        emit!(self, Instruction::Copy { i: 3 });
        self.set_no_location();
        emit!(self, Instruction::PopExcept);
        self.set_no_location();
        emit!(self, Instruction::Reraise { depth: 1 });
        self.set_no_location();

        self.use_cpython_label_block(end_block);
        Ok(())
    }

    fn compile_try_star_statement(
        &mut self,
        body: &[ast::Stmt],
        handlers: &[ast::ExceptHandler],
        orelse: &[ast::Stmt],
        finalbody: &[ast::Stmt],
    ) -> CompileResult<()> {
        if finalbody.is_empty() {
            return self.compile_try_star_except(body, handlers, orelse);
        }

        let body_block = self.new_block();
        let finally_except_block = self.new_block();
        let exit_block = self.new_block();
        let finally_cleanup_block = self.new_block();

        emit!(
            self,
            PseudoInstruction::SetupFinally {
                delta: finally_except_block
            }
        );
        self.use_cpython_label_block(body_block);
        let body_label = self.instr_sequence_label_for_block(body_block);
        let finally_except_label = self.instr_sequence_label_for_block(finally_except_block);
        self.push_fblock_labels(
            FBlockType::FinallyTry,
            body_label,
            finally_except_label,
            FBlockDatum::FinallyBody(finalbody.to_vec()),
        )?;

        if handlers.is_empty() {
            self.compile_statements(body)?;
        } else {
            self.compile_try_star_except(body, handlers, orelse)?;
        }

        emit!(self, PseudoInstruction::PopBlock);
        self.set_no_location();
        self.pop_fblock_label(FBlockType::FinallyTry, body_label);

        let symbol_table_cursors = self.current_symbol_table_cursors();
        self.compile_statements(finalbody)?;

        emit!(
            self,
            PseudoInstruction::JumpNoInterrupt { delta: exit_block }
        );
        self.set_no_location();

        self.set_symbol_table_cursors(symbol_table_cursors);

        self.use_cpython_label_block(finally_except_block);
        emit!(
            self,
            PseudoInstruction::SetupCleanup {
                delta: finally_cleanup_block
            }
        );
        self.set_no_location();
        emit!(self, Instruction::PushExcInfo);
        self.set_no_location();
        self.push_fblock_labels(
            FBlockType::FinallyEnd,
            finally_except_label,
            ir::InstructionSequenceLabel::NO_LABEL,
            FBlockDatum::None,
        )?;
        self.compile_statements(finalbody)?;
        self.pop_fblock_label(FBlockType::FinallyEnd, finally_except_label);
        emit!(self, Instruction::Reraise { depth: 0 });
        self.set_no_location();

        self.use_cpython_label_block(finally_cleanup_block);
        emit!(self, Instruction::Copy { i: 3 });
        self.set_no_location();
        emit!(self, Instruction::PopExcept);
        self.set_no_location();
        emit!(self, Instruction::Reraise { depth: 1 });
        self.set_no_location();

        self.use_cpython_label_block(exit_block);

        Ok(())
    }

    fn compile_try_star_except(
        &mut self,
        body: &[ast::Stmt],
        handlers: &[ast::ExceptHandler],
        orelse: &[ast::Stmt],
    ) -> CompileResult<()> {
        // compiler_try_star_except
        // Stack layout during handler processing: [prev_exc, orig, list, rest]
        let body_block = self.new_block();
        let handler_block = self.new_block();
        let else_block = self.new_block();
        let end_block = self.new_block();
        let cleanup_block = self.new_block();
        let reraise_star_block = self.new_block();

        // SETUP_FINALLY for try body
        emit!(
            self,
            PseudoInstruction::SetupFinally {
                delta: handler_block
            }
        );
        self.use_cpython_label_block(body_block);
        let body_label = self.instr_sequence_label_for_block(body_block);
        self.push_fblock_labels(
            FBlockType::TryExcept,
            body_label,
            ir::InstructionSequenceLabel::NO_LABEL,
            FBlockDatum::None,
        )?;
        self.compile_statements(body)?;
        self.pop_fblock_label(FBlockType::TryExcept, body_label);
        emit!(self, PseudoInstruction::PopBlock);
        self.set_no_location();
        emit!(
            self,
            PseudoInstruction::JumpNoInterrupt { delta: else_block }
        );
        self.set_no_location();

        // Exception handler entry
        self.use_cpython_label_block(handler_block);
        // Stack: [exc] (from exception table)

        emit!(
            self,
            PseudoInstruction::SetupCleanup {
                delta: cleanup_block
            }
        );
        self.set_no_location();

        // PUSH_EXC_INFO
        emit!(self, Instruction::PushExcInfo);
        self.set_no_location();
        // Stack: [prev_exc, exc]

        // Push EXCEPTION_GROUP_HANDLER fblock
        self.push_fblock_labels(
            FBlockType::ExceptionGroupHandler,
            ir::InstructionSequenceLabel::NO_LABEL,
            ir::InstructionSequenceLabel::NO_LABEL,
            FBlockDatum::None,
        )?;

        let n = handlers.len();
        for (i, handler) in handlers.iter().enumerate() {
            let ast::ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                type_,
                name,
                body,
                range: handler_range,
                ..
            }) = handler;
            let is_last_handler = i == n - 1;
            self.set_source_range(*handler_range);

            let next_handler_block = self.new_block();
            let except_with_error_block = self.new_block();
            let no_match_block = self.new_block();

            if i == 0 {
                // CPython initializes the except* work stack inside the first
                // handler iteration in codegen_try_star_except().
                emit!(self, Instruction::BuildList { count: 0 });
                emit!(self, Instruction::Copy { i: 2 });
            }

            // Compile exception type. The public-AST validator allows a
            // NULL type here, so codegen only emits CHECK_EG_MATCH when present.
            if let Some(exc_type) = type_ {
                self.compile_expression(exc_type)?;
                self.set_source_range(*handler_range);
            }

            if type_.is_some() {
                // Stack: [prev_exc, orig, list, rest, type]
                // ADDOP(c, loc, CHECK_EG_MATCH);
                emit!(self, Instruction::CheckEgMatch);
                // Stack: [prev_exc, orig, list, new_rest, match]

                // ADDOP_I(c, loc, COPY, 1);
                // ADDOP_JUMP(c, loc, POP_JUMP_IF_NONE, no_match);
                emit!(self, Instruction::Copy { i: 1 });
                emit!(
                    self,
                    Instruction::PopJumpIfNone {
                        delta: no_match_block
                    }
                );
            }

            // Handler matched
            // Stack: [prev_exc, orig, list, new_rest, match]
            // Note: CheckEgMatch already sets the matched exception as current exception
            let cleanup_end_block = self.new_block();
            let cleanup_body_block = self.new_block();

            // Store match to name or pop
            if let Some(alias) = name {
                self.store_name(alias.id())?;
            } else {
                emit!(self, Instruction::PopTop); // pop match
            }
            // Stack: [prev_exc, orig, list, new_rest]

            // HANDLER_CLEANUP fblock for handler body
            emit!(
                self,
                PseudoInstruction::SetupCleanup {
                    delta: cleanup_end_block
                }
            );
            self.use_cpython_label_block(cleanup_body_block);
            let cleanup_body_label = self.instr_sequence_label_for_block(cleanup_body_block);
            self.push_fblock_labels(
                FBlockType::HandlerCleanup,
                cleanup_body_label,
                ir::InstructionSequenceLabel::NO_LABEL,
                if let Some(alias) = name {
                    FBlockDatum::ExceptionName(alias.as_str().to_owned())
                } else {
                    FBlockDatum::None
                },
            )?;

            // Execute handler body
            self.compile_statements(body)?;

            // Handler body completed normally
            self.pop_fblock_label(FBlockType::HandlerCleanup, cleanup_body_label);
            emit!(self, PseudoInstruction::PopBlock);
            self.set_no_location();

            // Cleanup name binding
            if let Some(alias) = name {
                self.emit_no_location_exception_name_cleanup(alias.id())?;
            }

            emit!(
                self,
                PseudoInstruction::JumpNoInterrupt {
                    delta: next_handler_block
                }
            );
            self.set_no_location();

            // Handler raised an exception (cleanup_end label)
            self.use_cpython_label_block(cleanup_end_block);
            // Stack: [prev_exc, orig, list, new_rest, lasti, raised_exc]
            // (lasti is pushed because push_lasti=true in HANDLER_CLEANUP fblock)

            // Cleanup name binding
            if let Some(alias) = name {
                self.emit_no_location_exception_name_cleanup(alias.id())?;
            }

            // LIST_APPEND(3) - append raised_exc to list
            // Stack: [prev_exc, orig, list, new_rest, lasti, raised_exc]
            // After pop: [prev_exc, orig, list, new_rest, lasti] (len=5)
            // nth_value(i) = stack[len - i - 1], we need stack[2] = list
            // stack[5 - i - 1] = 2 -> i = 2
            emit!(self, Instruction::ListAppend { i: 3 });
            self.set_no_location();
            // Stack: [prev_exc, orig, list, new_rest, lasti]

            // POP_TOP - pop lasti
            emit!(self, Instruction::PopTop);
            self.set_no_location();
            // Stack: [prev_exc, orig, list, new_rest]

            emit!(
                self,
                PseudoInstruction::JumpNoInterrupt {
                    delta: except_with_error_block
                }
            );
            self.set_no_location();

            self.use_cpython_label_block(next_handler_block);
            emit!(self, Instruction::Nop);
            self.set_no_location();
            emit!(
                self,
                PseudoInstruction::JumpNoInterrupt {
                    delta: except_with_error_block
                }
            );
            self.set_no_location();

            self.use_cpython_label_block(no_match_block);
            self.set_source_range(*handler_range);
            emit!(self, Instruction::PopTop); // pop match (None)
            // Stack: [prev_exc, orig, list, new_rest]

            self.use_cpython_label_block(except_with_error_block);

            if is_last_handler {
                emit!(self, Instruction::ListAppend { i: 1 });
                self.set_no_location();
                emit!(
                    self,
                    PseudoInstruction::JumpNoInterrupt {
                        delta: reraise_star_block
                    }
                );
                self.set_no_location();
            }
        }

        // Pop EXCEPTION_GROUP_HANDLER fblock
        self.pop_fblock_label(
            FBlockType::ExceptionGroupHandler,
            ir::InstructionSequenceLabel::NO_LABEL,
        );
        let reraise_block = self.new_block();

        // Reraise star block
        self.use_cpython_label_block(reraise_star_block);
        // Stack: [prev_exc, orig, list]

        // CALL_INTRINSIC_2 PREP_RERAISE_STAR
        // Takes 2 args (orig, list) and produces result
        emit!(
            self,
            Instruction::CallIntrinsic2 {
                func: bytecode::IntrinsicFunction2::PrepReraiseStar
            }
        );
        self.set_no_location();
        // Stack: [prev_exc, result]

        // COPY 1
        emit!(self, Instruction::Copy { i: 1 });
        self.set_no_location();
        // Stack: [prev_exc, result, result]

        // POP_JUMP_IF_NOT_NONE reraise
        emit!(
            self,
            Instruction::PopJumpIfNotNone {
                delta: reraise_block
            }
        );
        self.set_no_location();
        // Stack: [prev_exc, result]

        // Nothing to reraise
        // POP_TOP - pop result (None)
        emit!(self, Instruction::PopTop);
        self.set_no_location();
        // Stack: [prev_exc]

        emit!(self, PseudoInstruction::PopBlock);
        self.set_no_location();
        // POP_EXCEPT - restore previous exception context
        emit!(self, Instruction::PopExcept);
        self.set_no_location();
        // Stack: []

        emit!(
            self,
            PseudoInstruction::JumpNoInterrupt { delta: end_block }
        );
        self.set_no_location();

        // Reraise the result
        self.use_cpython_label_block(reraise_block);

        // Stack: [prev_exc, result]
        emit!(self, PseudoInstruction::PopBlock);
        self.set_no_location();
        emit!(self, Instruction::Swap { i: 2 });
        self.set_no_location();
        // Stack: [result, prev_exc]

        // POP_EXCEPT
        emit!(self, Instruction::PopExcept);
        self.set_no_location();
        // Stack: [result]

        // RERAISE 0
        emit!(self, Instruction::Reraise { depth: 0 });
        self.set_no_location();

        self.use_cpython_label_block(cleanup_block);
        emit!(self, Instruction::Copy { i: 3 });
        self.set_no_location();
        emit!(self, Instruction::PopExcept);
        self.set_no_location();
        emit!(self, Instruction::Reraise { depth: 1 });
        self.set_no_location();

        self.use_cpython_label_block(else_block);
        self.compile_statements(orelse)?;

        self.use_cpython_label_block(end_block);

        Ok(())
    }

    /// Compile default arguments
    // = compiler_default_arguments
    fn compile_default_arguments(
        &mut self,
        parameters: &ast::Parameters,
        loc: TextRange,
    ) -> CompileResult<bytecode::MakeFunctionFlags> {
        let mut funcflags = bytecode::MakeFunctionFlags::new();

        // Handle positional defaults
        let defaults: Vec<_> = core::iter::empty()
            .chain(&parameters.posonlyargs)
            .chain(&parameters.args)
            .filter_map(|x| x.default.as_deref())
            .collect();

        if !defaults.is_empty() {
            // Compile defaults and build tuple
            for default in &defaults {
                self.compile_expression(default)?;
            }
            self.set_source_range(loc);
            emit!(
                self,
                Instruction::BuildTuple {
                    count: defaults.len().to_u32()
                }
            );
            funcflags.insert(bytecode::MakeFunctionFlag::Defaults);
        }

        // Handle keyword-only defaults
        let mut kw_with_defaults = vec![];
        for kwonlyarg in &parameters.kwonlyargs {
            if let Some(default) = &kwonlyarg.default {
                kw_with_defaults.push((&kwonlyarg.parameter, default));
            }
        }

        if !kw_with_defaults.is_empty() {
            // Compile kwdefaults and build dict
            for (arg, default) in &kw_with_defaults {
                self.set_source_range(loc);
                self.emit_load_const(ConstantData::Str {
                    value: self.mangle(arg.name().id()).as_str().into(),
                });
                self.compile_expression(default)?;
            }
            self.set_source_range(loc);
            emit!(
                self,
                Instruction::BuildMap {
                    count: kw_with_defaults.len().to_u32(),
                }
            );
            funcflags.insert(bytecode::MakeFunctionFlag::KwOnlyDefaults);
        }

        Ok(funcflags)
    }

    /// Compile function body and create function object
    // = compiler_function_body
    fn compile_function_body(
        &mut self,
        name: &str,
        parameters: &ast::Parameters,
        body: &[ast::Stmt],
        is_async: bool,
        funcflags: bytecode::MakeFunctionFlags,
        closure_range: TextRange,
    ) -> CompileResult<()> {
        // Always enter function scope
        self.enter_function(name, parameters)?;
        self.current_code_info()
            .flags
            .set(bytecode::CodeFlags::COROUTINE, is_async);

        // Set up context
        let prev_ctx = self.ctx;
        self.ctx = CompileContext {
            in_class: prev_ctx.in_class,
            func: if is_async {
                FunctionContext::AsyncFunction
            } else {
                FunctionContext::Function
            },
            // A function starts a new async scope only if it's async
            in_async_scope: is_async,
        };

        // Set qualname
        self.set_qualname();

        // Handle docstring - store in co_consts[0] if present
        let (doc_info, body) = split_doc_with_range(body, &self.opts);
        let doc_str = doc_info.as_ref().map(|(doc, _)| doc);
        if let Some(doc) = &doc_str {
            // Docstring present: store in co_consts[0] and set HAS_DOCSTRING flag
            self.current_code_info()
                .metadata
                .consts
                .insert_full(ConstantData::Str {
                    value: (*doc).to_string().into(),
                });
            self.current_code_info().flags |= bytecode::CodeFlags::HAS_DOCSTRING;
        }

        let start_label = self.use_cpython_function_start_label();

        // PEP 479: Wrap generator/coroutine body with StopIteration handler
        let is_gen = is_async || self.current_symbol_table().is_generator;
        let stop_iteration_block = if is_gen {
            let handler_block = self.new_block();
            self.insert_cpython_stopiteration_setup_cleanup(handler_block);
            self.push_fblock_labels(
                FBlockType::StopIteration,
                start_label,
                ir::InstructionSequenceLabel::NO_LABEL,
                FBlockDatum::None,
            )?;
            Some(handler_block)
        } else {
            None
        };
        // Compile body statements
        self.compile_statements(body)?;

        if stop_iteration_block.is_some() {
            self.emit_return_const_no_location(ConstantData::None);
        }

        // Close StopIteration handler and emit handler code
        if let Some(handler_block) = stop_iteration_block {
            self.pop_fblock_label(FBlockType::StopIteration, start_label);
            self.use_cpython_label_block(handler_block);
            emit!(
                self,
                Instruction::CallIntrinsic1 {
                    func: oparg::IntrinsicFunction1::StopIterationError
                }
            );
            self.set_no_location();
            emit!(self, Instruction::Reraise { depth: 1u32 });
            self.set_no_location();
        }
        self.emit_return_const_no_location(ConstantData::None);

        // Exit scope and create function object
        let code = self.exit_scope();
        self.ctx = prev_ctx;

        self.set_source_range(closure_range);

        // Create function object with closure
        self.make_closure(code, funcflags)?;

        // Note: docstring is now retrieved from co_consts[0] by the VM
        // when HAS_DOCSTRING flag is set, so no runtime __doc__ assignment needed

        Ok(())
    }

    /// Compile function annotations as a closure (PEP 649)
    /// Returns true if an __annotate__ closure was created
    /// Uses the matching annotation symbol table for proper scoping.
    fn compile_annotations_closure(
        &mut self,
        func_name: &str,
        parameters: &ast::Parameters,
        returns: Option<&ast::Expr>,
        func_range: TextRange,
    ) -> CompileResult<bool> {
        if !self.next_function_annotation_symbol_table_uses_annotations() {
            // CPython creates a hidden AnnotationBlock for every function
            // signature under `from __future__ import annotations`, including
            // an unannotated one.  It still belongs to this function: consume
            // it so the next function sees its own block rather than remaining
            // pinned to this unused entry.
            if self.push_annotation_symbol_table() {
                self.pop_annotation_symbol_table();
            }
            return Ok(false);
        }

        // Try to enter annotation scope - returns None if no matching symbol table exists.
        let Some(saved_ctx) = self.enter_annotation_scope(func_name, func_range)? else {
            return Ok(false);
        };

        // Count annotations
        let parameters_iter = parameters
            .args
            .iter()
            .map(|x| &x.parameter)
            .chain(parameters.posonlyargs.iter().map(|x| &x.parameter))
            .chain(parameters.vararg.as_deref())
            .chain(parameters.kwonlyargs.iter().map(|x| &x.parameter))
            .chain(parameters.kwarg.as_deref());

        let num_annotations =
            u32::try_from(parameters_iter.filter(|p| p.annotation.is_some()).count())
                .expect("too many annotations")
                + u32::from(returns.is_some());

        // Compile annotations inside the annotation scope
        let parameters_iter = parameters
            .args
            .iter()
            .map(|x| &x.parameter)
            .chain(parameters.posonlyargs.iter().map(|x| &x.parameter))
            .chain(parameters.vararg.as_deref())
            .chain(parameters.kwonlyargs.iter().map(|x| &x.parameter))
            .chain(parameters.kwarg.as_deref());

        for param in parameters_iter {
            if let Some(annotation) = &param.annotation {
                self.set_source_range(func_range);
                self.emit_load_const(ConstantData::Str {
                    value: self.mangle(param.name.id()).as_str().into(),
                });
                self.compile_annotation(annotation)?;
            }
        }

        // Handle return annotation
        if let Some(annotation) = returns {
            self.set_source_range(func_range);
            self.emit_load_const(ConstantData::Str {
                value: "return".into(),
            });
            self.compile_annotation(annotation)?;
        }

        // Build the map and return it
        self.set_source_range(func_range);
        emit!(
            self,
            Instruction::BuildMap {
                count: num_annotations,
            }
        );
        emit!(self, Instruction::ReturnValue);
        self.emit_return_const_no_location(ConstantData::None);

        // Exit the annotation scope and get the code object
        let annotate_code = self.exit_annotation_scope(saved_ctx);

        // Make a closure from the code object
        self.set_source_range(func_range);
        self.make_closure(annotate_code, bytecode::MakeFunctionFlags::new())?;

        Ok(true)
    }

    /// Collect annotated assignments from module/class body in AST order
    /// (including nested conditional blocks). This preserves the same walk
    /// order as symbol-table construction so the annotation scope's
    /// `sub_tables` cursor stays aligned.
    fn collect_annotations(
        body: &[ast::Stmt],
        parent_scope_type: CompilerScope,
    ) -> Vec<(&ast::StmtAnnAssign, bool)> {
        fn walk<'a>(
            stmts: &'a [ast::Stmt],
            out: &mut Vec<(&'a ast::StmtAnnAssign, bool)>,
            in_conditional_block: bool,
            module_scope: bool,
        ) {
            for stmt in stmts {
                match stmt {
                    ast::Stmt::AnnAssign(stmt) => {
                        out.push((stmt, module_scope || in_conditional_block));
                    }
                    ast::Stmt::If(ast::StmtIf {
                        body,
                        elif_else_clauses,
                        ..
                    }) => {
                        walk(body, out, true, module_scope);
                        for clause in elif_else_clauses {
                            walk(&clause.body, out, true, module_scope);
                        }
                    }
                    ast::Stmt::For(ast::StmtFor { body, orelse, .. })
                    | ast::Stmt::While(ast::StmtWhile { body, orelse, .. }) => {
                        walk(body, out, true, module_scope);
                        walk(orelse, out, true, module_scope);
                    }
                    ast::Stmt::With(ast::StmtWith { body, .. }) => {
                        walk(body, out, true, module_scope);
                    }
                    ast::Stmt::Try(ast::StmtTry {
                        body,
                        handlers,
                        orelse,
                        finalbody,
                        ..
                    }) => {
                        walk(body, out, true, module_scope);
                        for handler in handlers {
                            let ast::ExceptHandler::ExceptHandler(
                                ast::ExceptHandlerExceptHandler { body, .. },
                            ) = handler;
                            walk(body, out, true, module_scope);
                        }
                        walk(orelse, out, true, module_scope);
                        walk(finalbody, out, true, module_scope);
                    }
                    ast::Stmt::Match(ast::StmtMatch { cases, .. }) => {
                        for case in cases {
                            walk(&case.body, out, true, module_scope);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut annotations = Vec::new();
        walk(
            body,
            &mut annotations,
            false,
            parent_scope_type == CompilerScope::Module,
        );
        annotations
    }

    fn compile_annotation_for_symbol_cursor_only(
        &mut self,
        annotation: &ast::Expr,
    ) -> CompileResult<()> {
        self.consume_skipped_nested_scopes_in_expr(annotation)
    }

    /// Compile module-level __annotate__ function (PEP 649)
    /// Returns true if __annotate__ was created and stored
    fn compile_module_annotate(
        &mut self,
        body: &[ast::Stmt],
        loc: Option<TextRange>,
    ) -> CompileResult<bool> {
        let loc = loc.unwrap_or(self.current_source_range);
        // Get parent scope type BEFORE pushing annotation symbol table.
        let parent_scope_type = self.current_symbol_table().typ;
        let annotations = Self::collect_annotations(body, parent_scope_type);
        let simple_annotation_count = annotations
            .iter()
            .filter(|(stmt, _)| stmt.simple && matches!(stmt.target.as_ref(), ast::Expr::Name(_)))
            .count();

        if simple_annotation_count == 0 {
            return Ok(false);
        }

        // Check if we have conditional annotations
        let has_conditional = self.current_symbol_table().has_conditional_annotations;

        // Try to push annotation symbol table from current scope
        if !self.push_current_annotation_symbol_table() {
            return Ok(false);
        }

        // Annotation scopes are never async (even inside async functions)
        let saved_ctx = self.ctx;
        self.ctx = CompileContext {
            in_class: saved_ctx.in_class,
            func: FunctionContext::Function,
            in_async_scope: false,
        };

        // Enter annotation scope for code generation
        self.set_source_range(loc);
        let key = self.symbol_table_stack.len() - 1;
        let lineno = self.get_source_line_number().get();
        self.enter_scope(
            "__annotate__",
            CompilerScope::Annotation,
            key,
            lineno.to_u32(),
        )?;

        // Keep the internal ".format" name; the final code object
        // exposes this parameter as "format".
        self.configure_annotation_format_parameter();

        // Emit format validation: if format > VALUE_WITH_FAKE_GLOBALS: raise NotImplementedError
        self.emit_format_validation();

        self.set_source_range(loc);
        emit!(self, Instruction::BuildMap { count: 0 });

        let mut conditional_idx = 0usize;
        for (stmt, is_conditional) in annotations {
            let ast::StmtAnnAssign {
                target,
                annotation,
                simple,
                range,
                ..
            } = stmt;
            let simple_name = if *simple {
                match target.as_ref() {
                    ast::Expr::Name(ast::ExprName { id, .. }) => Some(id),
                    _ => None,
                }
            } else {
                None
            };

            if simple_name.is_none() {
                if !self.future_annotations {
                    self.compile_annotation_for_symbol_cursor_only(annotation)?;
                }
                continue;
            }

            let not_set_block = (has_conditional && is_conditional).then(|| self.new_block());
            let not_set_label = (!has_conditional || !is_conditional)
                .then(|| self.current_code_info().new_instr_sequence_label());
            let name = simple_name.expect("missing simple annotation name");

            if let Some(not_set_block) = not_set_block {
                self.set_source_range(*range);
                self.emit_load_const(ConstantData::Integer {
                    value: conditional_idx.into(),
                });
                conditional_idx += 1;
                if parent_scope_type == CompilerScope::Class {
                    let idx = self.get_free_var_index("__conditional_annotations__");
                    emit!(self, Instruction::LoadDeref { i: idx });
                } else {
                    let cond_annotations_name = self.name("__conditional_annotations__");
                    self.emit_load_global(cond_annotations_name, false);
                }
                emit!(
                    self,
                    Instruction::ContainsOp {
                        invert: bytecode::Invert::No
                    }
                );
                emit!(
                    self,
                    Instruction::PopJumpIfFalse {
                        delta: not_set_block
                    }
                );
            }

            self.compile_annotation(annotation)?;
            self.set_source_range(*range);
            emit!(self, Instruction::Copy { i: 2 });
            self.emit_load_const(ConstantData::Str {
                value: self.mangle(name).as_str().into(),
            });
            self.set_source_range(loc);
            emit!(self, Instruction::StoreSubscr);

            if let Some(not_set_block) = not_set_block {
                self.use_cpython_label_block(not_set_block);
            } else if let Some(not_set_label) = not_set_label {
                let result = self
                    .current_code_info()
                    .use_raw_instr_sequence_label(not_set_label);
                unwrap_internal(self, result);
            }
        }

        self.set_source_range(loc);
        emit!(self, Instruction::ReturnValue);
        self.emit_return_const_no_location(ConstantData::None);

        // Exit annotation scope - pop symbol table, restore to parent's annotation_block, and get code
        let annotation_table = self.pop_symbol_table();
        // Restore annotation_block to module's symbol table
        self.symbol_table_stack
            .last_mut()
            .expect("no module symbol table")
            .annotation_block = Some(Box::new(annotation_table));
        // Restore context
        self.ctx = saved_ctx;
        // Exit code scope
        let pop = self.code_stack.pop();
        let mut annotate_code = unwrap_internal(
            self,
            compiler_unwrap_option(self, pop).finalize_code(&self.opts),
        );
        Self::expose_annotation_format_parameter(&mut annotate_code);

        // Make a closure from the code object
        self.set_source_range(loc);
        self.make_closure(annotate_code, bytecode::MakeFunctionFlags::new())?;

        // Store as __annotate_func__ for classes, __annotate__ for modules
        let name = if parent_scope_type == CompilerScope::Class {
            "__annotate_func__"
        } else {
            "__annotate__"
        }
        .into();
        self.set_source_range(loc);
        self.store_name(&name)?;

        Ok(true)
    }

    // = compiler_function
    #[expect(clippy::too_many_arguments, reason = "ignore warning for now")]
    fn compile_function_def(
        &mut self,
        name: &Name,
        parameters: &ast::Parameters,
        body: &[ast::Stmt],
        decorator_list: &[ast::Decorator],
        returns: Option<&ast::Expr>, // TODO: use type hint somehow..
        is_async: bool,
        type_params: Option<&ast::TypeParams>,
        preserve_value_before_store: bool,
    ) -> CompileResult<()> {
        // CPython's FunctionDef/AsyncFunctionDef LOC(s) starts at the
        // definition line even when decorators are present.
        let stmt_source_range = self.current_source_range;
        let def_source_range = crate::decorated_definition_range(
            &self.source_file,
            stmt_source_range,
            decorator_list,
            if is_async { "async def " } else { "def " },
        );

        // The symtable visits defaults before decorators, but
        // codegen_function() emits decorators first. Keep the symbol table in
        // symtable order and only move the codegen cursor while compiling
        // decorators.
        let defaults_symbol_table_cursors = self.current_symbol_table_cursors();
        self.consume_skipped_nested_scopes_in_parameter_defaults(parameters)?;
        self.prepare_decorators(decorator_list)?;
        let after_decorators_symbol_table_cursors = self.current_symbol_table_cursors();
        self.set_symbol_table_cursors(defaults_symbol_table_cursors);

        // The first decorator line is used for code objects created by
        // this definition, but LOC(s) for the surrounding instructions.
        let firstlineno_range = decorator_list
            .first()
            .map_or(stmt_source_range, |decorator| decorator.expression.range());

        // compile defaults and return funcflags
        let funcflags = self.compile_default_arguments(parameters, def_source_range)?;
        self.set_symbol_table_cursors(after_decorators_symbol_table_cursors);

        let is_generic = type_params.is_some();
        let mut num_typeparam_args = 0u32;

        // Save context before entering TypeParams scope
        let saved_ctx = self.ctx;

        if is_generic {
            // Count args to pass to type params scope
            if funcflags.contains(&bytecode::MakeFunctionFlag::Defaults) {
                num_typeparam_args += 1;
            }
            if funcflags.contains(&bytecode::MakeFunctionFlag::KwOnlyDefaults) {
                num_typeparam_args += 1;
            }
            if num_typeparam_args == 2 {
                self.set_source_range(def_source_range);
                emit!(self, Instruction::Swap { i: 2 });
            }

            // Enter type params scope
            let type_params_name = format!("<generic parameters of {name}>");
            self.set_source_range(firstlineno_range);
            self.push_output(
                bytecode::CodeFlags::OPTIMIZED | bytecode::CodeFlags::NEWLOCALS,
                0,
                num_typeparam_args,
                0,
                &type_params_name,
            )?;

            // TypeParams scope is function-like
            self.ctx = CompileContext {
                in_class: saved_ctx.in_class,
                func: FunctionContext::Function,
                in_async_scope: false,
            };

            // Add parameter names to varnames for the type params scope
            // These will be passed as arguments when the closure is called.
            // `.defaults` is there whether or not the function has any: the
            // symbol table gives every generic function's type params scope
            // one. `.kwdefaults` only appears when it is really passed.
            let current_info = self.current_code_info();
            current_info
                .metadata
                .varnames
                .insert(".defaults".to_owned());
            if funcflags.contains(&bytecode::MakeFunctionFlag::KwOnlyDefaults) {
                current_info
                    .metadata
                    .varnames
                    .insert(".kwdefaults".to_owned());
            }

            // Compile type parameters
            self.compile_type_params(type_params.unwrap())?;

            // Load defaults/kwdefaults with LOAD_FAST
            self.set_source_range(def_source_range);
            for i in 0..num_typeparam_args {
                let var_num = oparg::VarNum::from(i);
                emit!(self, Instruction::LoadFast { var_num });
            }
        }

        // Compile annotations as closure (PEP 649)
        let mut annotations_flag = bytecode::MakeFunctionFlags::new();
        if self.compile_annotations_closure(name, parameters, returns, def_source_range)? {
            annotations_flag.insert(bytecode::MakeFunctionFlag::Annotate);
        }

        // Compile function body. codegen_function() uses the first
        // decorator line for co_firstlineno, but LOC(s) for MAKE_FUNCTION.
        self.set_source_range(firstlineno_range);
        let final_funcflags = funcflags | annotations_flag;
        self.compile_function_body(
            name,
            parameters,
            body,
            is_async,
            final_funcflags,
            def_source_range,
        )?;

        // Handle type params if present
        if is_generic {
            // SWAP to get function on top
            // Stack: [type_params_tuple, function] -> [function, type_params_tuple]
            self.set_source_range(def_source_range);
            emit!(self, Instruction::Swap { i: 2 });

            // Call INTRINSIC_SET_FUNCTION_TYPE_PARAMS
            self.set_source_range(def_source_range);
            emit!(
                self,
                Instruction::CallIntrinsic2 {
                    func: bytecode::IntrinsicFunction2::SetFunctionTypeParams,
                }
            );

            // Return the function object from type params scope
            emit!(self, Instruction::ReturnValue);
            self.set_no_location();

            // Set argcount for type params scope
            self.current_code_info().metadata.argcount = num_typeparam_args;
            self.current_code_info().nparams = num_typeparam_args as usize;

            // Exit type params scope and create closure
            let type_params_code = self.exit_scope();
            self.ctx = saved_ctx;

            // Make closure for type params code
            self.set_source_range(def_source_range);
            self.make_closure(type_params_code, bytecode::MakeFunctionFlags::new())?;

            if num_typeparam_args > 0 {
                self.set_source_range(def_source_range);
                emit!(
                    self,
                    Instruction::Swap {
                        i: num_typeparam_args + 1
                    }
                );
                self.set_source_range(def_source_range);
                emit!(
                    self,
                    Instruction::Call {
                        argc: num_typeparam_args - 1
                    }
                );
            } else {
                // Stack: [closure]
                self.set_source_range(def_source_range);
                emit!(self, Instruction::PushNull);
                // Stack: [closure, NULL]
                self.set_source_range(def_source_range);
                emit!(self, Instruction::Call { argc: 0 });
            }
        }

        // Apply decorators
        self.apply_decorators(decorator_list);

        // Store the function
        self.set_source_range(def_source_range);
        if preserve_value_before_store {
            emit!(self, Instruction::Copy { i: 1 });
        }
        self.store_name(name)?;

        Ok(())
    }

    /// Determines if a variable should be CELL or FREE type
    // = get_ref_type
    fn get_ref_type(&self, name: &Name) -> Result<SymbolScope, CodegenErrorType> {
        let table = self.symbol_table_stack.last().unwrap();

        // Special handling for __class__, __classdict__, and __conditional_annotations__ in class scope
        // This should only apply when we're actually IN a class body,
        // not when we're in a method nested inside a class.
        if table.typ == CompilerScope::Class
            && (name == "__class__"
                || name == "__classdict__"
                || name == "__conditional_annotations__")
        {
            return Ok(SymbolScope::Cell);
        }
        match table.lookup(name) {
            Some(symbol) => match symbol.scope {
                SymbolScope::Cell => Ok(SymbolScope::Cell),
                SymbolScope::Free => Ok(SymbolScope::Free),
                _ if symbol.flags.contains(SymbolFlags::DEF_FREE_CLASS) => Ok(SymbolScope::Free),
                _ => Err(CodegenErrorType::SyntaxError(format!(
                    "get_ref_type: invalid scope for '{name}'"
                ))),
            },
            None => Err(CodegenErrorType::SyntaxError(format!(
                "get_ref_type: cannot find symbol '{name}'"
            ))),
        }
    }

    /// Loads closure variables if needed and creates a function object
    // = compiler_make_closure
    fn make_closure(
        &mut self,
        code: CodeObject,
        flags: bytecode::MakeFunctionFlags,
    ) -> CompileResult<()> {
        // Handle free variables (closure)
        let has_freevars = !code.freevars.is_empty();
        if has_freevars {
            // Build closure tuple by loading free variables

            for var in &code.freevars {
                // Special case: If a class contains a method with a
                // free variable that has the same name as a method,
                // the name will be considered free *and* local in the
                // class. It should be handled by the closure, as
                // well as by the normal name lookup logic.

                // Get reference type using our get_ref_type function
                let ref_type = self
                    .get_ref_type(&var.as_str().into())
                    .map_err(|e| self.error(e))?;

                // Get parent code info
                let parent_code = self.code_stack.last().unwrap();
                let cellvars_len = parent_code.metadata.cellvars.len();

                // Look up the variable index based on reference type
                let idx = match ref_type {
                    SymbolScope::Cell => parent_code
                        .metadata
                        .cellvars
                        .get_index_of(var)
                        .or_else(|| {
                            parent_code
                                .metadata
                                .freevars
                                .get_index_of(var)
                                .map(|i| i + cellvars_len)
                        })
                        .ok_or_else(|| {
                            self.error(CodegenErrorType::SyntaxError(format!(
                                "compiler_make_closure: cannot find '{var}' in parent vars",
                            )))
                        })?,
                    SymbolScope::Free => parent_code
                        .metadata
                        .freevars
                        .get_index_of(var)
                        .map(|i| i + cellvars_len)
                        .or_else(|| parent_code.metadata.cellvars.get_index_of(var))
                        .ok_or_else(|| {
                            self.error(CodegenErrorType::SyntaxError(format!(
                                "compiler_make_closure: cannot find '{var}' in parent vars",
                            )))
                        })?,
                    _ => {
                        return Err(self.error(CodegenErrorType::SyntaxError(format!(
                            "compiler_make_closure: unexpected ref_type {ref_type:?} for '{var}'",
                        ))));
                    }
                };

                emit!(self, PseudoInstruction::LoadClosure { i: idx.to_u32() });
            }

            // Build tuple of closure variables
            emit!(
                self,
                Instruction::BuildTuple {
                    count: code.freevars.len().to_u32(),
                }
            );
        }

        // load code object and create function
        self.emit_load_const(ConstantData::Code {
            code: Box::new(code),
        });

        // Create function with no flags
        emit!(self, Instruction::MakeFunction);

        // Now set attributes one by one using SET_FUNCTION_ATTRIBUTE
        // Note: The order matters! Values must be on stack before calling SET_FUNCTION_ATTRIBUTE

        // Set closure if needed
        if has_freevars {
            emit!(
                self,
                Instruction::SetFunctionAttribute {
                    flag: bytecode::MakeFunctionFlag::Closure
                }
            );
        }

        // Set annotations if present
        if flags.contains(&bytecode::MakeFunctionFlag::Annotations) {
            emit!(
                self,
                Instruction::SetFunctionAttribute {
                    flag: bytecode::MakeFunctionFlag::Annotations
                }
            );
        }

        // Set __annotate__ closure if present (PEP 649)
        if flags.contains(&bytecode::MakeFunctionFlag::Annotate) {
            emit!(
                self,
                Instruction::SetFunctionAttribute {
                    flag: bytecode::MakeFunctionFlag::Annotate
                }
            );
        }

        // Set kwdefaults if present
        if flags.contains(&bytecode::MakeFunctionFlag::KwOnlyDefaults) {
            emit!(
                self,
                Instruction::SetFunctionAttribute {
                    flag: bytecode::MakeFunctionFlag::KwOnlyDefaults
                }
            );
        }

        // Set defaults if present
        if flags.contains(&bytecode::MakeFunctionFlag::Defaults) {
            emit!(
                self,
                Instruction::SetFunctionAttribute {
                    flag: bytecode::MakeFunctionFlag::Defaults
                }
            );
        }

        Ok(())
    }

    // Python/compile.c _PyCompile_MaybeAddStaticAttributeToClass
    fn maybe_add_static_attribute_to_class(&mut self, value: &ast::Expr, attr: &str) {
        if !matches!(value, ast::Expr::Name(n) if n.id.as_str() == "self") {
            return;
        }
        if let Some(class_unit) = self
            .code_stack
            .iter_mut()
            .rev()
            .find(|unit| unit.static_attributes.is_some())
        {
            class_unit
                .static_attributes
                .as_mut()
                .unwrap()
                .insert(attr.to_owned());
        }
    }

    /// Compile the class body into a code object
    // = compiler_class_body
    fn compile_class_body(
        &mut self,
        name: &str,
        body: &[ast::Stmt],
        type_params: Option<&ast::TypeParams>,
        firstlineno: u32,
    ) -> CompileResult<CodeObject> {
        // 1. Enter class scope
        let key = self.symbol_table_stack.len();
        self.push_symbol_table_matching(CompilerScope::Class, name)?;
        self.enter_scope(name, CompilerScope::Class, key, firstlineno)?;

        // Set qualname using the new method
        let qualname = self.set_qualname();

        // For class scopes, set u_private to the class name for name mangling
        self.code_stack.last_mut().unwrap().private = Some(name.to_owned());

        // 2. Set up class namespace
        let (doc_str, body) = split_doc_with_range(body, &self.opts);
        let class_body_prefix_range = self.source_line_start_range(firstlineno);
        self.set_source_range(class_body_prefix_range);

        // Load __name__ and store as __module__
        self.load_name(&"__name__".into())?;
        self.store_name(&"__module__".into())?;

        // Store __qualname__
        self.emit_load_const(ConstantData::Str {
            value: qualname.into(),
        });
        self.store_name(&"__qualname__".into())?;

        // Store __firstlineno__ before __doc__
        self.emit_load_const(ConstantData::Integer {
            value: BigInt::from(firstlineno),
        });
        self.store_name(&"__firstlineno__".into())?;

        // Set __type_params__ from the enclosing type-params closure when
        // compiling a generic class body.
        if type_params.is_some() {
            self.load_name(&".type_params".into())?;
            self.store_name(&"__type_params__".into())?;
        }

        // PEP 649: Initialize __classdict__ after synthetic generic-class
        // setup so nested generic classes match CPython's prologue order.
        if self.current_symbol_table().needs_classdict {
            emit!(self, Instruction::LoadLocals);
            let classdict_idx = self.get_cell_var_index("__classdict__");
            emit!(self, Instruction::StoreDeref { i: classdict_idx });
        }

        // Handle class annotation bookkeeping in CPython order.
        let annotations_used = self.current_symbol_table().annotations_used;
        if Self::scope_needs_conditional_annotations_cell(self.current_symbol_table()) {
            emit!(self, Instruction::BuildSet { count: 0 });
            self.store_name(&"__conditional_annotations__".into())?;
        }

        if self.future_annotations && annotations_used {
            emit!(self, Instruction::SetupAnnotations);
        }

        // Store __doc__ only if there's an explicit docstring.
        if let Some((doc, range)) = doc_str {
            let saved_range = self.current_source_range;
            self.set_source_range(range);
            self.emit_load_const(ConstantData::Str { value: doc.into() });
            self.store_name(&"__doc__".into())?;
            self.set_no_location();
            self.set_source_range(saved_range);
        }

        // 3. Compile the class body
        self.compile_statements(body)?;

        if annotations_used && !self.future_annotations {
            self.compile_module_annotate(body, Some(class_body_prefix_range))?;
        }

        // 4. Handle __classcell__ if needed
        let classcell_idx = self
            .code_stack
            .last_mut()
            .unwrap()
            .metadata
            .cellvars
            .iter()
            .position(|var| *var == "__class__");

        // Emit __static_attributes__ tuple
        {
            let mut attrs: Vec<String> = self
                .code_stack
                .last()
                .unwrap()
                .static_attributes
                .as_ref()
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            attrs.sort();
            self.emit_load_const(ConstantData::Tuple {
                elements: attrs
                    .into_iter()
                    .map(|s| ConstantData::Str { value: s.into() })
                    .collect(),
            });
            self.set_no_location();
            self.store_name(&"__static_attributes__".into())?;
            self.set_no_location();
        }

        // Store __classdictcell__ if __classdict__ is a cell variable
        if self.current_symbol_table().needs_classdict {
            let classdict_idx = u32::from(self.get_cell_var_index("__classdict__"));
            emit!(self, PseudoInstruction::LoadClosure { i: classdict_idx });
            self.set_no_location();
            self.store_name(&"__classdictcell__".into())?;
            self.set_no_location();
        }

        if let Some(classcell_idx) = classcell_idx {
            emit!(
                self,
                PseudoInstruction::LoadClosure {
                    i: classcell_idx.to_u32()
                }
            );
            self.set_no_location();
            emit!(self, Instruction::Copy { i: 1 });
            self.set_no_location();
            self.store_name(&"__classcell__".into())?;
            self.set_no_location();
        } else {
            self.emit_load_const(ConstantData::None);
            self.set_no_location();
        }

        // Return the class namespace
        self.emit_return_value();
        self.set_no_location();
        self.emit_return_const_no_location(ConstantData::None);

        // Exit scope and return the code object
        Ok(self.exit_scope())
    }

    fn compile_class_def(
        &mut self,
        name: &Name,
        body: &[ast::Stmt],
        decorator_list: &[ast::Decorator],
        type_params: Option<&ast::TypeParams>,
        arguments: Option<&ast::Arguments>,
        preserve_value_before_store: bool,
    ) -> CompileResult<()> {
        // CPython's ClassDef LOC(s) starts at the class line even when
        // decorators are present.
        let stmt_source_range = self.current_source_range;
        let class_source_range = crate::decorated_definition_range(
            &self.source_file,
            stmt_source_range,
            decorator_list,
            "class ",
        );
        self.prepare_decorators(decorator_list)?;

        let is_generic = type_params.is_some();
        let firstlineno_range = decorator_list
            .first()
            .map_or(stmt_source_range, |decorator| decorator.expression.range());
        #[expect(clippy::map_unwrap_or, reason = "Changing this will not compile")]
        let firstlineno = decorator_list
            .first()
            .map(|decorator| {
                self.source_file
                    .to_source_code()
                    .line_index(decorator.expression.range().start())
                    .get()
                    .to_u32()
            })
            .unwrap_or_else(|| self.get_source_line_number().get().to_u32());

        // Save context before entering any scopes
        let saved_ctx = self.ctx;

        // Step 1: If generic, enter type params scope and compile type params
        if is_generic {
            let type_params_name = format!("<generic parameters of {name}>");
            self.set_source_range(firstlineno_range);
            self.push_output(
                bytecode::CodeFlags::OPTIMIZED | bytecode::CodeFlags::NEWLOCALS,
                0,
                0,
                0,
                &type_params_name,
            )?;

            // Set private name for name mangling
            self.code_stack.last_mut().unwrap().private = Some(name.as_str().to_owned());

            // TypeParams scope is function-like
            self.ctx = CompileContext {
                in_class: saved_ctx.in_class,
                func: FunctionContext::Function,
                in_async_scope: false,
            };

            // Compile type parameters and store them in the synthetic cell that
            // generic class bodies close over.
            self.compile_type_params(type_params.unwrap())?;
            self.set_source_range(class_source_range);
            self.store_name(&".type_params".into())?;
        }

        // Step 2: Compile class body (always done, whether generic or not)
        let prev_ctx = self.ctx;
        self.ctx = CompileContext {
            func: FunctionContext::NoFunction,
            in_class: true,
            in_async_scope: false,
        };
        let pre_class_body_symbol_table_cursors = self.current_symbol_table_cursors();
        let class_code = self.compile_class_body(name, body, type_params, firstlineno)?;
        let post_class_body_symbol_table_cursors = self.current_symbol_table_cursors();
        self.set_symbol_table_cursors(pre_class_body_symbol_table_cursors);
        self.ctx = prev_ctx;
        self.set_source_range(class_source_range);

        // Step 3: Generate the rest of the code for the call
        if is_generic {
            // Generate class creation code
            emit!(self, Instruction::LoadBuildClass);
            emit!(self, Instruction::PushNull);

            // Create the class body function with the .type_params closure
            // captured through the class code object's freevars.
            self.make_closure(class_code, bytecode::MakeFunctionFlags::new())?;
            self.emit_load_const(ConstantData::Str {
                value: name.as_str().into(),
            });

            // Create .generic_base after the class function and name are on the
            // stack so the remaining call shape matches CPython's ordering.
            self.set_source_range(class_source_range);
            self.load_name(&".type_params".into())?;
            emit!(
                self,
                Instruction::CallIntrinsic1 {
                    func: bytecode::IntrinsicFunction1::SubscriptGeneric
                }
            );
            self.set_source_range(class_source_range);
            self.store_name(&".generic_base".into())?;

            let (bases, keywords) = arguments.map_or((&[][..], &[][..]), |args| {
                (&args.args[..], &args.keywords[..])
            });
            self.codegen_call_helper_impl(
                2,
                bases,
                keywords,
                class_source_range,
                None,
                Some(&".generic_base".into()),
            )?;

            // Return the created class
            self.emit_return_value();
            self.set_no_location();

            // Exit type params scope and wrap in function
            let type_params_code = self.exit_scope();
            self.ctx = saved_ctx;

            // Execute the type params function
            self.set_source_range(class_source_range);
            self.make_closure(type_params_code, bytecode::MakeFunctionFlags::new())?;
            self.set_source_range(class_source_range);
            emit!(self, Instruction::PushNull);
            self.set_source_range(class_source_range);
            emit!(self, Instruction::Call { argc: 0 });
        } else {
            // Non-generic class: standard path
            emit!(self, Instruction::LoadBuildClass);
            emit!(self, Instruction::PushNull);

            // Create class function with closure
            self.make_closure(class_code, bytecode::MakeFunctionFlags::new())?;
            self.emit_load_const(ConstantData::Str {
                value: name.as_str().into(),
            });

            if let Some(arguments) = arguments {
                self.codegen_call_helper(2, arguments, class_source_range, None)?;
            } else {
                self.set_source_range(class_source_range);
                emit!(self, Instruction::Call { argc: 2 });
            }
            self.set_symbol_table_cursors(post_class_body_symbol_table_cursors);
        }

        // Step 4: Apply decorators and store (common to both paths)
        self.apply_decorators(decorator_list);
        self.set_source_range(class_source_range);
        if preserve_value_before_store {
            emit!(self, Instruction::Copy { i: 1 });
        }
        self.store_name(name)
    }

    /// Compile an if statement with constant condition elimination.
    /// = compiler_if in CPython codegen.c
    fn compile_if(
        &mut self,
        test: &ast::Expr,
        body: &[ast::Stmt],
        elif_else_clauses: &[ast::ElifElseClause],
        stmt_range: TextRange,
    ) -> CompileResult<()> {
        let end_block = self.new_block();
        let next_block = if elif_else_clauses.is_empty() {
            end_block
        } else {
            self.new_block()
        };

        self.compile_jump_if_inner(test, false, next_block, Some(stmt_range))?;
        self.compile_statements(body)?;

        let Some((clause, rest)) = elif_else_clauses.split_first() else {
            self.use_cpython_label_block(end_block);
            return Ok(());
        };

        emit!(
            self,
            PseudoInstruction::JumpNoInterrupt { delta: end_block }
        );
        self.set_no_location();
        self.use_cpython_label_block(next_block);

        if let Some(test) = &clause.test {
            self.compile_if(test, &clause.body, rest, clause.range)?;
        } else {
            debug_assert!(rest.is_empty());
            self.compile_statements(&clause.body)?;
        }
        self.use_cpython_label_block(end_block);
        Ok(())
    }

    fn compile_while(
        &mut self,
        test: &ast::Expr,
        body: &[ast::Stmt],
        orelse: &[ast::Stmt],
        while_range: TextRange,
    ) -> CompileResult<()> {
        self.enter_conditional_block();

        let loop_block = self.new_block();
        let end_block = self.new_block();
        let anchor_block = self.new_block();
        let loop_label = self.instr_sequence_label_for_block(loop_block);
        let end_label = self.instr_sequence_label_for_block(end_block);
        self.use_cpython_label_block(loop_block);
        self.push_fblock_labels(
            FBlockType::WhileLoop,
            loop_label,
            end_label,
            FBlockDatum::None,
        )?;
        self.compile_jump_if_inner(test, false, anchor_block, Some(while_range))?;

        self.compile_loop_body_statements(body)?;
        emit!(self, PseudoInstruction::Jump { delta: loop_block });
        self.set_no_location();

        self.pop_fblock_label(FBlockType::WhileLoop, loop_label);
        self.use_cpython_label_block(anchor_block);
        self.compile_statements(orelse)?;
        self.use_cpython_label_block(end_block);

        self.leave_conditional_block();
        Ok(())
    }

    fn compile_with(
        &mut self,
        items: &[ast::WithItem],
        body: &[ast::Stmt],
        is_async: bool,
    ) -> CompileResult<()> {
        self.enter_conditional_block();
        let result = self.compile_with_inner(items, body, is_async);
        self.leave_conditional_block();
        result
    }

    fn compile_with_inner(
        &mut self,
        items: &[ast::WithItem],
        body: &[ast::Stmt],
        is_async: bool,
    ) -> CompileResult<()> {
        // Python 3.12+ style with statement:
        //
        // BEFORE_WITH          # TOS: ctx_mgr -> [__exit__, __enter__ result]
        // L1: STORE_NAME f     # exception table: L1 to L2 -> L3 [1] lasti
        // L2: ... body ...
        //     LOAD_CONST None  # normal exit
        //     LOAD_CONST None
        //     LOAD_CONST None
        //     CALL 2           # __exit__(None, None, None)
        //     POP_TOP
        //     JUMP after
        // L3: PUSH_EXC_INFO    # exception handler
        //     WITH_EXCEPT_START # call __exit__(type, value, tb), push result
        //     TO_BOOL
        //     POP_JUMP_IF_TRUE suppress
        //     RERAISE 2
        // suppress:
        //     POP_TOP          # pop exit result
        // L5: POP_EXCEPT
        //     POP_TOP          # pop __exit__
        //     POP_TOP          # pop prev_exc (or lasti depending on layout)
        //     JUMP after
        // L6: COPY 3           # cleanup handler for reraise
        //     POP_EXCEPT
        //     RERAISE 1
        // after: ...

        let Some((item, items)) = items.split_first() else {
            return Err(self.error(CodegenErrorType::EmptyWithItems));
        };
        let with_range = item.context_expr.range();

        let body_block = self.new_block();
        let exc_handler_block = self.new_block();
        let after_block = self.new_block();
        let cleanup_block = self.new_block();

        // Compile context expression and load __enter__/__exit__ methods
        self.compile_expression(&item.context_expr)?;
        self.set_source_range(with_range);

        // Stack: [cm]
        emit!(self, Instruction::Copy { i: 1 }); // [cm, cm]

        if is_async {
            if self.ctx.func != FunctionContext::AsyncFunction
                && !self.allows_top_level_await_in_current_context()
            {
                return Err(self.error(CodegenErrorType::InvalidAsyncWith));
            }
            // Load __aexit__ and __aenter__, then call __aenter__
            emit!(
                self,
                Instruction::LoadSpecial {
                    method: SpecialMethod::AExit
                }
            ); // [cm, aexit_func, self_ae]
            emit!(self, Instruction::Swap { i: 2 }); // [cm, self_ae, aexit_func]
            emit!(self, Instruction::Swap { i: 3 }); // [aexit_func, self_ae, cm]
            emit!(
                self,
                Instruction::LoadSpecial {
                    method: SpecialMethod::AEnter
                }
            ); // [aexit_func, self_ae, aenter_func, self_an]
            emit!(self, Instruction::Call { argc: 0 }); // [aexit_func, self_ae, awaitable]
            emit!(self, Instruction::GetAwaitable { r#where: 1 });
            self.emit_load_const(ConstantData::None);
            let _ = self.compile_yield_from_sequence(true);
        } else {
            // Load __exit__ and __enter__, then call __enter__
            emit!(
                self,
                Instruction::LoadSpecial {
                    method: SpecialMethod::Exit
                }
            ); // [cm, exit_func, self_exit]
            emit!(self, Instruction::Swap { i: 2 }); // [cm, self_exit, exit_func]
            emit!(self, Instruction::Swap { i: 3 }); // [exit_func, self_exit, cm]
            emit!(
                self,
                Instruction::LoadSpecial {
                    method: SpecialMethod::Enter
                }
            ); // [exit_func, self_exit, enter_func, self_enter]
            emit!(self, Instruction::Call { argc: 0 }); // [exit_func, self_exit, result]
        }

        // Stack: [..., __exit__, enter_result]
        // Push fblock for exception table - handler goes to exc_handler_block
        // preserve_lasti=true for with statements
        emit!(
            self,
            PseudoInstruction::SetupWith {
                delta: exc_handler_block
            }
        );
        self.use_cpython_label_block(body_block);
        let fblock_type = if is_async {
            FBlockType::AsyncWith
        } else {
            FBlockType::With
        };
        let body_label = self.instr_sequence_label_for_block(body_block);
        let exc_handler_label = self.instr_sequence_label_for_block(exc_handler_block);
        self.push_fblock_labels(
            fblock_type,
            body_label,
            exc_handler_label,
            FBlockDatum::None,
        )?;

        // Store or pop the enter result
        match &item.optional_vars {
            Some(var) => {
                self.set_source_range(var.range());
                self.compile_store(var)?;
            }
            None => {
                emit!(self, Instruction::PopTop);
            }
        }
        // Stack: [..., __exit__]

        // Compile body or nested with
        if items.is_empty() {
            if body.is_empty() {
                return Err(self.error(CodegenErrorType::EmptyWithBody));
            }
            self.compile_with_body_statements(body)?;
        } else {
            self.set_source_range(items[0].context_expr.range());
            self.compile_with_inner(items, body, is_async)?;
        }

        // CPython pops the async-with fblock before emitting POP_BLOCK, but
        // sync with emits the artificial POP_BLOCK before popping the fblock.
        if is_async {
            self.pop_fblock_label(fblock_type, body_label);
            self.set_source_range(with_range);
            emit!(self, PseudoInstruction::PopBlock);
        } else {
            emit!(self, PseudoInstruction::PopBlock);
            self.set_no_location();
            self.set_source_range(with_range);
            self.pop_fblock_label(fblock_type, body_label);
        }

        self.compile_call_exit_with_nones();
        if is_async {
            emit!(self, Instruction::GetAwaitable { r#where: 2 });
            self.emit_load_const(ConstantData::None);
            let _ = self.compile_yield_from_sequence(true);
        }
        emit!(self, Instruction::PopTop); // Pop __exit__ result
        emit!(self, PseudoInstruction::Jump { delta: after_block });

        // ===== Exception handler path =====
        // Stack at entry: [..., exit_func, self_exit, lasti, exc]
        // PUSH_EXC_INFO -> [..., exit_func, self_exit, lasti, prev_exc, exc]
        self.use_cpython_label_block(exc_handler_block);

        emit!(
            self,
            PseudoInstruction::SetupCleanup {
                delta: cleanup_block
            }
        );

        emit!(self, Instruction::PushExcInfo);

        // WITH_EXCEPT_START: call exit_func(self_exit, type, value, tb)
        // Stack: [..., exit_func, self_exit, lasti, prev_exc, exc]
        emit!(self, Instruction::WithExceptStart);

        if is_async {
            emit!(self, Instruction::GetAwaitable { r#where: 2 });
            self.emit_load_const(ConstantData::None);
            let _ = self.compile_yield_from_sequence(true);
        }

        self.compile_with_except_finish(cleanup_block);

        self.use_cpython_label_block(after_block);

        Ok(())
    }

    fn compile_for(
        &mut self,
        target: &ast::Expr,
        iter: &ast::Expr,
        body: &[ast::Stmt],
        orelse: &[ast::Stmt],
        is_async: bool,
        for_range: TextRange,
    ) -> CompileResult<()> {
        self.enter_conditional_block();

        // Start loop
        let for_block = self.new_block();
        let (body_label, send_block) = if is_async {
            (None, self.new_block())
        } else {
            let body_label = self.current_code_info().new_instr_sequence_label();
            (Some(body_label), BlockIdx::NULL)
        };
        let else_block = self.new_block();
        let after_block = self.new_block();
        let for_label = self.instr_sequence_label_for_block(for_block);
        let after_label = self.instr_sequence_label_for_block(after_block);
        let mut end_async_for_target = BlockIdx::NULL;

        if !is_async {
            // CPython codegen_for() pushes the loop fblock before compiling
            // the iterable expression.
            self.push_fblock_labels(
                FBlockType::ForLoop,
                for_label,
                after_label,
                FBlockDatum::None,
            )?;
        }

        // The thing iterated:
        self.compile_expression(iter)?;

        if is_async {
            if self.ctx.func != FunctionContext::AsyncFunction
                && !self.allows_top_level_await_in_current_context()
            {
                return Err(self.error(CodegenErrorType::InvalidAsyncFor));
            }
            self.set_source_range(iter.range());
            emit!(self, Instruction::GetAiter);

            self.use_cpython_label_block(for_block);
            self.set_source_range(for_range);

            // codegen_async_for: push fblock BEFORE SETUP_FINALLY
            self.push_fblock_labels(
                FBlockType::ForLoop,
                for_label,
                after_label,
                FBlockDatum::None,
            )?;

            // SETUP_FINALLY to guard the __anext__ call
            emit!(self, PseudoInstruction::SetupFinally { delta: else_block });
            emit!(self, Instruction::GetAnext);
            self.emit_load_const(ConstantData::None);
            self.use_cpython_label_block(send_block);
            let _ = self.compile_yield_from_sequence(true);
            end_async_for_target = send_block;
            // POP_BLOCK for SETUP_FINALLY - only GetANext/yield_from are protected
            emit!(self, PseudoInstruction::PopBlock);
            emit!(self, Instruction::NotTaken);

            // Success block for __anext__
            self.compile_store(target)?;
        } else {
            // Retrieve Iterator
            self.set_source_range(iter.range());
            emit!(self, Instruction::GetIter);

            self.use_cpython_label_block(for_block);

            emit!(self, Instruction::ForIter { delta: else_block });

            let saved_range = self.current_source_range;
            self.set_source_range(target.range());
            emit!(self, Instruction::Nop);
            let result = self
                .current_code_info()
                .use_raw_instr_sequence_label(body_label.expect("sync for must have body label"));
            unwrap_internal(self, result);
            self.compile_store(target)?;
            self.set_source_range(saved_range);
        };

        self.compile_loop_body_statements(body)?;
        emit!(self, PseudoInstruction::Jump { delta: for_block });
        self.set_no_location();

        if is_async {
            // codegen_async_for() pops the loop fblock before the
            // END_ASYNC_FOR exception block. Sync codegen_for() keeps the
            // fblock through END_FOR/POP_ITER and pops below.
            self.pop_fblock_label(FBlockType::ForLoop, for_label);
        }

        self.use_cpython_label_block(else_block);

        // Except block for __anext__ / end of sync for
        if is_async {
            // codegen_async_for emits END_ASYNC_FOR at the iterator location,
            // after the for-loop fblock has already been popped.
            let saved_range = self.current_source_range;
            self.set_source_range(iter.range());
            self.emit_end_async_for(end_async_for_target);
            self.set_source_range(saved_range);
        } else {
            // codegen_for emits END_FOR/POP_ITER with NO_LOCATION. Line numbers
            // are propagated later by flowgraph.c::resolve_line_numbers().
            emit!(self, Instruction::EndFor);
            self.set_no_location();
            emit!(self, Instruction::PopIter);
            self.set_no_location();
            self.pop_fblock_label(FBlockType::ForLoop, for_label);
        }
        self.compile_statements(orelse)?;

        self.use_cpython_label_block(after_block);

        // Implicit return after for-loop should be attributed to the `for` line
        self.set_source_range(iter.range());

        self.leave_conditional_block();
        Ok(())
    }

    fn compile_comprehension_iter(&mut self, generator: &ast::Comprehension) -> CompileResult<()> {
        let saved_range = self.current_source_range;
        self.compile_expression(&generator.iter)?;
        self.set_source_range(generator.iter.range());
        if generator.is_async {
            emit!(self, Instruction::GetAiter);
        } else {
            emit!(self, Instruction::GetIter);
        }
        self.set_source_range(saved_range);
        Ok(())
    }

    fn singleton_comprehension_assignment_iter(iter: &ast::Expr) -> Option<&ast::Expr> {
        let elts = match iter {
            ast::Expr::List(ast::ExprList { elts, .. }) => elts,
            ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => elts,
            _ => return None,
        };
        match elts.as_slice() {
            [elt] if !matches!(elt, ast::Expr::Starred(_)) => Some(elt),
            _ => None,
        }
    }

    /// Ensures that `pc.fail_pop` has at least `n + 1` entries.
    /// If not, new labels are generated and pushed until the required size is reached.
    fn ensure_fail_pop(&mut self, pc: &mut PatternContext, n: usize) {
        let required_size = n + 1;
        if required_size <= pc.fail_pop.len() {
            return;
        }
        while pc.fail_pop.len() < required_size {
            let new_block = self.new_block();
            pc.fail_pop.push(new_block);
        }
    }

    fn jump_to_fail_pop(&mut self, pc: &mut PatternContext, op: JumpOp) {
        // Compute the total number of items to pop:
        // items on top plus the captured objects.
        let pops = pc.on_top + pc.stores.len();
        // Ensure that the fail_pop vector has at least `pops + 1` elements.
        self.ensure_fail_pop(pc, pops);
        // Emit a jump using the jump target stored at index `pops`.
        match op {
            JumpOp::Jump => {
                emit!(
                    self,
                    PseudoInstruction::Jump {
                        delta: pc.fail_pop[pops]
                    }
                )
            }
            JumpOp::PopJumpIfFalse => {
                emit!(
                    self,
                    Instruction::PopJumpIfFalse {
                        delta: pc.fail_pop[pops]
                    }
                )
            }
        };
    }

    /// Emits the necessary POP instructions for all failure targets in the pattern context,
    /// then resets the fail_pop vector.
    fn emit_and_reset_fail_pop(&mut self, pc: &mut PatternContext, loc: TextRange) {
        // If the fail_pop vector is empty, nothing needs to be done.
        if pc.fail_pop.is_empty() {
            debug_assert!(pc.fail_pop.is_empty());
            return;
        }
        // Iterate over the fail_pop vector in reverse order, skipping the first label.
        for &label in pc.fail_pop.iter().skip(1).rev() {
            // CPython emit_and_reset_fail_pop() uses USE_LABEL here.
            self.use_cpython_label_block(label);
            // Emit the POP instruction.
            self.set_source_range(loc);
            emit!(self, Instruction::PopTop);
        }
        // Finally, use the first label.
        // CPython emit_and_reset_fail_pop() uses USE_LABEL here too.
        self.use_cpython_label_block(pc.fail_pop[0]);
        pc.fail_pop.clear();
        // Free the memory used by the vector.
        pc.fail_pop.shrink_to_fit();
    }

    /// Duplicate the effect of Python 3.10's ROT_* instructions using SWAPs.
    fn pattern_helper_rotate(&mut self, loc: TextRange, mut count: usize) {
        // Rotate TOS (top of stack) to position `count` down
        // This is done by a series of swaps
        // For count=1, no rotation needed (already at top)
        // For count=2, swap TOS with item 1 position down
        // For count=3, swap TOS with item 2 positions down, then with item 1 position down
        while count > 1 {
            // Emit a SWAP instruction with the current count.
            self.set_source_range(loc);
            emit!(
                self,
                Instruction::Swap {
                    i: u32::try_from(count).unwrap()
                }
            );
            count -= 1;
        }
    }

    /// Helper to store a captured name for a star pattern.
    ///
    /// If `n` is `None`, it emits a POP_TOP instruction. Otherwise, it first
    /// checks that the name is allowed and not already stored. Then it rotates
    /// the object on the stack beneath any preserved items and appends the name
    /// to the list of captured names.
    fn pattern_helper_store_name(
        &mut self,
        loc: TextRange,
        n: Option<&ast::Identifier>,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        match n {
            // If no name is provided, simply pop the top of the stack.
            None => {
                self.set_source_range(loc);
                emit!(self, Instruction::PopTop);
                Ok(())
            }
            Some(name) => {
                // Ensure we don't store the same name twice.
                // TODO: maybe pc.stores should be a set?
                if pc.stores.contains(name.id()) {
                    return Err(self.error_ranged(
                        CodegenErrorType::DuplicateStore(name.as_str().to_string()),
                        loc,
                    ));
                }

                // Calculate how many items to rotate:
                let rotations = pc.on_top + pc.stores.len() + 1;
                self.pattern_helper_rotate(loc, rotations);

                // Append the name to the captured stores.
                pc.stores.push(name.id().clone());
                Ok(())
            }
        }
    }

    fn pattern_wildcard_check(pattern: &ast::Pattern) -> bool {
        matches!(
            pattern,
            ast::Pattern::MatchAs(ast::PatternMatchAs { name: None, .. })
        )
    }

    fn pattern_wildcard_star_check(pattern: &ast::Pattern) -> bool {
        matches!(
            pattern,
            ast::Pattern::MatchStar(ast::PatternMatchStar { name: None, .. })
        )
    }

    fn pattern_unpack_helper(
        &mut self,
        loc: TextRange,
        elts: &[ast::Pattern],
    ) -> CompileResult<()> {
        let n = elts.len();
        let mut seen_star = false;
        for (i, elt) in elts.iter().enumerate() {
            if elt.is_match_star() && !seen_star {
                if i >= (1 << 8) || (n - i - 1) >= ((i32::MAX as usize) >> 8) {
                    return Err(self.error_ranged(
                        CodegenErrorType::TooManyExpressionsInStarUnpackingSequencePattern,
                        loc,
                    ));
                }
                let counts = UnpackExArgs {
                    before: u8::try_from(i).unwrap(),
                    after: u32::try_from(n - i - 1).unwrap(),
                };
                self.set_source_range(loc);
                emit!(self, Instruction::UnpackEx { counts });
                seen_star = true;
            } else if elt.is_match_star() {
                return Err(self.error_ranged(
                    CodegenErrorType::MultipleStarredExpressionsInSequencePattern,
                    loc,
                ));
            }
        }
        if !seen_star {
            self.set_source_range(loc);
            emit!(
                self,
                Instruction::UnpackSequence {
                    count: u32::try_from(n).unwrap()
                }
            );
        }
        Ok(())
    }

    fn pattern_helper_sequence_unpack(
        &mut self,
        loc: TextRange,
        patterns: &[ast::Pattern],
        _star: Option<usize>,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        // Unpack the sequence into individual subjects.
        self.pattern_unpack_helper(loc, patterns)?;
        let size = patterns.len();
        // Increase the on_top counter for the newly unpacked subjects.
        pc.on_top += size;
        // For each unpacked subject, compile its subpattern.
        for pattern in patterns {
            // Decrement on_top for each subject as it is consumed.
            pc.on_top -= 1;
            self.compile_pattern_subpattern(pattern, pc)?;
        }
        Ok(())
    }

    fn pattern_helper_sequence_subscr(
        &mut self,
        loc: TextRange,
        patterns: &[ast::Pattern],
        star: usize,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        // Keep the subject around for extracting elements.
        pc.on_top += 1;
        for (i, pattern) in patterns.iter().enumerate() {
            if Self::pattern_wildcard_check(pattern) {
                continue;
            }
            if i == star {
                // This must be a starred wildcard.
                debug_assert!(Self::pattern_wildcard_star_check(pattern));
                continue;
            }
            // Duplicate the subject.
            self.set_source_range(loc);
            emit!(self, Instruction::Copy { i: 1 });
            if i < star {
                // For indices before the star, use a nonnegative index equal to i.
                self.set_source_range(loc);
                self.emit_load_const(ConstantData::Integer { value: i.into() });
            } else {
                // For indices after the star, compute a nonnegative index:
                // index = len(subject) - (size - i)
                self.set_source_range(loc);
                emit!(self, Instruction::GetLen);
                self.set_source_range(loc);
                self.emit_load_const(ConstantData::Integer {
                    value: (patterns.len() - i).into(),
                });
                // Subtract to compute the correct index.
                self.set_source_range(loc);
                emit!(
                    self,
                    Instruction::BinaryOp {
                        op: BinaryOperator::Subtract
                    }
                );
            }
            // Use BINARY_OP/NB_SUBSCR to extract the element.
            self.set_source_range(loc);
            emit!(
                self,
                Instruction::BinaryOp {
                    op: BinaryOperator::Subscr
                }
            );
            // Compile the subpattern in irrefutable mode.
            self.compile_pattern_subpattern(pattern, pc)?;
        }
        // Pop the subject off the stack.
        pc.on_top -= 1;
        self.set_source_range(loc);
        emit!(self, Instruction::PopTop);
        Ok(())
    }

    fn compile_pattern_subpattern(
        &mut self,
        p: &ast::Pattern,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        // Save the current allow_irrefutable state.
        let old_allow_irrefutable = pc.allow_irrefutable;
        // Temporarily allow irrefutable patterns.
        pc.allow_irrefutable = true;
        // Compile the pattern.
        self.compile_pattern(p, pc)?;
        // Restore the original state.
        pc.allow_irrefutable = old_allow_irrefutable;
        Ok(())
    }

    fn compile_pattern_as(
        &mut self,
        p: &ast::PatternMatchAs,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        // If there is no sub-pattern, then it's an irrefutable match.
        if p.pattern.is_none() {
            if !pc.allow_irrefutable {
                if let Some(name) = p.name.as_ref() {
                    return Err(self.error_ranged(
                        CodegenErrorType::UnreachableNameCapturePattern(name.to_string()),
                        p.range,
                    ));
                }
                // A wildcard makes remaining patterns unreachable.
                return Err(
                    self.error_ranged(CodegenErrorType::UnreachableWildcardPattern, p.range)
                );
            }
            // If irrefutable matches are allowed, store the name (if any).
            return self.pattern_helper_store_name(p.range, p.name.as_ref(), pc);
        }

        // Otherwise, there is a sub-pattern. Duplicate the object on top of the stack.
        pc.on_top += 1;
        self.set_source_range(p.range);
        emit!(self, Instruction::Copy { i: 1 });
        // Compile the sub-pattern.
        self.compile_pattern(p.pattern.as_ref().unwrap(), pc)?;
        // After success, decrement the on_top counter.
        pc.on_top -= 1;
        // Store the captured name (if any).
        self.pattern_helper_store_name(p.range, p.name.as_ref(), pc)?;
        Ok(())
    }

    fn compile_pattern_star(
        &mut self,
        p: &ast::PatternMatchStar,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        self.pattern_helper_store_name(p.range, p.name.as_ref(), pc)?;
        Ok(())
    }

    /// Validates that keyword attributes in a class pattern are allowed
    /// and not duplicated.
    fn validate_kwd_attrs(
        &mut self,
        attrs: &[ast::Identifier],
        patterns: &[ast::Pattern],
    ) -> CompileResult<()> {
        let n_attrs = attrs.len();
        for i in 0..n_attrs {
            let attr = attrs[i].as_str();
            // Check for duplicates: compare with every subsequent attribute.
            for (j, ident) in attrs.iter().enumerate().take(n_attrs).skip(i + 1) {
                let other = ident.as_str();
                if attr == other {
                    return Err(self.error_ranged(
                        CodegenErrorType::RepeatedAttributePattern(attr.to_owned()),
                        patterns[j].range(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn compile_pattern_class(
        &mut self,
        p: &ast::PatternMatchClass,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        // Extract components from the MatchClass pattern.
        let match_class = p;
        let patterns = &match_class.arguments.patterns;

        // Extract keyword attributes and patterns.
        // Capacity is pre-allocated based on the number of keyword arguments.
        let mut kwd_attrs = Vec::with_capacity(match_class.arguments.keywords.len());
        let mut kwd_patterns = Vec::with_capacity(match_class.arguments.keywords.len());
        for kwd in &match_class.arguments.keywords {
            kwd_attrs.push(kwd.attr.clone());
            kwd_patterns.push(kwd.pattern.clone());
        }

        let nargs = patterns.len();
        let n_attrs = kwd_attrs.len();
        let n_kwd_patterns = kwd_patterns.len();
        if n_attrs != n_kwd_patterns {
            return Err(self.error_ranged(
                CodegenErrorType::SyntaxError(format!(
                    "kwd_attrs ({n_attrs}) / kwd_patterns ({n_kwd_patterns}) length mismatch in class pattern"
                )),
                p.range,
            ));
        }

        // Check for too many sub-patterns.
        if nargs > i32::MAX as usize
            || nargs.saturating_add(n_attrs).saturating_sub(1) > i32::MAX as usize
        {
            return Err(self.error_ranged(
                CodegenErrorType::SyntaxError(format!(
                    "too many sub-patterns in class pattern {}",
                    UnparseExpr::new(&match_class.cls, &self.source_file)
                )),
                p.range,
            ));
        }

        // Validate keyword attributes if any.
        if n_attrs != 0 {
            self.validate_kwd_attrs(&kwd_attrs, &kwd_patterns)?;
        }

        // Compile the class expression.
        self.compile_expression(&match_class.cls)?;
        self.set_source_range(p.range);

        // Create a new tuple of attribute names.
        let mut attr_names = vec![];
        for name in &kwd_attrs {
            // Py_NewRef(name) is emulated by cloning the name into a PyObject.
            attr_names.push(ConstantData::Str {
                value: name.as_str().to_string().into(),
            });
        }

        // Emit instructions:
        // 1. Load the new tuple of attribute names.
        self.emit_load_const(ConstantData::Tuple {
            elements: attr_names,
        });
        // 2. Emit MATCH_CLASS with nargs.
        emit!(
            self,
            Instruction::MatchClass {
                count: u32::try_from(nargs).unwrap()
            }
        );
        // 3. Duplicate the top of the stack.
        emit!(self, Instruction::Copy { i: 1 });
        // 4. Load None.
        self.emit_load_const(ConstantData::None);
        // 5. Compare with IS_OP 1.
        emit!(
            self,
            Instruction::IsOp {
                invert: Invert::Yes
            }
        );

        // At this point the TOS is a tuple of (nargs + n_attrs) attributes (or None).
        pc.on_top += 1;
        self.set_source_range(p.range);
        self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);

        // Unpack the tuple into (nargs + n_attrs) items.
        let total = nargs + n_attrs;
        emit!(
            self,
            Instruction::UnpackSequence {
                count: u32::try_from(total).unwrap()
            }
        );
        if total == 0 {
            pc.on_top -= 1;
        } else {
            pc.on_top += total - 1;
        }

        // Process each sub-pattern.
        for subpattern in patterns.iter().chain(kwd_patterns.iter()) {
            // Decrement the on_top counter for each sub-pattern
            pc.on_top -= 1;

            if Self::pattern_wildcard_check(subpattern) {
                self.set_source_range(p.range);
                emit!(self, Instruction::PopTop);
                continue; // Don't compile wildcard patterns
            }

            // Compile the subpattern without irrefutability checks.
            self.compile_pattern_subpattern(subpattern, pc)?;
        }
        Ok(())
    }

    fn compile_pattern_mapping(
        &mut self,
        p: &ast::PatternMatchMapping,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        let mapping = p;
        let keys = &mapping.keys;
        let patterns = &mapping.patterns;
        let size = keys.len();
        let star_target = &mapping.rest;

        // Validate pattern count matches key count
        if keys.len() != patterns.len() {
            return Err(self.error_ranged(
                CodegenErrorType::SyntaxError(format!(
                    "keys ({}) / patterns ({}) length mismatch in mapping pattern",
                    keys.len(),
                    patterns.len()
                )),
                p.range,
            ));
        }

        // `case {**_}:` is rejected before codegen. RustPython's parser
        // currently lets it through, so keep the compiler boundary equivalent.
        if let Some(rest) = star_target
            && rest.as_str() == "_"
        {
            return Err(self.error_ranged(
                CodegenErrorType::SyntaxError("invalid syntax".to_string()),
                rest.range,
            ));
        }

        // Step 1: Check if subject is a mapping
        // Stack: [subject]
        pc.on_top += 1;

        self.set_source_range(p.range);
        emit!(self, Instruction::MatchMapping);
        // Stack: [subject, is_mapping]

        self.set_source_range(p.range);
        self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);
        // Stack: [subject]

        // Special case: empty pattern {} with no rest
        if size == 0 && star_target.is_none() {
            // If the pattern is just "{}", we're done! Pop the subject
            pc.on_top -= 1;
            emit!(self, Instruction::PopTop);
            return Ok(());
        }

        // Length check for patterns with keys
        if size > 0 {
            // Check if the mapping has at least 'size' keys
            self.set_source_range(p.range);
            emit!(self, Instruction::GetLen);
            self.set_source_range(p.range);
            self.emit_load_const(ConstantData::Integer { value: size.into() });
            // Stack: [subject, len, size]
            self.set_source_range(p.range);
            emit!(
                self,
                Instruction::CompareOp {
                    opname: ComparisonOperator::GreaterOrEqual
                }
            );
            self.set_source_range(p.range);
            self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);
            // Stack: [subject]
        }

        // Check for overflow (INT_MAX < size - 1)
        if size.saturating_sub(1) > i32::MAX as usize {
            return Err(self.error_ranged(
                CodegenErrorType::SyntaxError(
                    "too many sub-patterns in mapping pattern".to_string(),
                ),
                p.range,
            ));
        }
        let size = size.to_u32();

        // Step 2: Validate and compile all keys.
        let mut seen = Vec::new();
        for key in keys {
            self.compile_pattern_mapping_key(&mut seen, p.range, key)?;
        }
        self.set_source_range(p.range);
        // Stack: [subject, key1, key2, ..., key_n]

        // Build tuple of keys (empty tuple if size==0)
        emit!(self, Instruction::BuildTuple { count: size });
        // Stack: [subject, keys_tuple]

        // Match keys
        emit!(self, Instruction::MatchKeys);
        // Stack: [subject, keys_tuple, values_or_none]
        pc.on_top += 2; // subject and keys_tuple are underneath

        // Check if match succeeded
        self.set_source_range(p.range);
        emit!(self, Instruction::Copy { i: 1 });
        // Stack: [subject, keys_tuple, values_tuple, values_tuple_copy]

        // Check if copy is None (consumes the copy like POP_JUMP_IF_NONE)
        self.set_source_range(p.range);
        self.emit_load_const(ConstantData::None);
        self.set_source_range(p.range);
        emit!(
            self,
            Instruction::IsOp {
                invert: Invert::Yes
            }
        );

        // Stack: [subject, keys_tuple, values_tuple, bool]
        self.set_source_range(p.range);
        self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);
        // Stack: [subject, keys_tuple, values_tuple]

        // Unpack values (the original values_tuple)
        emit!(self, Instruction::UnpackSequence { count: size });
        // Stack after unpack: [subject, keys_tuple, ...unpacked values...]
        if size == 0 {
            pc.on_top -= 1;
        } else {
            pc.on_top += size as usize - 1;
        }

        // Step 3: Process matched values
        for i in 0..size {
            pc.on_top -= 1;
            self.compile_pattern_subpattern(&patterns[i as usize], pc)?;
        }

        // After processing subpatterns, adjust on_top
        // "Whatever happens next should consume the tuple of keys and the subject"
        // Stack currently: [subject, keys_tuple, ...any captured values...]
        pc.on_top -= 2;

        // Step 4: Handle rest pattern or cleanup
        if let Some(rest_name) = star_target {
            // Build rest dict for **rest pattern
            // Stack: [subject, keys_tuple]

            // Build rest dict exactly
            self.set_source_range(p.range);
            emit!(self, Instruction::BuildMap { count: 0 });
            // Stack: [subject, keys_tuple, {}]
            self.set_source_range(p.range);
            emit!(self, Instruction::Swap { i: 3 });
            // Stack: [{}, keys_tuple, subject]
            self.set_source_range(p.range);
            emit!(self, Instruction::DictUpdate { i: 2 });
            // Stack after DICT_UPDATE: [rest_dict, keys_tuple]
            // DICT_UPDATE consumes source (subject) and leaves dict in place

            // Unpack keys and delete from rest_dict
            self.set_source_range(p.range);
            emit!(self, Instruction::UnpackSequence { count: size });
            // Stack: [rest_dict, k1, k2, ..., kn] (if size==0, nothing pushed)

            // Delete each key from rest_dict (skipped when size==0)
            // while (size) { COPY(1 + size--); SWAP(2); DELETE_SUBSCR }
            let mut remaining = size;
            while remaining > 0 {
                // Copy rest_dict which is at position (1 + remaining) from TOS
                self.set_source_range(p.range);
                emit!(self, Instruction::Copy { i: 1 + remaining });
                // Stack: [rest_dict, k1, ..., kn, rest_dict]
                self.set_source_range(p.range);
                emit!(self, Instruction::Swap { i: 2 });
                // Stack: [rest_dict, k1, ..., kn-1, rest_dict, kn]
                self.set_source_range(p.range);
                emit!(self, Instruction::DeleteSubscr);
                // Stack: [rest_dict, k1, ..., kn-1] (removed kn from rest_dict)
                remaining -= 1;
            }
            // Stack: [rest_dict] (plus any previously stored values)
            // pattern_helper_store_name will handle the rotation correctly

            // Store the rest dict
            self.pattern_helper_store_name(p.range, Some(rest_name), pc)?;
        } else {
            // Non-rest pattern: just clean up the stack

            // Pop them as we're not using them
            self.set_source_range(p.range);
            emit!(self, Instruction::PopTop); // Pop keys_tuple
            self.set_source_range(p.range);
            emit!(self, Instruction::PopTop); // Pop subject
        }

        Ok(())
    }

    fn compile_pattern_mapping_key(
        &mut self,
        seen: &mut Vec<ConstantData>,
        pattern_range: TextRange,
        key: &ast::Expr,
    ) -> CompileResult<()> {
        let is_attribute = matches!(key, ast::Expr::Attribute(_));
        let constant = match self.try_compile_match_mapping_key_constant(key)? {
            Some(constant) => Some(constant),
            None if is_attribute => None,
            None => {
                if Self::is_unexpected_match_literal_constant(key) {
                    return Err(self.error_ranged(
                        CodegenErrorType::SyntaxError(
                            "unexpected constant inside of a literal pattern".to_string(),
                        ),
                        pattern_range,
                    ));
                }
                return Err(self.error_ranged(
                    CodegenErrorType::SyntaxError(
                        "mapping pattern keys may only match literals and attribute lookups"
                            .to_string(),
                    ),
                    pattern_range,
                ));
            }
        };

        if let Some(constant) = constant {
            if seen
                .iter()
                .any(|seen| Self::match_mapping_keys_equal(seen, &constant))
            {
                let key_repr = Self::match_mapping_key_repr(&constant);
                return Err(self.error_ranged(
                    CodegenErrorType::SyntaxError(format!(
                        "mapping pattern checks duplicate key ({key_repr})"
                    )),
                    pattern_range,
                ));
            }
            seen.push(constant);
        }

        self.compile_match_pattern_expr(key)
    }

    fn try_compile_match_mapping_key_constant(
        &mut self,
        key: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        if let Some(constant) = self.try_fold_match_pattern_const_expr(key)? {
            return Ok(Some(constant));
        }
        self.try_compile_match_mapping_key_direct_constant(key)
    }

    fn try_compile_match_value_constant(
        &mut self,
        value: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        if let Some(constant) = self.try_fold_match_pattern_const_expr(value)? {
            return Ok(Some(constant));
        }
        self.try_compile_match_pattern_direct_literal(value)
    }

    fn match_mapping_keys_equal(left: &ConstantData, right: &ConstantData) -> bool {
        use ConstantData::{Bytes, Ellipsis, None, Str};

        if Self::match_mapping_numeric_keys_equal(left, right).unwrap_or(false) {
            return true;
        }

        match (left, right) {
            (Str { value: left }, Str { value: right }) => left == right,
            (Bytes { value: left }, Bytes { value: right }) => left == right,
            (None, None) | (Ellipsis, Ellipsis) => true,
            _ => false,
        }
    }

    fn match_mapping_key_repr(key: &ConstantData) -> String {
        match key {
            ConstantData::Integer { value } => value.to_string(),
            ConstantData::Float { value } => literal_float::to_string(*value),
            ConstantData::Complex { value } => literal_complex::to_string(value.re, value.im),
            ConstantData::Boolean { value } => {
                if *value {
                    "True".to_owned()
                } else {
                    "False".to_owned()
                }
            }
            ConstantData::Str { value } => UnicodeEscape::new_repr(value.as_ref())
                .str_repr()
                .to_string()
                .unwrap_or_else(|| value.to_string()),
            ConstantData::Bytes { value } => AsciiEscape::new_repr(value)
                .bytes_repr()
                .to_string()
                .unwrap_or_else(|| format!(r#"b"{}""#, value.escape_ascii())),
            ConstantData::None => "None".to_owned(),
            ConstantData::Ellipsis => "...".to_owned(),
            other => other.to_string(),
        }
    }

    fn match_mapping_numeric_keys_equal(left: &ConstantData, right: &ConstantData) -> Option<bool> {
        use ConstantData::{Boolean, Complex, Float, Integer};

        match (left, right) {
            (Integer { value: left }, Integer { value: right }) => Some(left == right),
            (Boolean { value: left }, Boolean { value: right }) => Some(left == right),
            (Boolean { value }, Integer { value: int })
            | (Integer { value: int }, Boolean { value }) => {
                Some(BigInt::from(u8::from(*value)) == *int)
            }
            (Float { value: left }, Float { value: right }) => Some(left == right),
            (Integer { value: int }, Float { value: float })
            | (Float { value: float }, Integer { value: int }) => {
                Some(Self::match_mapping_float_integer_equal(*float, int))
            }
            (Boolean { value }, Float { value: float })
            | (Float { value: float }, Boolean { value }) => Some(
                Self::match_mapping_float_integer_equal(*float, &BigInt::from(u8::from(*value))),
            ),
            (Complex { value: left }, Complex { value: right }) => {
                Some(left.re == right.re && left.im == right.im)
            }
            (Complex { value: complex }, other) | (other, Complex { value: complex }) => Some(
                complex.im == 0.0
                    && Self::match_mapping_float_real_constant_equal(complex.re, other)
                        .unwrap_or(false),
            ),
            _ => Option::None,
        }
    }

    fn match_mapping_float_real_constant_equal(float: f64, other: &ConstantData) -> Option<bool> {
        match other {
            ConstantData::Integer { value } => {
                Some(Self::match_mapping_float_integer_equal(float, value))
            }
            ConstantData::Boolean { value } => Some(Self::match_mapping_float_integer_equal(
                float,
                &BigInt::from(u8::from(*value)),
            )),
            ConstantData::Float { value } => Some(float == *value),
            _ => None,
        }
    }

    fn match_mapping_float_integer_equal(float: f64, int: &BigInt) -> bool {
        Self::match_mapping_float_to_integer(float).is_some_and(|float_int| &float_int == int)
    }

    fn match_mapping_float_to_integer(value: f64) -> Option<BigInt> {
        if !value.is_finite() {
            return None;
        }
        if value == 0.0 {
            return Some(BigInt::from(0));
        }

        let bits = value.to_bits();
        let negative = (bits >> 63) != 0;
        let exponent_bits = i32::try_from((bits >> 52) & 0x7ff).ok()?;
        let fraction = bits & ((1_u64 << 52) - 1);
        let (mantissa, exponent) = if exponent_bits == 0 {
            (fraction, -1074)
        } else {
            ((1_u64 << 52) | fraction, exponent_bits - 1023 - 52)
        };

        let mut integer = if exponent >= 0 {
            BigInt::from(mantissa) << u32::try_from(exponent).ok()?
        } else {
            let shift = u32::try_from(-exponent).ok()?;
            if shift >= u64::BITS {
                return None;
            }
            let mask = (1_u64 << shift) - 1;
            if mantissa & mask != 0 {
                return None;
            }
            BigInt::from(mantissa >> shift)
        };

        if negative {
            integer = -integer;
        }
        Some(integer)
    }

    fn compile_pattern_or(
        &mut self,
        p: &ast::PatternMatchOr,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        // Ensure the pattern is a MatchOr.
        let end = self.new_block(); // Create a new jump target label.
        let size = p.patterns.len();
        if size <= 1 {
            return Err(self.error(CodegenErrorType::SyntaxError(
                "MatchOr requires at least 2 patterns".to_owned(),
            )));
        }

        // Save the current pattern context.
        let old_pc = pc.clone();
        // Simulate Py_INCREF on pc.stores by cloning it.
        pc.stores = pc.stores.clone();
        let mut control: Option<Vec<Name>> = None; // Will hold the capture list of the first alternative.

        // Process each alternative.
        for (i, alt) in p.patterns.iter().enumerate() {
            // Create a fresh empty store for this alternative.
            pc.stores = Vec::new();
            // An irrefutable subpattern must be last (if allowed).
            pc.allow_irrefutable = (i == size - 1) && old_pc.allow_irrefutable;
            // Reset failure targets and the on_top counter.
            pc.fail_pop.clear();
            pc.on_top = 0;
            // Emit a COPY(1) instruction before compiling the alternative.
            self.set_source_range(alt.range());
            emit!(self, Instruction::Copy { i: 1 });
            self.compile_pattern(alt, pc)?;

            let n_stores = pc.stores.len();
            if i == 0 {
                // Save the captured names from the first alternative.
                control = Some(pc.stores.clone());
            } else {
                let control_vec = control.as_ref().unwrap();
                if n_stores != control_vec.len() {
                    return Err(
                        self.error_ranged(CodegenErrorType::ConflictingNameBindPattern, p.range())
                    );
                } else if n_stores > 0 {
                    // Check that the names occur in the same order.
                    for i_control in (0..n_stores).rev() {
                        let name = &control_vec[i_control];
                        // Find the index of `name` in the current stores.
                        let i_stores =
                            pc.stores.iter().position(|n| n == name).ok_or_else(|| {
                                self.error_ranged(
                                    CodegenErrorType::ConflictingNameBindPattern,
                                    p.range(),
                                )
                            })?;
                        if i_control != i_stores {
                            // The orders differ; we must reorder.
                            assert!(i_stores < i_control, "expected i_stores < i_control");
                            let rotations = i_stores + 1;
                            // Rotate pc.stores: take a slice of the first `rotations` items...
                            let rotated = pc.stores[0..rotations].to_vec();
                            // Remove those elements.
                            for _ in 0..rotations {
                                pc.stores.remove(0);
                            }
                            // Insert the rotated slice at the appropriate index.
                            let insert_pos = i_control - i_stores;
                            for (j, elem) in rotated.into_iter().enumerate() {
                                pc.stores.insert(insert_pos + j, elem);
                            }
                            // Also perform the same rotation on the evaluation stack.
                            self.set_source_range(alt.range());
                            for _ in 0..=i_stores {
                                self.pattern_helper_rotate(alt.range(), i_control + 1);
                            }
                        }
                    }
                }
            }
            // Emit a jump to the common end label and reset any failure jump targets.
            self.set_source_range(alt.range());
            emit!(self, PseudoInstruction::Jump { delta: end });
            self.set_source_range(alt.range());
            self.emit_and_reset_fail_pop(pc, alt.range());
        }

        // Restore the original pattern context.
        *pc = old_pc;
        // Simulate Py_INCREF on pc.stores.
        pc.stores = pc.stores.clone();
        // In C, old_pc.fail_pop is set to NULL to avoid freeing it later.
        // In Rust, old_pc is a local clone, so we need not worry about that.

        // No alternative matched: pop the subject and fail.
        self.set_source_range(p.range());
        emit!(self, Instruction::PopTop);
        self.jump_to_fail_pop(pc, JumpOp::Jump);

        // Use the label "end".
        // CPython codegen_pattern_or() emits USE_LABEL(c, end).
        self.use_cpython_label_block(end);

        // Adjust the final captures.
        let n_stores = control.as_ref().unwrap().len();
        let n_rots = n_stores + 1 + pc.on_top + pc.stores.len();
        for i in 0..n_stores {
            // Rotate the capture to its proper place.
            self.set_source_range(p.range());
            self.pattern_helper_rotate(p.range(), n_rots);
            let name = &control.as_ref().unwrap()[i];
            // Check for duplicate binding.
            if pc.stores.contains(name) {
                return Err(self.error_ranged(
                    CodegenErrorType::DuplicateStore(name.to_string()),
                    p.range(),
                ));
            }
            pc.stores.push(name.clone());
        }

        // Old context and control will be dropped automatically.
        // Finally, pop the copy of the subject.
        self.set_source_range(p.range());
        emit!(self, Instruction::PopTop);
        Ok(())
    }

    fn compile_pattern_sequence(
        &mut self,
        p: &ast::PatternMatchSequence,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        // Ensure the pattern is a MatchSequence.
        let patterns = &p.patterns; // a slice of ast::Pattern
        let size = patterns.len();
        let mut star: Option<usize> = None;
        let mut only_wildcard = true;
        let mut star_wildcard = false;

        // Find a starred pattern, if it exists. There may be at most one.
        for (i, pattern) in patterns.iter().enumerate() {
            if pattern.is_match_star() {
                if star.is_some() {
                    return Err(self.error_ranged(
                        CodegenErrorType::MultipleStarredNamesInSequencePattern,
                        p.range,
                    ));
                }
                // star wildcard check
                star_wildcard = Self::pattern_wildcard_star_check(pattern);
                only_wildcard &= star_wildcard;
                star = Some(i);
                continue;
            }
            // wildcard check
            only_wildcard &= Self::pattern_wildcard_check(pattern);
        }

        // Keep the subject on top during the sequence and length checks.
        pc.on_top += 1;
        self.set_source_range(p.range);
        emit!(self, Instruction::MatchSequence);
        self.set_source_range(p.range);
        self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);

        if star.is_none() {
            // No star: len(subject) == size
            self.set_source_range(p.range);
            emit!(self, Instruction::GetLen);
            self.set_source_range(p.range);
            self.emit_load_const(ConstantData::Integer { value: size.into() });
            self.set_source_range(p.range);
            emit!(
                self,
                Instruction::CompareOp {
                    opname: ComparisonOperator::Equal
                }
            );
            self.set_source_range(p.range);
            self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);
        } else if size > 1 {
            // Star exists: len(subject) >= size - 1
            self.set_source_range(p.range);
            emit!(self, Instruction::GetLen);
            self.set_source_range(p.range);
            self.emit_load_const(ConstantData::Integer {
                value: (size - 1).into(),
            });
            self.set_source_range(p.range);
            emit!(
                self,
                Instruction::CompareOp {
                    opname: ComparisonOperator::GreaterOrEqual
                }
            );
            self.set_source_range(p.range);
            self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);
        }

        // Whatever comes next should consume the subject.
        pc.on_top -= 1;
        if only_wildcard {
            // ast::Patterns like: [] / [_] / [_, _] / [*_] / [_, *_] / [_, _, *_] / etc.
            self.set_source_range(p.range);
            emit!(self, Instruction::PopTop);
        } else if star_wildcard {
            self.pattern_helper_sequence_subscr(p.range, patterns, star.unwrap(), pc)?;
        } else {
            self.pattern_helper_sequence_unpack(p.range, patterns, star, pc)?;
        }
        Ok(())
    }

    fn compile_pattern_value(
        &mut self,
        p: &ast::PatternMatchValue,
        pc: &mut PatternContext,
    ) -> CompileResult<()> {
        // Match CPython codegen_pattern_value(): compare, then normalize to bool
        // before the fail jump. Late IR folding will collapse COMPARE_OP+TO_BOOL
        // into COMPARE_OP bool(...) when applicable.
        if let Some(constant) = self.try_compile_match_value_constant(&p.value)? {
            self.set_source_range(p.value.range());
            self.emit_load_const(constant);
        } else if matches!(*p.value, ast::Expr::Attribute(_)) {
            self.compile_expression(&p.value)?;
        } else {
            if Self::is_unexpected_match_literal_constant(&p.value) {
                return Err(self.error_ranged(
                    CodegenErrorType::SyntaxError(
                        "unexpected constant inside of a literal pattern".to_string(),
                    ),
                    p.range,
                ));
            }
            return Err(self.error_ranged(
                CodegenErrorType::SyntaxError(
                    "patterns may only match literals and attribute lookups".to_string(),
                ),
                p.range,
            ));
        }
        self.set_source_range(p.range);
        emit!(
            self,
            Instruction::CompareOp {
                opname: bytecode::ComparisonOperator::Equal
            }
        );
        self.set_source_range(p.range);
        emit!(self, Instruction::ToBool);
        self.set_source_range(p.range);
        self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);
        Ok(())
    }

    fn compile_pattern_singleton(
        &mut self,
        p: &ast::PatternMatchSingleton,
        pc: &mut PatternContext,
    ) {
        // Load the singleton constant value.
        self.set_source_range(p.range);
        self.emit_load_const(match p.value {
            ast::Singleton::None => ConstantData::None,
            ast::Singleton::False => ConstantData::Boolean { value: false },
            ast::Singleton::True => ConstantData::Boolean { value: true },
        });
        // Compare using the "Is" operator.
        self.set_source_range(p.range);
        emit!(self, Instruction::IsOp { invert: Invert::No });
        // Jump to the failure label if the comparison is false.
        self.set_source_range(p.range);
        self.jump_to_fail_pop(pc, JumpOp::PopJumpIfFalse);
    }

    fn compile_pattern(
        &mut self,
        pattern_type: &ast::Pattern,
        pattern_context: &mut PatternContext,
    ) -> CompileResult<()> {
        let prev_source_range = self.current_source_range;
        self.set_source_range(pattern_type.range());
        let result = match &pattern_type {
            ast::Pattern::MatchValue(pattern_type) => {
                self.compile_pattern_value(pattern_type, pattern_context)
            }
            ast::Pattern::MatchSingleton(pattern_type) => {
                self.compile_pattern_singleton(pattern_type, pattern_context);
                Ok(())
            }
            ast::Pattern::MatchSequence(pattern_type) => {
                self.compile_pattern_sequence(pattern_type, pattern_context)
            }
            ast::Pattern::MatchMapping(pattern_type) => {
                self.compile_pattern_mapping(pattern_type, pattern_context)
            }
            ast::Pattern::MatchClass(pattern_type) => {
                self.compile_pattern_class(pattern_type, pattern_context)
            }
            ast::Pattern::MatchStar(pattern_type) => {
                self.compile_pattern_star(pattern_type, pattern_context)
            }
            ast::Pattern::MatchAs(pattern_type) => {
                self.compile_pattern_as(pattern_type, pattern_context)
            }
            ast::Pattern::MatchOr(pattern_type) => {
                self.compile_pattern_or(pattern_type, pattern_context)
            }
        };
        self.set_source_range(prev_source_range);
        result
    }

    fn compile_match_inner(
        &mut self,
        subject: &ast::Expr,
        cases: &[ast::MatchCase],
        pattern_context: &mut PatternContext,
    ) -> CompileResult<()> {
        self.compile_expression(subject)?;
        let end = self.new_block();

        let num_cases = cases.len();
        assert!(num_cases > 0);
        let has_default =
            num_cases > 1 && Self::pattern_wildcard_check(&cases.last().unwrap().pattern);

        let case_count = num_cases - usize::from(has_default);
        for (i, m) in cases.iter().enumerate().take(case_count) {
            // Only copy the subject if not on the last case
            if i != case_count - 1 {
                self.set_source_range(m.pattern.range());
                emit!(self, Instruction::Copy { i: 1 });
            }

            pattern_context.stores = Vec::with_capacity(1);
            pattern_context.allow_irrefutable = m.guard.is_some() || i == num_cases - 1;
            pattern_context.fail_pop.clear();
            pattern_context.on_top = 0;

            self.compile_pattern(&m.pattern, pattern_context)?;
            assert_eq!(pattern_context.on_top, 0);

            for name in &pattern_context.stores {
                self.set_source_range(m.pattern.range());
                self.compile_name(name, NameUsage::Store)?;
            }

            if let Some(ref guard) = m.guard {
                self.ensure_fail_pop(pattern_context, 0);
                self.compile_jump_if_inner(
                    guard,
                    false,
                    pattern_context.fail_pop[0],
                    Some(m.pattern.range()),
                )?;
            }

            if i != case_count - 1 {
                if let Some(first_stmt) = m.body.first() {
                    self.set_source_range(first_stmt.range());
                }
                emit!(self, Instruction::PopTop);
                // CPython emits NEXT_LOCATION here; resolve it after redundant
                // NOP removal so a following pass NOP survives.
                self.set_last_emitted_lineno_override(ir::NEXT_LOCATION_OVERRIDE);
            }

            self.compile_statements(&m.body)?;
            emit!(self, PseudoInstruction::Jump { delta: end });
            self.set_no_location();
            self.set_source_range(m.pattern.range());
            self.emit_and_reset_fail_pop(pattern_context, m.pattern.range());
        }

        if has_default {
            let m = &cases[num_cases - 1];
            self.set_source_range(m.pattern.range());
            if num_cases == 1 {
                emit!(self, Instruction::PopTop);
            } else {
                emit!(self, Instruction::Nop);
            }
            if let Some(ref guard) = m.guard {
                self.compile_jump_if_inner(guard, false, end, Some(m.pattern.range()))?;
            }
            self.compile_statements(&m.body)?;
        }
        // CPython codegen_match_inner() emits USE_LABEL(c, end).
        self.use_cpython_label_block(end);
        Ok(())
    }

    fn compile_match(
        &mut self,
        subject: &ast::Expr,
        cases: &[ast::MatchCase],
    ) -> CompileResult<()> {
        self.enter_conditional_block();
        let mut pattern_context = PatternContext::new();
        self.compile_match_inner(subject, cases, &mut pattern_context)?;
        self.leave_conditional_block();
        Ok(())
    }

    /// [CPython `compiler_addcompare`](https://github.com/python/cpython/blob/627894459a84be3488a1789919679c997056a03c/Python/compile.c#L2880-L2924)
    fn compile_addcompare(&mut self, op: ast::CmpOp) {
        match op {
            ast::CmpOp::Eq => emit!(
                self,
                Instruction::CompareOp {
                    opname: ComparisonOperator::Equal
                }
            ),
            ast::CmpOp::NotEq => emit!(
                self,
                Instruction::CompareOp {
                    opname: ComparisonOperator::NotEqual
                }
            ),
            ast::CmpOp::Lt => emit!(
                self,
                Instruction::CompareOp {
                    opname: ComparisonOperator::Less
                }
            ),
            ast::CmpOp::LtE => emit!(
                self,
                Instruction::CompareOp {
                    opname: ComparisonOperator::LessOrEqual
                }
            ),
            ast::CmpOp::Gt => emit!(
                self,
                Instruction::CompareOp {
                    opname: ComparisonOperator::Greater
                }
            ),
            ast::CmpOp::GtE => {
                emit!(
                    self,
                    Instruction::CompareOp {
                        opname: ComparisonOperator::GreaterOrEqual
                    }
                )
            }
            ast::CmpOp::In => emit!(self, Instruction::ContainsOp { invert: Invert::No }),
            ast::CmpOp::NotIn => emit!(
                self,
                Instruction::ContainsOp {
                    invert: Invert::Yes
                }
            ),
            ast::CmpOp::Is => emit!(self, Instruction::IsOp { invert: Invert::No }),
            ast::CmpOp::IsNot => emit!(
                self,
                Instruction::IsOp {
                    invert: Invert::Yes
                }
            ),
        }
    }

    /// Compile a chained comparison.
    ///
    /// ```py
    /// a == b == c == d
    /// ```
    ///
    /// Will compile into (pseudo code):
    ///
    /// ```py
    /// result = a == b
    /// if result:
    ///   result = b == c
    ///   if result:
    ///     result = c == d
    /// ```
    ///
    /// # See Also
    /// - [CPython `compiler_compare`](https://github.com/python/cpython/blob/627894459a84be3488a1789919679c997056a03c/Python/compile.c#L4678-L4717)
    fn compile_compare(
        &mut self,
        left: &ast::Expr,
        ops: &[ast::CmpOp],
        comparators: &[ast::Expr],
    ) -> CompileResult<()> {
        // Save the full Compare expression range for COMPARE_OP positions
        let compare_range = self.current_source_range;
        self.check_compare(compare_range, left, ops, comparators)?;
        let (last_op, mid_ops) = ops.split_last().unwrap();
        let (last_comparator, mid_comparators) = comparators.split_last().unwrap();

        // initialize lhs outside of loop
        self.compile_expression(left)?;

        if mid_comparators.is_empty() {
            self.compile_expression(last_comparator)?;
            self.set_source_range(compare_range);
            self.compile_addcompare(*last_op);

            return Ok(());
        }

        let cleanup = self.new_block();

        // for all comparisons except the last (as the last one doesn't need a conditional jump)
        for (op, comparator) in mid_ops.iter().zip(mid_comparators) {
            self.compile_expression(comparator)?;

            // store rhs for the next comparison in chain
            self.set_source_range(compare_range);
            emit!(self, Instruction::Swap { i: 2 });
            emit!(self, Instruction::Copy { i: 2 });

            self.compile_addcompare(*op);

            // if comparison result is false, we break with this value; if true, try the next one.
            emit!(self, Instruction::Copy { i: 1 });
            emit!(self, Instruction::ToBool);
            emit!(self, Instruction::PopJumpIfFalse { delta: cleanup });
            emit!(self, Instruction::PopTop);
        }

        self.compile_expression(last_comparator)?;
        self.set_source_range(compare_range);
        self.compile_addcompare(*last_op);

        let end = self.new_block();
        emit!(self, PseudoInstruction::JumpNoInterrupt { delta: end });
        self.set_no_location();

        // early exit left us with stack: `rhs, comparison_result`. We need to clean up rhs.
        self.use_cpython_label_block(cleanup);
        emit!(self, Instruction::Swap { i: 2 });
        emit!(self, Instruction::PopTop);

        self.use_cpython_label_block(end);
        Ok(())
    }

    fn compile_jump_if_compare(
        &mut self,
        left: &ast::Expr,
        ops: &[ast::CmpOp],
        comparators: &[ast::Expr],
        condition: bool,
        target_block: BlockIdx,
    ) -> CompileResult<()> {
        let compare_range = self.current_source_range;
        self.check_compare(compare_range, left, ops, comparators)?;
        let (last_op, mid_ops) = ops.split_last().unwrap();
        let (last_comparator, mid_comparators) = comparators.split_last().unwrap();

        if mid_comparators.is_empty() {
            self.compile_expression(left)?;
            self.compile_expression(last_comparator)?;
            self.set_source_range(compare_range);
            self.compile_addcompare(*last_op);
            self.emit_pop_jump_by_condition(condition, target_block);
            return Ok(());
        }

        let cleanup = self.new_block();
        self.compile_expression(left)?;

        for (op, comparator) in mid_ops.iter().zip(mid_comparators) {
            self.compile_expression(comparator)?;
            self.set_source_range(compare_range);
            emit!(self, Instruction::Swap { i: 2 });
            emit!(self, Instruction::Copy { i: 2 });
            self.compile_addcompare(*op);
            emit!(self, Instruction::ToBool);
            emit!(self, Instruction::PopJumpIfFalse { delta: cleanup });
        }

        self.compile_expression(last_comparator)?;
        self.set_source_range(compare_range);
        self.compile_addcompare(*last_op);
        emit!(self, Instruction::ToBool);
        self.emit_pop_jump_by_condition(condition, target_block);
        let end = self.new_block();
        emit!(self, PseudoInstruction::JumpNoInterrupt { delta: end });
        self.set_no_location();

        self.use_cpython_label_block(cleanup);
        emit!(self, Instruction::PopTop);
        if !condition {
            emit!(
                self,
                PseudoInstruction::JumpNoInterrupt {
                    delta: target_block
                }
            );
            self.set_no_location();
        }

        self.use_cpython_label_block(end);
        Ok(())
    }

    fn emit_pop_jump_by_condition(&mut self, condition: bool, target_block: BlockIdx) {
        if condition {
            emit!(
                self,
                Instruction::PopJumpIfTrue {
                    delta: target_block
                }
            );
        } else {
            emit!(
                self,
                Instruction::PopJumpIfFalse {
                    delta: target_block,
                }
            );
        }
    }

    fn compile_annotation(&mut self, annotation: &ast::Expr) -> CompileResult<()> {
        if self.future_annotations {
            self.set_source_range(annotation.range());
            self.emit_load_const(ConstantData::Str {
                value: UnparseExpr::new(annotation, &self.source_file)
                    .to_string()
                    .into(),
            });
        } else {
            let was_in_annotation = self.in_annotation;
            self.in_annotation = true;

            // Special handling for starred annotations (*Ts -> Unpack[Ts])
            let result = match annotation {
                ast::Expr::Starred(ast::ExprStarred { value, .. }) => {
                    // *args: *Ts (where Ts is a TypeVarTuple).
                    // Do [annotation_value] = [*Ts].
                    let saved_range = self.current_source_range;
                    self.compile_expression(value)?;
                    self.set_source_range(saved_range);
                    emit!(self, Instruction::UnpackSequence { count: 1 });
                    Ok(())
                }
                _ => self.compile_expression(annotation),
            };

            self.in_annotation = was_in_annotation;
            result?;
        }
        Ok(())
    }

    fn compile_check_annotation_expression(&mut self, expression: &ast::Expr) -> CompileResult<()> {
        self.compile_expression(expression)?;
        self.set_source_range(expression.range());
        emit!(self, Instruction::PopTop);
        Ok(())
    }

    fn compile_check_annotation_subscript(&mut self, expression: &ast::Expr) -> CompileResult<()> {
        match expression {
            ast::Expr::Slice(ast::ExprSlice {
                lower, upper, step, ..
            }) => {
                if let Some(lower) = lower {
                    self.compile_check_annotation_expression(lower)?;
                }
                if let Some(upper) = upper {
                    self.compile_check_annotation_expression(upper)?;
                }
                if let Some(step) = step {
                    self.compile_check_annotation_expression(step)?;
                }
            }
            ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                for element in elts {
                    self.compile_check_annotation_subscript(element)?;
                }
            }
            _ => self.compile_check_annotation_expression(expression)?,
        }
        Ok(())
    }

    fn compile_annotated_assign(
        &mut self,
        target: &ast::Expr,
        annotation: &ast::Expr,
        value: Option<&ast::Expr>,
        simple: bool,
        loc: TextRange,
    ) -> CompileResult<()> {
        // Perform the actual assignment first
        if let Some(value) = value {
            self.compile_expression(value)?;
            self.compile_store(target)?;
        }

        // If we have a simple name in module or class scope, store annotation
        if simple
            && !self.ctx.in_func()
            && let ast::Expr::Name(ast::ExprName { id, .. }) = target
        {
            if self.future_annotations {
                // PEP 563: Store stringified annotation directly to __annotations__
                // Compile annotation as string
                self.compile_annotation(annotation)?;
                self.set_source_range(loc);
                // Load __annotations__
                let annotations_name = self.name("__annotations__");
                emit!(
                    self,
                    Instruction::LoadName {
                        namei: annotations_name
                    }
                );
                // Load the variable name
                self.set_source_range(loc);
                self.emit_load_const(ConstantData::Str {
                    value: self.mangle(id).as_str().into(),
                });
                // Store: __annotations__[name] = annotation
                self.set_source_range(loc);
                emit!(self, Instruction::StoreSubscr);
            } else {
                // PEP 649: Handle conditional annotations
                if self.current_symbol_table().has_conditional_annotations {
                    let scope_type = self.current_symbol_table().typ;
                    let in_conditional_block = self.current_code_info().in_conditional_block > 0;
                    let is_conditional =
                        matches!(scope_type, CompilerScope::Module) || in_conditional_block;

                    if is_conditional {
                        let code_info = self.current_code_info();
                        let annotation_index = code_info.next_conditional_annotation_index;
                        code_info.next_conditional_annotation_index += 1;

                        self.set_source_range(loc);
                        if matches!(scope_type, CompilerScope::Class) {
                            let i = self.get_cell_var_index("__conditional_annotations__");
                            emit!(self, Instruction::LoadDeref { i });
                        } else {
                            let namei = self.name("__conditional_annotations__");
                            emit!(self, Instruction::LoadName { namei });
                        }
                        self.emit_load_const(ConstantData::Integer {
                            value: annotation_index.into(),
                        });
                        emit!(self, Instruction::SetAdd { i: 1 });
                        emit!(self, Instruction::PopTop);
                    }
                }
            }
        }

        if value.is_none() {
            match target {
                ast::Expr::Attribute(ast::ExprAttribute { value, .. }) => {
                    self.compile_check_annotation_expression(value)?;
                }
                ast::Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
                    self.compile_check_annotation_expression(value)?;
                    self.compile_check_annotation_subscript(slice)?;
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn compile_store(&mut self, target: &ast::Expr) -> CompileResult<()> {
        let prev_source_range = self.current_source_range;
        self.set_source_range(target.range());
        let result = (|| -> CompileResult<()> {
            match &target {
                ast::Expr::Name(ast::ExprName { id, .. }) => self.store_name(id)?,
                ast::Expr::Subscript(ast::ExprSubscript {
                    value, slice, ctx, ..
                }) => {
                    self.compile_subscript(value, slice, *ctx)?;
                }
                ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                    self.maybe_add_static_attribute_to_class(value, attr.as_str());
                    self.compile_expression(value)?;
                    self.set_source_range(self.update_start_location_to_match_attr(
                        target.range(),
                        target.range(),
                        attr.as_str(),
                    ));
                    let namei = self.name(attr.as_str());
                    emit!(self, Instruction::StoreAttr { namei });
                }
                ast::Expr::List(ast::ExprList { elts, .. })
                | ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                    let mut seen_star = false;

                    // Scan for star args:
                    for (i, element) in elts.iter().enumerate() {
                        if matches!(element, ast::Expr::Starred(_)) && !seen_star {
                            let before = i;
                            let after = elts.len() - i - 1;
                            if before >= (1 << 8) || after >= ((i32::MAX as usize) >> 8) {
                                return Err(self.error_ranged(
                                    CodegenErrorType::TooManyStarUnpack,
                                    target.range(),
                                ));
                            }
                            let before = before.to_u8().ok_or_else(|| {
                                self.error_ranged(
                                    CodegenErrorType::TooManyStarUnpack,
                                    target.range(),
                                )
                            })?;
                            let after = after.to_u32();
                            let counts = bytecode::UnpackExArgs { before, after };
                            emit!(self, Instruction::UnpackEx { counts });
                            seen_star = true;
                        } else if matches!(element, ast::Expr::Starred(_)) {
                            return Err(self.error(CodegenErrorType::MultipleStarArgs));
                        }
                    }

                    if !seen_star {
                        emit!(
                            self,
                            Instruction::UnpackSequence {
                                count: elts.len().to_u32(),
                            }
                        );
                    }

                    for element in elts {
                        if let ast::Expr::Starred(ast::ExprStarred { value, .. }) = &element {
                            self.compile_store(value)?;
                        } else {
                            self.compile_store(element)?;
                        }
                    }
                }
                _ => {
                    return Err(self.error(match target {
                        ast::Expr::Starred(_) => CodegenErrorType::SyntaxError(
                            "starred assignment target must be in a list or tuple".to_owned(),
                        ),
                        _ => CodegenErrorType::Assign(target.python_name()),
                    }));
                }
            }
            Ok(())
        })();

        self.set_source_range(prev_source_range);
        result
    }

    fn compile_augassign(
        &mut self,
        target: &ast::Expr,
        op: ast::Operator,
        value: &ast::Expr,
    ) -> CompileResult<()> {
        let stmt_range = self.current_source_range;
        let target_range = target.range();
        enum AugAssignKind<'a> {
            Name {
                id: &'a Name,
            },
            Subscript {
                use_slice_opt: bool,
            },
            Attr {
                idx: bytecode::NameIdx,
                attr_range: TextRange,
            },
        }

        let kind = match &target {
            ast::Expr::Name(ast::ExprName { id, .. }) => {
                self.set_source_range(target_range);
                self.compile_name(id, NameUsage::Load)?;
                AugAssignKind::Name { id }
            }
            ast::Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
                let use_slice_opt = self.should_apply_two_element_slice_optimization(slice);
                self.compile_expression(value)?;
                self.set_source_range(target_range);
                if use_slice_opt {
                    let ast::Expr::Slice(slice_expr) = slice.as_ref() else {
                        unreachable!(
                            "should_use_slice_optimization should only return true for ast::Expr::Slice"
                        );
                    };
                    self.compile_slice_two_parts(slice_expr)?;
                    self.set_source_range(target_range);
                    emit!(self, Instruction::Copy { i: 3 });
                    emit!(self, Instruction::Copy { i: 3 });
                    emit!(self, Instruction::Copy { i: 3 });
                    emit!(self, Instruction::BinarySlice);
                } else {
                    self.compile_expression(slice)?;
                    self.set_source_range(target_range);
                    emit!(self, Instruction::Copy { i: 2 });
                    emit!(self, Instruction::Copy { i: 2 });
                    emit!(
                        self,
                        Instruction::BinaryOp {
                            op: BinaryOperator::Subscr
                        }
                    );
                }
                AugAssignKind::Subscript { use_slice_opt }
            }
            ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                let attr = attr.as_str();
                self.compile_expression(value)?;
                let attr_range =
                    self.update_start_location_to_match_attr(target_range, target_range, attr);
                self.set_source_range(target_range);
                emit!(self, Instruction::Copy { i: 1 });
                let idx = self.name(attr);
                self.set_source_range(attr_range);
                self.emit_load_attr(idx);
                AugAssignKind::Attr { idx, attr_range }
            }
            _ => {
                return Err(self.error(CodegenErrorType::Assign(target.python_name())));
            }
        };

        self.compile_expression(value)?;
        self.set_source_range(stmt_range);
        self.compile_op(op, true);

        match kind {
            AugAssignKind::Name { id } => {
                // stack: RESULT
                self.set_source_range(target_range);
                self.compile_name(id, NameUsage::Store)?;
            }
            AugAssignKind::Subscript { use_slice_opt } => {
                self.set_source_range(target_range);
                if use_slice_opt {
                    // stack: CONTAINER START STOP RESULT
                    emit!(self, Instruction::Swap { i: 4 });
                    emit!(self, Instruction::Swap { i: 3 });
                    emit!(self, Instruction::Swap { i: 2 });
                    emit!(self, Instruction::StoreSlice);
                } else {
                    // stack: CONTAINER SLICE RESULT
                    emit!(self, Instruction::Swap { i: 3 });
                    emit!(self, Instruction::Swap { i: 2 });
                    emit!(self, Instruction::StoreSubscr);
                }
            }
            AugAssignKind::Attr { idx, attr_range } => {
                // stack: CONTAINER RESULT
                self.set_source_range(attr_range);
                emit!(self, Instruction::Swap { i: 2 });
                emit!(self, Instruction::StoreAttr { namei: idx });
            }
        }

        Ok(())
    }

    fn compile_op(&mut self, op: ast::Operator, inplace: bool) {
        let bin_op = match op {
            ast::Operator::Add => BinaryOperator::Add,
            ast::Operator::Sub => BinaryOperator::Subtract,
            ast::Operator::Mult => BinaryOperator::Multiply,
            ast::Operator::MatMult => BinaryOperator::MatrixMultiply,
            ast::Operator::Div => BinaryOperator::TrueDivide,
            ast::Operator::FloorDiv => BinaryOperator::FloorDivide,
            ast::Operator::Mod => BinaryOperator::Remainder,
            ast::Operator::Pow => BinaryOperator::Power,
            ast::Operator::LShift => BinaryOperator::Lshift,
            ast::Operator::RShift => BinaryOperator::Rshift,
            ast::Operator::BitOr => BinaryOperator::Or,
            ast::Operator::BitXor => BinaryOperator::Xor,
            ast::Operator::BitAnd => BinaryOperator::And,
        };

        let op = if inplace { bin_op.as_inplace() } else { bin_op };
        emit!(self, Instruction::BinaryOp { op })
    }

    /// Implement boolean short circuit evaluation logic.
    /// https://en.wikipedia.org/wiki/Short-circuit_evaluation
    ///
    /// This means, in a boolean statement 'x and y' the variable y will
    /// not be evaluated when x is false.
    ///
    /// The idea is to jump to a label if the expression is either true or false
    /// (indicated by the condition parameter).
    fn compile_jump_if_inner(
        &mut self,
        expression: &ast::Expr,
        condition: bool,
        target_block: BlockIdx,
        source_range: Option<TextRange>,
    ) -> CompileResult<()> {
        let prev_source_range = self.current_source_range;
        self.set_source_range(source_range.unwrap_or_else(|| expression.range()));

        // Compile expression for test, and jump to label if false
        let result = match &expression {
            ast::Expr::BoolOp(ast::ExprBoolOp { op, values, .. }) => {
                let (last_value, prefix_values) = values.split_last().unwrap();
                let cond2 = matches!(op, ast::BoolOp::Or);
                let next2 = if cond2 != condition {
                    self.new_block()
                } else {
                    target_block
                };

                for value in prefix_values {
                    self.compile_jump_if_inner(value, cond2, next2, source_range)?;
                }
                self.compile_jump_if_inner(last_value, condition, target_block, source_range)?;

                if next2 != target_block {
                    self.use_cpython_label_block(next2);
                }
                Ok(())
            }
            ast::Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::Not,
                operand,
                ..
            }) => self.compile_jump_if_inner(operand, !condition, target_block, source_range),
            ast::Expr::If(ast::ExprIf {
                test, body, orelse, ..
            }) => {
                let end = self.new_block();
                let next2 = self.new_block();
                self.compile_jump_if_inner(test, false, next2, source_range)?;
                self.compile_jump_if_inner(body, condition, target_block, source_range)?;
                emit!(self, PseudoInstruction::JumpNoInterrupt { delta: end });
                self.set_no_location();

                self.use_cpython_label_block(next2);
                self.compile_jump_if_inner(orelse, condition, target_block, source_range)?;

                self.use_cpython_label_block(end);
                Ok(())
            }
            ast::Expr::Compare(ast::ExprCompare {
                left,
                ops,
                comparators,
                ..
            }) if ops.len() > 1 => {
                self.set_source_range(expression.range());
                self.compile_jump_if_compare(left, ops, comparators, condition, target_block)
            }
            _ => {
                // Fall back case which always will work!
                self.compile_expression(expression)?;
                self.set_source_range(expression.range());
                emit!(self, Instruction::ToBool);
                if condition {
                    emit!(
                        self,
                        Instruction::PopJumpIfTrue {
                            delta: target_block,
                        }
                    );
                } else {
                    emit!(
                        self,
                        Instruction::PopJumpIfFalse {
                            delta: target_block,
                        }
                    );
                }
                Ok(())
            }
        };

        self.set_source_range(prev_source_range);
        result
    }

    fn compile_jump_if(
        &mut self,
        expression: &ast::Expr,
        condition: bool,
        target_block: BlockIdx,
    ) -> CompileResult<()> {
        self.compile_jump_if_inner(expression, condition, target_block, None)
    }

    /// Compile a boolean operation as an expression.
    /// This means, that the last value remains on the stack.
    fn compile_bool_op(&mut self, op: ast::BoolOp, values: &[ast::Expr]) -> CompileResult<()> {
        let boolop_range = self.current_source_range;
        let after_block = self.new_block();
        let (last_value, prefix_values) = values.split_last().unwrap();

        for value in prefix_values {
            self.compile_expression(value)?;
            self.set_source_range(boolop_range);
            self.emit_short_circuit_test(op, after_block);
            self.set_source_range(boolop_range);
            emit!(self, Instruction::PopTop);
        }

        self.compile_expression(last_value)?;
        self.use_cpython_label_block(after_block);
        Ok(())
    }

    /// Emit CPython-style pseudo conditional jump for short-circuit evaluation.
    /// flowgraph.c lowers it to `COPY 1; TO_BOOL; POP_JUMP_IF_*`.
    fn emit_short_circuit_test(&mut self, op: ast::BoolOp, target: BlockIdx) {
        match op {
            ast::BoolOp::And => {
                emit!(self, PseudoInstruction::JumpIfFalse { delta: target });
            }
            ast::BoolOp::Or => {
                emit!(self, PseudoInstruction::JumpIfTrue { delta: target });
            }
        }
    }

    fn compile_subdict(
        &mut self,
        items: &[ast::DictItem],
        begin: usize,
        end: usize,
        range: TextRange,
    ) -> CompileResult<()> {
        let n = end - begin;
        let big = n * 2 > STACK_USE_GUIDELINE as usize;
        if big {
            self.set_source_range(range);
            emit!(self, Instruction::BuildMap { count: 0 });
        }
        for item in &items[begin..end] {
            self.compile_expression(item.key.as_ref().unwrap())?;
            self.compile_expression(&item.value)?;
            if big {
                self.set_source_range(range);
                emit!(self, Instruction::MapAdd { i: 1 });
            }
        }
        if !big {
            self.set_source_range(range);
            emit!(self, Instruction::BuildMap { count: n.to_u32() });
        }
        Ok(())
    }

    fn compile_dict(&mut self, items: &[ast::DictItem], range: TextRange) -> CompileResult<()> {
        let n = items.len();
        let mut have_dict = false;
        let mut elements = 0usize;

        for (i, item) in items.iter().enumerate() {
            if item.key.is_none() {
                if elements != 0 {
                    self.compile_subdict(items, i - elements, i, range)?;
                    if have_dict {
                        self.set_source_range(range);
                        emit!(self, Instruction::DictUpdate { i: 1 });
                    }
                    have_dict = true;
                    elements = 0;
                }
                if !have_dict {
                    self.set_source_range(range);
                    emit!(self, Instruction::BuildMap { count: 0 });
                    have_dict = true;
                }
                self.compile_expression(&item.value)?;
                self.set_source_range(range);
                emit!(self, Instruction::DictUpdate { i: 1 });
            } else if elements * 2 > STACK_USE_GUIDELINE as usize {
                self.compile_subdict(items, i - elements, i + 1, range)?;
                if have_dict {
                    self.set_source_range(range);
                    emit!(self, Instruction::DictUpdate { i: 1 });
                }
                have_dict = true;
                elements = 0;
            } else {
                elements += 1;
            }
        }

        if elements != 0 {
            self.compile_subdict(items, n - elements, n, range)?;
            if have_dict {
                self.set_source_range(range);
                emit!(self, Instruction::DictUpdate { i: 1 });
            }
            have_dict = true;
        }
        if !have_dict {
            self.set_source_range(range);
            emit!(self, Instruction::BuildMap { count: 0 });
        }

        Ok(())
    }

    /// Compile the yield-from/await sequence using SEND/END_SEND/CLEANUP_THROW.
    /// compiler_add_yield_from
    /// This generates:
    ///   send:
    ///     SEND exit
    ///     SETUP_FINALLY fail (via exception table)
    ///     YIELD_VALUE 1
    ///     POP_BLOCK (NO_LOCATION)
    ///     RESUME
    ///     JUMP send
    ///   fail:
    ///     CLEANUP_THROW
    ///     JUMP exit
    ///   exit:
    ///     END_SEND
    fn compile_yield_from_sequence(&mut self, is_await: bool) -> BlockIdx {
        let send_block = self.new_block();
        let fail_block = self.new_block();
        let exit_block = self.new_block();

        // send:
        self.use_cpython_label_block(send_block);
        emit!(self, Instruction::Send { delta: exit_block });

        // SETUP_FINALLY fail - set up exception handler for YIELD_VALUE
        emit!(self, PseudoInstruction::SetupFinally { delta: fail_block });

        // YIELD_VALUE with arg=1 (yield-from/await mode - not wrapped for async gen)
        emit!(self, Instruction::YieldValue { arg: 1 });

        // POP_BLOCK before RESUME
        emit!(self, PseudoInstruction::PopBlock);
        self.set_no_location();

        // RESUME
        emit!(
            self,
            Instruction::Resume {
                context: if is_await {
                    oparg::ResumeContext::from(oparg::ResumeLocation::AfterAwait)
                } else {
                    oparg::ResumeContext::from(oparg::ResumeLocation::AfterYieldFrom)
                }
            }
        );

        // JUMP_BACKWARD_NO_INTERRUPT send
        emit!(
            self,
            PseudoInstruction::JumpNoInterrupt { delta: send_block }
        );

        // fail: CLEANUP_THROW
        // Stack when exception: [receiver, yielded_value, exc]
        // CLEANUP_THROW: [sub_iter, last_sent_val, exc] -> [None, value]
        // CPython lets this block fall through to END_SEND during codegen;
        // push_cold_blocks_to_end later inserts the no-interrupt jump after
        // moving the cold fail block behind the warm exit path.
        self.use_cpython_label_block(fail_block);
        emit!(self, Instruction::CleanupThrow);

        // exit: END_SEND
        // Stack: [receiver, value] (from SEND) or [None, value] (from CLEANUP_THROW)
        // END_SEND: [receiver/None, value] -> [value]
        self.use_cpython_label_block(exit_block);
        emit!(self, Instruction::EndSend);

        send_block
    }

    fn ast_constant_value(&self, expr: &ast::Expr) -> Option<ConstantData> {
        expr.as_constant_expr()
            .map(|expr| ast_constant_value_to_constant_data(expr.value.clone()))
    }

    fn single_runtime_interpolation(
        expr_tstring: &ast::ExprTString,
    ) -> Option<(&ast::ConstantValue, Option<&ast::Expr>)> {
        let tstring = expr_tstring.as_single_part_tstring()?;
        let interpolation = tstring.elements.first()?.as_interpolation()?;
        Some((
            interpolation.runtime_str.as_ref()?,
            interpolation.runtime_interpolation_format_spec.as_deref(),
        ))
    }

    fn compile_expression(&mut self, expression: &ast::Expr) -> CompileResult<()> {
        nanocode_trace::trace!("Compiling {expression:?}");
        let range = expression.range();
        self.set_source_range(range);

        if let Some(constant) = self.ast_constant_value(expression) {
            self.emit_load_const(constant);
            return Ok(());
        }

        match &expression {
            ast::Expr::Call(ast::ExprCall {
                func, arguments, ..
            }) => self.compile_call(func, arguments)?,
            ast::Expr::BoolOp(ast::ExprBoolOp { op, values, .. }) => {
                self.compile_bool_op(*op, values)?
            }
            ast::Expr::BinOp(ast::ExprBinOp {
                left, op, right, ..
            }) => {
                self.compile_expression(left)?;
                self.compile_expression(right)?;

                // Restore full expression range before emitting the operation
                self.set_source_range(range);
                self.compile_op(*op, false);
            }
            ast::Expr::Subscript(ast::ExprSubscript {
                value, slice, ctx, ..
            }) => {
                self.compile_subscript(value, slice, *ctx)?;
            }
            ast::Expr::UnaryOp(ast::ExprUnaryOp { op, operand, .. }) => {
                self.compile_expression(operand)?;

                // Restore full expression range before emitting the operation
                self.set_source_range(range);
                match op {
                    ast::UnaryOp::UAdd => emit!(
                        self,
                        Instruction::CallIntrinsic1 {
                            func: bytecode::IntrinsicFunction1::UnaryPositive
                        }
                    ),
                    ast::UnaryOp::USub => emit!(self, Instruction::UnaryNegative),
                    ast::UnaryOp::Not => {
                        emit!(self, Instruction::ToBool);
                        emit!(self, Instruction::UnaryNot);
                    }
                    ast::UnaryOp::Invert => emit!(self, Instruction::UnaryInvert),
                };
            }
            ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                // Check for super() attribute access optimization
                if let Some(super_type) = self.can_optimize_super_call(value, attr.as_str()) {
                    // super().attr or super(cls, self).attr optimization
                    // Stack: [global_super, class, self] → LOAD_SUPER_ATTR → [attr]
                    let ast::Expr::Call(ast::ExprCall {
                        func: super_func, ..
                    }) = value.as_ref()
                    else {
                        unreachable!("can_optimize_super_call only accepts calls");
                    };
                    self.load_args_for_super(&super_type, super_func.range(), value.range())?;
                    let attr_access_range =
                        self.update_start_location_to_match_attr(range, range, attr.as_str());
                    self.set_source_range(range);
                    let idx = self.name(attr.as_str());
                    match super_type {
                        SuperCallType::TwoArg { .. } => {
                            self.emit_load_super_attr(idx);
                        }
                        SuperCallType::ZeroArg => {
                            self.emit_load_zero_super_attr(idx);
                        }
                    }
                    self.set_source_range(attr_access_range);
                    emit!(self, Instruction::Nop);
                } else {
                    // Normal attribute access
                    self.compile_expression(value)?;
                    self.set_source_range(self.update_start_location_to_match_attr(
                        range,
                        range,
                        attr.as_str(),
                    ));
                    let idx = self.name(attr.as_str());
                    self.emit_load_attr(idx);
                }
            }
            ast::Expr::Compare(ast::ExprCompare {
                left,
                ops,
                comparators,
                ..
            }) => {
                self.compile_compare(left, ops, comparators)?;
            }
            ast::Expr::Constant(ast::ExprConstant { value, .. }) => {
                self.emit_load_const(ast_constant_value_to_constant_data(value.clone()));
            }
            ast::Expr::List(ast::ExprList { elts, range, .. }) => {
                self.set_source_range(*range);
                self.starunpack_helper(elts, 0, CollectionType::List)?;
            }
            ast::Expr::Tuple(ast::ExprTuple { elts, range, .. }) => {
                self.set_source_range(*range);
                self.starunpack_helper(elts, 0, CollectionType::Tuple)?;
            }
            ast::Expr::Set(ast::ExprSet { elts, range, .. }) => {
                self.set_source_range(*range);
                self.starunpack_helper(elts, 0, CollectionType::Set)?;
            }
            ast::Expr::Dict(ast::ExprDict { items, range, .. }) => {
                self.compile_dict(items, *range)?;
            }
            ast::Expr::Slice(ast::ExprSlice {
                lower,
                upper,
                step,
                range,
                ..
            }) => {
                if let Some(folded_const) = self.try_fold_constant_slice(
                    lower.as_deref(),
                    upper.as_deref(),
                    step.as_deref(),
                )? {
                    self.set_source_range(*range);
                    self.emit_load_const(folded_const);
                    return Ok(());
                }
                if let Some(lower) = lower {
                    self.compile_expression(lower)?;
                } else {
                    self.set_source_range(*range);
                    self.emit_load_const(ConstantData::None);
                }
                if let Some(upper) = upper {
                    self.compile_expression(upper)?;
                } else {
                    self.set_source_range(*range);
                    self.emit_load_const(ConstantData::None);
                }
                if let Some(step) = step {
                    self.compile_expression(step)?;
                }
                let argc = match step {
                    Some(_) => BuildSliceArgCount::Three,
                    None => BuildSliceArgCount::Two,
                };
                self.set_source_range(*range);
                emit!(self, Instruction::BuildSlice { argc });
            }
            ast::Expr::Yield(ast::ExprYield { value, .. }) => {
                if !self.ctx.in_func() {
                    return Err(self.error(CodegenErrorType::InvalidYield));
                }
                self.mark_generator();
                match value {
                    Some(expression) => self.compile_expression(expression)?,
                    Option::None => self.emit_load_const(ConstantData::None),
                };
                self.set_source_range(range);
                if self.ctx.func == FunctionContext::AsyncFunction {
                    emit!(
                        self,
                        Instruction::CallIntrinsic1 {
                            func: bytecode::IntrinsicFunction1::AsyncGenWrap
                        }
                    );
                }
                // arg=0: direct yield (wrapped for async generators)
                emit!(self, Instruction::YieldValue { arg: 0 });
                emit!(
                    self,
                    Instruction::Resume {
                        context: oparg::ResumeContext::from(oparg::ResumeLocation::AfterYield)
                    }
                );
            }
            ast::Expr::Await(ast::ExprAwait { value, .. }) => {
                if self.ctx.func != FunctionContext::AsyncFunction
                    && !self.allows_top_level_await_in_current_context()
                {
                    return Err(self.error(CodegenErrorType::InvalidAwait));
                }
                self.compile_expression(value)?;
                self.set_source_range(range);
                emit!(self, Instruction::GetAwaitable { r#where: 0 });
                self.emit_load_const(ConstantData::None);
                let _ = self.compile_yield_from_sequence(true);
            }
            ast::Expr::YieldFrom(ast::ExprYieldFrom { value, .. }) => {
                match self.ctx.func {
                    FunctionContext::NoFunction => {
                        return Err(self.error(CodegenErrorType::InvalidYieldFrom));
                    }
                    FunctionContext::AsyncFunction => {
                        return Err(self.error(CodegenErrorType::AsyncYieldFrom));
                    }
                    FunctionContext::Function => {}
                }
                self.mark_generator();
                self.compile_expression(value)?;
                self.set_source_range(range);
                emit!(self, Instruction::GetYieldFromIter);
                self.emit_load_const(ConstantData::None);
                let _ = self.compile_yield_from_sequence(false);
            }
            ast::Expr::Name(ast::ExprName { id, .. }) => self.load_name(id)?,
            ast::Expr::Lambda(ast::ExprLambda {
                parameters,
                body,
                range,
                ..
            }) => {
                let default_params = ast::Parameters::default();
                let params = parameters.as_deref().unwrap_or(&default_params);
                validate_duplicate_params(params).map_err(|e| self.error(e))?;

                let prev_ctx = self.ctx;
                let name = "<lambda>".to_owned();

                // Prepare defaults before entering function
                let defaults: Vec<_> = core::iter::empty()
                    .chain(&params.posonlyargs)
                    .chain(&params.args)
                    .filter_map(|x| x.default.as_deref())
                    .collect();
                let have_defaults = !defaults.is_empty();

                if have_defaults {
                    let size = defaults.len().to_u32();
                    for element in &defaults {
                        self.compile_expression(element)?;
                    }
                    self.set_source_range(*range);
                    emit!(self, Instruction::BuildTuple { count: size });
                }

                // Prepare keyword-only defaults
                let mut kw_with_defaults = vec![];
                for kwonlyarg in &params.kwonlyargs {
                    if let Some(default) = &kwonlyarg.default {
                        kw_with_defaults.push((&kwonlyarg.parameter, default));
                    }
                }

                let have_kwdefaults = !kw_with_defaults.is_empty();
                if have_kwdefaults {
                    let default_kw_count = kw_with_defaults.len();
                    for (arg, default) in &kw_with_defaults {
                        self.set_source_range(*range);
                        self.emit_load_const(ConstantData::Str {
                            value: self.mangle(arg.name().id()).as_str().into(),
                        });
                        self.compile_expression(default)?;
                    }
                    self.set_source_range(*range);
                    emit!(
                        self,
                        Instruction::BuildMap {
                            count: default_kw_count.to_u32(),
                        }
                    );
                }

                self.enter_function(&name, params)?;
                let mut func_flags = bytecode::MakeFunctionFlags::new();
                if have_defaults {
                    func_flags.insert(bytecode::MakeFunctionFlag::Defaults);
                }
                if have_kwdefaults {
                    func_flags.insert(bytecode::MakeFunctionFlag::KwOnlyDefaults);
                }

                // Set qualname for lambda
                self.set_qualname();

                self.ctx = CompileContext {
                    in_class: prev_ctx.in_class,
                    func: FunctionContext::Function,
                    // Lambda is never async, so new scope is not async
                    in_async_scope: false,
                };

                self.compile_expression(body)?;
                let is_generator = self
                    .current_code_info()
                    .flags
                    .contains(bytecode::CodeFlags::GENERATOR);
                if is_generator {
                    // codegen_lambda() calls OptimizeAndAssemble with
                    // addNone=0, so AddReturnAtEnd appends RETURN_VALUE without
                    // adding None to co_consts.
                    emit!(self, Instruction::ReturnValue);
                    self.set_no_location();
                } else {
                    self.set_source_range(body.range());
                    self.emit_return_value();
                    self.emit_return_const_no_location(ConstantData::None);
                }
                let code = self.exit_scope();

                // Create lambda function with closure
                self.set_source_range(*range);
                self.make_closure(code, func_flags)?;

                self.ctx = prev_ctx;
            }
            ast::Expr::ListComp(ast::ExprListComp {
                elt,
                generators,
                range,
                ..
            }) => {
                self.compile_comprehension(
                    "<listcomp>",
                    Some(
                        Instruction::BuildList {
                            count: OpArgMarker::marker(),
                        }
                        .into(),
                    ),
                    generators,
                    &|compiler, collection_add_i| {
                        compiler.compile_comprehension_element(elt)?;
                        compiler.set_source_range(elt.range());
                        emit!(
                            compiler,
                            Instruction::ListAppend {
                                i: collection_add_i.to_u32(),
                            }
                        );
                        Ok(())
                    },
                    ComprehensionType::List,
                    Self::contains_await(elt) || Self::generators_contain_await(generators),
                    *range,
                    elt.range(),
                    elt.range(),
                )?;
            }
            ast::Expr::SetComp(ast::ExprSetComp {
                elt,
                generators,
                range,
                ..
            }) => {
                self.compile_comprehension(
                    "<setcomp>",
                    Some(
                        Instruction::BuildSet {
                            count: OpArgMarker::marker(),
                        }
                        .into(),
                    ),
                    generators,
                    &|compiler, collection_add_i| {
                        compiler.compile_comprehension_element(elt)?;
                        compiler.set_source_range(elt.range());
                        emit!(
                            compiler,
                            Instruction::SetAdd {
                                i: collection_add_i.to_u32(),
                            }
                        );
                        Ok(())
                    },
                    ComprehensionType::Set,
                    Self::contains_await(elt) || Self::generators_contain_await(generators),
                    *range,
                    elt.range(),
                    elt.range(),
                )?;
            }
            ast::Expr::DictComp(ast::ExprDictComp {
                key,
                value,
                generators,
                range,
                ..
            }) => {
                let Some(key) = key.as_deref() else {
                    self.set_source_range(*range);
                    return Err(self.error(CodegenErrorType::SyntaxError(
                        "dict unpacking cannot be used in dict comprehension".to_owned(),
                    )));
                };
                self.compile_comprehension(
                    "<dictcomp>",
                    Some(
                        Instruction::BuildMap {
                            count: OpArgMarker::marker(),
                        }
                        .into(),
                    ),
                    generators,
                    &|compiler, collection_add_i| {
                        // changed evaluation order for Py38 named expression PEP 572
                        compiler.compile_expression(key)?;
                        compiler.compile_expression(value)?;

                        compiler.set_source_range(TextRange::new(
                            key.range().start(),
                            value.range().end(),
                        ));
                        emit!(
                            compiler,
                            Instruction::MapAdd {
                                i: collection_add_i.to_u32(),
                            }
                        );

                        Ok(())
                    },
                    ComprehensionType::Dict,
                    Self::contains_await(key)
                        || Self::contains_await(value)
                        || Self::generators_contain_await(generators),
                    *range,
                    TextRange::new(key.range().start(), value.range().end()),
                    key.range(),
                )?;
            }
            ast::Expr::Generator(ast::ExprGenerator {
                elt,
                generators,
                range,
                ..
            }) => {
                self.compile_generator_expression(elt, generators, *range)?;
            }
            ast::Expr::Starred(ast::ExprStarred { value, .. }) => {
                if self.in_annotation {
                    // In annotation context, starred expressions are allowed (PEP 646)
                    // For now, just compile the inner value without wrapping with Unpack
                    // This is a temporary solution until we figure out how to properly import typing
                    self.compile_expression(value)?;
                } else {
                    return Err(self.error(CodegenErrorType::InvalidStarExpr));
                }
            }
            ast::Expr::If(ast::ExprIf {
                test, body, orelse, ..
            }) => {
                let after_block = self.new_block();
                let else_block = self.new_block();
                self.compile_jump_if(test, false, else_block)?;

                // True case
                self.compile_expression(body)?;
                emit!(
                    self,
                    PseudoInstruction::JumpNoInterrupt { delta: after_block }
                );
                self.set_no_location();

                // False case
                self.use_cpython_label_block(else_block);
                self.compile_expression(orelse)?;

                // End
                self.use_cpython_label_block(after_block);
            }

            ast::Expr::Named(ast::ExprNamed {
                target,
                value,
                node_index: _,
                range,
            }) => {
                // Walrus targets in inlined comps should NOT be hidden from locals()
                if self.current_code_info().in_inlined_comp
                    && let ast::Expr::Name(ast::ExprName { id, .. }) = target.as_ref()
                {
                    let name = self.mangle(id);
                    let info = self.code_stack.last_mut().unwrap();
                    info.metadata.fast_hidden.insert(name.to_string(), false);
                    info.metadata
                        .fast_hidden_final
                        .swap_remove(name.into_owned().as_str());
                }
                self.compile_expression(value)?;
                self.set_source_range(*range);
                emit!(self, Instruction::Copy { i: 1 });
                self.compile_store(target)?;
                self.set_source_range(target.range());
            }
            ast::Expr::FString(fstring) => {
                if let Some(joined_str) = fstring.runtime_joined_str.as_ref() {
                    return self.compile_runtime_joined_str(fstring, joined_str);
                }
                self.compile_expr_fstring(fstring)?;
            }
            ast::Expr::TString(tstring) => {
                if let Some(template_str) = tstring.runtime_template_str.as_ref() {
                    return self.compile_runtime_template_str(tstring, template_str);
                }
                if let Some(interpolation) = Self::single_runtime_interpolation(tstring)
                    && self.compile_runtime_interpolation(tstring, interpolation)?
                {
                    return Ok(());
                }
                self.compile_expr_tstring(tstring)?;
            }
            ast::Expr::StringLiteral(string) => {
                let value = string_literal_value(&self.source_file, &string.value);
                self.emit_load_const(ConstantData::Str { value });
            }
            ast::Expr::BytesLiteral(bytes) => {
                let iter = bytes.value.iter().flat_map(|x| x.iter().copied());
                let v: Vec<u8> = iter.collect();
                self.emit_load_const(ConstantData::Bytes { value: v });
            }
            ast::Expr::NumberLiteral(number) => match &number.value {
                ast::Number::Int(int) => {
                    let value = ruff_int_to_bigint(int).map_err(|e| self.error(e))?;
                    self.emit_load_const(ConstantData::Integer { value });
                }
                ast::Number::Float(float) => {
                    self.emit_load_const(ConstantData::Float { value: *float });
                }
                ast::Number::Complex { real, imag } => {
                    self.emit_load_const(ConstantData::Complex {
                        value: Complex::new(*real, *imag),
                    });
                }
            },
            ast::Expr::BooleanLiteral(b) => {
                self.emit_load_const(ConstantData::Boolean { value: b.value });
            }
            ast::Expr::NoneLiteral(_) => {
                self.emit_load_const(ConstantData::None);
            }
            ast::Expr::EllipsisLiteral(_) => {
                self.emit_load_const(ConstantData::Ellipsis);
            }
            ast::Expr::IpyEscapeCommand(expr) => {
                return Err(self.error_ranged(
                    CodegenErrorType::SyntaxError("invalid syntax".to_owned()),
                    expr.range,
                ));
            }
        }
        Ok(())
    }

    /// The called name of a `name(genexpr)` call, the shape
    /// `maybe_optimize_function_call()` reserves a `skip_optimization` label
    /// for. An `await` or an `async for` inside the generator does not
    /// disqualify it: the inlined loop raises the same `TypeError` that
    /// calling the builtin on an async generator would.
    fn cpython_genexpr_call_name<'a>(
        &self,
        func: &'a ast::Expr,
        args: &ast::Arguments,
    ) -> Option<&'a str> {
        let ast::Expr::Name(ast::ExprName { id, .. }) = func else {
            return None;
        };
        let [ast::Expr::Generator(ast::ExprGenerator { .. })] = &args.args[..] else {
            return None;
        };
        if !args.keywords.is_empty() {
            return None;
        }
        let table = self.current_symbol_table();
        table.sub_tables.get(table.next_sub_table)?;
        Some(id.as_str())
    }

    fn detect_builtin_generator_call(
        &self,
        func: &ast::Expr,
        args: &ast::Arguments,
    ) -> Option<BuiltinGeneratorCallKind> {
        match self.cpython_genexpr_call_name(func, args)? {
            "tuple" => Some(BuiltinGeneratorCallKind::Tuple),
            "all" => Some(BuiltinGeneratorCallKind::All),
            "any" => Some(BuiltinGeneratorCallKind::Any),
            _ => None,
        }
    }

    /// Emit the optimized inline loop for builtin(genexpr) calls.
    ///
    /// Stack on entry: `[func]` where `func` is the builtin candidate.
    /// On return the compiler is positioned at the skip-optimization block so the
    /// normal call path can compile the original generator argument again.
    fn optimize_builtin_generator_call(
        &mut self,
        kind: BuiltinGeneratorCallKind,
        generator_expr: &ast::Expr,
        loc: TextRange,
        end: BlockIdx,
    ) -> CompileResult<()> {
        let common_constant = match kind {
            BuiltinGeneratorCallKind::Tuple => bytecode::CommonConstant::BuiltinTuple,
            BuiltinGeneratorCallKind::All => bytecode::CommonConstant::BuiltinAll,
            BuiltinGeneratorCallKind::Any => bytecode::CommonConstant::BuiltinAny,
        };

        let skip_optimization = self.new_block();

        // Stack: [func] - copy function for identity check
        self.set_source_range(loc);
        emit!(self, Instruction::Copy { i: 1 });
        emit!(
            self,
            Instruction::LoadCommonConstant {
                idx: common_constant
            }
        );
        emit!(self, Instruction::IsOp { invert: Invert::No });
        emit!(
            self,
            Instruction::PopJumpIfFalse {
                delta: skip_optimization
            }
        );
        emit!(self, Instruction::PopTop);

        if matches!(kind, BuiltinGeneratorCallKind::Tuple) {
            self.set_source_range(loc);
            emit!(self, Instruction::BuildList { count: 0 });
        }

        let symbol_table_cursors = self.current_symbol_table_cursors();
        self.compile_expression(generator_expr)?;
        self.set_symbol_table_cursors(symbol_table_cursors);

        let loop_block = self.new_block();
        let cleanup = self.new_block();
        self.use_cpython_label_block(loop_block);
        self.set_source_range(loc);
        emit!(self, Instruction::ForIter { delta: cleanup });

        match kind {
            BuiltinGeneratorCallKind::Tuple => {
                self.set_source_range(loc);
                emit!(self, Instruction::ListAppend { i: 2 });
                self.set_source_range(loc);
                emit!(self, PseudoInstruction::Jump { delta: loop_block });
            }
            BuiltinGeneratorCallKind::All => {
                self.set_source_range(loc);
                emit!(self, Instruction::ToBool);
                emit!(self, Instruction::PopJumpIfTrue { delta: loop_block });
                emit!(self, Instruction::PopIter);
                self.set_no_location();
                self.set_source_range(loc);
                self.emit_load_const(ConstantData::Boolean { value: false });
                self.set_source_range(loc);
                emit!(self, PseudoInstruction::Jump { delta: end });
            }
            BuiltinGeneratorCallKind::Any => {
                self.set_source_range(loc);
                emit!(self, Instruction::ToBool);
                emit!(self, Instruction::PopJumpIfFalse { delta: loop_block });
                emit!(self, Instruction::PopIter);
                self.set_no_location();
                self.set_source_range(loc);
                self.emit_load_const(ConstantData::Boolean { value: true });
                self.set_source_range(loc);
                emit!(self, PseudoInstruction::Jump { delta: end });
            }
        }

        self.use_cpython_label_block(cleanup);
        emit!(self, Instruction::EndFor);
        self.set_no_location();
        emit!(self, Instruction::PopIter);
        self.set_no_location();
        match kind {
            BuiltinGeneratorCallKind::Tuple => {
                self.set_source_range(loc);
                emit!(
                    self,
                    Instruction::CallIntrinsic1 {
                        func: IntrinsicFunction1::ListToTuple
                    }
                );
            }
            BuiltinGeneratorCallKind::All => {
                self.set_source_range(loc);
                self.emit_load_const(ConstantData::Boolean { value: true });
            }
            BuiltinGeneratorCallKind::Any => {
                self.set_source_range(loc);
                self.emit_load_const(ConstantData::Boolean { value: false });
            }
        }
        self.set_source_range(loc);
        emit!(self, PseudoInstruction::Jump { delta: end });

        self.use_cpython_label_block(skip_optimization);
        Ok(())
    }

    fn can_use_cpython_method_call(&self, value: &ast::Expr, args: &ast::Arguments) -> bool {
        let is_import = matches!(value, ast::Expr::Name(ast::ExprName { id, .. })
            if self.is_name_imported(id.as_str()));
        if is_import {
            return false;
        }

        if args.args.len() + args.keywords.len() + usize::from(!args.keywords.is_empty())
            >= STACK_USE_GUIDELINE as usize
        {
            return false;
        }

        !args
            .args
            .iter()
            .any(|arg| matches!(arg, ast::Expr::Starred(_)))
            && args.keywords.iter().all(|kw| kw.arg.is_some())
    }

    fn compile_method_call_arguments(
        &mut self,
        args: &ast::Arguments,
        call_range: TextRange,
        kw_names_range: TextRange,
    ) -> CompileResult<()> {
        for arg in &args.args {
            self.compile_expression(arg)?;
        }

        if args.keywords.is_empty() {
            self.set_source_range(call_range);
            emit!(
                self,
                Instruction::Call {
                    argc: args.args.len().to_u32()
                }
            );
            return Ok(());
        }

        let mut kwarg_names = Vec::with_capacity(args.keywords.len());
        for keyword in &args.keywords {
            kwarg_names.push(ConstantData::Str {
                value: keyword.arg.as_ref().unwrap().as_str().into(),
            });
            self.compile_expression(&keyword.value)?;
        }
        self.set_source_range(kw_names_range);
        self.emit_load_const(ConstantData::Tuple {
            elements: kwarg_names,
        });
        self.set_source_range(call_range);
        emit!(
            self,
            Instruction::CallKw {
                argc: (args.args.len() + args.keywords.len()).to_u32()
            }
        );
        Ok(())
    }

    fn compile_call(&mut self, func: &ast::Expr, args: &ast::Arguments) -> CompileResult<()> {
        // Save the call expression's source range so CALL instructions use the
        // call start line, not the last argument's line.
        let call_range = self.current_source_range;
        self.validate_keywords(&args.keywords)?;
        let uses_ex_call = self.call_uses_ex_call(args);

        // Method call: obj → LOAD_ATTR_METHOD → [method, self_or_null] → args → CALL
        // Regular call: func → PUSH_NULL → args → CALL
        if let ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) = &func {
            if !self.can_use_cpython_method_call(value, args) {
                self.check_caller(func)?;
                self.compile_expression(func)?;
                self.set_source_range(func.range());
                emit!(self, Instruction::PushNull);
                self.codegen_call_helper(0, args, call_range, None)?;
                return Ok(());
            }

            // Check for super() method call optimization
            if let Some(super_type) = self.can_optimize_super_call(value, attr.as_str()) {
                // super().method() or super(cls, self).method() optimization
                // CALL path: [global_super, class, self] → LOAD_SUPER_METHOD → [method, self]
                let ast::Expr::Call(ast::ExprCall {
                    func: super_func, ..
                }) = value.as_ref()
                else {
                    unreachable!("can_optimize_super_call only accepts calls");
                };
                self.load_args_for_super(&super_type, super_func.range(), value.range())?;
                let attr_access_range = self.update_start_location_to_match_attr(
                    func.range(),
                    func.range(),
                    attr.as_str(),
                );
                let method_call_range = self.update_start_location_to_match_attr(
                    call_range,
                    func.range(),
                    attr.as_str(),
                );
                self.set_source_range(func.range());
                let idx = self.name(attr.as_str());
                match super_type {
                    SuperCallType::TwoArg { .. } => {
                        self.emit_load_super_method(idx);
                    }
                    SuperCallType::ZeroArg => {
                        self.emit_load_zero_super_method(idx);
                    }
                }
                // NOP for line tracking at .method( line
                self.set_source_range(attr_access_range);
                emit!(self, Instruction::Nop);
                // CALL at .method( line (not the full expression line)
                self.compile_method_call_arguments(args, method_call_range, attr_access_range)?;
            } else {
                self.compile_expression(value)?;
                let idx = self.name(attr.as_str());
                let attr_access_range = self.update_start_location_to_match_attr(
                    func.range(),
                    func.range(),
                    attr.as_str(),
                );
                let method_call_range = self.update_start_location_to_match_attr(
                    call_range,
                    func.range(),
                    attr.as_str(),
                );
                self.set_source_range(attr_access_range);
                self.emit_load_attr_method(idx);
                self.compile_method_call_arguments(args, method_call_range, attr_access_range)?;
            }
        } else if let Some(kind) = (!uses_ex_call)
            .then(|| self.detect_builtin_generator_call(func, args))
            .flatten()
        {
            let skip_normal_call = self.new_block();
            self.check_caller(func)?;
            self.compile_expression(func)?;
            self.optimize_builtin_generator_call(
                kind,
                &args.args[0],
                func.range(),
                skip_normal_call,
            )?;
            self.set_source_range(func.range());
            emit!(self, Instruction::PushNull);
            self.codegen_call_helper(0, args, call_range, None)?;
            self.use_cpython_label_block(skip_normal_call);
        } else {
            // Regular call: push func, then NULL for self_or_null slot
            // Stack layout: [func, NULL, args...] - same as method call [func, self, args...]
            // CPython `codegen_call()` always creates and uses
            // `skip_normal_call`, even when `maybe_optimize_function_call()`
            // leaves it untargeted.
            let skip_normal_call = self.current_code_info().new_instr_sequence_label();
            let genexpr_call_name = (!uses_ex_call)
                .then(|| self.cpython_genexpr_call_name(func, args))
                .flatten()
                .is_some();
            self.check_caller(func)?;
            self.compile_expression(func)?;
            if genexpr_call_name {
                // CPython `maybe_optimize_function_call()` creates and uses
                // `skip_optimization` for every name(genexpr) shape after
                // loading the function, even when the name is not all/any/tuple.
                let skip_optimization = self.current_code_info().new_instr_sequence_label();
                let result = self
                    .current_code_info()
                    .use_raw_instr_sequence_label(skip_optimization);
                unwrap_internal(self, result);
            }
            self.set_source_range(func.range());
            emit!(self, Instruction::PushNull);
            self.codegen_call_helper(0, args, call_range, None)?;
            let result = self
                .current_code_info()
                .use_raw_instr_sequence_label(skip_normal_call);
            unwrap_internal(self, result);
        }
        Ok(())
    }

    fn call_uses_ex_call(&self, arguments: &ast::Arguments) -> bool {
        let has_starred = arguments
            .args
            .iter()
            .any(|arg| matches!(arg, ast::Expr::Starred(_)));
        let has_double_star = arguments.keywords.iter().any(|k| k.arg.is_none());
        let too_big =
            arguments.args.len() + arguments.keywords.len() * 2 > STACK_USE_GUIDELINE as usize;
        has_starred || has_double_star || too_big
    }

    /// Reject duplicate keyword-argument names in a call.
    fn validate_keywords(&mut self, keywords: &[ast::Keyword]) -> CompileResult<()> {
        for (i, keyword) in keywords.iter().enumerate() {
            let Some(arg) = &keyword.arg else {
                continue;
            };
            for other in &keywords[i + 1..] {
                if other
                    .arg
                    .as_ref()
                    .is_some_and(|other| other.as_str() == arg.as_str())
                {
                    return Err(self.error_ranged(
                        CodegenErrorType::SyntaxError(format!("keyword argument repeated: {arg}")),
                        other.range,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Compile subkwargs: emit key-value pairs for BUILD_MAP
    fn codegen_subkwargs(
        &mut self,
        keywords: &[ast::Keyword],
        begin: usize,
        end: usize,
        call_range: TextRange,
    ) -> CompileResult<()> {
        let n = end - begin;
        assert!(n > 0);

        // For large kwargs, use BUILD_MAP(0) + MAP_ADD to avoid stack overflow.
        let big = n * 2 > STACK_USE_GUIDELINE as usize;

        if big {
            emit!(self, Instruction::BuildMap { count: 0 });
            self.set_no_location();
        }

        for kw in &keywords[begin..end] {
            // Key first, then value - this is critical!
            self.set_source_range(call_range);
            self.emit_load_const(ConstantData::Str {
                value: kw.arg.as_ref().unwrap().as_str().into(),
            });
            self.compile_expression(&kw.value)?;

            if big {
                emit!(self, Instruction::MapAdd { i: 1 });
                self.set_no_location();
            }
        }

        if !big {
            self.set_source_range(call_range);
            emit!(self, Instruction::BuildMap { count: n.to_u32() });
        }

        Ok(())
    }

    /// Compile call arguments and emit the appropriate CALL instruction.
    /// `call_range` is the source range of the call expression, used to set
    /// the correct line number on the CALL instruction.
    fn codegen_call_helper(
        &mut self,
        additional_positional: u32,
        arguments: &ast::Arguments,
        call_range: TextRange,
        kw_names_range: Option<TextRange>,
    ) -> CompileResult<()> {
        self.codegen_call_helper_impl(
            additional_positional,
            &arguments.args,
            &arguments.keywords,
            call_range,
            kw_names_range,
            None,
        )
    }

    fn codegen_call_helper_impl(
        &mut self,
        additional_positional: u32,
        args: &[ast::Expr],
        keywords: &[ast::Keyword],
        call_range: TextRange,
        kw_names_range: Option<TextRange>,
        injected_arg: Option<&Name>,
    ) -> CompileResult<()> {
        self.validate_keywords(keywords)?;

        let nelts = args.len();
        let nkwelts = keywords.len();

        // Check if we have starred args or **kwargs
        let has_starred = args.iter().any(|arg| matches!(arg, ast::Expr::Starred(_)));
        let has_double_star = keywords.iter().any(|k| k.arg.is_none());

        // Check if exceeds CPython's stack-use guideline.
        // With CALL_KW, kwargs values go on stack but keys go in a const tuple,
        // so stack usage is: func + null + positional_args + kwarg_values + kwnames_tuple
        let too_big = nelts + nkwelts * 2 > STACK_USE_GUIDELINE as usize;

        if !has_starred && !has_double_star && !too_big {
            // Simple call path: no * or ** args
            for arg in args {
                self.compile_expression(arg)?;
            }
            let injected_count = if let Some(injected_arg) = injected_arg {
                self.set_source_range(call_range);
                self.load_name(injected_arg)?;
                1
            } else {
                0
            };

            if nkwelts > 0 {
                // Compile keyword values and build kwnames tuple
                let mut kwarg_names = Vec::with_capacity(nkwelts);
                for keyword in keywords {
                    kwarg_names.push(ConstantData::Str {
                        value: keyword.arg.as_ref().unwrap().as_str().into(),
                    });
                    self.compile_expression(&keyword.value)?;
                }

                // Restore call expression range for kwnames and CALL_KW
                self.set_source_range(kw_names_range.unwrap_or(call_range));
                self.emit_load_const(ConstantData::Tuple {
                    elements: kwarg_names,
                });

                self.set_source_range(call_range);
                let argc =
                    additional_positional + nelts.to_u32() + injected_count + nkwelts.to_u32();
                emit!(self, Instruction::CallKw { argc });
            } else {
                self.set_source_range(call_range);
                let argc = additional_positional + nelts.to_u32() + injected_count;
                emit!(self, Instruction::Call { argc });
            }
        } else {
            // ex_call path: has * or ** args

            // Compile positional arguments
            if additional_positional == 0 && nelts == 1 && matches!(args[0], ast::Expr::Starred(_))
            {
                // Single starred arg: pass value directly to CallFunctionEx.
                // Runtime will convert to tuple and validate with function name.
                if let ast::Expr::Starred(ast::ExprStarred { value, .. }) = &args[0] {
                    self.compile_expression(value)?;
                }
            } else {
                // CPython `codegen_call_helper_impl()` sends every other
                // CALL_FUNCTION_EX positional shape through
                // `starunpack_helper_impl(..., BUILD_LIST, LIST_APPEND,
                // LIST_EXTEND, tuple=1)`, even when the only reason for the
                // ex-call path is too many non-starred positional arguments.
                self.set_source_range(call_range);
                self.starunpack_helper_impl(
                    args,
                    injected_arg,
                    additional_positional,
                    CollectionType::Tuple,
                )?;
            }

            self.compile_call_function_ex_keywords(keywords, call_range)?;

            self.set_source_range(call_range);
            emit!(self, Instruction::CallFunctionEx);
        }

        Ok(())
    }

    fn compile_call_function_ex_keywords(
        &mut self,
        keywords: &[ast::Keyword],
        call_range: TextRange,
    ) -> CompileResult<()> {
        if keywords.is_empty() {
            self.set_source_range(call_range);
            emit!(self, Instruction::PushNull);
            return Ok(());
        }

        let mut have_dict = false;
        let mut nseen = 0usize;

        for (i, keyword) in keywords.iter().enumerate() {
            if keyword.arg.is_none() {
                if nseen > 0 {
                    self.codegen_subkwargs(keywords, i - nseen, i, call_range)?;
                    if have_dict {
                        self.set_source_range(call_range);
                        emit!(self, Instruction::DictMerge { i: 1 });
                    }
                    have_dict = true;
                    nseen = 0;
                }

                if !have_dict {
                    self.set_source_range(call_range);
                    emit!(self, Instruction::BuildMap { count: 0 });
                    have_dict = true;
                }

                self.compile_expression(&keyword.value)?;
                self.set_source_range(call_range);
                emit!(self, Instruction::DictMerge { i: 1 });
            } else {
                nseen += 1;
            }
        }

        if nseen > 0 {
            self.codegen_subkwargs(keywords, keywords.len() - nseen, keywords.len(), call_range)?;
            if have_dict {
                self.set_source_range(call_range);
                emit!(self, Instruction::DictMerge { i: 1 });
            }
            have_dict = true;
        }

        debug_assert!(have_dict);
        Ok(())
    }

    fn compile_comprehension_element(&mut self, element: &ast::Expr) -> CompileResult<()> {
        self.compile_expression(element).map_err(|e| {
            if let CodegenErrorType::InvalidStarExpr = e.error {
                self.error(CodegenErrorType::SyntaxError(
                    "iterable unpacking cannot be used in comprehension".to_owned(),
                ))
            } else {
                e
            }
        })
    }

    fn compile_generator_expression(
        &mut self,
        elt: &ast::Expr,
        generators: &[ast::Comprehension],
        range: TextRange,
    ) -> CompileResult<()> {
        // Check if element or generators contain async content
        // This makes the generator expression into an async generator
        let element_contains_await =
            Self::contains_await(elt) || Self::generators_contain_await(generators);
        self.compile_comprehension(
            "<genexpr>",
            None,
            generators,
            &|compiler, _collection_add_i| {
                // Compile the element expression
                // Note: if element is an async comprehension, compile_expression
                // already handles awaiting it, so we don't need to await again here
                compiler.compile_comprehension_element(elt)?;

                compiler.mark_generator();
                if compiler.ctx.func == FunctionContext::AsyncFunction {
                    compiler.set_source_range(elt.range());
                    emit!(
                        compiler,
                        Instruction::CallIntrinsic1 {
                            func: bytecode::IntrinsicFunction1::AsyncGenWrap
                        }
                    );
                }
                // arg=0: direct yield (wrapped for async generators)
                compiler.set_source_range(elt.range());
                emit!(compiler, Instruction::YieldValue { arg: 0 });
                emit!(
                    compiler,
                    Instruction::Resume {
                        context: oparg::ResumeContext::from(oparg::ResumeLocation::AfterYield)
                    }
                );
                emit!(compiler, Instruction::PopTop);

                Ok(())
            },
            ComprehensionType::Generator,
            element_contains_await,
            range,
            elt.range(),
            elt.range(),
        )
    }

    fn consume_next_sub_table(&mut self) -> CompileResult<()> {
        {
            let _ = self.push_symbol_table()?;
        }
        self.pop_symbol_table();
        Ok(())
    }

    fn consume_function_annotation_symbol_table_if_used(&mut self) -> CompileResult<()> {
        if !self.next_function_annotation_symbol_table_uses_annotations() {
            return Ok(());
        }
        if !self.push_annotation_symbol_table() {
            let current_table = self.current_symbol_table();
            return Err(self.error(CodegenErrorType::SyntaxError(format!(
                "no annotation symbol table available in {} (type: {:?})",
                current_table.name, current_table.typ
            ))));
        }
        self.pop_annotation_symbol_table();
        Ok(())
    }

    fn consume_skipped_nested_scopes_in_expr(
        &mut self,
        expression: &ast::Expr,
    ) -> CompileResult<()> {
        use ast::visitor::Visitor;

        struct SkippedScopeVisitor<'a, 'warnings> {
            compiler: &'a mut Compiler<'warnings>,
            error: Option<CodegenError>,
        }

        impl SkippedScopeVisitor<'_, '_> {
            fn consume_scope(&mut self) {
                if self.error.is_none() {
                    self.error = self.compiler.consume_next_sub_table().err();
                }
            }

            fn consume_inlined_comprehension_scope(&mut self) -> bool {
                if self.error.is_some() {
                    return false;
                }
                let Some(current_table) = self.compiler.symbol_table_stack.last_mut() else {
                    return false;
                };
                if current_table.next_inlined_comprehension_block
                    < current_table.inlined_comprehension_blocks.len()
                {
                    current_table.next_inlined_comprehension_block += 1;
                    true
                } else {
                    false
                }
            }

            fn visit_comprehension_tail(
                &mut self,
                elt1: &ast::Expr,
                elt2: Option<&ast::Expr>,
                generators: &[ast::Comprehension],
            ) {
                if let Some(outermost) = generators.first() {
                    self.visit_expr(&outermost.target);
                    for if_expr in &outermost.ifs {
                        self.visit_expr(if_expr);
                    }
                }
                for generator in generators.iter().skip(1) {
                    self.visit_expr(&generator.target);
                    self.visit_expr(&generator.iter);
                    for if_expr in &generator.ifs {
                        self.visit_expr(if_expr);
                    }
                }
                if let Some(elt2) = elt2 {
                    self.visit_expr(elt2);
                }
                self.visit_expr(elt1);
            }
        }

        impl ast::visitor::Visitor<'_> for SkippedScopeVisitor<'_, '_> {
            fn visit_expr(&mut self, expr: &ast::Expr) {
                if self.error.is_some() {
                    return;
                }

                match expr {
                    ast::Expr::Lambda(ast::ExprLambda { parameters, .. }) => {
                        // Defaults are scanned before enter_scope in the
                        // symbol table builder, so their nested scopes
                        // precede the lambda scope in sub_tables.
                        if let Some(params) = parameters.as_deref() {
                            for default in params
                                .posonlyargs
                                .iter()
                                .chain(&params.args)
                                .chain(&params.kwonlyargs)
                                .filter_map(|p| p.default.as_deref())
                            {
                                self.visit_expr(default);
                            }
                        }
                        self.consume_scope();
                    }
                    ast::Expr::Generator(ast::ExprGenerator { generators, .. }) => {
                        if let Some(first) = generators.first() {
                            self.visit_expr(&first.iter);
                        }
                        self.consume_scope();
                    }
                    ast::Expr::ListComp(ast::ExprListComp {
                        elt, generators, ..
                    })
                    | ast::Expr::SetComp(ast::ExprSetComp {
                        elt, generators, ..
                    }) => {
                        if let Some(first) = generators.first() {
                            self.visit_expr(&first.iter);
                        }
                        if self.consume_inlined_comprehension_scope() {
                            self.visit_comprehension_tail(elt, None, generators);
                        } else {
                            self.consume_scope();
                        }
                    }
                    ast::Expr::DictComp(ast::ExprDictComp {
                        key,
                        value,
                        generators,
                        ..
                    }) => {
                        if let Some(first) = generators.first() {
                            self.visit_expr(&first.iter);
                        }
                        if self.consume_inlined_comprehension_scope() {
                            if let Some(key) = key.as_deref() {
                                self.visit_comprehension_tail(key, Some(value), generators);
                            } else {
                                self.visit_comprehension_tail(value, None, generators);
                            }
                        } else {
                            self.consume_scope();
                        }
                    }
                    _ => ast::visitor::walk_expr(self, expr),
                }
            }
        }

        let mut visitor = SkippedScopeVisitor {
            compiler: self,
            error: None,
        };
        visitor.visit_expr(expression);
        if let Some(err) = visitor.error {
            Err(err)
        } else {
            Ok(())
        }
    }

    fn consume_skipped_nested_scopes_in_parameter_defaults(
        &mut self,
        parameters: &ast::Parameters,
    ) -> CompileResult<()> {
        for default in parameters
            .posonlyargs
            .iter()
            .chain(&parameters.args)
            .chain(&parameters.kwonlyargs)
            .filter_map(|arg| arg.default.as_deref())
        {
            self.consume_skipped_nested_scopes_in_expr(default)?;
        }
        Ok(())
    }

    fn consume_skipped_nested_scopes_in_statements(
        &mut self,
        statements: &[ast::Stmt],
    ) -> CompileResult<()> {
        use ast::visitor::Visitor;

        struct SkippedStatementScopeVisitor<'a, 'warnings> {
            compiler: &'a mut Compiler<'warnings>,
            error: Option<CodegenError>,
        }

        impl SkippedStatementScopeVisitor<'_, '_> {
            fn consume_scope(&mut self) {
                if self.error.is_none() {
                    self.error = self.compiler.consume_next_sub_table().err();
                }
            }

            fn consume_function_annotation_scope_if_used(&mut self) {
                if self.error.is_none() {
                    self.error = self
                        .compiler
                        .consume_function_annotation_symbol_table_if_used()
                        .err();
                }
            }

            fn visit_parameter_defaults(&mut self, parameters: &ast::Parameters) {
                for default in parameters
                    .posonlyargs
                    .iter()
                    .chain(&parameters.args)
                    .chain(&parameters.kwonlyargs)
                    .filter_map(|arg| arg.default.as_deref())
                {
                    self.visit_expr(default);
                }
            }

            fn visit_decorators(&mut self, decorators: &[ast::Decorator]) {
                for decorator in decorators {
                    self.visit_expr(&decorator.expression);
                }
            }

            fn visit_arguments(&mut self, arguments: &ast::Arguments) {
                for arg in &arguments.args {
                    self.visit_expr(arg);
                }
                for keyword in &arguments.keywords {
                    self.visit_expr(&keyword.value);
                }
            }
        }

        impl ast::visitor::Visitor<'_> for SkippedStatementScopeVisitor<'_, '_> {
            fn visit_stmt(&mut self, stmt: &ast::Stmt) {
                if self.error.is_some() {
                    return;
                }

                match stmt {
                    ast::Stmt::FunctionDef(ast::StmtFunctionDef {
                        parameters,
                        decorator_list,
                        type_params,
                        ..
                    }) => {
                        self.visit_parameter_defaults(parameters);
                        self.visit_decorators(decorator_list);
                        if type_params.is_some() {
                            self.consume_scope();
                        } else {
                            self.consume_function_annotation_scope_if_used();
                            self.consume_scope();
                        }
                    }
                    ast::Stmt::ClassDef(ast::StmtClassDef {
                        arguments,
                        decorator_list,
                        type_params,
                        ..
                    }) => {
                        self.visit_decorators(decorator_list);
                        if type_params.is_some() {
                            self.consume_scope();
                        }
                        if let Some(arguments) = arguments {
                            self.visit_arguments(arguments);
                        }
                        self.consume_scope();
                    }
                    ast::Stmt::TypeAlias(ast::StmtTypeAlias { type_params, .. }) => {
                        if type_params.is_some() {
                            self.consume_scope();
                        }
                        self.consume_scope();
                    }
                    ast::Stmt::AnnAssign(ast::StmtAnnAssign { target, value, .. }) => {
                        self.visit_expr(target);
                        if let Some(value) = value {
                            self.visit_expr(value);
                        }
                    }
                    ast::Stmt::If(ast::StmtIf {
                        test,
                        body,
                        elif_else_clauses,
                        ..
                    }) => {
                        self.visit_expr(test);
                        for stmt in body {
                            self.visit_stmt(stmt);
                        }
                        for clause in elif_else_clauses {
                            if let Some(test) = &clause.test {
                                self.visit_expr(test);
                            }
                            for stmt in &clause.body {
                                self.visit_stmt(stmt);
                            }
                        }
                    }
                    ast::Stmt::Try(ast::StmtTry {
                        body,
                        handlers,
                        orelse,
                        finalbody,
                        ..
                    }) => {
                        for stmt in body {
                            self.visit_stmt(stmt);
                        }
                        for handler in handlers {
                            self.visit_except_handler(handler);
                        }
                        for stmt in orelse {
                            self.visit_stmt(stmt);
                        }
                        for stmt in finalbody {
                            self.visit_stmt(stmt);
                        }
                    }
                    _ => ast::visitor::walk_stmt(self, stmt),
                }
            }

            fn visit_expr(&mut self, expr: &ast::Expr) {
                if self.error.is_some() {
                    return;
                }
                self.error = self
                    .compiler
                    .consume_skipped_nested_scopes_in_expr(expr)
                    .err();
            }

            fn visit_except_handler(&mut self, handler: &ast::ExceptHandler) {
                if self.error.is_some() {
                    return;
                }
                let ast::ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                    type_,
                    body,
                    ..
                }) = handler;
                if let Some(type_) = type_ {
                    self.visit_expr(type_);
                }
                for stmt in body {
                    self.visit_stmt(stmt);
                }
            }
        }

        let mut visitor = SkippedStatementScopeVisitor {
            compiler: self,
            error: None,
        };
        for statement in statements {
            visitor.visit_stmt(statement);
        }
        if let Some(err) = visitor.error {
            Err(err)
        } else {
            Ok(())
        }
    }

    fn consume_skipped_nested_scopes_in_except_handlers(
        &mut self,
        handlers: &[ast::ExceptHandler],
    ) -> CompileResult<()> {
        use ast::visitor::Visitor;

        struct SkippedHandlerScopeVisitor<'a, 'warnings> {
            compiler: &'a mut Compiler<'warnings>,
            error: Option<CodegenError>,
        }

        impl ast::visitor::Visitor<'_> for SkippedHandlerScopeVisitor<'_, '_> {
            fn visit_expr(&mut self, expr: &ast::Expr) {
                if self.error.is_some() {
                    return;
                }
                self.error = self
                    .compiler
                    .consume_skipped_nested_scopes_in_expr(expr)
                    .err();
            }

            fn visit_stmt(&mut self, stmt: &ast::Stmt) {
                if self.error.is_some() {
                    return;
                }
                self.error = self
                    .compiler
                    .consume_skipped_nested_scopes_in_statements(slice::from_ref(stmt))
                    .err();
            }
        }

        let mut visitor = SkippedHandlerScopeVisitor {
            compiler: self,
            error: None,
        };
        for handler in handlers {
            visitor.visit_except_handler(handler);
            if visitor.error.is_some() {
                break;
            }
        }
        if let Some(err) = visitor.error {
            Err(err)
        } else {
            Ok(())
        }
    }

    fn current_symbol_table_cursors(&self) -> SymbolTableCursors {
        let table = self
            .symbol_table_stack
            .last()
            .expect("no current symbol table");
        SymbolTableCursors {
            sub_table: table.next_sub_table,
            hidden_annotation_block: table.next_hidden_annotation_block,
            inlined_comprehension_block: table.next_inlined_comprehension_block,
        }
    }

    fn set_symbol_table_cursors(&mut self, cursors: SymbolTableCursors) {
        let table = self
            .symbol_table_stack
            .last_mut()
            .expect("no current symbol table");
        table.next_sub_table = cursors.sub_table;
        table.next_hidden_annotation_block = cursors.hidden_annotation_block;
        table.next_inlined_comprehension_block = cursors.inlined_comprehension_block;
    }

    /// Advance the nested-scope cursors to the first scope that begins on or
    /// after `line_number`.
    ///
    /// A statement list can be compiled more than once — `finally` bodies are
    /// re-emitted on every path that leaves the try block early — and each copy
    /// has to be handed the same scopes as the last. Positioning by line works
    /// because `finally` is the last clause of its statement: every scope the
    /// preceding clauses opened begins on an earlier line than the body being
    /// re-compiled, and every scope that body opens begins on its own line or
    /// later.
    fn seek_symbol_table_cursors_to_line(&mut self, line_number: u32) {
        fn seek(tables: &[SymbolTable], cursor: &mut usize, line_number: u32) {
            while tables
                .get(*cursor)
                .is_some_and(|table| table.line_number < line_number)
            {
                *cursor += 1;
            }
        }

        let table = self
            .symbol_table_stack
            .last_mut()
            .expect("no current symbol table");
        seek(&table.sub_tables, &mut table.next_sub_table, line_number);
        seek(
            &table.hidden_annotation_blocks,
            &mut table.next_hidden_annotation_block,
            line_number,
        );
        seek(
            &table.inlined_comprehension_blocks,
            &mut table.next_inlined_comprehension_block,
            line_number,
        );
    }

    fn lookup_comprehension_symbol_table_after_skipped_nested_scopes_in_expr(
        &mut self,
        expression: &ast::Expr,
        comprehension_type: ComprehensionType,
    ) -> CompileResult<(SymbolTable, ComprehensionSymbolSource)> {
        let saved_cursor = self
            .symbol_table_stack
            .last()
            .expect("no current symbol table")
            .next_sub_table;
        let saved_inlined_cursor = self
            .symbol_table_stack
            .last()
            .expect("no current symbol table")
            .next_inlined_comprehension_block;
        let result = (|| {
            self.consume_skipped_nested_scopes_in_expr(expression)?;
            let current_table = self
                .symbol_table_stack
                .last()
                .expect("no current symbol table");
            if comprehension_type != ComprehensionType::Generator
                && let Some(table) = current_table
                    .inlined_comprehension_blocks
                    .get(current_table.next_inlined_comprehension_block)
            {
                return Ok((table.clone(), ComprehensionSymbolSource::Inlined));
            }
            if let Some(table) = current_table.sub_tables.get(current_table.next_sub_table) {
                Ok((table.clone(), ComprehensionSymbolSource::Child))
            } else {
                let name = current_table.name.clone();
                let typ = current_table.typ;
                Err(self.error(CodegenErrorType::SyntaxError(format!(
                    "no symbol table available in {name} (type: {typ:?})"
                ))))
            }
        })();
        self.symbol_table_stack
            .last_mut()
            .expect("no current symbol table")
            .next_sub_table = saved_cursor;
        self.symbol_table_stack
            .last_mut()
            .expect("no current symbol table")
            .next_inlined_comprehension_block = saved_inlined_cursor;
        result
    }

    fn push_output_with_symbol_table(
        &mut self,
        table: SymbolTable,
        flags: bytecode::CodeFlags,
        posonlyarg_count: u32,
        arg_count: u32,
        kwonlyarg_count: u32,
        obj_name: &str,
    ) -> CompileResult<()> {
        let scope_type = table.typ;
        self.symbol_table_stack.push(table);

        let key = self.symbol_table_stack.len() - 1;
        let lineno = self.get_source_line_number().get();
        self.enter_scope(obj_name, scope_type, key, lineno.to_u32())?;

        if let Some(info) = self.code_stack.last_mut() {
            info.flags = flags
                | (info.flags
                    & (bytecode::CodeFlags::NESTED
                        | bytecode::CodeFlags::METHOD
                        | bytecode::CodeFlags::FUTURE_DIVISION
                        | bytecode::CodeFlags::FUTURE_ABSOLUTE_IMPORT
                        | bytecode::CodeFlags::FUTURE_WITH_STATEMENT
                        | bytecode::CodeFlags::FUTURE_PRINT_FUNCTION
                        | bytecode::CodeFlags::FUTURE_UNICODE_LITERALS
                        | bytecode::CodeFlags::FUTURE_BARRY_AS_BDFL
                        | bytecode::CodeFlags::FUTURE_GENERATOR_STOP
                        | bytecode::CodeFlags::FUTURE_ANNOTATIONS));
            info.metadata.argcount = arg_count;
            info.metadata.posonlyargcount = posonlyarg_count;
            info.metadata.kwonlyargcount = kwonlyarg_count;
        }
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "ignore warning for now")]
    fn compile_comprehension(
        &mut self,
        name: &str,
        init_collection: Option<AnyInstruction>,
        generators: &[ast::Comprehension],
        compile_element: &dyn Fn(&mut Self, usize) -> CompileResult<()>,
        comprehension_type: ComprehensionType,
        element_contains_await: bool,
        comprehension_range: TextRange,
        element_range: TextRange,
        outer_backedge_range: TextRange,
    ) -> CompileResult<()> {
        let prev_ctx = self.ctx;
        let has_an_async_gen = generators.iter().any(|g| g.is_async);
        let is_top_level_await_context = self.opts.allow_top_level_await
            && prev_ctx.func == FunctionContext::NoFunction
            && !prev_ctx.in_class;

        // Check for async comprehension outside async function (list/set/dict only, not generator expressions)
        // Use in_async_scope to allow nested async comprehensions inside an async function
        if comprehension_type != ComprehensionType::Generator
            && (has_an_async_gen || element_contains_await)
            && !prev_ctx.in_async_scope
            && !is_top_level_await_context
        {
            return Err(self.error(CodegenErrorType::InvalidAsyncComprehension));
        }

        // async comprehensions are allowed in various contexts:
        // - list/set/dict comprehensions in async functions (or nested within)
        // - always for generator expressions
        let is_async_list_set_dict_comprehension = comprehension_type
            != ComprehensionType::Generator
            && (has_an_async_gen || element_contains_await)
            && (prev_ctx.in_async_scope || is_top_level_await_context);

        let is_async_generator_comprehension = comprehension_type == ComprehensionType::Generator
            && (has_an_async_gen || element_contains_await);

        debug_assert!(!(is_async_list_set_dict_comprehension && is_async_generator_comprehension));

        let is_async = is_async_list_set_dict_comprehension || is_async_generator_comprehension;

        // We must have at least one generator:
        assert!(!generators.is_empty());
        let outermost = &generators[0];
        let (comp_table, comp_source) = self
            .lookup_comprehension_symbol_table_after_skipped_nested_scopes_in_expr(
                &outermost.iter,
                comprehension_type,
            )?;

        let is_inlined = self.is_inlined_comprehension_context(comprehension_type, &comp_table);

        if is_inlined {
            // CPython inlines every non-generator comprehension that the
            // symtable marked as comp_inlined, including async variants.
            // codegen_comprehension() only branches on ste_comp_inlined here
            // and relies on the inlined path itself to handle GET_AITER /
            // async-comprehension cleanup.
            return self.compile_inlined_comprehension(
                &comp_table,
                init_collection,
                generators,
                compile_element,
                (comprehension_range, element_range, outer_backedge_range),
                comp_source,
            );
        }

        // Non-inlined path: create a new code object (generator expressions, etc.)
        self.ctx = CompileContext {
            in_class: prev_ctx.in_class,
            func: if is_async {
                FunctionContext::AsyncFunction
            } else {
                FunctionContext::Function
            },
            // Inherit in_async_scope from parent - nested async comprehensions are allowed
            // if we're anywhere inside an async function
            in_async_scope: prev_ctx.in_async_scope || is_async,
        };

        let flags = bytecode::CodeFlags::NEWLOCALS | bytecode::CodeFlags::OPTIMIZED;
        let flags = if is_async {
            flags | bytecode::CodeFlags::COROUTINE
        } else {
            flags
        };

        // The symbol table follows CPython's symtable walk: nested scopes
        // in the outermost iterator are recorded before the comprehension
        // scope itself. Peek past those nested scopes so we can enter the
        // correct comprehension table here, then let the real outermost
        // iterator compile consume its nested scopes later in parent scope.
        self.push_output_with_symbol_table(comp_table, flags, 0, 1, 0, name)?;

        // Set qualname for comprehension
        self.set_qualname();
        self.set_source_range(comprehension_range);

        let arg0 = self.varname(".0");

        let return_none = init_collection.is_none();

        // CPython codegen_comprehension() wraps only generator expressions
        // with codegen_wrap_in_stopiteration_handler(); unlike function bodies,
        // it does not push a COMPILE_FBLOCK_STOP_ITERATION fblock.
        let stop_iteration_block = if comprehension_type == ComprehensionType::Generator {
            let handler_block = self.new_block();
            self.insert_cpython_stopiteration_setup_cleanup(handler_block);
            Some(handler_block)
        } else {
            None
        };

        // Create empty object of proper type:
        if let Some(init_collection) = init_collection {
            self._emit(init_collection, OpArg::new(0), BlockIdx::NULL)
        }

        let mut loop_labels = vec![];
        let mut real_loop_depth = 0;
        for (gen_index, generator) in generators.iter().enumerate() {
            if gen_index > 0
                && !generator.is_async
                && let Some(singleton_iter) =
                    Self::singleton_comprehension_assignment_iter(&generator.iter)
            {
                // CPython allocates start/if_cleanup/anchor labels before the
                // singleton sub-iterator fast path sets start = NO_LABEL.
                let _start_label = self.current_code_info().new_instr_sequence_label();
                let if_cleanup_block = self.new_block();
                let _anchor_label = self.current_code_info().new_instr_sequence_label();
                self.compile_expression(singleton_iter)?;
                self.compile_store(&generator.target)?;

                if !generator.ifs.is_empty() {
                    for if_condition in &generator.ifs {
                        self.compile_jump_if(if_condition, false, if_cleanup_block)?;
                    }
                }
                loop_labels.push(ComprehensionLoopControl::IfCleanupOnly { if_cleanup_block });
                continue;
            }

            let loop_block = self.new_block();
            let (send_block, after_block, if_cleanup_block) = if generator.is_async {
                let send_block = self.new_block();
                let after_block = self.new_block();
                let if_cleanup_block = self.new_block();
                (send_block, after_block, if_cleanup_block)
            } else {
                let if_cleanup_block = self.new_block();
                let after_block = self.new_block();
                (BlockIdx::NULL, after_block, if_cleanup_block)
            };

            if gen_index == 0 {
                // Load iterator onto stack (passed as first argument):
                emit!(self, Instruction::LoadFast { var_num: arg0 });
            } else {
                // Evaluate iterated item:
                self.compile_comprehension_iter(generator)?;
            }

            self.use_cpython_label_block(loop_block);
            let mut end_async_for_target = BlockIdx::NULL;
            if generator.is_async {
                let loop_label = self.instr_sequence_label_for_block(loop_block);
                self.push_fblock_labels(
                    FBlockType::AsyncComprehensionGenerator,
                    loop_label,
                    ir::InstructionSequenceLabel::NO_LABEL,
                    FBlockDatum::None,
                )?;
                emit!(self, PseudoInstruction::SetupFinally { delta: after_block });
                emit!(self, Instruction::GetAnext);
                self.emit_load_const(ConstantData::None);
                self.use_cpython_label_block(send_block);
                let _ = self.compile_yield_from_sequence(true);
                end_async_for_target = send_block;
                // POP_BLOCK before store: only __anext__/yield_from are
                // protected by SetupFinally targeting END_ASYNC_FOR.
                emit!(self, PseudoInstruction::PopBlock);
                self.compile_store(&generator.target)?;
            } else {
                let saved_range = self.current_source_range;
                self.set_source_range(generator.iter.range());
                emit!(self, Instruction::ForIter { delta: after_block });
                self.set_source_range(saved_range);
                self.compile_store(&generator.target)?;
            }
            real_loop_depth += 1;
            let backedge_range = if gen_index + 1 == generators.len() {
                element_range
            } else {
                outer_backedge_range
            };
            loop_labels.push(ComprehensionLoopControl::Iteration {
                loop_block,
                if_cleanup_block,
                after_block,
                backedge_range,
                is_async: generator.is_async,
                end_async_for_target,
            });

            // CPython always lowers comprehension guards through codegen_jump_if
            // and leaves constant-folding to later CFG optimization passes.
            for if_condition in &generator.ifs {
                self.compile_jump_if(if_condition, false, if_cleanup_block)?;
            }
        }

        compile_element(self, real_loop_depth + 1)?;

        for loop_control in loop_labels.iter().rev().copied() {
            match loop_control {
                ComprehensionLoopControl::Iteration {
                    loop_block,
                    if_cleanup_block,
                    after_block,
                    backedge_range,
                    is_async,
                    end_async_for_target,
                } => {
                    self.use_cpython_label_block(if_cleanup_block);
                    self.set_source_range(backedge_range);
                    emit!(self, PseudoInstruction::Jump { delta: loop_block });

                    if is_async {
                        let loop_label = self.instr_sequence_label_for_block(loop_block);
                        self.pop_fblock_label(FBlockType::AsyncComprehensionGenerator, loop_label);
                    }

                    self.use_cpython_label_block(after_block);
                    if is_async {
                        self.set_source_range(comprehension_range);
                        // EndAsyncFor pops both the exception and the aiter
                        // (handler depth is before GetANext, so aiter is at handler depth)
                        self.emit_end_async_for(end_async_for_target);
                    } else {
                        self.emit_sync_comprehension_end_for();
                    }
                }
                ComprehensionLoopControl::IfCleanupOnly { if_cleanup_block } => {
                    self.use_cpython_label_block(if_cleanup_block);
                }
            }
        }

        if return_none {
            self.emit_return_const_no_location(ConstantData::None);
        } else {
            self.set_source_range(comprehension_range);
            self.emit_return_value();
        }

        // Close StopIteration handler and emit handler code
        if let Some(handler_block) = stop_iteration_block {
            self.use_cpython_label_block(handler_block);
            emit!(
                self,
                Instruction::CallIntrinsic1 {
                    func: oparg::IntrinsicFunction1::StopIterationError
                }
            );
            self.set_no_location();
            emit!(self, Instruction::Reraise { depth: 1u32 });
            self.set_no_location();
        }
        self.emit_return_const_no_location(ConstantData::None);

        let code = self.exit_scope();

        self.ctx = prev_ctx;

        // Create comprehension function with closure
        self.set_source_range(comprehension_range);
        self.make_closure(code, bytecode::MakeFunctionFlags::new())?;

        // Evaluate iterated item and get its iterator.
        self.compile_comprehension_iter(outermost)?;
        self.symbol_table_stack
            .last_mut()
            .expect("no current symbol table")
            .next_sub_table += 1;

        // Call just created <listcomp> function:
        self.set_source_range(comprehension_range);
        emit!(self, Instruction::Call { argc: 0 });
        if is_async_list_set_dict_comprehension {
            emit!(self, Instruction::GetAwaitable { r#where: 0 });
            self.emit_load_const(ConstantData::None);
            let _ = self.compile_yield_from_sequence(true);
        }

        Ok(())
    }

    /// Compile an inlined comprehension (PEP 709)
    /// This generates bytecode inline without creating a new code object
    fn compile_inlined_comprehension(
        &mut self,
        comp_table: &SymbolTable,
        init_collection: Option<AnyInstruction>,
        generators: &[ast::Comprehension],
        compile_element: &dyn Fn(&mut Self, usize) -> CompileResult<()>,
        ranges: (TextRange, TextRange, TextRange),
        comp_source: ComprehensionSymbolSource,
    ) -> CompileResult<()> {
        let (comprehension_range, element_range, outer_backedge_range) = ranges;
        // Compile the outermost iterator first. Its expression may reference
        // nested scopes (e.g. lambdas) whose sub_tables sit at the current
        // position in the parent's list. Those must be consumed before we
        // splice in the comprehension's own children.
        self.compile_comprehension_iter(&generators[0])?;
        match comp_source {
            ComprehensionSymbolSource::Child => {
                self.symbol_table_stack
                    .last_mut()
                    .expect("no current symbol table")
                    .next_sub_table += 1;
            }
            ComprehensionSymbolSource::Inlined => {
                self.symbol_table_stack
                    .last_mut()
                    .expect("no current symbol table")
                    .next_inlined_comprehension_block += 1;
            }
        }

        let was_in_inlined_comp = self.current_code_info().in_inlined_comp;
        let saved_source_range = self.current_source_range;
        let tweak_in_class_block = {
            let ct = self.current_symbol_table();
            ct.typ == CompilerScope::Class && !was_in_inlined_comp
        };
        self.current_code_info().in_inlined_comp = true;

        let mut temp_symbols: IndexMap<Name, Symbol> = IndexMap::default();
        let mut changed_fast_hidden = Vec::new();

        let result = (|| {
            // If the symbol table still carries the inlined comprehension as
            // a child, splice its children here. The symtable normally
            // performs this splice before codegen, and the Inlined source path
            // has already done so.
            if matches!(comp_source, ComprehensionSymbolSource::Child)
                && !comp_table.sub_tables.is_empty()
            {
                let current_table = self
                    .symbol_table_stack
                    .last_mut()
                    .expect("no current symbol table");
                let insert_pos = current_table.next_sub_table;
                for (i, st) in comp_table.sub_tables.iter().enumerate() {
                    current_table.sub_tables.insert(insert_pos + i, st.clone());
                }
            }
            let mut pushed_locals: Vec<Name> = Vec::new();
            let mut fast_hidden_locals: Vec<Name> = Vec::new();
            for (name, sym) in &comp_table.symbols {
                if sym.flags.contains(SymbolFlags::DEF_PARAM) {
                    continue; // skip .0
                }
                let is_local = sym.flags.contains(SymbolFlags::DEF_LOCAL)
                    && !sym.flags.contains(SymbolFlags::DEF_NONLOCAL);
                if is_local {
                    pushed_locals.push(name.clone());
                }
                if is_local || tweak_in_class_block {
                    fast_hidden_locals.push(name.clone());
                }
            }

            // TweakInlinedComprehensionScopes: temporarily override parent
            // symbols with comprehension scopes where they differ. For
            // module/class scopes, also enable temporary fast locals for
            // comprehension-bound names only.
            for (name, comp_sym) in &comp_table.symbols {
                if comp_sym.flags.contains(SymbolFlags::DEF_PARAM) {
                    continue; // skip .0
                }
                let comp_scope = comp_sym.scope;

                let current_table = self.symbol_table_stack.last().expect("no symbol table");
                if let Some(outer_sym) = current_table.symbols.get(name) {
                    let outer_scope = outer_sym.scope;
                    if (comp_scope != outer_scope
                        && comp_scope != SymbolScope::Free
                        && !(comp_scope == SymbolScope::Cell && outer_scope == SymbolScope::Free))
                        || tweak_in_class_block
                    {
                        temp_symbols.insert(name.clone(), outer_sym.clone());
                        let current_table =
                            self.symbol_table_stack.last_mut().expect("no symbol table");
                        current_table.symbols.insert(name.clone(), comp_sym.clone());
                    }
                }
            }
            if !self.ctx.in_func() {
                for name in &fast_hidden_locals {
                    if self
                        .current_code_info()
                        .metadata
                        .fast_hidden
                        .get(name.as_str())
                        .is_none_or(|&hidden| !hidden)
                    {
                        self.current_code_info()
                            .metadata
                            .fast_hidden
                            .insert(name.clone().into(), true);
                        self.current_code_info()
                            .metadata
                            .fast_hidden_final
                            .insert(name.clone().into());
                        changed_fast_hidden.push(name.clone());
                    }
                }
            }

            // Step 2: Save local variables that will be shadowed by the comprehension.
            // For each variable, we push the fast local value via LoadFastAndClear.
            // For merged CELL variables, LoadFastAndClear saves the cell object from
            // the merged slot, and MAKE_CELL creates a new empty cell in-place.
            // MAKE_CELL has no stack effect (operates only on fastlocals).
            self.set_source_range(comprehension_range);
            let mut total_stack_items: usize = 0;
            for name in &pushed_locals {
                let var_num = self.varname(name);
                emit!(self, Instruction::LoadFastAndClear { var_num });
                total_stack_items += 1;
                // If the comp symbol is CELL, emit MAKE_CELL to create fresh cell
                if let Some(comp_sym) = comp_table.symbols.get(name)
                    && comp_sym.scope == SymbolScope::Cell
                {
                    let i = if self
                        .current_symbol_table()
                        .symbols
                        .get(name)
                        .is_some_and(|s| s.scope == SymbolScope::Free)
                    {
                        self.get_free_var_index(name)
                    } else {
                        self.get_cell_var_index(name)
                    };
                    emit!(self, Instruction::MakeCell { i });
                }
            }

            // Step 3: SWAP iterator to TOS (above saved locals + cell values)
            if total_stack_items > 0 {
                emit!(
                    self,
                    Instruction::Swap {
                        i: u32::try_from(total_stack_items + 1).unwrap()
                    }
                );
            }

            // CPython's codegen_push_inlined_comprehension_locals()
            // installs the virtual cleanup before codegen_comprehension()
            // emits BUILD_LIST/BUILD_SET/BUILD_MAP for the result object.
            let cleanup_block = if !pushed_locals.is_empty() {
                let cleanup_block = self.new_block();
                emit!(
                    self,
                    PseudoInstruction::SetupFinally {
                        delta: cleanup_block
                    }
                );
                Some(cleanup_block)
            } else {
                None
            };

            // Step 4: Create the collection (list/set/dict)
            if let Some(init_collection) = init_collection {
                self._emit(init_collection, OpArg::new(0), BlockIdx::NULL);
                // SWAP to get iterator on top
                emit!(self, Instruction::Swap { i: 2 });
            }

            // Step 5: Compile the comprehension loop(s)
            let mut loop_labels: Vec<ComprehensionLoopControl> = vec![];
            let mut real_loop_depth = 0;
            for (i, generator) in generators.iter().enumerate() {
                if i > 0
                    && !generator.is_async
                    && let Some(singleton_iter) =
                        Self::singleton_comprehension_assignment_iter(&generator.iter)
                {
                    // CPython allocates start/if_cleanup/anchor labels before
                    // the singleton sub-iterator fast path sets start = NO_LABEL.
                    let _start_label = self.current_code_info().new_instr_sequence_label();
                    let if_cleanup_block = self.new_block();
                    let _anchor_label = self.current_code_info().new_instr_sequence_label();
                    self.compile_expression(singleton_iter)?;
                    self.compile_store(&generator.target)?;

                    if !generator.ifs.is_empty() {
                        for if_condition in &generator.ifs {
                            self.compile_jump_if(if_condition, false, if_cleanup_block)?;
                        }
                    }
                    loop_labels.push(ComprehensionLoopControl::IfCleanupOnly { if_cleanup_block });
                    continue;
                }

                let loop_block = self.new_block();
                let (send_block, after_block, if_cleanup_block) = if generator.is_async {
                    let send_block = self.new_block();
                    let after_block = self.new_block();
                    let if_cleanup_block = self.new_block();
                    (send_block, after_block, if_cleanup_block)
                } else {
                    let if_cleanup_block = self.new_block();
                    let after_block = self.new_block();
                    (BlockIdx::NULL, after_block, if_cleanup_block)
                };

                if i > 0 {
                    self.compile_comprehension_iter(generator)?;
                }

                self.use_cpython_label_block(loop_block);

                let mut end_async_for_target = BlockIdx::NULL;
                if generator.is_async {
                    let loop_label = self.instr_sequence_label_for_block(loop_block);
                    self.push_fblock_labels(
                        FBlockType::AsyncComprehensionGenerator,
                        loop_label,
                        ir::InstructionSequenceLabel::NO_LABEL,
                        FBlockDatum::None,
                    )?;
                    emit!(self, PseudoInstruction::SetupFinally { delta: after_block });
                    emit!(self, Instruction::GetAnext);
                    self.emit_load_const(ConstantData::None);
                    self.use_cpython_label_block(send_block);
                    let _ = self.compile_yield_from_sequence(true);
                    end_async_for_target = send_block;
                    emit!(self, PseudoInstruction::PopBlock);
                    self.compile_store(&generator.target)?;
                } else {
                    let saved_range = self.current_source_range;
                    self.set_source_range(generator.iter.range());
                    emit!(self, Instruction::ForIter { delta: after_block });
                    self.set_source_range(saved_range);
                    self.compile_store(&generator.target)?;
                }

                real_loop_depth += 1;
                let backedge_range = if i + 1 == generators.len() {
                    element_range
                } else {
                    outer_backedge_range
                };
                loop_labels.push(ComprehensionLoopControl::Iteration {
                    loop_block,
                    if_cleanup_block,
                    after_block,
                    backedge_range,
                    is_async: generator.is_async,
                    end_async_for_target,
                });

                // CPython always lowers comprehension guards through codegen_jump_if
                // and leaves constant-folding to later CFG optimization passes.
                for if_condition in &generator.ifs {
                    self.compile_jump_if(if_condition, false, if_cleanup_block)?;
                }
            }

            // Step 6: Compile the element expression and append to collection
            compile_element(self, real_loop_depth + 1)?;

            // Step 7: Close all loops
            for loop_control in loop_labels.iter().rev().copied() {
                match loop_control {
                    ComprehensionLoopControl::Iteration {
                        loop_block,
                        if_cleanup_block,
                        after_block,
                        backedge_range,
                        is_async,
                        end_async_for_target,
                    } => {
                        self.use_cpython_label_block(if_cleanup_block);
                        self.set_source_range(backedge_range);
                        emit!(self, PseudoInstruction::Jump { delta: loop_block });

                        if is_async {
                            let loop_label = self.instr_sequence_label_for_block(loop_block);
                            self.pop_fblock_label(
                                FBlockType::AsyncComprehensionGenerator,
                                loop_label,
                            );
                        }

                        self.use_cpython_label_block(after_block);
                        if is_async {
                            self.set_source_range(comprehension_range);
                            self.emit_end_async_for(end_async_for_target);
                        } else {
                            self.emit_sync_comprehension_end_for();
                        }
                    }
                    ComprehensionLoopControl::IfCleanupOnly { if_cleanup_block } => {
                        self.use_cpython_label_block(if_cleanup_block);
                    }
                }
            }

            // Step 8: Clean up - restore saved locals (and cell values)
            self.set_source_range(comprehension_range);
            if let Some(cleanup_block) = cleanup_block {
                emit!(self, PseudoInstruction::PopBlock);
                self.set_no_location();

                // Match CPython codegen_pop_inlined_comprehension_locals():
                // the synthetic jump that skips the exception cleanup uses
                // JUMP_NO_INTERRUPT, which becomes JUMP_BACKWARD_NO_INTERRUPT
                // when the cleanup tail sits above the final restore block.
                let end_block = self.new_block();
                emit!(
                    self,
                    PseudoInstruction::JumpNoInterrupt { delta: end_block }
                );
                self.set_no_location();

                // Exception cleanup path
                self.use_cpython_label_block(cleanup_block);
                // Stack: [saved_values..., collection, exception]
                emit!(self, Instruction::Swap { i: 2 });
                self.set_no_location();
                emit!(self, Instruction::PopTop); // Pop incomplete collection
                self.set_no_location();

                // Restore locals and cell values
                self.set_source_range(comprehension_range);
                emit!(
                    self,
                    Instruction::Swap {
                        i: u32::try_from(total_stack_items + 1).unwrap()
                    }
                );
                for name in pushed_locals.iter().rev() {
                    let var_num = self.varname(name).as_u32();
                    emit!(self, PseudoInstruction::StoreFastMaybeNull { var_num });
                }
                // Re-raise the exception
                emit!(self, Instruction::Reraise { depth: 0 });
                self.set_no_location();

                // Normal end path
                self.use_cpython_label_block(end_block);
                self.set_source_range(comprehension_range);
            }

            // SWAP result to TOS (above saved values)
            if total_stack_items > 0 {
                emit!(
                    self,
                    Instruction::Swap {
                        i: u32::try_from(total_stack_items + 1).unwrap()
                    }
                );
            }

            // Restore saved locals (StoreFast restores the saved cell object for merged cells)
            for name in pushed_locals.iter().rev() {
                let var_num = self.varname(name).as_u32();
                emit!(self, PseudoInstruction::StoreFastMaybeNull { var_num });
            }
            self.set_source_range(saved_source_range);

            Ok(())
        })();

        let current_table = self.symbol_table_stack.last_mut().expect("no symbol table");
        for (name, original_sym) in temp_symbols {
            current_table.symbols.insert(name, original_sym);
        }
        for name in changed_fast_hidden {
            self.current_code_info()
                .metadata
                .fast_hidden
                .insert(name.into(), false);
        }
        self.current_code_info().in_inlined_comp = was_in_inlined_comp;

        result
    }

    fn compile_future_features(&mut self, features: &[ast::Alias]) -> Result<(), CodegenError> {
        for feature in features {
            let future_feature = feature.name.as_str().try_into().map_err(|name| {
                self.error_ranged(CodegenErrorType::InvalidFutureFeature(name), feature.range)
            })?;

            match future_feature {
                FutureFeature::Braces => {
                    return Err(
                        self.error_ranged(CodegenErrorType::InvalidFutureBraces, feature.range)
                    );
                }
                FutureFeature::Annotations => {
                    self.future_annotations = true;
                    self.future_features
                        .insert(bytecode::CodeFlags::FUTURE_ANNOTATIONS);
                    self.current_code_info()
                        .flags
                        .insert(bytecode::CodeFlags::FUTURE_ANNOTATIONS);
                }
                FutureFeature::BarryAsFLUFL => {
                    self.future_features
                        .insert(bytecode::CodeFlags::FUTURE_BARRY_AS_BDFL);
                    self.current_code_info()
                        .flags
                        .insert(bytecode::CodeFlags::FUTURE_BARRY_AS_BDFL);
                }
                FutureFeature::AbsoluteImport
                | FutureFeature::Division
                | FutureFeature::GeneratorStop
                | FutureFeature::Generators
                | FutureFeature::NestedScopes
                | FutureFeature::PrintFunction
                | FutureFeature::UnicodeLiterals
                | FutureFeature::WithStatement => {
                    // Python 3 features. They are already implemented by default.
                }
            }
        }
        Ok(())
    }

    // Low level helper functions:
    /// CPython `_PyCfgBuilder_Addop()`: start a new basic block if the current
    /// one is terminated, then append the instruction to the current block.
    fn cpython_cfg_builder_addop(&mut self, info: ir::InstructionInfo) {
        self.maybe_start_cpython_cfg_addop_block();
        self.push_emitted_instruction(info);
    }

    fn push_emitted_instruction(&mut self, info: ir::InstructionInfo) {
        self.current_code_info()
            .addop_to_instr_sequence(info)
            .expect("malformed instruction sequence emission");
        self.current_code_info()
            .addop_to_current_block(info)
            .expect("malformed CFG emission");
    }

    fn push_emitted_instruction_with_target_label(
        &mut self,
        info: ir::InstructionInfo,
        target_label: ir::InstructionSequenceLabel,
    ) {
        self.current_code_info()
            .addop_to_instr_sequence_with_target_label(info, target_label)
            .expect("malformed instruction sequence emission");
        self.current_code_info()
            .addop_to_current_block(info)
            .expect("malformed CFG emission");
    }

    fn last_emitted_instruction_mut(&mut self) -> Option<&mut ir::InstructionInfo> {
        self.current_code_info().last_current_block_instr_mut()
    }

    fn set_last_emitted_lineno_override(&mut self, lineno_override: i32) {
        self.current_code_info()
            .set_last_instr_sequence_lineno_override(lineno_override);
        if let Some(last) = self.last_emitted_instruction_mut() {
            last.lineno_override = Some(lineno_override);
        }
    }

    fn _emit<I: Into<AnyInstruction>>(&mut self, instr: I, arg: OpArg, target: BlockIdx) {
        if self.do_not_emit_bytecode > 0 {
            return;
        }
        let instr = instr.into();
        let opcode = AnyOpcode::from(instr);
        debug_assert!(
            !instr.is_assembler(),
            "CPython codegen_addop_* must not emit assembler-only opcodes"
        );
        debug_assert!(
            opcode.has_arg() || instr.has_target() || u32::from(arg) == 0,
            "CPython _PyInstructionSequence_Addop requires either OPCODE_HAS_ARG, HAS_TARGET, or oparg == 0"
        );
        debug_assert!(
            target == BlockIdx::NULL || instr.has_target(),
            "CPython codegen_addop_j only accepts HAS_TARGET opcodes"
        );
        let range = self.current_source_range;
        let source = self.source_file.to_source_code();
        let location = source.source_location(range.start(), PositionEncoding::Utf8);
        let end_location = source.source_location(range.end(), PositionEncoding::Utf8);
        let except_handler = None;
        self.cpython_cfg_builder_addop(ir::InstructionInfo {
            instr,
            arg,
            target,
            location,
            end_location,
            except_handler,
            lineno_override: None,
        });
    }

    /// CPython `codegen_addop_j()`: emit a HAS_TARGET instruction with a
    /// jump_target_label oparg.
    fn emit_jump_label<I: Into<AnyInstruction>>(
        &mut self,
        instr: I,
        target_label: ir::InstructionSequenceLabel,
    ) {
        if self.do_not_emit_bytecode > 0 {
            return;
        }
        let instr = instr.into();
        debug_assert!(
            instr.has_target(),
            "CPython codegen_addop_j only accepts HAS_TARGET opcodes"
        );
        debug_assert!(
            !instr.is_assembler(),
            "CPython codegen_addop_j must not emit assembler-only opcodes"
        );
        let range = self.current_source_range;
        let source = self.source_file.to_source_code();
        let location = source.source_location(range.start(), PositionEncoding::Utf8);
        let end_location = source.source_location(range.end(), PositionEncoding::Utf8);
        let target = self
            .current_code_info()
            .block_for_instr_sequence_label(target_label);
        self.maybe_start_cpython_cfg_addop_block();
        self.push_emitted_instruction_with_target_label(
            ir::InstructionInfo {
                instr,
                arg: OpArg::NULL,
                target,
                location,
                end_location,
                except_handler: None,
                lineno_override: None,
            },
            target_label,
        );
    }

    /// Mark the last emitted instruction as having no source location.
    /// Prevents it from triggering LINE events in sys.monitoring.
    fn set_no_location(&mut self) {
        self.set_last_emitted_lineno_override(-1);
    }

    /// CPython `codegen_sync_comprehension_generator()` emits END_FOR/POP_ITER
    /// with NO_LOCATION and lets `flowgraph.c::propagate_line_numbers()` copy
    /// the FOR_ITER location onto the cleanup block.
    fn emit_sync_comprehension_end_for(&mut self) {
        emit!(self, Instruction::EndFor);
        self.set_no_location();
        emit!(self, Instruction::PopIter);
        self.set_no_location();
    }

    /// CPython `codegen_wrap_in_stopiteration_handler()` inserts
    /// `SETUP_CLEANUP` into the instruction sequence at index 0 with
    /// `_PyInstructionSequence_InsertInstruction()`. Generator and cell/free
    /// prefixes are inserted later by `flowgraph.c::insert_prefix_instructions()`.
    fn insert_cpython_stopiteration_setup_cleanup(&mut self, handler_block: BlockIdx) {
        let code = self.current_code_info();
        let entry = code
            .blocks
            .first_mut()
            .expect("code unit must have an entry block");
        debug_assert!(
            entry
                .used_instructions()
                .first()
                .is_some_and(|info| match info.instr.real() {
                    Some(Instruction::Resume { context }) => matches!(
                        context.get(info.arg).location(),
                        oparg::ResumeLocation::AtFuncStart
                    ),
                    _ => false,
                }),
            "scope entry must start with a function-start RESUME"
        );
        debug_assert!(
            !entry.used_instructions().iter().any(|info| matches!(
                info.instr.real(),
                Some(
                    Instruction::ReturnGenerator
                        | Instruction::MakeCell { .. }
                        | Instruction::CopyFreeVars { .. }
                )
            )),
            "CPython inserts StopIteration cleanup before CFG prefix instructions"
        );

        let result = code.insert_start_setup_cleanup(handler_block);
        unwrap_internal(self, result);
    }

    fn emit_no_arg<I: Into<AnyInstruction>>(&mut self, ins: I) {
        self._emit(ins, OpArg::NULL, BlockIdx::NULL)
    }

    fn emit_arg<A: OpArgType, T: EmitArg<A>, I: Into<AnyInstruction>>(
        &mut self,
        arg: T,
        f: impl FnOnce(OpArgMarker<A>) -> I,
    ) {
        let (op, arg, target) = arg.emit(f);
        self._emit(op, arg, target)
    }

    // fn block_done()

    fn arg_constant(&mut self, constant: ConstantData) -> oparg::ConstIdx {
        let info = self.current_code_info();
        if let ConstantData::Code { code } = &constant
            && let Some(idx) = info.metadata.consts.iter().position(|existing| {
                matches!(
                    existing,
                    ConstantData::Code {
                        code: existing_code
                    } if Self::code_objects_equivalent(existing_code, code)
                )
            })
        {
            return u32::try_from(idx)
                .expect("constant table index overflow")
                .into();
        }
        info.metadata.consts.insert_full(constant).0.to_u32().into()
    }

    fn constants_equivalent(lhs: &ConstantData, rhs: &ConstantData) -> bool {
        match (lhs, rhs) {
            (ConstantData::Code { code: lhs }, ConstantData::Code { code: rhs }) => {
                Self::code_objects_equivalent(lhs, rhs)
            }
            (ConstantData::Tuple { elements: lhs }, ConstantData::Tuple { elements: rhs })
            | (
                ConstantData::Frozenset { elements: lhs },
                ConstantData::Frozenset { elements: rhs },
            ) => {
                lhs.len() == rhs.len()
                    && lhs
                        .iter()
                        .zip(rhs.iter())
                        .all(|(lhs, rhs)| Self::constants_equivalent(lhs, rhs))
            }
            (ConstantData::Slice { elements: lhs }, ConstantData::Slice { elements: rhs }) => lhs
                .iter()
                .zip(rhs.iter())
                .all(|(lhs, rhs)| Self::constants_equivalent(lhs, rhs)),
            _ => lhs == rhs,
        }
    }

    fn code_objects_equivalent(lhs: &bytecode::CodeObject, rhs: &bytecode::CodeObject) -> bool {
        lhs.instructions.len() == rhs.instructions.len()
            && lhs
                .instructions
                .iter()
                .zip(rhs.instructions.iter())
                .all(|(lhs, rhs)| u8::from(lhs.op) == u8::from(rhs.op) && lhs.arg == rhs.arg)
            && lhs.locations == rhs.locations
            && lhs.flags.bits() == rhs.flags.bits()
            && lhs.posonlyarg_count == rhs.posonlyarg_count
            && lhs.arg_count == rhs.arg_count
            && lhs.kwonlyarg_count == rhs.kwonlyarg_count
            && lhs.source_path == rhs.source_path
            && lhs.first_line_number == rhs.first_line_number
            && lhs.max_stackdepth == rhs.max_stackdepth
            && lhs.obj_name == rhs.obj_name
            && lhs.qualname == rhs.qualname
            && lhs.constants.len() == rhs.constants.len()
            && lhs
                .constants
                .iter()
                .zip(rhs.constants.iter())
                .all(|(lhs, rhs)| Self::constants_equivalent(lhs, rhs))
            && lhs.names == rhs.names
            && lhs.varnames == rhs.varnames
            && lhs.cellvars == rhs.cellvars
            && lhs.freevars == rhs.freevars
            && lhs.localspluskinds == rhs.localspluskinds
            && lhs.linetable == rhs.linetable
            && lhs.exceptiontable == rhs.exceptiontable
    }

    fn constant_as_fold_int(constant: &ConstantData) -> Option<(BigInt, bool)> {
        match constant {
            ConstantData::Boolean { value } => Some((BigInt::from(u8::from(*value)), true)),
            ConstantData::Integer { value } => Some((value.clone(), false)),
            _ => None,
        }
    }

    fn try_fold_constant_binop(
        op: ast::Operator,
        left: &ConstantData,
        right: &ConstantData,
    ) -> Option<ConstantData> {
        let (left_int, left_is_bool) = Self::constant_as_fold_int(left)?;
        let (right_int, right_is_bool) = Self::constant_as_fold_int(right)?;

        if !(left_is_bool && right_is_bool) {
            return None;
        }

        match op {
            ast::Operator::BitAnd => Some(ConstantData::Boolean {
                value: !left_int.is_zero() & !right_int.is_zero(),
            }),
            ast::Operator::BitOr => Some(ConstantData::Boolean {
                value: !left_int.is_zero() | !right_int.is_zero(),
            }),
            ast::Operator::BitXor => Some(ConstantData::Boolean {
                value: !left_int.is_zero() ^ !right_int.is_zero(),
            }),
            _ => None,
        }
    }

    fn try_fold_constant_expr(&mut self, expr: &ast::Expr) -> CompileResult<Option<ConstantData>> {
        if let Some(constant) = self.ast_constant_value(expr) {
            return Ok(Some(constant));
        }
        Ok(Some(match expr {
            ast::Expr::NumberLiteral(num) => match &num.value {
                ast::Number::Int(int) => ConstantData::Integer {
                    value: ruff_int_to_bigint(int).map_err(|e| self.error(e))?,
                },
                ast::Number::Float(f) => ConstantData::Float { value: *f },
                ast::Number::Complex { real, imag } => ConstantData::Complex {
                    value: Complex::new(*real, *imag),
                },
            },
            ast::Expr::StringLiteral(s) => ConstantData::Str {
                value: string_literal_value(&self.source_file, &s.value),
            },
            ast::Expr::BytesLiteral(b) => ConstantData::Bytes {
                value: b.value.bytes().collect(),
            },
            ast::Expr::BooleanLiteral(b) => ConstantData::Boolean { value: b.value },
            ast::Expr::NoneLiteral(_) => ConstantData::None,
            ast::Expr::EllipsisLiteral(_) => ConstantData::Ellipsis,
            ast::Expr::Name(ast::ExprName { id, ctx, .. })
                if matches!(ctx, ast::ExprContext::Load) && id.as_str() == "__debug__" =>
            {
                ConstantData::Boolean {
                    value: self.opts.optimize == 0,
                }
            }
            ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                let mut elements = Vec::with_capacity(elts.len());
                for elt in elts {
                    let Some(constant) = self.try_fold_constant_expr(elt)? else {
                        return Ok(None);
                    };
                    elements.push(constant);
                }
                ConstantData::Tuple { elements }
            }
            ast::Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
                let Some(container) = self.try_fold_constant_expr(value)? else {
                    return Ok(None);
                };
                let Some(index) = self.try_fold_constant_expr(slice)? else {
                    return Ok(None);
                };
                let ConstantData::Integer { value: index } = index else {
                    return Ok(None);
                };
                let Some(index): Option<i64> = index.try_into().ok() else {
                    return Ok(None);
                };

                match container {
                    ConstantData::Str { value } => {
                        let string = value.to_string();
                        if string.contains(char::REPLACEMENT_CHARACTER) {
                            return Ok(None);
                        }
                        let chars: Vec<_> = string.chars().collect();
                        let Some(len) = i64::try_from(chars.len()).ok() else {
                            return Ok(None);
                        };
                        let idx: i64 = if index < 0 { len + index } else { index };
                        let Some(idx) = usize::try_from(idx).ok() else {
                            return Ok(None);
                        };
                        let Some(ch) = chars.get(idx) else {
                            return Ok(None);
                        };
                        ConstantData::Str {
                            value: ch.to_string().into(),
                        }
                    }
                    ConstantData::Bytes { value } => {
                        let Some(len) = i64::try_from(value.len()).ok() else {
                            return Ok(None);
                        };
                        let idx: i64 = if index < 0 { len + index } else { index };
                        let Some(idx) = usize::try_from(idx).ok() else {
                            return Ok(None);
                        };
                        let Some(byte) = value.get(idx) else {
                            return Ok(None);
                        };
                        ConstantData::Integer {
                            value: BigInt::from(*byte),
                        }
                    }
                    ConstantData::Tuple { elements } => {
                        let Some(len) = i64::try_from(elements.len()).ok() else {
                            return Ok(None);
                        };
                        let idx: i64 = if index < 0 { len + index } else { index };
                        let Some(idx) = usize::try_from(idx).ok() else {
                            return Ok(None);
                        };
                        let Some(element) = elements.get(idx) else {
                            return Ok(None);
                        };
                        element.clone()
                    }
                    _ => return Ok(None),
                }
            }
            ast::Expr::BinOp(ast::ExprBinOp {
                left, op, right, ..
            }) => {
                let Some(left) = self.try_fold_constant_expr(left)? else {
                    return Ok(None);
                };
                let Some(right) = self.try_fold_constant_expr(right)? else {
                    return Ok(None);
                };
                let Some(constant) = Self::try_fold_constant_binop(*op, &left, &right) else {
                    return Ok(None);
                };
                constant
            }
            ast::Expr::UnaryOp(ast::ExprUnaryOp { op, operand, .. }) => {
                let Some(constant) = self.try_fold_constant_expr(operand)? else {
                    return Ok(None);
                };
                match (op, constant) {
                    (ast::UnaryOp::UAdd, value) => value,
                    (ast::UnaryOp::USub, ConstantData::Integer { value }) => {
                        ConstantData::Integer { value: -value }
                    }
                    (ast::UnaryOp::USub, ConstantData::Float { value }) => {
                        ConstantData::Float { value: -value }
                    }
                    (ast::UnaryOp::USub, ConstantData::Complex { value }) => {
                        ConstantData::Complex { value: -value }
                    }
                    (ast::UnaryOp::Invert, ConstantData::Integer { value }) => {
                        ConstantData::Integer { value: !value }
                    }
                    (ast::UnaryOp::Not, ConstantData::Tuple { .. }) => return Ok(None),
                    (ast::UnaryOp::Not, value) => ConstantData::Boolean {
                        value: !Self::constant_truthiness(&value),
                    },
                    _ => return Ok(None),
                }
            }
            ast::Expr::BoolOp(ast::ExprBoolOp { op, values, .. }) => {
                let mut constants = Vec::with_capacity(values.len());
                for value in values {
                    let Some(constant) = self.try_fold_constant_expr(value)? else {
                        return Ok(None);
                    };
                    constants.push(constant);
                }
                let mut iter = constants.into_iter();
                let Some(first) = iter.next() else {
                    return Ok(None);
                };
                let mut selected = first;
                match op {
                    ast::BoolOp::Or => {
                        if !Self::constant_truthiness(&selected) {
                            for constant in iter {
                                let is_truthy = Self::constant_truthiness(&constant);
                                selected = constant;
                                if is_truthy {
                                    break;
                                }
                            }
                        }
                    }
                    ast::BoolOp::And => {
                        if Self::constant_truthiness(&selected) {
                            for constant in iter {
                                let is_truthy = Self::constant_truthiness(&constant);
                                selected = constant;
                                if !is_truthy {
                                    break;
                                }
                            }
                        }
                    }
                }
                selected
            }
            _ => return Ok(None),
        }))
    }

    fn try_compile_ast_constant(
        &mut self,
        expr: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        if let Some(constant) = self.ast_constant_value(expr) {
            return Ok(Some(constant));
        }
        Ok(Some(match expr {
            ast::Expr::NumberLiteral(num) => match &num.value {
                ast::Number::Int(int) => ConstantData::Integer {
                    value: ruff_int_to_bigint(int).map_err(|e| self.error(e))?,
                },
                ast::Number::Float(value) => ConstantData::Float { value: *value },
                ast::Number::Complex { real, imag } => ConstantData::Complex {
                    value: Complex::new(*real, *imag),
                },
            },
            ast::Expr::StringLiteral(s) => ConstantData::Str {
                value: string_literal_value(&self.source_file, &s.value),
            },
            ast::Expr::BytesLiteral(b) => ConstantData::Bytes {
                value: b.value.bytes().collect(),
            },
            ast::Expr::BooleanLiteral(b) => ConstantData::Boolean { value: b.value },
            ast::Expr::NoneLiteral(_) => ConstantData::None,
            ast::Expr::EllipsisLiteral(_) => ConstantData::Ellipsis,
            _ => return Ok(None),
        }))
    }

    fn try_compile_match_mapping_key_direct_constant(
        &mut self,
        expr: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        if let Some(constant) = self.ast_constant_value(expr) {
            return Ok(match constant {
                ConstantData::Integer { .. }
                | ConstantData::Float { .. }
                | ConstantData::Bytes { .. }
                | ConstantData::Complex { .. }
                | ConstantData::Str { .. }
                | ConstantData::Boolean { .. }
                | ConstantData::None => Some(constant),
                _ => None,
            });
        }
        if matches!(
            expr,
            ast::Expr::BooleanLiteral(_) | ast::Expr::NoneLiteral(_)
        ) {
            return self.try_compile_ast_constant(expr);
        }
        self.try_compile_match_pattern_direct_literal(expr)
    }

    fn is_unexpected_match_literal_constant(expr: &ast::Expr) -> bool {
        if let Some(constant) = expr
            .as_constant_expr()
            .map(|expr| ast_constant_value_to_constant_data(expr.value.clone()))
        {
            return matches!(
                constant,
                ConstantData::Boolean { .. } | ConstantData::None | ConstantData::Ellipsis
            );
        }
        matches!(
            expr,
            ast::Expr::BooleanLiteral(_)
                | ast::Expr::NoneLiteral(_)
                | ast::Expr::EllipsisLiteral(_)
        )
    }

    fn try_compile_match_pattern_direct_literal(
        &mut self,
        expr: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        if let Some(constant) = self.ast_constant_value(expr) {
            return Ok(match constant {
                ConstantData::Integer { .. }
                | ConstantData::Float { .. }
                | ConstantData::Bytes { .. }
                | ConstantData::Complex { .. }
                | ConstantData::Str { .. } => Some(constant),
                _ => None,
            });
        }
        match expr {
            ast::Expr::NumberLiteral(_)
            | ast::Expr::StringLiteral(_)
            | ast::Expr::BytesLiteral(_) => self.try_compile_ast_constant(expr),
            _ => Ok(None),
        }
    }

    fn try_negate_match_pattern_constant(constant: ConstantData) -> Option<ConstantData> {
        match constant {
            ConstantData::Integer { value } => Some(ConstantData::Integer { value: -value }),
            ConstantData::Float { value } => Some(ConstantData::Float { value: -value }),
            ConstantData::Complex { value } => Some(ConstantData::Complex { value: -value }),
            _ => None,
        }
    }

    fn try_fold_match_pattern_binop(
        op: ast::Operator,
        left: &ConstantData,
        right: &ConstantData,
    ) -> Option<ConstantData> {
        let left = match left {
            ConstantData::Integer { value } => value.to_f64()?,
            ConstantData::Float { value } => *value,
            _ => return None,
        };
        let ConstantData::Complex { value: right } = right else {
            return None;
        };
        let value = match op {
            ast::Operator::Add => Complex::new(left + right.re, right.im),
            ast::Operator::Sub => Complex::new(left - right.re, -right.im),
            _ => return None,
        };
        Some(ConstantData::Complex { value })
    }

    fn try_fold_match_pattern_const_expr(
        &mut self,
        expr: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        // CPython 3.14 ast_preprocess.c::fold_const_match_patterns()
        // folds only the constant forms needed by match patterns before
        // codegen_pattern_value()/codegen_pattern_mapping_key() visit them.
        Ok(match expr {
            ast::Expr::UnaryOp(ast::ExprUnaryOp {
                op: ast::UnaryOp::USub,
                operand,
                ..
            }) => {
                let Some(constant) = self.try_compile_match_pattern_number_constant(operand)?
                else {
                    return Ok(None);
                };
                Self::try_negate_match_pattern_constant(constant)
            }
            ast::Expr::BinOp(ast::ExprBinOp {
                left, op, right, ..
            }) if matches!(op, ast::Operator::Add | ast::Operator::Sub) => {
                let Some(left) = self.try_compile_match_pattern_signed_real_constant(left)? else {
                    return Ok(None);
                };
                let Some(right) = self.try_compile_match_pattern_imaginary_constant(right)? else {
                    return Ok(None);
                };
                Self::try_fold_match_pattern_binop(*op, &left, &right)
            }
            _ => None,
        })
    }

    fn try_compile_match_pattern_signed_real_constant(
        &mut self,
        expr: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        if let Some(constant) = self.try_compile_match_pattern_real_constant(expr)? {
            return Ok(Some(constant));
        }
        let ast::Expr::UnaryOp(ast::ExprUnaryOp {
            op: ast::UnaryOp::USub,
            operand,
            ..
        }) = expr
        else {
            return Ok(None);
        };
        let Some(constant) = self.try_compile_match_pattern_real_constant(operand)? else {
            return Ok(None);
        };
        Ok(Self::try_negate_match_pattern_constant(constant))
    }

    fn try_compile_match_pattern_real_constant(
        &mut self,
        expr: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        let Some(constant) = self.try_compile_match_pattern_number_constant(expr)? else {
            return Ok(None);
        };
        Ok(match constant {
            ConstantData::Integer { .. } | ConstantData::Float { .. } => Some(constant),
            _ => None,
        })
    }

    fn try_compile_match_pattern_imaginary_constant(
        &mut self,
        expr: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        let Some(constant) = self.try_compile_match_pattern_number_constant(expr)? else {
            return Ok(None);
        };
        Ok(match constant {
            ConstantData::Complex { .. } => Some(constant),
            _ => None,
        })
    }

    fn try_compile_match_pattern_number_constant(
        &mut self,
        expr: &ast::Expr,
    ) -> CompileResult<Option<ConstantData>> {
        if let Some(constant) = self.ast_constant_value(expr) {
            return Ok(match constant {
                ConstantData::Integer { .. }
                | ConstantData::Float { .. }
                | ConstantData::Complex { .. } => Some(constant),
                _ => None,
            });
        }
        match expr {
            ast::Expr::NumberLiteral(_) => self.try_compile_ast_constant(expr),
            _ => Ok(None),
        }
    }

    fn compile_match_pattern_expr(&mut self, expr: &ast::Expr) -> CompileResult<()> {
        if let Some(constant) = self.try_fold_match_pattern_const_expr(expr)? {
            self.set_source_range(expr.range());
            self.emit_load_const(constant);
        } else {
            self.compile_expression(expr)?;
        }
        Ok(())
    }

    fn emit_load_const(&mut self, constant: ConstantData) {
        let idx = self.arg_constant(constant);
        self.emit_arg(idx, |consti| Instruction::LoadConst { consti })
    }

    fn try_fold_constant_slice(
        &mut self,
        lower: Option<&ast::Expr>,
        upper: Option<&ast::Expr>,
        step: Option<&ast::Expr>,
    ) -> CompileResult<Option<ConstantData>> {
        if [lower, upper, step]
            .into_iter()
            .flatten()
            .any(|expr| !self.is_constant_expr(expr))
        {
            return Ok(None);
        }

        let start = match lower {
            Some(expr) => {
                let Some(constant) = self.try_fold_constant_expr(expr)? else {
                    return Ok(None);
                };
                constant
            }
            None => ConstantData::None,
        };
        let stop = match upper {
            Some(expr) => {
                let Some(constant) = self.try_fold_constant_expr(expr)? else {
                    return Ok(None);
                };
                constant
            }
            None => ConstantData::None,
        };
        let step = match step {
            Some(expr) => {
                let Some(constant) = self.try_fold_constant_expr(expr)? else {
                    return Ok(None);
                };
                constant
            }
            None => ConstantData::None,
        };

        Ok(Some(ConstantData::Slice {
            elements: Box::new([start, stop, step]),
        }))
    }

    fn emit_return_const(&mut self, constant: ConstantData) {
        self.emit_load_const(constant);
        emit!(self, Instruction::ReturnValue)
    }

    fn emit_return_const_no_location(&mut self, constant: ConstantData) {
        self.emit_load_const(constant);
        self.set_no_location();
        emit!(self, Instruction::ReturnValue);
        self.set_no_location();
    }

    fn emit_end_async_for(&mut self, send_target: BlockIdx) {
        self._emit(Instruction::EndAsyncFor, OpArg::NULL, send_target);
    }

    /// Emit LOAD_ATTR for attribute access (method=false).
    /// Encodes: (name_idx << 1) | 0
    fn emit_load_attr(&mut self, name_idx: u32) {
        let encoded = LoadAttr::new(name_idx, false);
        self.emit_arg(encoded, |namei| Instruction::LoadAttr { namei })
    }

    /// Emit LOAD_ATTR with method flag set (for method calls).
    /// Encodes: (name_idx << 1) | 1
    fn emit_load_attr_method(&mut self, name_idx: u32) {
        let encoded = LoadAttr::new(name_idx, true);
        self.emit_arg(encoded, |namei| Instruction::LoadAttr { namei })
    }

    /// Emit LOAD_GLOBAL.
    /// Encodes: (name_idx << 1) | push_null_bit
    fn emit_load_global(&mut self, name_idx: u32, push_null: bool) {
        let encoded = (name_idx << 1) | u32::from(push_null);
        self.emit_arg(encoded, |namei| Instruction::LoadGlobal { namei });
    }

    /// Emit LOAD_SUPER_ATTR for 2-arg super().attr access.
    /// Encodes: (name_idx << 2) | 0b10 (method=0, class=1)
    fn emit_load_super_attr(&mut self, name_idx: u32) {
        let encoded = LoadSuperAttr::new(name_idx, false, true);
        self.emit_arg(encoded, |namei| Instruction::LoadSuperAttr { namei })
    }

    /// Emit LOAD_SUPER_ATTR for 2-arg super().method() call.
    /// Encodes: (name_idx << 2) | 0b11 (method=1, class=1)
    fn emit_load_super_method(&mut self, name_idx: u32) {
        let encoded = LoadSuperAttr::new(name_idx, true, true);
        self.emit_arg(encoded, |namei| Instruction::LoadSuperAttr { namei })
    }

    /// Emit LOAD_SUPER_ATTR for 0-arg super().attr access.
    /// Encodes: (name_idx << 2) | 0b00 (method=0, class=0)
    fn emit_load_zero_super_attr(&mut self, name_idx: u32) {
        let encoded = LoadSuperAttr::new(name_idx, false, false);
        self.emit_arg(encoded, |namei| Instruction::LoadSuperAttr { namei })
    }

    /// Emit LOAD_SUPER_ATTR for 0-arg super().method() call.
    /// Encodes: (name_idx << 2) | 0b01 (method=1, class=0)
    fn emit_load_zero_super_method(&mut self, name_idx: u32) {
        let encoded = LoadSuperAttr::new(name_idx, true, false);
        self.emit_arg(encoded, |namei| Instruction::LoadSuperAttr { namei })
    }

    fn emit_return_value(&mut self) {
        emit!(self, Instruction::ReturnValue)
    }

    fn allows_top_level_await_in_current_context(&self) -> bool {
        self.opts.allow_top_level_await
            && self.ctx.func == FunctionContext::NoFunction
            && !self.ctx.in_class
    }

    fn current_code_info(&mut self) -> &mut ir::CodeInfo {
        self.code_stack.last_mut().expect("no code on stack")
    }

    /// Enter a conditional block (if/for/while/match/try/with)
    /// PEP 649: Track conditional annotation context
    fn enter_conditional_block(&mut self) {
        self.current_code_info().in_conditional_block += 1;
    }

    /// Leave a conditional block
    fn leave_conditional_block(&mut self) {
        let code_info = self.current_code_info();
        debug_assert!(code_info.in_conditional_block > 0);
        code_info.in_conditional_block -= 1;
    }

    /// Compile break or continue statement with proper fblock cleanup.
    /// compiler_break, compiler_continue
    /// This handles unwinding through With blocks and exception handlers.
    fn compile_break_continue(
        &mut self,
        range: ruff_text_size::TextRange,
        is_break: bool,
    ) -> CompileResult<()> {
        if self.do_not_emit_bytecode > 0 {
            // Still validate that we're inside a loop even in dead code
            let code = self.current_code_info();
            let mut found_loop = false;
            for i in (0..code.fblock.len()).rev() {
                match code.fblock[i].fb_type {
                    FBlockType::WhileLoop | FBlockType::ForLoop => {
                        found_loop = true;
                        break;
                    }
                    FBlockType::ExceptionGroupHandler => {
                        return Err(self.error_ranged(
                            CodegenErrorType::BreakContinueReturnInExceptStar,
                            range,
                        ));
                    }
                    _ => {}
                }
            }
            if !found_loop {
                if is_break {
                    return Err(self.error_ranged(CodegenErrorType::InvalidBreak, range));
                }
                return Err(self.error_ranged(CodegenErrorType::InvalidContinue, range));
            }
            return Ok(());
        }

        let prev_source_range = self.current_source_range;
        self.set_source_range(range);
        emit!(self, Instruction::Nop);

        let (mut unwind_loc, loop_fblock) = self.unwind_fblock_stack_with_loop(false, true)?;
        let Some(loop_fblock) = loop_fblock else {
            self.set_source_range(prev_source_range);
            if is_break {
                return Err(self.error_ranged(CodegenErrorType::InvalidBreak, range));
            }
            return Err(self.error_ranged(CodegenErrorType::InvalidContinue, range));
        };

        // CPython `codegen_break()` unwinds the loop fblock itself after
        // `codegen_unwind_fblock_stack()` returns it. `codegen_continue()`
        // jumps directly to the loop body label.
        if is_break {
            self.unwind_fblock(&loop_fblock, false, &mut unwind_loc)?;
        }

        // Jump to target
        let target_label = if is_break {
            debug_assert!(loop_fblock.fb_exit.is_jump_target_label());
            loop_fblock.fb_exit
        } else {
            debug_assert!(loop_fblock.fb_block.is_jump_target_label());
            loop_fblock.fb_block
        };
        let jump_is_artificial = if let Some(loc) = unwind_loc {
            self.set_source_range(loc);
            false
        } else {
            true
        };
        self.emit_jump_label(
            PseudoInstruction::Jump {
                delta: OpArgMarker::marker(),
            },
            target_label,
        );
        if jump_is_artificial {
            self.set_no_location();
        }
        self.set_source_range(prev_source_range);

        Ok(())
    }

    /// CPython `_PyCfgBuilder_Addop()` calls
    /// `cfg_builder_maybe_start_new_block()` before appending each instruction.
    /// That keeps any `IS_TERMINATOR_OPCODE` as the final instruction in its
    /// basicblock before `flowgraph.c::check_cfg()`.
    fn maybe_start_cpython_cfg_addop_block(&mut self) {
        let code = self.current_code_info();
        let cur = code.current_block;
        if !code.blocks[cur.idx()]
            .used_instructions()
            .last()
            .is_some_and(|instr| instr.instr.is_terminator())
        {
            return;
        }

        debug_assert_eq!(code.blocks[cur.idx()].next, BlockIdx::NULL);
        let block = self.cpython_cfg_builder_new_block();
        self.cpython_cfg_builder_use_next_block(block);
    }

    /// CPython `codegen_funcbody()` emits `NEW_JUMP_TARGET_LABEL(start)` and
    /// `USE_LABEL(start)` after scope entry. That label is part of the
    /// instruction-sequence label map, but it does not become a CFG
    /// `basicblock.b_label` unless some emitted instruction targets it.
    fn use_cpython_function_start_label(&mut self) -> ir::InstructionSequenceLabel {
        let (label, result) = {
            let code = self.current_code_info();
            let label = code.new_instr_sequence_label();
            let result = code.use_raw_instr_sequence_label(label);
            (label, result)
        };
        unwrap_internal(self, result);
        label
    }

    fn instr_sequence_label_for_block(&mut self, block: BlockIdx) -> ir::InstructionSequenceLabel {
        let result = self
            .current_code_info()
            .instr_sequence_label_for_block(block);
        unwrap_internal(self, result)
    }

    /// Switch to a block as CPython instruction-sequence labels would resolve.
    ///
    /// Consecutive `USE_LABEL()` calls can map multiple labels to the same
    /// instruction offset before `_PyCfg_FromInstructionSequence()` builds a
    /// CFG. Reuse an empty current block even if it already carries a label so
    /// codegen CFG path preserves that aliasing.
    fn use_cpython_label_block(&mut self, block: BlockIdx) {
        let result = self.current_code_info().use_instr_sequence_label(block);
        unwrap_internal(self, result);
        let code = self.current_code_info();
        let block = code.resolve_instr_sequence_label(block);
        let cur = code.current_block;
        let can_reuse_current = cur != block
            && code.blocks[cur.idx()].is_empty()
            && code.blocks[cur.idx()].next == BlockIdx::NULL
            && code.blocks[block.idx()].is_empty()
            && code.blocks[block.idx()].next == BlockIdx::NULL;

        if !can_reuse_current {
            let result = code.mark_cpython_cfg_label(block);
            unwrap_internal(self, result);
            self.switch_to_block(block);
            return;
        }

        {
            let target = &code.blocks[block.idx()];
            debug_assert!(!target.except_handler);
            debug_assert!(!target.preserve_lasti);
            debug_assert_eq!(target.start_depth, ir::START_DEPTH_UNSET);
            debug_assert!(!target.cold);
        }
        let result = code.mark_cpython_cfg_label(cur);
        unwrap_internal(self, result);

        let result = self
            .current_code_info()
            .use_instr_sequence_label_at_block(block, cur);
        unwrap_internal(self, result);
    }

    fn new_block(&mut self) -> BlockIdx {
        let result = self
            .current_code_info()
            .blocks
            .try_reserve(1)
            .map_err(|_| InternalError::MalformedControlFlowGraph);
        unwrap_internal(self, result);
        let code = self.current_code_info();
        let idx = BlockIdx::new(code.blocks.len().to_u32());
        code.blocks.push(Block::default());
        let result = code.push_unmapped_instr_sequence_label();
        unwrap_internal(self, result);
        idx
    }

    fn new_unlabeled_block(&mut self) -> BlockIdx {
        let result = self
            .current_code_info()
            .blocks
            .try_reserve(1)
            .map_err(|_| InternalError::MalformedControlFlowGraph);
        unwrap_internal(self, result);
        let code = self.current_code_info();
        let idx = BlockIdx::new(code.blocks.len().to_u32());
        code.blocks.push(Block::default());
        let result = code.push_unlabeled_instr_sequence_block();
        unwrap_internal(self, result);
        idx
    }

    /// flowgraph.c cfg_builder_new_block
    fn cpython_cfg_builder_new_block(&mut self) -> BlockIdx {
        self.new_unlabeled_block()
    }

    /// flowgraph.c cfg_builder_use_next_block
    fn cpython_cfg_builder_use_next_block(&mut self, block: BlockIdx) {
        let code = self.current_code_info();
        let cur = code.current_block;
        code.blocks[cur.idx()].next = block;
        code.current_block = block;
    }

    fn switch_to_block(&mut self, block: BlockIdx) {
        let result = self.current_code_info().use_instr_sequence_label(block);
        unwrap_internal(self, result);
        let code = self.current_code_info();
        let block = code.resolve_instr_sequence_label(block);
        let prev = code.current_block;
        assert_ne!(prev, block, "recursive switching {prev:?} -> {block:?}");
        assert_eq!(
            code.blocks[block].next,
            BlockIdx::NULL,
            "switching {prev:?} -> {block:?} to completed block"
        );
        let prev_block = &mut code.blocks[prev.idx()];
        assert_eq!(
            u32::from(prev_block.next),
            u32::MAX,
            "switching {prev:?} -> {block:?} from block that's already got a next"
        );
        prev_block.next = block;
        code.current_block = block;
    }

    const fn set_source_range(&mut self, range: TextRange) {
        self.current_source_range = range;
    }

    fn update_start_location_to_match_attr(
        &self,
        loc_range: TextRange,
        attr_range: TextRange,
        attr: &str,
    ) -> TextRange {
        let source = self.source_file.to_source_code();
        if source.line_index(loc_range.start()) == source.line_index(attr_range.end()) {
            return loc_range;
        }
        let Ok(attr_len) = u32::try_from(attr.chars().count()) else {
            return TextRange::new(loc_range.start(), loc_range.end());
        };
        let attr_len = TextSize::new(attr_len);
        if attr_len > attr_range.len() {
            return TextRange::new(loc_range.start(), loc_range.end());
        }
        TextRange::new(attr_range.end() - attr_len, loc_range.end())
    }

    fn source_line_start_range(&self, lineno: u32) -> TextRange {
        let source = self.source_file.to_source_code();
        let line = OneIndexed::new(lineno as usize).unwrap_or(OneIndexed::MIN);
        let start = source.line_start(line);
        TextRange::new(start, start)
    }

    fn module_start_location(&self, body: &[ast::Stmt]) -> TextRange {
        body.first()
            .map_or_else(|| self.source_line_start_range(1), Ranged::range)
    }

    fn get_source_line_number(&mut self) -> OneIndexed {
        self.source_file
            .to_source_code()
            .line_index(self.current_source_range.start())
    }

    fn mark_generator(&mut self) {
        let is_async = self.ctx.func == FunctionContext::AsyncFunction;
        let flags = &mut self.current_code_info().flags;
        if is_async {
            flags.remove(bytecode::CodeFlags::COROUTINE);
            flags.insert(bytecode::CodeFlags::ASYNC_GENERATOR);
        } else {
            flags.insert(bytecode::CodeFlags::GENERATOR);
        }
    }

    /// Whether the expression contains an await expression and
    /// thus requires the function to be async.
    ///
    /// Both:
    /// ```py
    /// async with: ...
    /// async for: ...
    /// ```
    /// are statements, so we won't check for them here
    fn contains_await(expression: &ast::Expr) -> bool {
        use ast::visitor::Visitor;

        #[derive(Default)]
        struct AwaitVisitor {
            found: bool,
        }

        impl ast::visitor::Visitor<'_> for AwaitVisitor {
            fn visit_expr(&mut self, expr: &ast::Expr) {
                if self.found {
                    return;
                }

                match expr {
                    ast::Expr::Await(_) => self.found = true,
                    ast::Expr::ListComp(ast::ExprListComp { generators, .. })
                    | ast::Expr::SetComp(ast::ExprSetComp { generators, .. })
                    | ast::Expr::DictComp(ast::ExprDictComp { generators, .. })
                        if generators.iter().any(|generator| generator.is_async) =>
                    {
                        self.found = true
                    }
                    _ => ast::visitor::walk_expr(self, expr),
                }
            }
        }

        let mut visitor = AwaitVisitor::default();
        visitor.visit_expr(expression);
        visitor.found
    }

    /// Check if any of the generators (except the first one's iter) contains an await expression.
    /// The first generator's iter is evaluated outside the comprehension scope.
    fn generators_contain_await(generators: &[ast::Comprehension]) -> bool {
        for (i, generator) in generators.iter().enumerate() {
            // First generator's iter is evaluated outside the comprehension
            if i > 0 && Self::contains_await(&generator.iter) {
                return true;
            }
            // Check ifs in all generators
            for if_expr in &generator.ifs {
                if Self::contains_await(if_expr) {
                    return true;
                }
            }
        }
        false
    }

    fn compile_expr_fstring(&mut self, fstring: &ast::ExprFString) -> CompileResult<()> {
        let fstring_range = fstring.range;
        let fstring = fstring.value.as_slice();
        if self.count_fstring_parts(fstring) > STACK_USE_GUIDELINE {
            return self.compile_fstring_parts_joined(fstring, fstring_range);
        }

        let mut element_count = 0;
        let mut pending_literal = None;
        let mut pending_literal_range = None;
        let mut pending_literal_no_location = false;
        for part in fstring {
            self.compile_fstring_part_into(
                part,
                &mut pending_literal,
                &mut pending_literal_range,
                &mut pending_literal_no_location,
                &mut element_count,
                None,
            )?;
        }
        self.finish_fstring(
            pending_literal,
            pending_literal_range,
            pending_literal_no_location,
            element_count,
            Some(fstring_range),
        );
        Ok(())
    }

    fn compile_runtime_joined_str(
        &mut self,
        fstring: &ast::ExprFString,
        values: &[ast::Expr],
    ) -> CompileResult<()> {
        let range = fstring.range;
        let value_count: u32 = values
            .len()
            .try_into()
            .expect("JoinedStr value count overflowed");
        if value_count > STACK_USE_GUIDELINE {
            self.set_source_range(range);
            self.emit_load_const(ConstantData::Str {
                value: Wtf8Buf::new(),
            });
            let join_idx = self.get_global_name_index("join");
            self.emit_load_attr_method(join_idx);
            emit!(self, Instruction::BuildList { count: 0 });
            for value in values {
                self.compile_expression(value)?;
                self.set_source_range(range);
                emit!(self, Instruction::ListAppend { i: 1 });
            }
            self.set_source_range(range);
            emit!(self, Instruction::Call { argc: 1 });
        } else {
            for value in values {
                self.compile_expression(value)?;
            }
            if value_count > 1 {
                self.set_source_range(range);
                emit!(self, Instruction::BuildString { count: value_count });
            } else if value_count == 0 {
                self.set_source_range(range);
                self.emit_load_const(ConstantData::Str {
                    value: Wtf8Buf::new(),
                });
            }
        }
        Ok(())
    }

    fn compile_fstring_parts_joined(
        &mut self,
        fstring: &[ast::FStringPart],
        fstring_range: TextRange,
    ) -> CompileResult<()> {
        self.set_source_range(fstring_range);
        self.emit_load_const(ConstantData::Str {
            value: Wtf8Buf::new(),
        });
        let join_idx = self.get_global_name_index("join");
        self.emit_load_attr_method(join_idx);
        emit!(self, Instruction::BuildList { count: 0 });

        let mut element_count = 0;
        let mut pending_literal = None;
        let mut pending_literal_range = None;
        let mut pending_literal_no_location = false;
        for part in fstring {
            self.compile_fstring_part_into(
                part,
                &mut pending_literal,
                &mut pending_literal_range,
                &mut pending_literal_no_location,
                &mut element_count,
                Some(fstring_range),
            )?;
        }
        self.finish_fstring_join(
            pending_literal,
            pending_literal_range,
            pending_literal_no_location,
            element_count,
            fstring_range,
        );
        Ok(())
    }

    fn compile_fstring_part_into(
        &mut self,
        part: &ast::FStringPart,
        pending_literal: &mut Option<Wtf8Buf>,
        pending_literal_range: &mut Option<TextRange>,
        pending_literal_no_location: &mut bool,
        element_count: &mut u32,
        join_append_range: Option<TextRange>,
    ) -> CompileResult<()> {
        match part {
            ast::FStringPart::Literal(string) => {
                let value = string_literal_part_value(&self.source_file, string);
                if pending_literal.is_none() {
                    *pending_literal_range = Some(string.range);
                    *pending_literal_no_location = string.range == TextRange::default();
                    *pending_literal = Some(value);
                } else if let Some(pending) = pending_literal.as_mut() {
                    Self::extend_pending_literal_range(pending_literal_range, string.range);
                    *pending_literal_no_location &= string.range == TextRange::default();
                    pending.push_wtf8(value.as_ref());
                }
                Ok(())
            }
            ast::FStringPart::FString(fstring) => self.compile_fstring_elements_into(
                fstring.flags,
                &fstring.elements,
                pending_literal,
                (pending_literal_range, pending_literal_no_location),
                element_count,
                join_append_range,
            ),
        }
    }

    fn finish_fstring(
        &mut self,
        mut pending_literal: Option<Wtf8Buf>,
        mut pending_literal_range: Option<TextRange>,
        mut pending_literal_no_location: bool,
        mut element_count: u32,
        fstring_range: Option<TextRange>,
    ) {
        self.emit_pending_fstring_literal(
            &mut pending_literal,
            &mut pending_literal_range,
            &mut pending_literal_no_location,
            &mut element_count,
            None,
        );

        if element_count == 0 {
            if let Some(fstring_range) = fstring_range {
                self.set_source_range(fstring_range);
            }
            self.emit_load_const(ConstantData::Str {
                value: Wtf8Buf::new(),
            });
        } else if element_count > 1 {
            if let Some(fstring_range) = fstring_range {
                self.set_source_range(fstring_range);
            }
            emit!(
                self,
                Instruction::BuildString {
                    count: element_count
                }
            );
        }
    }

    fn finish_fstring_join(
        &mut self,
        mut pending_literal: Option<Wtf8Buf>,
        mut pending_literal_range: Option<TextRange>,
        mut pending_literal_no_location: bool,
        mut element_count: u32,
        fstring_range: TextRange,
    ) {
        self.emit_pending_fstring_literal(
            &mut pending_literal,
            &mut pending_literal_range,
            &mut pending_literal_no_location,
            &mut element_count,
            Some(fstring_range),
        );
        self.set_source_range(fstring_range);
        emit!(self, Instruction::Call { argc: 1 });
    }

    fn emit_pending_fstring_literal(
        &mut self,
        pending_literal: &mut Option<Wtf8Buf>,
        pending_literal_range: &mut Option<TextRange>,
        pending_literal_no_location: &mut bool,
        element_count: &mut u32,
        join_append_range: Option<TextRange>,
    ) {
        let Some(value) = pending_literal.take() else {
            return;
        };
        let range = pending_literal_range.take();
        let no_location = *pending_literal_no_location;
        *pending_literal_no_location = false;

        // An empty literal fragment contributes nothing, so it is dropped. An
        // f-string left with no fragments at all still loads an empty string,
        // positioned at the whole f-string rather than at any one fragment.
        if value.is_empty() {
            return;
        }

        if let Some(range) = range {
            self.set_source_range(range);
        }
        self.emit_load_const(ConstantData::Str { value });
        if no_location {
            self.set_no_location();
        }
        *element_count += 1;
        if let Some(join_append_range) = join_append_range {
            self.set_source_range(join_append_range);
            emit!(self, Instruction::ListAppend { i: 1 });
        }
    }

    fn extend_pending_literal_range(pending: &mut Option<TextRange>, range: TextRange) {
        let Some(existing) = pending else {
            *pending = Some(range);
            return;
        };
        if *existing == TextRange::default() {
            *existing = range;
        } else if range != TextRange::default() {
            *existing = TextRange::new(existing.start(), range.end());
        }
    }

    fn count_fstring_parts(&self, fstring: &[ast::FStringPart]) -> u32 {
        let mut element_count = 0;
        let mut pending_literal = None;
        for part in fstring {
            self.count_fstring_part_into(part, &mut pending_literal, &mut element_count);
        }
        Self::count_pending_fstring_literal(&mut pending_literal, &mut element_count);
        element_count
    }

    fn count_fstring_part_into(
        &self,
        part: &ast::FStringPart,
        pending_literal: &mut Option<Wtf8Buf>,
        element_count: &mut u32,
    ) {
        match part {
            ast::FStringPart::Literal(string) => {
                let value = string_literal_part_value(&self.source_file, string);
                if let Some(pending) = pending_literal.as_mut() {
                    pending.push_wtf8(value.as_ref());
                } else {
                    *pending_literal = Some(value);
                }
            }
            ast::FStringPart::FString(fstring) => self.count_fstring_elements_into(
                fstring.flags,
                &fstring.elements,
                pending_literal,
                element_count,
            ),
        }
    }

    fn count_pending_fstring_literal(
        pending_literal: &mut Option<Wtf8Buf>,
        element_count: &mut u32,
    ) {
        let Some(value) = pending_literal.take() else {
            return;
        };

        if value.is_empty() {
            return;
        }

        *element_count += 1;
    }

    fn compile_fstring_elements(
        &mut self,
        flags: ast::FStringFlags,
        fstring_elements: &ast::InterpolatedStringElements,
        fstring_range: Option<TextRange>,
    ) -> CompileResult<()> {
        if self.count_fstring_elements(flags, fstring_elements) > STACK_USE_GUIDELINE {
            let fstring_range = fstring_range.unwrap_or(self.current_source_range);
            return self.compile_fstring_elements_joined(flags, fstring_elements, fstring_range);
        }

        let mut element_count = 0;
        let mut pending_literal: Option<Wtf8Buf> = None;
        let mut pending_literal_range: Option<TextRange> = None;
        let mut pending_literal_no_location = false;
        self.compile_fstring_elements_into(
            flags,
            fstring_elements,
            &mut pending_literal,
            (&mut pending_literal_range, &mut pending_literal_no_location),
            &mut element_count,
            None,
        )?;
        self.finish_fstring(
            pending_literal,
            pending_literal_range,
            pending_literal_no_location,
            element_count,
            fstring_range,
        );
        Ok(())
    }

    fn compile_fstring_elements_joined(
        &mut self,
        flags: ast::FStringFlags,
        fstring_elements: &ast::InterpolatedStringElements,
        fstring_range: TextRange,
    ) -> CompileResult<()> {
        self.set_source_range(fstring_range);
        self.emit_load_const(ConstantData::Str {
            value: Wtf8Buf::new(),
        });
        let join_idx = self.get_global_name_index("join");
        self.emit_load_attr_method(join_idx);
        emit!(self, Instruction::BuildList { count: 0 });

        let mut element_count = 0;
        let mut pending_literal: Option<Wtf8Buf> = None;
        let mut pending_literal_range: Option<TextRange> = None;
        let mut pending_literal_no_location = false;
        self.compile_fstring_elements_into(
            flags,
            fstring_elements,
            &mut pending_literal,
            (&mut pending_literal_range, &mut pending_literal_no_location),
            &mut element_count,
            Some(fstring_range),
        )?;
        self.finish_fstring_join(
            pending_literal,
            pending_literal_range,
            pending_literal_no_location,
            element_count,
            fstring_range,
        );
        Ok(())
    }

    fn cpython_format_spec_range(&self, range: TextRange) -> TextRange {
        let start = range.start().to_usize();
        if start == 0 {
            return range;
        }
        let source = self.source_file.source_text().as_bytes();
        if source.get(start - 1) == Some(&b':') {
            TextRange::new(range.start() - TextSize::new(1), range.end())
        } else {
            range
        }
    }

    fn compile_fstring_elements_into(
        &mut self,
        flags: ast::FStringFlags,
        fstring_elements: &ast::InterpolatedStringElements,
        pending_literal: &mut Option<Wtf8Buf>,
        pending_literal_meta: (&mut Option<TextRange>, &mut bool),
        element_count: &mut u32,
        join_append_range: Option<TextRange>,
    ) -> CompileResult<()> {
        let (pending_literal_range, pending_literal_no_location) = pending_literal_meta;
        for element in fstring_elements {
            match element {
                ast::InterpolatedStringElement::Literal(string) => {
                    let value =
                        interpolated_string_literal_value(&self.source_file, string, flags.into());
                    if pending_literal.is_none() {
                        *pending_literal_range = Some(string.range);
                        *pending_literal_no_location = string.range == TextRange::default();
                        *pending_literal = Some(value);
                    } else if let Some(pending) = pending_literal.as_mut() {
                        Self::extend_pending_literal_range(pending_literal_range, string.range);
                        *pending_literal_no_location &= string.range == TextRange::default();
                        pending.push_wtf8(value.as_ref());
                    }
                }
                ast::InterpolatedStringElement::Interpolation(fstring_expr) => {
                    let mut conversion = match fstring_expr.conversion {
                        ast::ConversionFlag::None => ConvertValueOparg::None,
                        ast::ConversionFlag::Str => ConvertValueOparg::Str,
                        ast::ConversionFlag::Repr => ConvertValueOparg::Repr,
                        ast::ConversionFlag::Ascii => ConvertValueOparg::Ascii,
                    };

                    if let Some(debug_text) = &fstring_expr.debug_text {
                        let (text, debug_text_range) = interpolation_debug_text(
                            &self.source_file,
                            debug_text,
                            fstring_expr.expression.range(),
                        );
                        let text: Wtf8Buf = text.into();
                        Self::extend_pending_literal_range(pending_literal_range, debug_text_range);
                        *pending_literal_no_location = false;
                        pending_literal
                            .get_or_insert_with(Wtf8Buf::new)
                            .push_wtf8(text.as_ref());

                        // If debug text is present, apply repr conversion when no `format_spec` specified.
                        // See action_helpers.c: fstring_find_expr_replacement
                        if matches!(
                            (conversion, &fstring_expr.format_spec),
                            (ConvertValueOparg::None, None)
                        ) {
                            conversion = ConvertValueOparg::Repr;
                        }
                    }

                    self.emit_pending_fstring_literal(
                        pending_literal,
                        pending_literal_range,
                        pending_literal_no_location,
                        element_count,
                        join_append_range,
                    );

                    self.compile_expression(&fstring_expr.expression)?;

                    let formatted_value_range = fstring_expr.range;
                    match conversion {
                        ConvertValueOparg::None => {}
                        ConvertValueOparg::Str
                        | ConvertValueOparg::Repr
                        | ConvertValueOparg::Ascii => {
                            self.set_source_range(formatted_value_range);
                            emit!(self, Instruction::ConvertValue { oparg: conversion })
                        }
                    }

                    if let Some(format_spec) =
                        fstring_expr.runtime_formatted_value_format_spec.as_deref()
                    {
                        self.compile_expression(format_spec)?;

                        self.set_source_range(formatted_value_range);
                        emit!(self, Instruction::FormatWithSpec);
                    } else {
                        match &fstring_expr.format_spec {
                            Some(format_spec) => {
                                let format_spec_range =
                                    self.cpython_format_spec_range(format_spec.range);
                                self.compile_fstring_elements(
                                    flags,
                                    &format_spec.elements,
                                    Some(format_spec_range),
                                )?;

                                self.set_source_range(formatted_value_range);
                                emit!(self, Instruction::FormatWithSpec);
                            }
                            None => {
                                self.set_source_range(formatted_value_range);
                                emit!(self, Instruction::FormatSimple);
                            }
                        }
                    }

                    *element_count += 1;
                    if let Some(join_append_range) = join_append_range {
                        self.set_source_range(join_append_range);
                        emit!(self, Instruction::ListAppend { i: 1 });
                    }
                }
            }
        }

        Ok(())
    }

    fn count_fstring_elements(
        &self,
        flags: ast::FStringFlags,
        fstring_elements: &ast::InterpolatedStringElements,
    ) -> u32 {
        let mut element_count = 0;
        let mut pending_literal = None;
        self.count_fstring_elements_into(
            flags,
            fstring_elements,
            &mut pending_literal,
            &mut element_count,
        );
        Self::count_pending_fstring_literal(&mut pending_literal, &mut element_count);
        element_count
    }

    fn count_fstring_elements_into(
        &self,
        flags: ast::FStringFlags,
        fstring_elements: &ast::InterpolatedStringElements,
        pending_literal: &mut Option<Wtf8Buf>,
        element_count: &mut u32,
    ) {
        for element in fstring_elements {
            match element {
                ast::InterpolatedStringElement::Literal(string) => {
                    let value =
                        interpolated_string_literal_value(&self.source_file, string, flags.into());
                    if let Some(pending) = pending_literal.as_mut() {
                        pending.push_wtf8(value.as_ref());
                    } else {
                        *pending_literal = Some(value);
                    }
                }
                ast::InterpolatedStringElement::Interpolation(fstring_expr) => {
                    if let Some(debug_text) = &fstring_expr.debug_text {
                        let (text, _) = interpolation_debug_text(
                            &self.source_file,
                            debug_text,
                            fstring_expr.expression.range(),
                        );
                        let text: Wtf8Buf = text.into();
                        pending_literal
                            .get_or_insert_with(Wtf8Buf::new)
                            .push_wtf8(text.as_ref());
                    }

                    Self::count_pending_fstring_literal(pending_literal, element_count);
                    *element_count += 1;
                }
            }
        }
    }

    fn compile_expr_tstring(&mut self, expr_tstring: &ast::ExprTString) -> CompileResult<()> {
        // ast::TStringValue can contain multiple ast::TString parts (implicit
        // concatenation). Match CPython's stack order by materializing the
        // strings tuple first, then evaluating interpolations left-to-right.
        let tstring_value = &expr_tstring.value;

        let mut all_strings: Vec<(Wtf8Buf, TextRange)> = Vec::new();
        let mut current_string = Wtf8Buf::new();
        let mut current_string_range = None;
        let mut interp_count: u32 = 0;

        for tstring in tstring_value {
            self.collect_tstring_strings(
                tstring,
                &mut all_strings,
                &mut current_string,
                &mut current_string_range,
                &mut interp_count,
                expr_tstring.range,
            );
        }

        all_strings.push((
            core::mem::take(&mut current_string),
            current_string_range.unwrap_or(expr_tstring.range),
        ));

        let string_count: u32 = all_strings
            .len()
            .try_into()
            .expect("t-string string count overflowed");
        for (s, range) in &all_strings {
            self.set_source_range(*range);
            self.emit_load_const(ConstantData::Str { value: s.clone() });
        }
        self.set_source_range(expr_tstring.range);
        emit!(
            self,
            Instruction::BuildTuple {
                count: string_count
            }
        );

        for tstring in tstring_value {
            self.compile_tstring_interpolations(tstring)?;
        }

        self.set_source_range(expr_tstring.range);
        emit!(
            self,
            Instruction::BuildTuple {
                count: interp_count
            }
        );
        self.set_source_range(expr_tstring.range);
        emit!(self, Instruction::BuildTemplate);

        Ok(())
    }

    fn compile_runtime_template_str(
        &mut self,
        expr_tstring: &ast::ExprTString,
        values: &[ast::Expr],
    ) -> CompileResult<()> {
        let mut last_was_interpolation = true;
        let mut strings_len = 0;
        for value in values {
            if self.runtime_template_value_interpolation(value).is_some() {
                if last_was_interpolation {
                    self.set_source_range(expr_tstring.range);
                    self.emit_load_const(ConstantData::Str {
                        value: Wtf8Buf::new(),
                    });
                    strings_len += 1;
                }
                last_was_interpolation = true;
            } else {
                self.compile_expression(value)?;
                strings_len += 1;
                last_was_interpolation = false;
            }
        }
        if last_was_interpolation {
            self.set_source_range(expr_tstring.range);
            self.emit_load_const(ConstantData::Str {
                value: Wtf8Buf::new(),
            });
            strings_len += 1;
        }
        self.set_source_range(expr_tstring.range);
        emit!(self, Instruction::BuildTuple { count: strings_len });

        let mut interpolations_len = 0;
        for value in values {
            if let Some((tstring, interpolation)) = self.runtime_template_value_interpolation(value)
            {
                self.compile_runtime_interpolation(tstring, interpolation)?;
                interpolations_len += 1;
            }
        }
        self.set_source_range(expr_tstring.range);
        emit!(
            self,
            Instruction::BuildTuple {
                count: interpolations_len
            }
        );
        self.set_source_range(expr_tstring.range);
        emit!(self, Instruction::BuildTemplate);
        Ok(())
    }

    fn runtime_template_value_interpolation<'a>(
        &self,
        value: &'a ast::Expr,
    ) -> Option<(
        &'a ast::ExprTString,
        (&'a ast::ConstantValue, Option<&'a ast::Expr>),
    )> {
        let ast::Expr::TString(tstring) = value else {
            return None;
        };
        let interpolation = Self::single_runtime_interpolation(tstring)?;
        Self::single_tstring_interpolation(tstring)?;
        Some((tstring, interpolation))
    }

    fn compile_runtime_interpolation(
        &mut self,
        expr_tstring: &ast::ExprTString,
        interpolation: (&ast::ConstantValue, Option<&ast::Expr>),
    ) -> CompileResult<bool> {
        let Some(interp) = Self::single_tstring_interpolation(expr_tstring) else {
            return Ok(false);
        };

        self.compile_interpolation(interp, interpolation)?;
        Ok(true)
    }

    fn compile_interpolation(
        &mut self,
        interp: &ast::InterpolatedElement,
        interpolation: (&ast::ConstantValue, Option<&ast::Expr>),
    ) -> CompileResult<()> {
        let (str, format_spec) = interpolation;
        self.compile_expression(&interp.expression)?;
        self.set_source_range(interp.range);
        self.emit_load_const(ast_constant_value_to_constant_data(str.clone()));

        let conversion = match interp.conversion {
            ast::ConversionFlag::None => 0,
            ast::ConversionFlag::Str => 1,
            ast::ConversionFlag::Repr => 2,
            ast::ConversionFlag::Ascii => 3,
        };

        let has_format_spec = format_spec.is_some();
        if let Some(format_spec) = format_spec {
            self.compile_expression(format_spec)?;
        }

        let format = 2 | (conversion << 2) | u32::from(has_format_spec);
        self.set_source_range(interp.range);
        emit!(self, Instruction::BuildInterpolation { format });
        Ok(())
    }

    fn single_tstring_interpolation(
        expr_tstring: &ast::ExprTString,
    ) -> Option<&ast::InterpolatedElement> {
        let [tstring] = expr_tstring.value.as_slice() else {
            return None;
        };
        let mut elements = tstring.elements.iter();
        let ast::InterpolatedStringElement::Interpolation(interp) = elements.next()? else {
            return None;
        };
        if elements.next().is_some() {
            return None;
        }
        Some(interp)
    }

    fn collect_tstring_strings(
        &self,
        tstring: &ast::TString,
        strings: &mut Vec<(Wtf8Buf, TextRange)>,
        current_string: &mut Wtf8Buf,
        current_string_range: &mut Option<TextRange>,
        interp_count: &mut u32,
        template_range: TextRange,
    ) {
        for element in &tstring.elements {
            match element {
                ast::InterpolatedStringElement::Literal(lit) => {
                    if current_string_range.is_none() {
                        *current_string_range = Some(lit.range);
                    } else {
                        Self::extend_pending_literal_range(current_string_range, lit.range);
                    }
                    current_string.push_wtf8(&interpolated_string_literal_value(
                        &self.source_file,
                        lit,
                        tstring.flags.into(),
                    ));
                }
                ast::InterpolatedStringElement::Interpolation(interp) => {
                    if let Some(debug_text) = &interp.debug_text {
                        let (text, debug_text_range) = interpolation_debug_text(
                            &self.source_file,
                            debug_text,
                            interp.expression.range(),
                        );
                        if current_string_range.is_none() {
                            *current_string_range = Some(debug_text_range);
                        } else {
                            Self::extend_pending_literal_range(
                                current_string_range,
                                debug_text_range,
                            );
                        }
                        current_string.push_str(&text);
                        strings.push((
                            core::mem::take(current_string),
                            current_string_range.take().unwrap_or(template_range),
                        ));
                    } else {
                        strings.push((
                            core::mem::take(current_string),
                            current_string_range.take().unwrap_or(template_range),
                        ));
                    }
                    *interp_count += 1;
                }
            }
        }
    }

    fn compile_tstring_interpolations(&mut self, tstring: &ast::TString) -> CompileResult<()> {
        for element in &tstring.elements {
            let ast::InterpolatedStringElement::Interpolation(interp) = element else {
                continue;
            };

            if let Some(runtime_str) = interp.runtime_str.as_ref() {
                let interpolation = (
                    runtime_str,
                    interp.runtime_interpolation_format_spec.as_deref(),
                );
                self.compile_interpolation(interp, interpolation)?;
                continue;
            }

            self.compile_expression(&interp.expression)?;

            let expr_range = interp.expression.range();
            let expr_source = if interp.range.start() < expr_range.start()
                && interp.range.end() >= expr_range.end()
            {
                let after_brace = interp.range.start() + TextSize::new(1);
                self.source_file
                    .source_text()
                    .slice(TextRange::new(after_brace, expr_range.end()))
            } else {
                self.source_file.source_text().slice(expr_range)
            }
            .to_string();
            self.set_source_range(interp.range);
            self.emit_load_const(ConstantData::Str {
                value: expr_source.into(),
            });

            let mut conversion: u32 = match interp.conversion {
                ast::ConversionFlag::None => 0,
                ast::ConversionFlag::Str => 1,
                ast::ConversionFlag::Repr => 2,
                ast::ConversionFlag::Ascii => 3,
            };

            if interp.debug_text.is_some() && conversion == 0 && interp.format_spec.is_none() {
                conversion = 2;
            }

            let has_format_spec = interp.format_spec.is_some();
            if let Some(format_spec) = &interp.format_spec {
                let format_spec_range = self.cpython_format_spec_range(format_spec.range);
                self.compile_fstring_elements(
                    ast::FStringFlags::empty(),
                    &format_spec.elements,
                    Some(format_spec_range),
                )?;
            }

            // CPython keeps bit 1 set in BUILD_INTERPOLATION's oparg and uses
            // bit 0 for the optional format spec.
            let format = 2 | (conversion << 2) | u32::from(has_format_spec);
            self.set_source_range(interp.range);
            emit!(self, Instruction::BuildInterpolation { format });
        }

        Ok(())
    }
}

trait EmitArg<Arg: OpArgType> {
    fn emit<I: Into<AnyInstruction>>(
        self,
        f: impl FnOnce(OpArgMarker<Arg>) -> I,
    ) -> (AnyInstruction, OpArg, BlockIdx);
}

impl<T: OpArgType> EmitArg<T> for T {
    fn emit<I: Into<AnyInstruction>>(
        self,
        f: impl FnOnce(OpArgMarker<T>) -> I,
    ) -> (AnyInstruction, OpArg, BlockIdx) {
        let (marker, arg) = OpArgMarker::new(self);
        (f(marker).into(), arg, BlockIdx::NULL)
    }
}

impl EmitArg<bytecode::Label> for BlockIdx {
    fn emit<I: Into<AnyInstruction>>(
        self,
        f: impl FnOnce(OpArgMarker<bytecode::Label>) -> I,
    ) -> (AnyInstruction, OpArg, BlockIdx) {
        (f(OpArgMarker::marker()).into(), OpArg::NULL, self)
    }
}

/// Strips leading whitespace from a docstring.
///
/// `inspect.cleandoc` is a good reference, but has a few incompatibilities.
// = _PyCompile_CleanDoc
fn clean_doc(doc: &str) -> String {
    let doc = expandtabs(doc, 8);
    // First pass: find minimum indentation of non-blank lines AFTER the first line.
    // A "blank line" is one containing only spaces (or empty).
    let margin = doc
        .split('\n')
        .skip(1) // skip first line
        .filter(|line| line.chars().any(|c| c != ' ')) // non-blank lines only
        .map(|line| line.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0);

    let mut cleaned = String::with_capacity(doc.len());
    // Strip all leading spaces from the first line
    if let Some(first_line) = doc.split('\n').next() {
        let trimmed = first_line.trim_start();
        // Early exit: no leading spaces on first line AND margin == 0
        if trimmed.len() == first_line.len() && margin == 0 {
            return doc.to_owned();
        }
        cleaned.push_str(trimmed);
    }
    // Subsequent lines: skip up to `margin` leading spaces
    for line in doc.split('\n').skip(1) {
        cleaned.push('\n');
        let skip = line.chars().take(margin).take_while(|c| *c == ' ').count();
        cleaned.push_str(&line[skip..]);
    }

    cleaned
}

// copied from rustpython_common::str, so we don't have to depend on it just for this function
fn expandtabs(input: &str, tab_size: usize) -> String {
    let tab_stop = tab_size;
    let mut expanded_str = String::with_capacity(input.len());
    let mut tab_size = tab_stop;
    let mut col_count = 0usize;
    for ch in input.chars() {
        match ch {
            '\t' => {
                let num_spaces = tab_size - col_count;
                col_count += num_spaces;
                let expand = " ".repeat(num_spaces);
                expanded_str.push_str(&expand);
            }
            '\r' | '\n' => {
                expanded_str.push(ch);
                col_count = 0;
                tab_size = 0;
            }
            _ => {
                expanded_str.push(ch);
                col_count += 1;
            }
        }
        if col_count >= tab_size {
            tab_size += tab_stop;
        }
    }
    expanded_str
}

fn split_doc_with_range<'a>(
    body: &'a [ast::Stmt],
    opts: &CompileOpts,
) -> (Option<(String, TextRange)>, &'a [ast::Stmt]) {
    if let Some((ast::Stmt::Expr(expr), body_rest)) = body.split_first() {
        let doc_comment: Option<(&str, TextRange)> = match &*expr.value {
            ast::Expr::StringLiteral(value) => Some((value.value.to_str(), expr.value.range())),
            ast::Expr::Constant(ast::ExprConstant {
                value: ast::ConstantValue::Str(value),
                ..
            }) => Some((value.as_ref(), expr.value.range())),
            // f-strings are not allowed in Python doc comments.
            ast::Expr::FString(_) => None,
            _ => None,
        };
        if let Some((doc, range)) = doc_comment {
            return if opts.optimize < 2 {
                (Some((clean_doc(doc), range)), body_rest)
            } else {
                (None, body_rest)
            };
        }
    }
    (None, body)
}


pub fn ruff_int_to_bigint(int: &ast::Int) -> Result<BigInt, CodegenErrorType> {
    if let Some(small) = int.as_u64() {
        Ok(BigInt::from(small))
    } else {
        parse_big_integer(int)
    }
}

/// Converts a `ruff` ast integer into a `BigInt`.
/// Unlike small integers, big integers may be stored in one of four possible radix representations.
fn parse_big_integer(int: &ast::Int) -> Result<BigInt, CodegenErrorType> {
    // TODO: Improve ruff API
    // Can we avoid this copy?
    let s = format!("{int}");
    let mut s = s.as_str();
    // See: https://peps.python.org/pep-0515/#literal-grammar
    let radix = match s.get(0..2) {
        Some("0b" | "0B") => {
            s = s.get(2..).unwrap_or(s);
            2
        }
        Some("0o" | "0O") => {
            s = s.get(2..).unwrap_or(s);
            8
        }
        Some("0x" | "0X") => {
            s = s.get(2..).unwrap_or(s);
            16
        }
        _ => 10,
    };

    BigInt::from_str_radix(s, radix).map_err(|e| {
        CodegenErrorType::SyntaxError(format!(
            "unparsed integer literal (radix {radix}): {s} ({e})"
        ))
    })
}

// Note: Not a good practice in general. Keep this trait private only for compiler
trait ToU32 {
    fn to_u32(self) -> u32;
}

impl ToU32 for usize {
    fn to_u32(self) -> u32 {
        self.try_into().unwrap()
    }
}

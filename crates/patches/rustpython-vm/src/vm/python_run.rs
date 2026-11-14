//! Python code execution functions.

use crate::{
    Py, PyObjectRef, PyResult, VirtualMachine,
    builtins::PyCode,
    compiler::{self},
    scope::Scope,
};

impl VirtualMachine {
    /// PyRun_SimpleString
    ///
    /// Execute a string of Python code in a new scope with builtins.
    pub fn run_simple_string(&self, source: &str) -> PyResult {
        let scope = self.new_scope_with_builtins();
        self.run_string(scope, source, "<string>")
    }

    /// PyRun_String
    ///
    /// Execute a string of Python code with explicit scope and source path.
    pub fn run_string(&self, scope: Scope, source: &str, source_path: &str) -> PyResult {
        let code_obj = self
            .compile(source, compiler::Mode::Exec, source_path)
            .map_err(|err| err.into_pyexception(self, Some(source)))?;
        // linecache._register_code(code, source, filename)
        let _ = self.register_code_in_linecache(&code_obj, source);
        self.run_code_obj(code_obj, scope)
    }

    /// Register a code object's source in linecache._interactive_cache
    /// so that traceback can display source lines and caret indicators.
    pub fn register_code_in_linecache(&self, code: &Py<PyCode>, source: &str) -> PyResult<()> {
        let linecache = self.import("linecache", 0)?;
        let register = linecache.get_attr("_register_code", self)?;
        let source_str = self.ctx.new_str(source);
        let filename = self.ctx.new_str(code.source_path().as_str());
        register.call(
            (PyObjectRef::from(code.to_owned()), source_str, filename),
            self,
        )?;
        Ok(())
    }

    #[deprecated(note = "use run_string instead")]
    pub fn run_code_string(&self, scope: Scope, source: &str, source_path: &str) -> PyResult {
        self.run_string(scope, source, source_path)
    }

    pub fn run_block_expr(&self, scope: Scope, source: &str) -> PyResult {
        let code_obj = self
            .compile(source, compiler::Mode::BlockExpr, "<embedded>")
            .map_err(|err| err.into_pyexception(self, Some(source)))?;
        self.run_code_obj(code_obj, scope)
    }
}

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

#[cfg(test)]
mod tests {
    use crate::object::AsObject;
    use rustpython_vm::Interpreter;

    fn interpreter() -> Interpreter {
        Interpreter::without_stdlib(Default::default())
    }

    #[test]
    fn block_expr_return_const() {
        interpreter().enter(|vm| {
            let scope = vm.new_scope_with_builtins();
            let value = vm.unwrap_pyresult(vm.run_block_expr(scope, "1"));
            let value = vm.unwrap_pyresult(value.try_int(vm));
            let value: u32 = vm.unwrap_pyresult(value.try_to_primitive(vm));
            assert_eq!(value, 1);
        })
    }

    #[test]
    fn block_expr_return_nonconst() {
        interpreter().enter(|vm| {
            let scope = vm.new_scope_with_builtins();
            vm.unwrap_pyresult(scope.globals.set_item("x", vm.new_pyobj(3), vm));
            let value = vm.unwrap_pyresult(vm.run_block_expr(scope, "2 + x"));
            let value = vm.unwrap_pyresult(value.try_int(vm));
            let value: u32 = vm.unwrap_pyresult(value.try_to_primitive(vm));
            assert_eq!(value, 5);
        })
    }

    #[test]
    fn block_expr_return_function_def() {
        interpreter().enter(|vm| {
            let scope = vm.new_scope_with_builtins();
            let value =
                vm.unwrap_pyresult(vm.run_block_expr(scope.clone(), "def f():\n    return 7"));
            vm.unwrap_pyresult(scope.globals.set_item("returned", value, vm));
            let value = vm.unwrap_pyresult(vm.run_block_expr(scope, "returned is f"));
            assert!(value.is(&vm.ctx.true_value));
        })
    }

    #[test]
    fn block_expr_return_class_def() {
        interpreter().enter(|vm| {
            let scope = vm.new_scope_with_builtins();
            let value =
                vm.unwrap_pyresult(vm.run_block_expr(scope.clone(), "class C:\n    value = 11"));
            vm.unwrap_pyresult(scope.globals.set_item("returned", value, vm));
            let value = vm.unwrap_pyresult(vm.run_block_expr(scope, "returned is C"));
            assert!(value.is(&vm.ctx.true_value));
        })
    }
}

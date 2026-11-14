use crate::{PyResult, VirtualMachine, compiler, scope::Scope};

pub fn eval(vm: &VirtualMachine, source: &str, scope: Scope, source_path: &str) -> PyResult {
    match vm.compile(source, compiler::Mode::Eval, source_path) {
        Ok(bytecode) => {
            nanocode_trace::debug!("Code object: {bytecode:?}");
            vm.run_code_obj(bytecode, scope)
        }
        Err(err) => Err(err.into_pyexception(vm, Some(source))),
    }
}

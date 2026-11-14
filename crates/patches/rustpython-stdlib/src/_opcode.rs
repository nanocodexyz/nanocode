pub(crate) use _opcode::module_def;

#[pymodule]
mod _opcode {
    use crate::vm::{
        AsObject, PyObjectRef, PyResult, VirtualMachine,
        builtins::{PyInt, PyIntRef},
        bytecode::{AnyOpcode, oparg},
    };

    fn try_from_i32(raw: i32) -> Result<AnyOpcode, ()> {
        u16::try_from(raw)
            .map_err(|_| ())?
            .try_into()
            .map_err(|_| ())
    }

    // prepare specialization
    #[pyattr]
    const ENABLE_SPECIALIZATION: i8 = 1;

    #[pyattr]
    const ENABLE_SPECIALIZATION_FT: i8 = 1;

    #[derive(FromArgs)]
    struct StackEffectArgs {
        #[pyarg(positional)]
        opcode: PyIntRef,
        #[pyarg(positional, optional)]
        oparg: Option<PyObjectRef>,
        #[pyarg(named, optional)]
        jump: Option<PyObjectRef>,
    }

    #[pyfunction]
    fn stack_effect(args: StackEffectArgs, vm: &VirtualMachine) -> PyResult<i32> {
        let oparg = args.oparg.map_or(Ok(0), |v| {
            if !v.fast_isinstance(vm.ctx.types.int_type) {
                return Err(vm.new_type_error(format!(
                    "'{}' object cannot be interpreted as an integer",
                    v.class().name()
                )));
            }
            v.downcast_ref::<PyInt>()
                .ok_or_else(|| {
                    vm.new_type_error(format!(
                        "'{}' object cannot be interpreted as an integer",
                        v.class().name()
                    ))
                })?
                .try_to_primitive::<u32>(vm)
        })?;

        let jump: Option<bool> = match args.jump {
            Some(v) => {
                if vm.is_none(&v) {
                    None
                } else {
                    Some(v.try_to_bool(vm).map_err(|_| {
                        vm.new_value_error("stack_effect: jump must be False, True or None")
                    })?)
                }
            }
            None => None,
        };

        let instr = args
            .opcode
            .try_to_primitive::<u16>(vm)
            .and_then(|v| {
                AnyOpcode::try_from(v)
                    .map_err(|_| vm.new_exception_empty(vm.ctx.exceptions.value_error.to_owned()))
            })
            .map_err(|_| vm.new_value_error("invalid opcode or oparg"))?;

        // Raise ValueError if specialized.
        if instr.real().is_some_and(|op| op.deopt().is_some()) {
            return Err(vm.new_value_error("invalid opcode or oparg"));
        }

        let effect = match jump {
            Some(true) => instr.stack_effect_jump(oparg),
            Some(false) => instr.stack_effect(oparg),
            // jump=None: max of both paths (CPython convention)
            None => instr
                .stack_effect(oparg)
                .max(instr.stack_effect_jump(oparg)),
        };
        Ok(effect)
    }

    #[derive(FromArgs)]
    struct OpcodeArg {
        #[pyarg(any)]
        opcode: i32,
    }

    #[pyfunction]
    fn is_valid(OpcodeArg { opcode }: OpcodeArg) -> bool {
        try_from_i32(opcode).is_ok()
    }

    #[pyfunction]
    fn has_arg(OpcodeArg { opcode }: OpcodeArg) -> bool {
        try_from_i32(opcode).is_ok_and(|op| op.has_arg())
    }

    #[pyfunction]
    fn has_const(OpcodeArg { opcode }: OpcodeArg) -> bool {
        try_from_i32(opcode).is_ok_and(|op| op.has_const())
    }

    #[pyfunction]
    fn has_name(OpcodeArg { opcode }: OpcodeArg) -> bool {
        try_from_i32(opcode).is_ok_and(|op| op.has_name())
    }

    #[pyfunction]
    fn has_jump(OpcodeArg { opcode }: OpcodeArg) -> bool {
        try_from_i32(opcode).is_ok_and(|op| op.has_jump())
    }

    #[pyfunction]
    fn has_free(OpcodeArg { opcode }: OpcodeArg) -> bool {
        try_from_i32(opcode).is_ok_and(|op| op.has_free())
    }

    #[pyfunction]
    fn has_local(OpcodeArg { opcode }: OpcodeArg) -> bool {
        try_from_i32(opcode).is_ok_and(|op| op.has_local())
    }

    #[pyfunction]
    fn has_exc(OpcodeArg { opcode }: OpcodeArg) -> bool {
        try_from_i32(opcode).is_ok_and(|op| op.is_block_push())
    }

    #[pyfunction]
    fn get_intrinsic1_descs(vm: &VirtualMachine) -> Vec<PyObjectRef> {
        oparg::IntrinsicFunction1::iter()
            .map(|x| vm.ctx.new_str(x.desc()).into())
            .collect()
    }

    #[pyfunction]
    fn get_intrinsic2_descs(vm: &VirtualMachine) -> Vec<PyObjectRef> {
        oparg::IntrinsicFunction2::iter()
            .map(|x| vm.ctx.new_str(x.desc()).into())
            .collect()
    }

    #[pyfunction]
    fn get_nb_ops(vm: &VirtualMachine) -> Vec<PyObjectRef> {
        oparg::BinaryOperator::iter()
            .map(|x| {
                vm.ctx
                    .new_tuple(vec![
                        vm.ctx.new_str(x.desc()).into(),
                        vm.ctx.new_str(x.to_string()).into(),
                    ])
                    .into()
            })
            .collect()
    }

    #[pyfunction]
    fn get_special_method_names(vm: &VirtualMachine) -> Vec<PyObjectRef> {
        oparg::SpecialMethod::iter()
            .map(|x| vm.ctx.new_str(x.to_string()).into())
            .collect()
    }

    #[derive(FromArgs)]
    #[allow(dead_code)]
    struct ExecutorArgs {
        #[pyarg(any)]
        code: PyObjectRef,
        #[pyarg(any)]
        offset: i32,
    }

    #[pyfunction]
    fn get_executor(_args: ExecutorArgs, vm: &VirtualMachine) -> PyObjectRef {
        // TODO
        vm.ctx.none()
    }

    #[pyfunction]
    fn get_specialization_stats(vm: &VirtualMachine) -> PyObjectRef {
        vm.ctx.none()
    }
}

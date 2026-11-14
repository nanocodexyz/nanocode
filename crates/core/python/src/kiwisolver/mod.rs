mod kiwi;

pub(crate) use _cext::module_def;

#[rustpython_vm::pymodule(name = "kiwisolver._cext")]
mod _cext {
    use super::kiwi::{self, RelationalOperator, general};
    use rustpython_vm::{
        AsObject, FromArgs, Py, PyObject, PyObjectRef, PyPayload, PyRef, PyResult, Traverse,
        VirtualMachine,
        builtins::{
            PyBaseException, PyBaseExceptionRef, PyFloat, PyInt, PyModule, PyStr, PyTuple,
            PyTupleRef, PyType, try_bigint_to_f64,
        },
        class::PyClassImpl,
        function::{Either, FuncArgs, OptionalArg, PyComparisonValue},
        object::PyAtomicRef,
        protocol::PyNumberMethods,
        pyclass,
        types::{AsNumber, Constructor, PyComparisonOp, Representable},
    };
    use std::{cell::RefCell, collections::BTreeMap};

    const MODULE_DOC: &str = "kiwisolver extension module";
    const EXCEPTIONS_MODULE: &str = "kiwisolver.exceptions";
    const EXCEPTIONS: [&str; 6] = [
        "DuplicateConstraint",
        "UnsatisfiableConstraint",
        "UnknownConstraint",
        "DuplicateEditVariable",
        "UnknownEditVariable",
        "BadRequiredStrength",
    ];
    const BAD_REQUIRED_STRENGTH: &str = "A required strength cannot be used in this context.";
    const FLOAT_DIVISION_BY_ZERO: &str = "float division by zero";

    #[pyattr(name = "__version__")]
    const PY_KIWI_VERSION: &str = "1.5.1";

    #[pyattr(name = "__kiwi_version__")]
    const KIWI_VERSION: &str = kiwi::KIWI_VERSION;

    #[pyattr]
    fn strength(_vm: &VirtualMachine) -> PyRef<Strength> {
        PyRef::new_ref(Strength, Strength::make_static_type(), None)
    }

    pub(crate) fn module_exec(vm: &VirtualMachine, module: &Py<PyModule>) -> PyResult<()> {
        __module_exec(vm, module);
        let exceptions = exceptions(vm)?;
        for name in EXCEPTIONS {
            module.set_attr(name, exceptions.get_attr(name, vm)?, vm)?;
        }
        module.set_attr("__doc__", vm.ctx.new_str(MODULE_DOC), vm)?;
        Ok(())
    }

    fn exceptions(vm: &VirtualMachine) -> PyResult {
        let names = EXCEPTIONS
            .iter()
            .map(|name| vm.ctx.new_str(*name).into())
            .collect();
        vm.import_from(EXCEPTIONS_MODULE, vm.ctx.new_tuple(names), 0)
    }

    fn raise(vm: &VirtualMachine, name: &'static str, value: PyObjectRef) -> PyBaseExceptionRef {
        let raised = exceptions(vm)
            .and_then(|module| module.get_attr(name, vm))
            .and_then(|class| class.call((value,), vm))
            .and_then(|error| {
                error.downcast::<PyBaseException>().map_err(|error| {
                    vm.new_type_error(format!(
                        "exceptions must derive from BaseException, not {}",
                        error.class().slot_name()
                    ))
                })
            });
        match raised {
            Ok(error) | Err(error) => error,
        }
    }

    fn solver_error(
        vm: &VirtualMachine,
        error: kiwi::Exception,
        value: &PyObject,
    ) -> PyBaseExceptionRef {
        let name = match error {
            kiwi::Exception::DuplicateConstraint => "DuplicateConstraint",
            kiwi::Exception::UnsatisfiableConstraint => "UnsatisfiableConstraint",
            kiwi::Exception::UnknownConstraint => "UnknownConstraint",
            kiwi::Exception::DuplicateEditVariable => "DuplicateEditVariable",
            kiwi::Exception::UnknownEditVariable => "UnknownEditVariable",
            kiwi::Exception::BadRequiredStrength => {
                return raise(
                    vm,
                    "BadRequiredStrength",
                    vm.ctx.new_str(BAD_REQUIRED_STRENGTH).into(),
                );
            }
            kiwi::Exception::InternalSolverError(message) => return vm.new_runtime_error(message),
        };
        raise(vm, name, value.to_owned())
    }

    fn type_error(vm: &VirtualMachine, obj: &PyObject, expected: &str) -> PyBaseExceptionRef {
        vm.new_type_error(format!(
            "Expected object of type `{expected}`. Got object of type `{}` instead.",
            obj.class().slot_name()
        ))
    }

    fn text(obj: &PyObject, vm: &VirtualMachine) -> PyResult<String> {
        let text = obj
            .downcast_ref::<PyStr>()
            .ok_or_else(|| type_error(vm, obj, "str"))?;
        Ok(text.try_as_utf8(vm)?.as_str().to_owned())
    }

    fn double(obj: &PyObject, vm: &VirtualMachine) -> PyResult<f64> {
        if let Some(float) = obj.downcast_ref::<PyFloat>() {
            return Ok(float.to_f64());
        }
        if let Some(int) = obj.downcast_ref::<PyInt>() {
            return try_bigint_to_f64(int.as_bigint(), vm);
        }
        Err(type_error(vm, obj, "float, int, or long"))
    }

    fn to_strength(obj: &PyObject, vm: &VirtualMachine) -> PyResult<f64> {
        let Some(name) = obj.downcast_ref::<PyStr>() else {
            return double(obj, vm);
        };
        match name.try_as_utf8(vm)?.as_str() {
            "required" => Ok(kiwi::REQUIRED),
            "strong" => Ok(kiwi::STRONG),
            "medium" => Ok(kiwi::MEDIUM),
            "weak" => Ok(kiwi::WEAK),
            other => Err(vm.new_value_error(format!(
                "string strength must be 'required', 'strong', 'medium', or 'weak', not '{other}'"
            ))),
        }
    }

    fn relational_op(obj: &PyObject, vm: &VirtualMachine) -> PyResult<RelationalOperator> {
        let op = obj
            .downcast_ref::<PyStr>()
            .ok_or_else(|| type_error(vm, obj, "str"))?;
        match op.try_as_utf8(vm)?.as_str() {
            "==" => Ok(RelationalOperator::Eq),
            "<=" => Ok(RelationalOperator::Le),
            ">=" => Ok(RelationalOperator::Ge),
            other => Err(vm.new_value_error(format!(
                "relational operator must be '==', '<=', or '>=', not '{other}'"
            ))),
        }
    }

    const fn op_text(op: RelationalOperator) -> &'static str {
        match op {
            RelationalOperator::Eq => "==",
            RelationalOperator::Le => "<=",
            RelationalOperator::Ge => ">=",
        }
    }

    enum Operand {
        Expression(PyRef<Expression>),
        Term(PyRef<Term>),
        Variable(PyRef<Variable>),
        Number(f64),
    }

    impl Operand {
        fn into_object(self, vm: &VirtualMachine) -> PyObjectRef {
            match self {
                Self::Expression(expression) => expression.into(),
                Self::Term(term) => term.into(),
                Self::Variable(variable) => variable.into(),
                Self::Number(number) => vm.ctx.new_float(number).into(),
            }
        }
    }

    fn operand(obj: &PyObject, vm: &VirtualMachine) -> PyResult<Option<Operand>> {
        let obj = match obj.to_owned().downcast::<Expression>() {
            Ok(expression) => return Ok(Some(Operand::Expression(expression))),
            Err(obj) => obj,
        };
        let obj = match obj.downcast::<Term>() {
            Ok(term) => return Ok(Some(Operand::Term(term))),
            Err(obj) => obj,
        };
        let obj = match obj.downcast::<Variable>() {
            Ok(variable) => return Ok(Some(Operand::Variable(variable))),
            Err(obj) => obj,
        };
        if let Some(float) = obj.downcast_ref::<PyFloat>() {
            return Ok(Some(Operand::Number(float.to_f64())));
        }
        if let Some(int) = obj.downcast_ref::<PyInt>() {
            return try_bigint_to_f64(int.as_bigint(), vm)
                .map(|number| Some(Operand::Number(number)));
        }
        Ok(None)
    }

    fn new_term(variable: PyRef<Variable>, coefficient: f64, vm: &VirtualMachine) -> PyRef<Term> {
        Term {
            variable,
            coefficient,
        }
        .into_ref(&vm.ctx)
    }

    fn new_expression(terms: PyTupleRef, constant: f64, vm: &VirtualMachine) -> PyRef<Expression> {
        Expression { terms, constant }.into_ref(&vm.ctx)
    }

    fn scale(expression: &Expression, factor: f64, vm: &VirtualMachine) -> PyRef<Expression> {
        let terms = expression
            .terms()
            .map(|term| new_term(term.variable.clone(), term.coefficient * factor, vm).into())
            .collect();
        new_expression(vm.ctx.new_tuple(terms), expression.constant * factor, vm)
    }

    fn multiply(first: &Operand, second: &Operand, vm: &VirtualMachine) -> Option<Operand> {
        match (first, second) {
            (Operand::Variable(variable), Operand::Number(number))
            | (Operand::Number(number), Operand::Variable(variable)) => {
                Some(Operand::Term(new_term(variable.clone(), *number, vm)))
            }
            (Operand::Term(term), Operand::Number(number))
            | (Operand::Number(number), Operand::Term(term)) => Some(Operand::Term(new_term(
                term.variable.clone(),
                term.coefficient * number,
                vm,
            ))),
            (Operand::Expression(expression), Operand::Number(number))
            | (Operand::Number(number), Operand::Expression(expression)) => {
                Some(Operand::Expression(scale(expression, *number, vm)))
            }
            _ => None,
        }
    }

    fn divide(first: &Operand, second: &Operand, vm: &VirtualMachine) -> PyResult<Option<Operand>> {
        match (first, second) {
            (Operand::Number(_), _) => Ok(None),
            (_, Operand::Number(number)) => {
                if *number == 0.0 {
                    return Err(vm.new_zero_division_error(FLOAT_DIVISION_BY_ZERO));
                }
                Ok(multiply(first, &Operand::Number(1.0 / number), vm))
            }
            _ => Ok(None),
        }
    }

    fn negate(value: &Operand, vm: &VirtualMachine) -> Option<Operand> {
        multiply(value, &Operand::Number(-1.0), vm)
    }

    fn add(first: &Operand, second: &Operand, vm: &VirtualMachine) -> Option<Operand> {
        let added = match (first, second) {
            (Operand::Expression(a), Operand::Expression(b)) => {
                let terms = a
                    .terms
                    .as_slice()
                    .iter()
                    .chain(b.terms.as_slice())
                    .cloned()
                    .collect();
                new_expression(vm.ctx.new_tuple(terms), a.constant + b.constant, vm)
            }
            (Operand::Expression(a), Operand::Term(b)) => {
                let mut terms = a.terms.as_slice().to_vec();
                terms.push(b.clone().into());
                new_expression(vm.ctx.new_tuple(terms), a.constant, vm)
            }
            (Operand::Expression(_), Operand::Variable(b))
            | (Operand::Term(_), Operand::Variable(b)) => {
                return add(first, &Operand::Term(new_term(b.clone(), 1.0, vm)), vm);
            }
            (Operand::Expression(a), Operand::Number(number)) => {
                new_expression(a.terms.clone(), a.constant + number, vm)
            }
            (Operand::Term(a), Operand::Number(number)) => {
                new_expression(vm.ctx.new_tuple(vec![a.clone().into()]), *number, vm)
            }
            (Operand::Term(_), Operand::Expression(_)) => return add(second, first, vm),
            (Operand::Term(a), Operand::Term(b)) => new_expression(
                vm.ctx.new_tuple(vec![a.clone().into(), b.clone().into()]),
                0.0,
                vm,
            ),
            (Operand::Variable(a), _) => {
                return add(&Operand::Term(new_term(a.clone(), 1.0, vm)), second, vm);
            }
            (Operand::Number(_), Operand::Number(_)) => return None,
            (Operand::Number(_), _) => return add(second, first, vm),
        };
        Some(Operand::Expression(added))
    }

    fn subtract(first: &Operand, second: &Operand, vm: &VirtualMachine) -> Option<Operand> {
        match (first, second) {
            (Operand::Number(_), Operand::Number(_)) => None,
            (_, Operand::Number(number)) => add(first, &Operand::Number(-number), vm),
            _ => add(first, &negate(second, vm)?, vm),
        }
    }

    fn binary(
        a: &PyObject,
        b: &PyObject,
        vm: &VirtualMachine,
        op: fn(&Operand, &Operand, &VirtualMachine) -> PyResult<Option<Operand>>,
    ) -> PyResult {
        let (Some(first), Some(second)) = (operand(a, vm)?, operand(b, vm)?) else {
            return Ok(vm.ctx.not_implemented());
        };
        Ok(op(&first, &second, vm)?
            .map_or_else(|| vm.ctx.not_implemented(), |result| result.into_object(vm)))
    }

    static ARITHMETIC: PyNumberMethods = PyNumberMethods {
        add: Some(|a, b, vm| binary(a, b, vm, |x, y, vm| Ok(add(x, y, vm)))),
        subtract: Some(|a, b, vm| binary(a, b, vm, |x, y, vm| Ok(subtract(x, y, vm)))),
        multiply: Some(|a, b, vm| binary(a, b, vm, |x, y, vm| Ok(multiply(x, y, vm)))),
        true_divide: Some(|a, b, vm| binary(a, b, vm, divide)),
        negative: Some(|number, vm| {
            let negated = operand(number.obj, vm)?.and_then(|value| negate(&value, vm));
            Ok(negated.map_or_else(|| vm.ctx.not_implemented(), |result| result.into_object(vm)))
        }),
        ..PyNumberMethods::NOT_IMPLEMENTED
    };

    fn richcompare(
        zelf: &PyObject,
        other: &PyObject,
        op: PyComparisonOp,
        vm: &VirtualMachine,
    ) -> PyResult<Either<PyObjectRef, PyComparisonValue>> {
        let relational = match op {
            PyComparisonOp::Eq => RelationalOperator::Eq,
            PyComparisonOp::Le => RelationalOperator::Le,
            PyComparisonOp::Ge => RelationalOperator::Ge,
            _ => {
                return Err(vm.new_type_error(format!(
                    "unsupported operand type(s) for {}: '{}' and '{}'",
                    op.operator_token(),
                    zelf.class().slot_name(),
                    other.class().slot_name()
                )));
            }
        };
        let (Some(first), Some(second)) = (operand(zelf, vm)?, operand(other, vm)?) else {
            return Ok(Either::B(PyComparisonValue::NotImplemented));
        };
        let Some(Operand::Expression(expression)) = subtract(&first, &second, vm) else {
            return Ok(Either::B(PyComparisonValue::NotImplemented));
        };
        let constraint = new_constraint(&expression, relational, kiwi::REQUIRED, vm);
        Ok(Either::A(constraint.into_ref(&vm.ctx).into()))
    }

    fn reduce(expression: &Expression, vm: &VirtualMachine) -> PyRef<Expression> {
        let mut coeffs: BTreeMap<u64, (PyRef<Variable>, f64)> = BTreeMap::new();
        for term in expression.terms() {
            coeffs
                .entry(term.variable.variable.identity())
                .or_insert_with(|| (term.variable.clone(), 0.0))
                .1 += term.coefficient;
        }
        let terms = coeffs
            .into_values()
            .map(|(variable, coefficient)| new_term(variable, coefficient, vm).into())
            .collect();
        new_expression(vm.ctx.new_tuple(terms), expression.constant, vm)
    }

    fn new_constraint(
        expression: &Expression,
        op: RelationalOperator,
        strength: f64,
        vm: &VirtualMachine,
    ) -> Constraint {
        let expression = reduce(expression, vm);
        let terms = expression
            .terms()
            .map(|term| kiwi::Term {
                variable: term.variable.variable.clone(),
                coefficient: term.coefficient,
            })
            .collect();
        let converted = kiwi::Expression {
            terms,
            constant: expression.constant,
        };
        Constraint {
            constraint: kiwi::constraint(&converted, op, strength),
            expression,
        }
    }

    fn expression_text(expression: &Expression) -> String {
        let mut out = String::new();
        for term in expression.terms() {
            out.push_str(&general(term.coefficient));
            out.push_str(" * ");
            out.push_str(&term.variable.variable.name());
            out.push_str(" + ");
        }
        out.push_str(&general(expression.constant));
        out
    }

    #[pyclass(no_attr, module = "kiwisolver", name = "Strength")]
    #[derive(Debug, PyPayload)]
    struct Strength;

    #[pyclass]
    impl Strength {
        #[pygetset]
        const fn weak(_zelf: &Py<Self>) -> f64 {
            kiwi::WEAK
        }

        #[pygetset]
        const fn medium(_zelf: &Py<Self>) -> f64 {
            kiwi::MEDIUM
        }

        #[pygetset]
        const fn strong(_zelf: &Py<Self>) -> f64 {
            kiwi::STRONG
        }

        #[pygetset]
        const fn required(_zelf: &Py<Self>) -> f64 {
            kiwi::REQUIRED
        }

        #[pymethod]
        fn create(
            _zelf: &Py<Self>,
            a: PyObjectRef,
            b: PyObjectRef,
            c: PyObjectRef,
            w: OptionalArg<PyObjectRef>,
            vm: &VirtualMachine,
        ) -> PyResult<f64> {
            let a = double(&a, vm)?;
            let b = double(&b, vm)?;
            let c = double(&c, vm)?;
            let w = match w {
                OptionalArg::Present(w) => double(&w, vm)?,
                OptionalArg::Missing => 1.0,
            };
            Ok(kiwi::create(a, b, c, w))
        }
    }

    #[pyattr]
    #[pyclass(module = "kiwisolver", name = "Variable", unhashable = true, traverse)]
    #[derive(Debug, PyPayload)]
    struct Variable {
        #[pytraverse(skip)]
        variable: kiwi::Variable,
        context: PyAtomicRef<Option<PyObject>>,
    }

    #[derive(FromArgs)]
    struct VariableArgs {
        #[pyarg(any, optional)]
        name: OptionalArg<PyObjectRef>,
        #[pyarg(any, optional)]
        context: OptionalArg<PyObjectRef>,
    }

    impl Constructor for Variable {
        type Args = VariableArgs;

        fn py_new(_cls: &Py<PyType>, args: Self::Args, vm: &VirtualMachine) -> PyResult<Self> {
            let name = match args.name {
                OptionalArg::Present(name) => text(&name, vm)?,
                OptionalArg::Missing => String::new(),
            };
            Ok(Self {
                variable: kiwi::variable(name),
                context: args.context.into_option().into(),
            })
        }
    }

    impl Representable for Variable {
        fn repr_str(zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
            Ok(zelf.variable.name())
        }
    }

    impl AsNumber for Variable {
        fn as_number() -> &'static PyNumberMethods {
            &ARITHMETIC
        }
    }

    #[pyclass(with(Constructor, Representable, AsNumber), flags(BASETYPE))]
    impl Variable {
        #[pymethod]
        fn name(zelf: &Py<Self>) -> String {
            zelf.variable.name()
        }

        #[pymethod(name = "setName")]
        fn set_name(zelf: &Py<Self>, name: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
            zelf.variable.set_name(text(&name, vm)?);
            Ok(())
        }

        #[pymethod]
        fn context(zelf: &Py<Self>, vm: &VirtualMachine) -> PyObjectRef {
            zelf.context.load_owned().unwrap_or_else(|| vm.ctx.none())
        }

        #[pymethod(name = "setContext")]
        fn set_context(zelf: &Py<Self>, value: PyObjectRef, vm: &VirtualMachine) {
            zelf.context.swap_to_temporary_refs(Some(value), vm);
        }

        #[pymethod]
        fn value(zelf: &Py<Self>) -> f64 {
            zelf.variable.value()
        }

        #[pyslot]
        fn slot_richcompare(
            zelf: &PyObject,
            other: &PyObject,
            op: PyComparisonOp,
            vm: &VirtualMachine,
        ) -> PyResult<Either<PyObjectRef, PyComparisonValue>> {
            richcompare(zelf, other, op, vm)
        }
    }

    #[pyattr]
    #[pyclass(module = "kiwisolver", name = "Term", unhashable = true, traverse)]
    #[derive(Debug, PyPayload)]
    struct Term {
        variable: PyRef<Variable>,
        #[pytraverse(skip)]
        coefficient: f64,
    }

    #[derive(FromArgs)]
    struct TermArgs {
        #[pyarg(any)]
        variable: PyObjectRef,
        #[pyarg(any, optional)]
        coefficient: OptionalArg<PyObjectRef>,
    }

    impl Constructor for Term {
        type Args = TermArgs;

        fn py_new(_cls: &Py<PyType>, args: Self::Args, vm: &VirtualMachine) -> PyResult<Self> {
            let variable = args
                .variable
                .downcast::<Variable>()
                .map_err(|variable| type_error(vm, &variable, "Variable"))?;
            let coefficient = match args.coefficient {
                OptionalArg::Present(coefficient) => double(&coefficient, vm)?,
                OptionalArg::Missing => 1.0,
            };
            Ok(Self {
                variable,
                coefficient,
            })
        }
    }

    impl Representable for Term {
        fn repr_str(zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
            Ok(format!(
                "{} * {}",
                general(zelf.coefficient),
                zelf.variable.variable.name()
            ))
        }
    }

    impl AsNumber for Term {
        fn as_number() -> &'static PyNumberMethods {
            &ARITHMETIC
        }
    }

    #[pyclass(with(Constructor, Representable, AsNumber), flags(BASETYPE))]
    impl Term {
        #[pymethod]
        fn variable(zelf: &Py<Self>) -> PyRef<Variable> {
            zelf.variable.clone()
        }

        #[pymethod]
        fn coefficient(zelf: &Py<Self>) -> f64 {
            zelf.coefficient
        }

        #[pymethod]
        fn value(zelf: &Py<Self>) -> f64 {
            zelf.coefficient * zelf.variable.variable.value()
        }

        #[pyslot]
        fn slot_richcompare(
            zelf: &PyObject,
            other: &PyObject,
            op: PyComparisonOp,
            vm: &VirtualMachine,
        ) -> PyResult<Either<PyObjectRef, PyComparisonValue>> {
            richcompare(zelf, other, op, vm)
        }
    }

    #[pyattr]
    #[pyclass(
        module = "kiwisolver",
        name = "Expression",
        unhashable = true,
        traverse
    )]
    #[derive(Debug, PyPayload)]
    struct Expression {
        terms: PyTupleRef,
        #[pytraverse(skip)]
        constant: f64,
    }

    impl Expression {
        fn terms(&self) -> impl Iterator<Item = &Py<Term>> {
            self.terms.as_slice().iter().map(|term| {
                term.downcast_ref::<Term>()
                    .expect("an Expression holds only Terms")
            })
        }
    }

    #[derive(FromArgs)]
    struct ExpressionArgs {
        #[pyarg(any)]
        terms: PyObjectRef,
        #[pyarg(any, optional)]
        constant: OptionalArg<PyObjectRef>,
    }

    impl Constructor for Expression {
        type Args = ExpressionArgs;

        fn py_new(_cls: &Py<PyType>, args: Self::Args, vm: &VirtualMachine) -> PyResult<Self> {
            let terms = if args.terms.class().is(vm.ctx.types.tuple_type) {
                args.terms
                    .downcast::<PyTuple>()
                    .map_err(|terms| type_error(vm, &terms, "tuple"))?
            } else {
                vm.ctx.new_tuple(vm.extract_elements_with(&args.terms, Ok)?)
            };
            if let Some(item) = terms
                .as_slice()
                .iter()
                .find(|item| item.downcast_ref::<Term>().is_none())
            {
                return Err(type_error(vm, item, "Term"));
            }
            let constant = match args.constant {
                OptionalArg::Present(constant) => double(&constant, vm)?,
                OptionalArg::Missing => 0.0,
            };
            Ok(Self { terms, constant })
        }
    }

    impl Representable for Expression {
        fn repr_str(zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
            Ok(expression_text(zelf))
        }
    }

    impl AsNumber for Expression {
        fn as_number() -> &'static PyNumberMethods {
            &ARITHMETIC
        }
    }

    #[pyclass(with(Constructor, Representable, AsNumber), flags(BASETYPE))]
    impl Expression {
        #[pymethod(name = "terms")]
        fn terms_tuple(zelf: &Py<Self>) -> PyTupleRef {
            zelf.terms.clone()
        }

        #[pymethod]
        fn constant(zelf: &Py<Self>) -> f64 {
            zelf.constant
        }

        #[pymethod]
        fn value(zelf: &Py<Self>) -> f64 {
            let mut result = zelf.constant;
            for term in zelf.terms() {
                result += term.coefficient * term.variable.variable.value();
            }
            result
        }

        #[pyslot]
        fn slot_richcompare(
            zelf: &PyObject,
            other: &PyObject,
            op: PyComparisonOp,
            vm: &VirtualMachine,
        ) -> PyResult<Either<PyObjectRef, PyComparisonValue>> {
            richcompare(zelf, other, op, vm)
        }
    }

    #[pyattr]
    #[pyclass(module = "kiwisolver", name = "Constraint", traverse)]
    #[derive(Debug, PyPayload)]
    struct Constraint {
        expression: PyRef<Expression>,
        #[pytraverse(skip)]
        constraint: kiwi::Constraint,
    }

    #[derive(FromArgs)]
    struct ConstraintArgs {
        #[pyarg(any)]
        expression: PyObjectRef,
        #[pyarg(any)]
        op: PyObjectRef,
        #[pyarg(any, optional)]
        strength: OptionalArg<PyObjectRef>,
    }

    impl Constructor for Constraint {
        type Args = ConstraintArgs;

        fn py_new(_cls: &Py<PyType>, args: Self::Args, vm: &VirtualMachine) -> PyResult<Self> {
            let expression = args
                .expression
                .downcast::<Expression>()
                .map_err(|expression| type_error(vm, &expression, "Expression"))?;
            let op = relational_op(&args.op, vm)?;
            let strength = match args.strength {
                OptionalArg::Present(strength) => to_strength(&strength, vm)?,
                OptionalArg::Missing => kiwi::REQUIRED,
            };
            Ok(new_constraint(&expression, op, strength, vm))
        }
    }

    impl Representable for Constraint {
        fn repr_str(zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
            let mut out = expression_text(&zelf.expression);
            out.push(' ');
            out.push_str(op_text(zelf.constraint.op()));
            out.push_str(" 0 | strength = ");
            out.push_str(&general(zelf.constraint.strength()));
            if zelf.constraint.violated() {
                out.push_str(" (VIOLATED)");
            }
            Ok(out)
        }
    }

    impl AsNumber for Constraint {
        fn as_number() -> &'static PyNumberMethods {
            static NUMBER: PyNumberMethods = PyNumberMethods {
                or: Some(|a, b, vm| {
                    let (old, value) = match (
                        a.downcast_ref::<Constraint>(),
                        b.downcast_ref::<Constraint>(),
                    ) {
                        (Some(old), _) => (old, b),
                        (None, Some(old)) => (old, a),
                        (None, None) => return Ok(vm.ctx.not_implemented()),
                    };
                    let strength = to_strength(value, vm)?;
                    let constraint = Constraint {
                        expression: old.expression.clone(),
                        constraint: kiwi::restrengthen(&old.constraint, strength),
                    };
                    Ok(constraint.into_ref(&vm.ctx).into())
                }),
                ..PyNumberMethods::NOT_IMPLEMENTED
            };
            &NUMBER
        }
    }

    #[pyclass(with(Constructor, Representable, AsNumber), flags(BASETYPE))]
    impl Constraint {
        #[pymethod]
        fn expression(zelf: &Py<Self>) -> PyRef<Expression> {
            zelf.expression.clone()
        }

        #[pymethod]
        fn op(zelf: &Py<Self>) -> &'static str {
            op_text(zelf.constraint.op())
        }

        #[pymethod]
        fn strength(zelf: &Py<Self>) -> f64 {
            zelf.constraint.strength()
        }

        #[pymethod]
        fn violated(zelf: &Py<Self>) -> bool {
            zelf.constraint.violated()
        }
    }

    #[pyattr]
    #[pyclass(module = "kiwisolver", name = "Solver")]
    #[derive(Debug, Default, PyPayload)]
    struct Solver {
        solver: RefCell<kiwi::Solver>,
    }

    impl Constructor for Solver {
        type Args = FuncArgs;

        fn py_new(_cls: &Py<PyType>, args: Self::Args, vm: &VirtualMachine) -> PyResult<Self> {
            if !args.args.is_empty() || !args.kwargs.is_empty() {
                return Err(vm.new_type_error("Solver.__new__ takes no arguments"));
            }
            Ok(Self::default())
        }
    }

    fn constraint_arg<'a>(
        other: &'a PyObject,
        vm: &VirtualMachine,
    ) -> PyResult<&'a Py<Constraint>> {
        other
            .downcast_ref::<Constraint>()
            .ok_or_else(|| type_error(vm, other, "Constraint"))
    }

    fn variable_arg<'a>(other: &'a PyObject, vm: &VirtualMachine) -> PyResult<&'a Py<Variable>> {
        other
            .downcast_ref::<Variable>()
            .ok_or_else(|| type_error(vm, other, "Variable"))
    }

    #[pyclass(with(Constructor), flags(BASETYPE))]
    impl Solver {
        #[pymethod(name = "addConstraint")]
        fn add_constraint(
            zelf: &Py<Self>,
            other: PyObjectRef,
            vm: &VirtualMachine,
        ) -> PyResult<()> {
            let cn = constraint_arg(&other, vm)?;
            let added = zelf.solver.borrow_mut().add_constraint(&cn.constraint);
            added.map_err(|error| solver_error(vm, error, &other))
        }

        #[pymethod(name = "removeConstraint")]
        fn remove_constraint(
            zelf: &Py<Self>,
            other: PyObjectRef,
            vm: &VirtualMachine,
        ) -> PyResult<()> {
            let cn = constraint_arg(&other, vm)?;
            let removed = zelf.solver.borrow_mut().remove_constraint(&cn.constraint);
            removed.map_err(|error| solver_error(vm, error, &other))
        }

        #[pymethod(name = "hasConstraint")]
        fn has_constraint(
            zelf: &Py<Self>,
            other: PyObjectRef,
            vm: &VirtualMachine,
        ) -> PyResult<bool> {
            let cn = constraint_arg(&other, vm)?;
            Ok(zelf.solver.borrow().has_constraint(&cn.constraint))
        }

        #[pymethod(name = "addEditVariable")]
        fn add_edit_variable(
            zelf: &Py<Self>,
            variable: PyObjectRef,
            strength: PyObjectRef,
            vm: &VirtualMachine,
        ) -> PyResult<()> {
            let var = variable_arg(&variable, vm)?;
            let strength = to_strength(&strength, vm)?;
            let added = zelf
                .solver
                .borrow_mut()
                .add_edit_variable(&var.variable, strength);
            added.map_err(|error| solver_error(vm, error, &variable))
        }

        #[pymethod(name = "removeEditVariable")]
        fn remove_edit_variable(
            zelf: &Py<Self>,
            other: PyObjectRef,
            vm: &VirtualMachine,
        ) -> PyResult<()> {
            let var = variable_arg(&other, vm)?;
            let removed = zelf.solver.borrow_mut().remove_edit_variable(&var.variable);
            removed.map_err(|error| solver_error(vm, error, &other))
        }

        #[pymethod(name = "hasEditVariable")]
        fn has_edit_variable(
            zelf: &Py<Self>,
            other: PyObjectRef,
            vm: &VirtualMachine,
        ) -> PyResult<bool> {
            let var = variable_arg(&other, vm)?;
            Ok(zelf.solver.borrow().has_edit_variable(&var.variable))
        }

        #[pymethod(name = "suggestValue")]
        fn suggest_value(
            zelf: &Py<Self>,
            variable: PyObjectRef,
            value: PyObjectRef,
            vm: &VirtualMachine,
        ) -> PyResult<()> {
            let var = variable_arg(&variable, vm)?;
            let value = double(&value, vm)?;
            let suggested = zelf.solver.borrow_mut().suggest_value(&var.variable, value);
            suggested.map_err(|error| solver_error(vm, error, &variable))
        }

        #[pymethod(name = "updateVariables")]
        fn update_variables(zelf: &Py<Self>) {
            zelf.solver.borrow().update_variables();
        }

        #[pymethod]
        fn reset(zelf: &Py<Self>) {
            zelf.solver.borrow_mut().reset();
        }

        #[pymethod]
        fn dump(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<()> {
            let dumps = vm.ctx.new_str(Self::dumps(zelf));
            let repr = dumps.as_object().repr(vm)?;
            let stdout = vm.sys_module.get_attr("stdout", vm)?;
            vm.call_method(&stdout, "write", (repr,))?;
            Ok(())
        }

        #[pymethod]
        fn dumps(zelf: &Py<Self>) -> String {
            zelf.solver.borrow().dumps()
        }
    }
}

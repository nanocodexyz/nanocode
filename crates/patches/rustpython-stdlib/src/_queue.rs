pub(crate) use _queue::module_def;

#[pymodule]
mod _queue {
    use alloc::collections::VecDeque;
    use core::time::Duration;
    use std::time::Instant;

    use crate::vm::{
        AsObject, Py, PyObjectRef, PyPayload, PyRef, PyResult, VirtualMachine,
        builtins::{PyBaseExceptionRef, PyException, PyGenericAlias, PyStr, PyType, PyTypeRef},
        function::TimeoutSeconds,
        protocol::PyNumberMethods,
        types::{AsNumber, Constructor, Representable},
    };

    type BufInner = VecDeque<PyObjectRef>;

    use crate::common::lock::PyMutex;

    type Buf = PyMutex<BufInner>;

    const INITIAL_RING_BUF_CAPACITY: usize = 8;

    #[pyattr]
    #[pyclass(module = "_queue", name = "Empty", base = PyException)]
    #[repr(transparent)]
    pub(crate) struct PyEmptyError(PyException);

    #[pyclass(flags(HAS_WEAKREF, BASETYPE))]
    impl PyEmptyError {}

    /// ## See Also
    ///
    /// [`empty_error`](https://github.com/python/cpython/blob/v3.14.5/Modules/_queuemodule.c#L347-L355).
    fn empty_error(vm: &VirtualMachine) -> PyBaseExceptionRef {
        vm.new_exception_empty(PyEmptyError::class(&vm.ctx).to_owned())
    }

    #[pyattr]
    #[pyclass(module = "_queue", name = "SimpleQueue")]
    #[derive(Debug, PyPayload)]
    struct PySimpleQueue {
        buf: Buf,
    }

    impl Default for PySimpleQueue {
        fn default() -> Self {
            Self {
                buf: Buf::new(VecDeque::with_capacity(INITIAL_RING_BUF_CAPACITY)),
            }
        }
    }

    impl PySimpleQueue {
        #[expect(unused_variables, reason = "only the semaphore needs the vm")]
        fn push(&self, item: PyObjectRef, vm: &VirtualMachine) {
            self.buf.lock().push_back(item);
        }

        /// Returns a strong reference from the head of the buffer.
        ///
        /// ## See Also
        ///
        /// [`RingBuf_Get`](https://github.com/python/cpython/blob/v3.14.5/Modules/_queuemodule.c#L133-L154).
        fn get_inner(
            buf: &mut BufInner,
        ) -> Option<PyObjectRef> {
            let cap = buf.capacity();

            if buf.len() < (cap / 4) {
                // Items is less than 25% occupied, shrink it by 50%. This allows for
                // growth without immediately needing to resize the underlying items array
                buf.shrink_to(cap / 2)
            }

            buf.pop_front()
        }
    }

    #[derive(FromArgs)]
    struct ItemArg {
        #[pyarg(any)]
        item: PyObjectRef,
    }

    #[derive(FromArgs)]
    struct PutArgs {
        #[pyarg(any)]
        item: PyObjectRef,
        #[expect(
            dead_code,
            reason = "Intentional. Provide compatibility with the Queue class"
        )]
        #[pyarg(any, default = true)]
        block: bool,
        #[expect(
            dead_code,
            reason = "Intentional. Provide compatibility with the Queue class"
        )]
        #[pyarg(any, optional)]
        timeout: Option<PyObjectRef>,
    }

    #[derive(FromArgs)]
    struct GetArgs {
        #[pyarg(any, default = true)]
        block: bool,
        #[pyarg(any, optional)]
        timeout: Option<TimeoutSeconds>,
    }

    #[pyclass(
        with(Constructor, Representable),
        flags(BASETYPE, HAS_WEAKREF, IMMUTABLETYPE)
    )]
    impl PySimpleQueue {
        #[pymethod]
        fn empty(zelf: &Py<Self>) -> bool {
            zelf.buf.lock().is_empty()
        }

        #[pymethod]
        fn qsize(zelf: &Py<Self>) -> usize {
            zelf.buf.lock().len()
        }

        #[pymethod]
        fn put(zelf: &Py<Self>, args: PutArgs, vm: &VirtualMachine) {
            let PutArgs { item, .. } = args;
            zelf.push(item, vm);
        }

        #[pymethod]
        fn put_nowait(zelf: &Py<Self>, ItemArg { item }: ItemArg, vm: &VirtualMachine) {
            zelf.push(item, vm);
        }

        #[pymethod]
        fn get(zelf: &Py<Self>, args: GetArgs, vm: &VirtualMachine) -> PyResult<PyObjectRef> {
            let GetArgs { block, timeout } = args;

            // Non-blocking: just try once
            if !block {
                return Self::get_inner(&mut zelf.buf.lock()).ok_or_else(|| empty_error(vm));
            }

            #[expect(
                    unused_variables,
                    reason = "We are still validating the 'timeout' arg even if we don't have threading"
                )]
            let deadline = match timeout.map(|v| v.to_secs_f64()) {
                Some(v) if v < 0.0 => {
                    return Err(vm.new_value_error("'timeout' must be a non-negative number"));
                }
                Some(v) => Some(Instant::now() + Duration::from_secs_f64(v)),
                None => None,
            };

            Self::get_inner(&mut zelf.buf.lock()).ok_or_else(|| empty_error(vm))
        }

        #[pymethod]
        fn get_nowait(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyObjectRef> {
            Self::get_inner(&mut zelf.buf.lock()).ok_or_else(|| empty_error(vm))
        }

        #[pyclassmethod]
        fn __class_getitem__(
            cls: PyTypeRef,
            object: PyObjectRef,
            vm: &VirtualMachine,
        ) -> PyResult<PyGenericAlias> {
            PyGenericAlias::from_args(cls, object, vm)
        }
    }

    impl Constructor for PySimpleQueue {
        type Args = ();

        fn py_new(_cls: &Py<PyType>, _args: Self::Args, _vm: &VirtualMachine) -> PyResult<Self> {
            Ok(Self::default())
        }
    }

    impl AsNumber for PySimpleQueue {
        fn as_number() -> &'static PyNumberMethods {
            static AS_NUMBER: PyNumberMethods = PyNumberMethods {
                boolean: Some(|number, _vm| {
                    let zelf = number.obj.downcast_ref::<PySimpleQueue>().unwrap();
                    Ok(!zelf.buf.lock().is_empty())
                }),
                ..PyNumberMethods::NOT_IMPLEMENTED
            };
            &AS_NUMBER
        }
    }

    impl Representable for PySimpleQueue {
        fn repr(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyRef<PyStr>> {
            Ok(vm.ctx.new_str(format!(
                "<{} at {:#x}>",
                Self::class(&vm.ctx).slot_name(),
                zelf.get_id()
            )))
        }

        fn repr_str(_zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
            unreachable!("repr() is overridden directly")
        }
    }
}

// type aliases instead of new-types because you can't do `fn method(self: PyRc<Self>)` with a
// newtype; requires the arbitrary_self_types unstable feature

pub type PyRc<T> = alloc::rc::Rc::<T>;

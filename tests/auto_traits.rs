//! Compile-time checks: the public handles keep their auto traits.

use std::panic::{RefUnwindSafe, UnwindSafe};

use dldota::{Callback, Context, Library, Symbol};

fn assert_traits<T: Send + Sync + UnwindSafe + RefUnwindSafe>() {}

#[test]
fn public_types_are_thread_and_unwind_safe() {
    assert_traits::<Context>();
    assert_traits::<Library<'static>>();
    assert_traits::<Symbol<'static, unsafe extern "C" fn()>>();
    assert_traits::<Callback<'static, unsafe extern "C" fn()>>();
}

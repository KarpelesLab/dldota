use std::ffi::{CStr, CString, c_void};
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::PathBuf;
use std::ptr::NonNull;

use crate::error::{Error, Result};
use crate::heap::HeapUsage;
use crate::loader;

/// `dlopen` mode flags. The values are identical in glibc and musl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenFlags(pub i32);

impl OpenFlags {
    pub const LAZY: Self = Self(0x1);
    pub const NOW: Self = Self(0x2);
    pub const NOLOAD: Self = Self(0x4);
    pub const LOCAL: Self = Self(0x0);
    pub const GLOBAL: Self = Self(0x100);
    pub const NODELETE: Self = Self(0x1000);
}

impl std::ops::BitOr for OpenFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Default)]
pub struct ContextBuilder {
    interpreter: Option<PathBuf>,
    env: Option<Vec<CString>>,
}

impl ContextBuilder {
    /// Use this dynamic loader instead of auto-detecting the system one.
    pub fn interpreter(mut self, path: impl Into<PathBuf>) -> Self {
        self.interpreter = Some(path.into());
        self
    }

    /// Environment seen by the foreign loader and libc (`KEY=VALUE` entries).
    /// Defaults to the current process environment. `LD_LIBRARY_PATH`,
    /// `LD_PRELOAD` and `GLIBC_TUNABLES` set here only affect this context.
    pub fn env(mut self, env: impl IntoIterator<Item = CString>) -> Self {
        self.env = Some(env.into_iter().collect());
        self
    }

    /// Map and boot the system dynamic loader.
    pub fn build(self) -> Result<Context> {
        let interpreter = match self.interpreter {
            Some(path) => path,
            None => loader::find_interpreter()?,
        };
        let _ = (interpreter, self.env);
        Err(Error::Unimplemented("loader bootstrap"))
    }
}

/// A booted foreign dynamic loader, with its own libc and heap.
///
/// Call [`Context::unload`] to tear it down. Dropping a `Context` without
/// unloading leaks the foreign world rather than risk unmapping code that a
/// foreign thread is still running.
pub struct Context {
    _private: (),
}

impl Context {
    /// Boot the auto-detected system loader with the current environment.
    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    pub fn builder() -> ContextBuilder {
        ContextBuilder::default()
    }

    /// `dlopen` through the foreign loader.
    pub fn open(&self, name: &CStr, flags: OpenFlags) -> Result<Library<'_>> {
        let handle = self.open_raw(Some(name), flags)?;
        Ok(Library { ctx: self, handle })
    }

    /// Tear down the foreign world: run library destructors and exit handlers,
    /// then unmap every object, the loader, and the foreign heap.
    ///
    /// Fails with [`Error::Busy`] while threads created by foreign code are
    /// still alive; the context is handed back intact.
    pub fn unload(self) -> std::result::Result<(), (Self, Error)> {
        Err((self, Error::Unimplemented("unload")))
    }

    /// Current usage of the foreign heap. Every allocation made by foreign
    /// code, including libc-internal ones, goes through the context's allocator.
    pub fn heap_usage(&self) -> HeapUsage {
        HeapUsage::default()
    }

    /// The foreign global scope (`dlopen(NULL)`): the system libc and anything
    /// opened with [`OpenFlags::GLOBAL`]. Use it to reach libc functions
    /// without knowing the libc's file name, which differs between glibc and musl.
    pub fn global(&self) -> Result<Library<'_>> {
        let handle = self.open_raw(None, OpenFlags::LAZY)?;
        Ok(Library { ctx: self, handle })
    }

    /// Wrap a host function so foreign code can call it, e.g. as a `qsort`
    /// comparator. The returned pointer switches the thread pointer back to the
    /// host for the duration of the call; it is valid while the
    /// [`Callback`] lives.
    ///
    /// # Safety
    /// `F` must be an `extern "C" fn` pointer type.
    pub unsafe fn callback<F: Copy>(&self, f: F) -> Result<Callback<'_, F>> {
        const { assert!(size_of::<F>() == size_of::<*mut c_void>()) };
        // SAFETY: size checked above; F is a function pointer per the contract.
        let raw = unsafe { std::mem::transmute_copy::<F, NonNull<c_void>>(&f) };
        let thunk = self.callback_raw(raw)?;
        // SAFETY: the thunk has the same ABI and signature as `f`.
        let thunk_fn = unsafe { std::mem::transmute_copy::<NonNull<c_void>, F>(&thunk) };
        Ok(Callback {
            ctx: self,
            thunk,
            thunk_fn,
        })
    }

    /// Create a callback thunk for the raw host function `f`.
    pub fn callback_raw(&self, f: NonNull<c_void>) -> Result<NonNull<c_void>> {
        let _ = f;
        Err(Error::Unimplemented("callbacks"))
    }

    /// Release a thunk created by [`Context::callback_raw`].
    pub fn free_callback_raw(&self, thunk: NonNull<c_void>) {
        let _ = thunk;
    }

    /// `dlopen` returning the raw foreign handle; `None` opens the global scope.
    pub fn open_raw(&self, name: Option<&CStr>, flags: OpenFlags) -> Result<NonNull<c_void>> {
        let _ = (name, flags);
        Err(Error::Unimplemented("dlopen"))
    }

    /// `dlsym` on a raw foreign handle. The returned pointer is a trampoline
    /// that switches the thread pointer around the call.
    pub fn symbol_raw(&self, handle: NonNull<c_void>, name: &CStr) -> Result<NonNull<c_void>> {
        let _ = (handle, name);
        Err(Error::Unimplemented("dlsym"))
    }

    /// `dlclose` on a raw foreign handle.
    pub fn close_raw(&self, handle: NonNull<c_void>) -> Result<()> {
        let _ = handle;
        Err(Error::Unimplemented("dlclose"))
    }
}

/// A library opened through a [`Context`]; `dlclose`d on drop.
pub struct Library<'ctx> {
    ctx: &'ctx Context,
    handle: NonNull<c_void>,
}

// SAFETY: a dlopen handle is an opaque identifier; the foreign dlsym/dlclose
// it is passed to are thread-safe.
unsafe impl Send for Library<'_> {}
unsafe impl Sync for Library<'_> {}

impl<'ctx> Library<'ctx> {
    /// Look up `name` and reinterpret it as `T`, typically an
    /// `unsafe extern "C" fn` type.
    ///
    /// # Safety
    /// `T` must be pointer-sized and match the symbol's real type.
    pub unsafe fn symbol<T: Copy>(&self, name: &CStr) -> Result<Symbol<'_, T>> {
        const { assert!(size_of::<T>() == size_of::<*mut c_void>()) };
        let ptr = self.ctx.symbol_raw(self.handle, name)?;
        // SAFETY: size checked above; type correctness is the caller's contract.
        let value = unsafe { std::mem::transmute_copy::<NonNull<c_void>, T>(&ptr) };
        Ok(Symbol {
            value,
            _lib: PhantomData,
        })
    }

    pub fn as_raw(&self) -> NonNull<c_void> {
        self.handle
    }
}

impl Drop for Library<'_> {
    fn drop(&mut self) {
        let _ = self.ctx.close_raw(self.handle);
    }
}

/// A symbol borrowed from a [`Library`]; cannot outlive it.
pub struct Symbol<'lib, T> {
    value: T,
    _lib: PhantomData<&'lib ()>,
}

impl<T> Deref for Symbol<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

/// A host function made callable from foreign code by [`Context::callback`].
/// Derefs to the thunk, to be passed to foreign code.
pub struct Callback<'ctx, F> {
    ctx: &'ctx Context,
    thunk: NonNull<c_void>,
    thunk_fn: F,
}

impl<F> Deref for Callback<'_, F> {
    type Target = F;
    fn deref(&self) -> &F {
        &self.thunk_fn
    }
}

impl<F> Drop for Callback<'_, F> {
    fn drop(&mut self) {
        self.ctx.free_callback_raw(self.thunk);
    }
}

// SAFETY: the thunk is immutable code owned by the context.
unsafe impl<F: Send> Send for Callback<'_, F> {}
unsafe impl<F: Sync> Sync for Callback<'_, F> {}

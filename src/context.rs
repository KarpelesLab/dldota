use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::ops::Deref;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::gate::Gate;
use crate::heap::{self, HeapUsage};
use crate::loader;
use crate::runtime::{self, Runtime};
use crate::thunk::Thunks;

type DlOpen = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void;
type DlSym = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type DlClose = unsafe extern "C" fn(*mut c_void) -> c_int;
type DlError = unsafe extern "C" fn() -> *const c_char;

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
        let env = self.env.unwrap_or_else(|| {
            std::env::vars_os()
                .filter_map(|(k, v)| {
                    let mut kv = k.as_bytes().to_vec();
                    kv.push(b'=');
                    kv.extend_from_slice(v.as_bytes());
                    CString::new(kv).ok()
                })
                .collect()
        });
        // Leaked: thunks and foreign code hold pointers to the gate and the
        // heap, and the foreign world outlives a dropped Context (see `unload`).
        let gate: &'static Gate = Box::leak(Box::new(Gate::new()));
        let rt: &'static Runtime = Box::leak(Box::new(Runtime::new(loader::page_size())));
        let mut thunks = Thunks::default();
        let heap_data = &rt.heap as *const _ as usize;
        let rt_data = rt as *const Runtime as usize;
        let defs = heap::exports()
            .into_iter()
            .map(|(name, func)| (name, func, heap_data))
            .chain(
                runtime::exports()
                    .into_iter()
                    .map(|(name, func)| (name, func, rt_data)),
            )
            .map(|(name, func, data)| Ok((name, thunks.bound(func, data)?)))
            .collect::<Result<Vec<_>>>()?;

        let booted = loader::boot(&interpreter, env, &defs)?;
        gate.boot_tp
            .store(booted.foreign_tp, std::sync::atomic::Ordering::Release);
        rt.heap.set_errno_location(booted.errno_location);

        let mut wrap = |target: usize| thunks.outbound(gate, target);
        // SAFETY: each thunk forwards to the function of that signature.
        let inner = unsafe {
            Inner {
                dlopen: std::mem::transmute::<usize, DlOpen>(wrap(booted.dlopen)?),
                dlsym: std::mem::transmute::<usize, DlSym>(wrap(booted.dlsym)?),
                dlclose: std::mem::transmute::<usize, DlClose>(wrap(booted.dlclose)?),
                dlerror: std::mem::transmute::<usize, DlError>(wrap(booted.dlerror)?),
                gate,
                rt,
                thunks: Mutex::new(thunks),
                fflush: None,
                booted,
            }
        };
        let mut inner = inner;
        inner.resolve_libc();
        Ok(Context {
            inner: Box::leak(Box::new(inner)),
        })
    }
}

/// A booted foreign dynamic loader, with its own libc and heap.
///
/// Call [`Context::unload`] to tear it down. Dropping a `Context` without
/// unloading leaks the foreign world rather than risk unmapping code that a
/// foreign thread is still running.
pub struct Context {
    inner: &'static Inner,
}

pub(crate) struct Inner {
    pub(crate) gate: &'static Gate,
    pub(crate) rt: &'static Runtime,
    pub(crate) thunks: Mutex<Thunks>,
    dlopen: DlOpen,
    dlsym: DlSym,
    dlclose: DlClose,
    dlerror: DlError,
    /// The foreign `fflush`, through an outbound thunk.
    pub(crate) fflush: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    pub(crate) booted: loader::Booted,
}

impl Inner {
    /// Look up libc functions the runtime needs: the real `pthread_create`
    /// (called raw, from foreign code) and `fflush` (for unload).
    fn resolve_libc(&mut self) {
        // glibc < 2.34 keeps pthread_create in libpthread; musl answers both.
        for lib in [c"libc.so.6", c"libpthread.so.0"] {
            // SAFETY: valid C strings; NOLOAD only finds loaded objects.
            let handle =
                unsafe { (self.dlopen)(lib.as_ptr(), OpenFlags::LAZY.0 | OpenFlags::NOLOAD.0) };
            if handle.is_null() {
                continue;
            }
            // SAFETY: a live handle and valid C strings.
            unsafe {
                let create = (self.dlsym)(handle, c"pthread_create".as_ptr());
                if !create.is_null() {
                    self.rt.set_pthread_create(create as usize);
                }
                let fflush = (self.dlsym)(handle, c"fflush".as_ptr());
                if self.fflush.is_none() && !fflush.is_null() {
                    let mut thunks = self.thunks.lock().unwrap_or_else(|e| e.into_inner());
                    if let Ok(t) = thunks.outbound(self.gate, fflush as usize) {
                        self.fflush = Some(std::mem::transmute::<
                            usize,
                            unsafe extern "C" fn(*mut c_void) -> c_int,
                        >(t));
                    }
                }
                (self.dlclose)(handle);
            }
        }
    }

    /// The foreign `dlerror()` text. Call with the gate held, right after the
    /// failing call, so no other thread's error replaces it.
    fn last_error(&self, fallback: &str) -> Error {
        // SAFETY: dlerror returns NULL or a NUL-terminated string.
        let msg = unsafe { (self.dlerror)() };
        if msg.is_null() {
            Error::Dl(fallback.to_owned())
        } else {
            // SAFETY: checked non-NULL above.
            Error::Dl(
                unsafe { CStr::from_ptr(msg) }
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }
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
        // SAFETY: consuming `self` ends every Library/Symbol/Callback borrow.
        if let Err(e) = unsafe { self.inner.unload() } {
            return Err((self, e));
        }
        // SAFETY: these were leaked in `build` and nothing refers to them now.
        unsafe {
            let inner = Box::from_raw(self.inner as *const Inner as *mut Inner);
            drop(Box::from_raw(inner.gate as *const Gate as *mut Gate));
            drop(Box::from_raw(inner.rt as *const Runtime as *mut Runtime));
        }
        Ok(())
    }

    /// Current usage of the foreign heap. Every allocation made by foreign
    /// code, including libc-internal ones, goes through the context's allocator.
    pub fn heap_usage(&self) -> HeapUsage {
        self.inner.rt.heap.usage()
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
        let mut thunks = self.inner.thunks.lock().unwrap_or_else(|e| e.into_inner());
        let thunk = thunks.inbound(self.inner.gate, f.as_ptr() as usize)?;
        Ok(NonNull::new(thunk as *mut c_void).expect("thunk address"))
    }

    /// Release a thunk created by [`Context::callback_raw`].
    pub fn free_callback_raw(&self, thunk: NonNull<c_void>) {
        let _ = thunk;
    }

    /// `dlopen` returning the raw foreign handle; `None` opens the global scope.
    pub fn open_raw(&self, name: Option<&CStr>, flags: OpenFlags) -> Result<NonNull<c_void>> {
        let name_ptr = name.map_or(std::ptr::null(), CStr::as_ptr);
        self.inner.gate.with(|| {
            // SAFETY: NULL or a valid C string.
            let handle = unsafe { (self.inner.dlopen)(name_ptr, flags.0) };
            NonNull::new(handle).ok_or_else(|| self.inner.last_error("dlopen failed"))
        })
    }

    /// `dlsym` on a raw foreign handle. The returned pointer is a trampoline
    /// that switches the thread pointer around the call.
    pub fn symbol_raw(&self, handle: NonNull<c_void>, name: &CStr) -> Result<NonNull<c_void>> {
        let target = self.inner.gate.with(|| {
            // SAFETY: a handle from dlopen and a valid C string.
            let sym = unsafe { (self.inner.dlsym)(handle.as_ptr(), name.as_ptr()) };
            NonNull::new(sym).ok_or_else(|| self.inner.last_error("dlsym failed"))
        })?;
        let mut thunks = self.inner.thunks.lock().unwrap_or_else(|e| e.into_inner());
        let thunk = thunks.outbound(self.inner.gate, target.as_ptr() as usize)?;
        Ok(NonNull::new(thunk as *mut c_void).expect("thunk address"))
    }

    /// `dlclose` on a raw foreign handle.
    pub fn close_raw(&self, handle: NonNull<c_void>) -> Result<()> {
        self.inner.gate.with(|| {
            // SAFETY: a handle from dlopen, closed once.
            match unsafe { (self.inner.dlclose)(handle.as_ptr()) } {
                0 => Ok(()),
                _ => Err(self.inner.last_error("dlclose failed")),
            }
        })
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

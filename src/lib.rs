// Secure memory allocation with OS-level memory locking.

use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use pyo3::exceptions::{PyBufferError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use zeroize::Zeroize;

#[cfg(unix)]
use libc::{mlock, munlock};

#[cfg(windows)]
use windows_sys::Win32::System::Memory::{VirtualLock, VirtualUnlock};

const LEAKED_VIEW_MSG: &str =
    "a view of the secure buffer is still alive after the access scope ended \
     (a slice or copy of the memoryview was kept)";

// SecureMemory
#[pyclass]
pub struct SecureMemory {
    /// Fixed-size backing store.  It must never be resized: exported buffer
    /// views hold a raw pointer into it.
    data: Vec<u8>,
    locked: bool,
    /// Number of live buffer-protocol exports over `data`.
    exports: AtomicUsize,
}

#[pymethods]
impl SecureMemory {
    /// Allocate `size` zero-initialised bytes and attempt to lock them into RAM.
    #[new]
    fn new(size: usize) -> PyResult<Self> {
        let data = vec![0u8; size];
        let locked = Self::lock_memory(&data);
        Ok(SecureMemory {
            data,
            locked,
            exports: AtomicUsize::new(0),
        })
    }

    // write / read
    fn write(&mut self, offset: usize, data: &[u8]) -> PyResult<()> {
        let end = offset
            .checked_add(data.len())
            .ok_or_else(|| PyErr::new::<pyo3::exceptions::PyValueError, _>("Offset overflow"))?;
        if end > self.data.len() {
            return Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(
                "Write would exceed buffer size",
            ));
        }
        self.data[offset..end].copy_from_slice(data);
        Ok(())
    }

    /// Copy `length` bytes at `offset` into a Python `bytes` object.
    ///
    /// The copy lives outside the zeroize boundary.  Prefer `with_buffer()`.
    fn read<'py>(
        &self,
        py: Python<'py>,
        offset: usize,
        length: usize,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let end = offset
            .checked_add(length)
            .ok_or_else(|| PyErr::new::<pyo3::exceptions::PyValueError, _>("Offset overflow"))?;
        if end > self.data.len() {
            return Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(
                "Read would exceed buffer size",
            ));
        }
        Ok(PyBytes::new(py, &self.data[offset..end]))
    }

    /// Zeroize the buffer in place.  The length is unchanged.
    fn clear(&mut self) {
        // Zeroize the *slice*.  `Vec<u8>::zeroize()` also truncates the Vec
        // to length 0, which would shrink `len()`, make later writes fail and
        // leave existing buffer views describing memory the Vec no longer
        // reports.
        self.data.as_mut_slice().zeroize();
    }

    /// Returns `True` if the OS memory-locking call succeeded.
    /// On Windows this calls `VirtualLock`; on Unix, `mlock`.
    fn is_locked(&self) -> bool {
        self.locked
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    /// Copy the contents into a Python `bytes` object.
    ///
    /// Note: the returned object is an unavoidable copy outside our
    /// zeroize boundary.  Minimise the lifetime of the returned value, or
    /// better, use `with_buffer()` / `view()` / `memoryview(sm)`, which do
    /// not copy.
    fn as_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.data)
    }

    /// Call `callback(view)` with a read-only `memoryview` over the buffer
    /// (no copy) and return its result.
    ///
    /// The view is always released afterwards, even if the callback raises.
    /// If the callback kept a derived view alive (e.g. `mv[1:4]`), a
    /// `BufferError` is raised.  Copying out of the view (`bytes(mv)`) is the
    /// caller's explicit choice and is not prevented.
    fn with_buffer<'py>(
        slf: &Bound<'py, Self>,
        callback: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let view = Self::memoryview(slf)?;
        let result = callback.call1((&view,));
        // Release even if the callback failed, so the view cannot be reused.
        let released = view.call_method0("release");
        drop(view);
        let value = result?;
        released?;
        if slf.borrow().exports() != 0 {
            return Err(PyBufferError::new_err(LEAKED_VIEW_MSG));
        }
        Ok(value)
    }

    /// Return a context manager yielding a read-only `memoryview` over the
    /// buffer (no copy), used as `with sm.view() as mv: ...`.  The view is
    /// released on exit, and a `BufferError` is raised if a derived view
    /// (e.g. a slice) was kept alive past the `with` block.
    fn view(slf: &Bound<'_, Self>) -> SecureView {
        SecureView {
            owner: slf.clone().unbind(),
            view: None,
        }
    }

    /// Buffer protocol: expose the buffer read-only, without copying.
    ///
    /// The exported view holds a strong reference to this object, so the
    /// memory cannot be freed (or zeroized by `Drop`) while a view exists.
    /// `clear()` and `write()` remain visible through live views.
    ///
    /// # Safety
    ///
    /// Called by CPython with a valid `view` pointer (or null, which is
    /// rejected).  `data` is never resized, so the exported pointer stays
    /// valid for as long as the exporter is alive.
    unsafe fn __getbuffer__(
        slf: Bound<'_, Self>,
        view: *mut ffi::Py_buffer,
        flags: c_int,
    ) -> PyResult<()> {
        if view.is_null() {
            return Err(PyBufferError::new_err("View is null"));
        }
        if (flags & ffi::PyBUF_WRITABLE) == ffi::PyBUF_WRITABLE {
            return Err(PyBufferError::new_err(
                "SecureMemory exposes read-only buffers; use write() to modify it",
            ));
        }

        let (buf, len) = {
            let this = slf.borrow();
            this.exports.fetch_add(1, Ordering::AcqRel);
            (this.data.as_ptr() as *mut c_void, this.data.len())
        };

        unsafe {
            (*view).obj = slf.into_ptr();
            (*view).buf = buf;
            (*view).len = len as isize;
            (*view).readonly = 1;
            (*view).itemsize = 1;
            (*view).format = if (flags & ffi::PyBUF_FORMAT) == ffi::PyBUF_FORMAT {
                c"B".as_ptr() as *mut _
            } else {
                ptr::null_mut()
            };
            (*view).ndim = 1;
            (*view).shape = if (flags & ffi::PyBUF_ND) == ffi::PyBUF_ND {
                &mut (*view).len
            } else {
                ptr::null_mut()
            };
            (*view).strides = if (flags & ffi::PyBUF_STRIDES) == ffi::PyBUF_STRIDES {
                &mut (*view).itemsize
            } else {
                ptr::null_mut()
            };
            (*view).suboffsets = ptr::null_mut();
            (*view).internal = ptr::null_mut();
        }
        Ok(())
    }

    /// # Safety
    ///
    /// Called by CPython once for every successful `__getbuffer__`.
    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.exports.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Context manager returned by `SecureMemory.view()`.
#[pyclass]
pub struct SecureView {
    owner: Py<SecureMemory>,
    view: Option<Py<PyAny>>,
}

#[pymethods]
impl SecureView {
    fn __enter__<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        if self.view.is_some() {
            return Err(PyValueError::new_err("view is already active"));
        }
        let mv = SecureMemory::memoryview(self.owner.bind(py))?;
        self.view = Some(mv.clone().unbind());
        Ok(mv)
    }

    #[pyo3(signature = (exc_type=None, _exc_value=None, _traceback=None))]
    fn __exit__(
        &mut self,
        py: Python<'_>,
        exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let released = match self.view.take() {
            Some(v) => v.bind(py).call_method0("release").map(|_| ()),
            None => Ok(()),
        };
        // Do not mask an exception that is already propagating.
        if exc_type.is_none() {
            released?;
            if self.owner.borrow(py).exports() != 0 {
                return Err(PyBufferError::new_err(LEAKED_VIEW_MSG));
            }
        }
        Ok(false)
    }
}

impl SecureMemory {
    /// Read-only `memoryview` over this object (goes through `__getbuffer__`).
    fn memoryview<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        slf.py()
            .import("builtins")?
            .getattr("memoryview")?
            .call1((slf,))
    }

    /// Number of live buffer-protocol exports.
    fn exports(&self) -> usize {
        self.exports.load(Ordering::Acquire)
    }
}

// Platform-specific locking
impl SecureMemory {
    #[cfg(unix)]
    fn lock_memory(data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        unsafe { mlock(data.as_ptr() as *const libc::c_void, data.len()) == 0 }
    }

    #[cfg(unix)]
    fn unlock_memory(data: &[u8]) {
        if data.is_empty() {
            return;
        }
        unsafe {
            munlock(data.as_ptr() as *const libc::c_void, data.len());
        }
    }

    #[cfg(windows)]
    fn lock_memory(data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        unsafe { VirtualLock(data.as_ptr() as *mut _, data.len()) != 0 }
    }

    #[cfg(windows)]
    fn unlock_memory(data: &[u8]) {
        if data.is_empty() {
            return;
        }
        unsafe {
            VirtualUnlock(data.as_ptr() as *mut _, data.len());
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn lock_memory(_data: &[u8]) -> bool {
        false // Unsupported platform — heap allocation only
    }

    #[cfg(not(any(unix, windows)))]
    fn unlock_memory(_data: &[u8]) {}
}

// Drop: zeroize then unlock

impl Drop for SecureMemory {
    fn drop(&mut self) {
        // Zeroize the slice, not the Vec: `Vec::zeroize` truncates to length 0,
        // after which `unlock_memory` would see an empty slice and silently
        // skip `munlock`.
        self.data.as_mut_slice().zeroize();
        if self.locked {
            Self::unlock_memory(&self.data);
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin<'py>(py: Python<'py>, name: &str) -> Bound<'py, PyAny> {
        py.import("builtins").unwrap().getattr(name).unwrap()
    }

    fn tobytes(view: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
        view.call_method0("tobytes")?.extract::<Vec<u8>>()
    }

    #[test]
    fn test_create_and_len() {
        let mem = SecureMemory::new(32).unwrap();
        assert_eq!(mem.len(), 32);
    }

    #[test]
    fn test_write_and_clear() {
        let mut mem = SecureMemory::new(32).unwrap();
        mem.write(0, b"secret data").unwrap();
        mem.clear();
        assert_eq!(mem.data.len(), 32, "clear() must not shrink the buffer");
        assert!(mem.data.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_clear_keeps_buffer_usable() {
        let mut mem = SecureMemory::new(8).unwrap();
        mem.write(0, b"abcdefgh").unwrap();
        mem.clear();
        assert_eq!(mem.len(), 8);
        assert!(mem.write(0, b"new data").is_ok());
        assert_eq!(&mem.data[..], b"new data");
    }

    #[test]
    fn test_write_bounds() {
        let mut mem = SecureMemory::new(4).unwrap();
        assert!(mem.write(0, b"abcd").is_ok());
        assert!(mem.write(1, b"abcd").is_err()); // would overflow
    }

    #[test]
    fn test_read_bounds() {
        let mem = SecureMemory::new(4).unwrap();
        Python::attach(|py| {
            assert!(mem.read(py, 0, 4).is_ok());
            assert!(mem.read(py, 1, 4).is_err());
        });
    }

    #[test]
    fn test_buffer_is_zero_copy_and_readonly() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(8).unwrap()).unwrap();
            obj.borrow_mut().write(0, b"secret!!").unwrap();

            let mv = SecureMemory::memoryview(&obj).unwrap();
            assert_eq!(mv.len().unwrap(), 8);
            assert!(mv.getattr("readonly").unwrap().extract::<bool>().unwrap());
            assert!(mv.set_item(0, 1).is_err(), "view must be read-only");
            assert_eq!(obj.borrow().exports(), 1);

            // Zero-copy: clear() is visible through the live view.
            obj.borrow_mut().clear();
            assert_eq!(tobytes(&mv).unwrap(), vec![0u8; 8]);

            mv.call_method0("release").unwrap();
            drop(mv);
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_writable_buffer_request_rejected() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4).unwrap()).unwrap();
            // Ask for a writable buffer directly.  Python-level helpers such as
            // ctypes' from_buffer() request a read-only view and then check
            // `readonly`, so they raise TypeError instead of reaching this path.
            let mut view = std::mem::MaybeUninit::<ffi::Py_buffer>::uninit();
            let rc = unsafe {
                ffi::PyObject_GetBuffer(obj.as_ptr(), view.as_mut_ptr(), ffi::PyBUF_WRITABLE)
            };
            assert_eq!(rc, -1, "a writable buffer request must fail");
            let err = PyErr::take(py).expect("an exception must be set");
            assert!(err.is_instance_of::<PyBufferError>(py));
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_with_buffer_returns_result_and_releases() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4).unwrap()).unwrap();
            obj.borrow_mut().write(0, b"abcd").unwrap();

            // `bytes(mv)` is an explicit, caller-chosen copy.
            let out = SecureMemory::with_buffer(&obj, &builtin(py, "bytes")).unwrap();
            assert_eq!(out.extract::<Vec<u8>>().unwrap(), b"abcd".to_vec());
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_with_buffer_releases_when_callback_raises() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4).unwrap()).unwrap();
            // `getattr(memoryview)` raises TypeError: it needs at least two arguments.
            let err = SecureMemory::with_buffer(&obj, &builtin(py, "getattr")).unwrap_err();
            assert!(err.is_instance_of::<pyo3::exceptions::PyTypeError>(py));
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_with_buffer_detects_retained_view() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4).unwrap()).unwrap();
            // `memoryview(mv)` returns a derived view that survives mv.release().
            let err = SecureMemory::with_buffer(&obj, &builtin(py, "memoryview")).unwrap_err();
            assert!(err.is_instance_of::<PyBufferError>(py));
            // The derived view was dropped with the error path.
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_view_context_manager_releases_on_exit() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4).unwrap()).unwrap();
            obj.borrow_mut().write(0, b"abcd").unwrap();

            let guard = Bound::new(py, SecureMemory::view(&obj)).unwrap();
            let mv = guard.call_method0("__enter__").unwrap();
            assert_eq!(tobytes(&mv).unwrap(), b"abcd".to_vec());
            assert_eq!(obj.borrow().exports(), 1);

            guard
                .call_method1("__exit__", (py.None(), py.None(), py.None()))
                .unwrap();
            assert!(tobytes(&mv).is_err(), "view must be unusable after exit");
            assert_eq!(obj.borrow().exports(), 0);
        });
    }
}

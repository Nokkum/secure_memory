// Secure memory allocation with OS-level memory locking.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ffi::{c_int, c_void};
use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyBufferError, PyMemoryError, PyOSError, PyValueError};
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

const WRITE_WHILE_VIEWED_MSG: &str =
    "cannot write to the secure buffer while a view of it is alive \
     (release the memoryview, and any slice or copy of it, first)";

const BUSY_MSG: &str = "secure buffer is being modified; try again";

// SecureMemory
#[pymodule]
fn secure_memory(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<SecureMemory>()?;
    m.add_class::<SecureView>()?;
    Ok(())
}

#[pyclass]
pub struct SecureMemory {
    /// Fixed-size backing store on pages of its own.  It must never be
    /// resized or moved: exported buffer views hold a raw pointer into it.
    data: PageBuf,
    locked: bool,
    /// Number of live buffer-protocol exports over `data`.  `write()` is
    /// refused while this is non-zero, so a live view never observes a
    /// partially written buffer.
    exports: AtomicUsize,
}

#[pymethods]
impl SecureMemory {
    /// Allocate `size` zero-initialised bytes and attempt to lock them into RAM.
    ///
    /// The bytes live on whole pages that belong to this object alone (the
    /// allocation is rounded up to a multiple of the page size), so locking
    /// and unlocking never touches memory owned by anything else.  Raises
    /// `MemoryError` if the allocation fails.
    #[new]
    #[pyo3(signature = (size, *, require_lock = false))]
    fn new(size: usize, require_lock: bool) -> PyResult<Self> {
        let data = PageBuf::new_zeroed(size).ok_or_else(|| {
            PyMemoryError::new_err(format!("cannot allocate a secure buffer of {size} bytes"))
        })?;
        let (ptr, region_len) = data.region();
        let locked = if region_len == 0 {
            // Nothing to lock (zero-length buffer), so nothing can leak.
            false
        } else {
            match Self::lock_memory(ptr, region_len) {
                Ok(()) => true,
                // `data` was never written to, so dropping it here leaks nothing.
                Err(e) if require_lock => {
                    return Err(PyOSError::new_err(format!(
                        "could not lock {region_len} bytes of secure memory into RAM: {e}"
                    )));
                }
                Err(_) => false,
            }
        };
        if region_len != 0 {
            Self::exclude_from_core_dumps(ptr, region_len);
        }
        Ok(SecureMemory {
            data,
            locked,
            exports: AtomicUsize::new(0),
        })
    }

    // write / read

    /// Copy `data` into the buffer at `offset`.
    ///
    /// Raises `BufferError` if any view of the buffer is still alive
    /// (`memoryview(sm)`, `with_buffer()`, `view()`, or a slice kept from
    /// one).  Views point straight at the backing store, and a consumer that
    /// releases the GIL (hashing, socket/file I/O) could otherwise read while
    /// this copy is in flight and see a torn mix of old and new bytes.
    /// Release the views first, then write.
    fn write(&mut self, py: Python<'_>, offset: usize, data: PyBuffer<u8>) -> PyResult<()> {
        let dest = self.write_range(offset, data.item_count())?;
        data.copy_to_slice(py, dest)
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
        // Zeroize in place; the length must not change, or later writes would
        // fail and existing buffer views would describe memory the object no
        // longer reports.
        self.data.as_mut_slice().zeroize();
    }

    /// Returns `True` if the OS memory-locking call succeeded.
    /// On Windows this calls `VirtualLock`; on Unix, `mlock`.  A zero-length
    /// buffer has nothing to lock and reports `False`.
    fn is_locked(&self) -> bool {
        self.locked
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    /// Supports the built-in `len(sm)`.
    fn __len__(&self) -> usize {
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
        if Self::exports_checked(slf)? != 0 {
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
    ///
    /// While any view is alive the contents are stable: `write()` raises
    /// `BufferError`.  `clear()` is the one exception (see its docs) and is
    /// visible through live views.
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
            // `try_borrow`, not `borrow`: if `write()`/`clear()` holds the
            // object right now, fail with a `BufferError` instead of
            // panicking (cannot happen while the GIL is held; see the
            // threading note at the top of this file).
            let this = slf
                .try_borrow()
                .map_err(|_| PyBufferError::new_err(BUSY_MSG))?;
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
            if SecureMemory::exports_checked(self.owner.bind(py))? != 0 {
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

    /// Check that `len` bytes can be written at `offset` and return that part
    /// of the buffer.  Refuses while any view is alive (see `write()`).
    fn write_range(&mut self, offset: usize, len: usize) -> PyResult<&mut [u8]> {
        if self.exports() != 0 {
            return Err(PyBufferError::new_err(WRITE_WHILE_VIEWED_MSG));
        }
        let end = offset
            .checked_add(len)
            .ok_or_else(|| PyValueError::new_err("Offset overflow"))?;
        if end > self.data.len() {
            return Err(PyValueError::new_err("Write would exceed buffer size"));
        }
        Ok(&mut self.data.as_mut_slice()[offset..end])
    }

    /// Copy a Rust slice into the buffer; the same checks as `write()`.
    #[cfg(test)]
    fn write_slice(&mut self, offset: usize, src: &[u8]) -> PyResult<()> {
        self.write_range(offset, src.len())?.copy_from_slice(src);
        Ok(())
    }

    /// Like `exports()`, but starting from the Python object, and failing with
    /// a `BufferError` (never a panic) if the object is currently borrowed
    /// mutably.
    fn exports_checked(slf: &Bound<'_, Self>) -> PyResult<usize> {
        let this = slf
            .try_borrow()
            .map_err(|_| PyBufferError::new_err(BUSY_MSG))?;
        Ok(this.exports())
    }
}

// Page-aligned backing store

/// Size of a memory page on the host (queried once).
fn page_size() -> usize {
    static PAGE_SIZE: OnceLock<usize> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        let size = query_page_size();
        // `Layout` needs a power-of-two alignment; fall back to the common
        // page size if the OS reports something odd.
        if size.is_power_of_two() {
            size
        } else {
            4096
        }
    })
}

#[cfg(unix)]
fn query_page_size() -> usize {
    // SAFETY: `sysconf` has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        0
    }
}

#[cfg(windows)]
fn query_page_size() -> usize {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    // SAFETY: `SYSTEM_INFO` is plain old data, and `GetSystemInfo` fills it in.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    info.dwPageSize as usize
}

#[cfg(not(any(unix, windows)))]
fn query_page_size() -> usize {
    4096
}

/// A fixed-size, zero-initialised byte buffer that starts on a page boundary
/// and spans a whole number of pages.
///
/// `mlock`/`munlock` (and `VirtualLock`/`VirtualUnlock`) work on whole pages,
/// and page locks do not nest: a plain `Vec<u8>` shares its pages with other
/// heap data and with other `SecureMemory` buffers, so unlocking one buffer on
/// drop could silently unlock another one's pages.  Owning the pages outright
/// avoids that.
///
/// The memory is released on drop but not wiped: `SecureMemory`'s own `Drop`
/// does the zeroize and unlock first.
struct PageBuf {
    ptr: NonNull<u8>,
    /// Usable length requested by the caller.
    len: usize,
    /// Layout of the allocation (a multiple of the page size), or `None` for a
    /// zero-length buffer, which allocates nothing.
    layout: Option<Layout>,
}

// SAFETY: `PageBuf` uniquely owns its allocation, exactly like `Box<[u8]>`.
unsafe impl Send for PageBuf {}
unsafe impl Sync for PageBuf {}

impl PageBuf {
    /// Allocate `len` zeroed bytes, or `None` if the size is unrepresentable
    /// or the allocator is out of memory.
    fn new_zeroed(len: usize) -> Option<Self> {
        if len == 0 {
            return Some(PageBuf {
                ptr: NonNull::dangling(),
                len: 0,
                layout: None,
            });
        }
        let page = page_size();
        // Round up to a whole number of pages (`page` is a power of two).
        let size = len.checked_add(page - 1)? & !(page - 1);
        let layout = Layout::from_size_align(size, page).ok()?;
        // SAFETY: `layout` has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })?;
        Some(PageBuf {
            ptr,
            len,
            layout: Some(layout),
        })
    }

    /// The whole-page region owned by this buffer, as `(start, byte length)`;
    /// the length is zero for a zero-length buffer.  This, not `len`, is what
    /// gets locked and unlocked.
    fn region(&self) -> (*mut u8, usize) {
        (self.ptr.as_ptr(), self.layout.map_or(0, |l| l.size()))
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` initialised bytes (or `len == 0`
        // and `ptr` is a non-null, aligned dangling pointer).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as in `as_slice`, and `&mut self` is exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Deref for PageBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl DerefMut for PageBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl Drop for PageBuf {
    fn drop(&mut self) {
        if let Some(layout) = self.layout {
            // SAFETY: allocated in `new_zeroed` with exactly this layout.
            unsafe { dealloc(self.ptr.as_ptr(), layout) };
        }
    }
}

// Platform-specific locking
impl SecureMemory {
    // The `ptr`/`len` arguments below must describe a live, page-aligned
    // region owned by the caller (see `PageBuf::region`).

    #[cfg(unix)]
    fn lock_memory(ptr: *mut u8, len: usize) -> std::io::Result<()> {
        // SAFETY: see above.
        if unsafe { mlock(ptr as *const libc::c_void, len) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(unix)]
    fn unlock_memory(ptr: *mut u8, len: usize) {
        // SAFETY: see above.
        unsafe {
            munlock(ptr as *const libc::c_void, len);
        }
    }

    #[cfg(windows)]
    fn lock_memory(ptr: *mut u8, len: usize) -> std::io::Result<()> {
        // SAFETY: see above.
        if unsafe { VirtualLock(ptr as *mut _, len) } != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(windows)]
    fn unlock_memory(ptr: *mut u8, len: usize) {
        // SAFETY: see above.
        unsafe {
            VirtualUnlock(ptr as *mut _, len);
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn lock_memory(_ptr: *mut u8, _len: usize) -> std::io::Result<()> {
        // Unsupported platform: heap allocation only.
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "memory locking is not supported on this platform",
        ))
    }

    #[cfg(not(any(unix, windows)))]
    fn unlock_memory(_ptr: *mut u8, _len: usize) {}

    /// Best effort: keep the region out of core dumps.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn exclude_from_core_dumps(ptr: *mut u8, len: usize) {
        // SAFETY: see above.  Failure only means the pages stay dumpable.
        unsafe {
            libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_DONTDUMP);
        }
    }

    /// Undo `exclude_from_core_dumps` before the region is freed.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn include_in_core_dumps(ptr: *mut u8, len: usize) {
        // SAFETY: see above.
        unsafe {
            libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_DODUMP);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn exclude_from_core_dumps(_ptr: *mut u8, _len: usize) {}

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn include_in_core_dumps(_ptr: *mut u8, _len: usize) {}
}

// Drop: zeroize then unlock

impl Drop for SecureMemory {
    fn drop(&mut self) {
        // Zeroize first, then unlock: the pages must never be swappable while
        // they still hold the secret.
        self.data.as_mut_slice().zeroize();
        let (ptr, len) = self.data.region();
        if len != 0 {
            if self.locked {
                Self::unlock_memory(ptr, len);
            }
            // The pages go back to the allocator next, so undo the core-dump
            // exclusion rather than leave unrelated heap data out of dumps.
            Self::include_in_core_dumps(ptr, len);
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
        let mem = SecureMemory::new(32, false).unwrap();
        assert_eq!(mem.len(), 32);
        assert_eq!(mem.__len__(), 32);
    }

    #[test]
    fn test_zero_size_buffer() {
        let mem = SecureMemory::new(0, false).unwrap();
        assert_eq!(mem.len(), 0);
        assert!(!mem.is_locked());
    }

    #[test]
    fn test_buffer_starts_on_page_boundary_and_keeps_requested_length() {
        let mut mem = SecureMemory::new(5, false).unwrap();
        assert_eq!(mem.data.as_ptr() as usize % page_size(), 0);
        // The rounded-up padding is not reachable through the API.
        assert_eq!(mem.len(), 5);
        assert!(mem.write_slice(0, b"abcde").is_ok());
        assert!(mem.write_slice(1, b"abcde").is_err());
    }

    /// `VmFlags` line of the `/proc/self/smaps` mapping containing `addr`.
    #[cfg(target_os = "linux")]
    fn vm_flags_of(addr: usize) -> Option<String> {
        let smaps = std::fs::read_to_string("/proc/self/smaps").ok()?;
        let mut in_mapping = false;
        for line in smaps.lines() {
            // Mapping header: "start-end perms offset dev inode [path]".
            if let Some((start, rest)) = line.split_once('-') {
                if let Some((end, _)) = rest.split_once(' ') {
                    if let (Ok(s), Ok(e)) = (
                        usize::from_str_radix(start, 16),
                        usize::from_str_radix(end, 16),
                    ) {
                        in_mapping = s <= addr && addr < e;
                        continue;
                    }
                }
            }
            if in_mapping {
                if let Some(flags) = line.strip_prefix("VmFlags:") {
                    return Some(flags.to_string());
                }
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_buffer_is_excluded_from_core_dumps() {
        let mem = SecureMemory::new(16, false).unwrap();
        let addr = mem.data.as_ptr() as usize;
        let Some(flags) = vm_flags_of(addr) else {
            eprintln!("skipped: no VmFlags in /proc/self/smaps on this system");
            return;
        };
        // "dd" = do not include area into core dumps.
        assert!(
            flags.split_whitespace().any(|f| f == "dd"),
            "VmFlags:{flags}"
        );
    }

    #[test]
    fn test_require_lock_on_empty_buffer_succeeds() {
        assert!(SecureMemory::new(0, true).is_ok());
    }

    #[test]
    fn test_require_lock_agrees_with_is_locked() {
        // Whether the OS allows locking depends on the environment
        // (RLIMIT_MEMLOCK, CAP_IPC_LOCK), so accept both outcomes, but they
        // must be coherent with each other.
        Python::attach(|_py| match SecureMemory::new(64, true) {
            Ok(mem) => assert!(mem.is_locked()),
            Err(_) => assert!(!SecureMemory::new(64, false).unwrap().is_locked()),
        });
    }

    #[test]
    fn test_absurd_size_raises_memory_error() {
        Python::attach(|py| {
            let Err(err) = SecureMemory::new(usize::MAX, false) else {
                panic!("allocating usize::MAX bytes must fail");
            };
            assert!(err.is_instance_of::<PyMemoryError>(py));
        });
    }

    #[test]
    fn test_write_and_clear() {
        let mut mem = SecureMemory::new(32, false).unwrap();
        mem.write_slice(0, b"secret data").unwrap();
        mem.clear();
        assert_eq!(mem.data.len(), 32, "clear() must not shrink the buffer");
        assert!(mem.data.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_clear_keeps_buffer_usable() {
        let mut mem = SecureMemory::new(8, false).unwrap();
        mem.write_slice(0, b"abcdefgh").unwrap();
        mem.clear();
        assert_eq!(mem.len(), 8);
        assert!(mem.write_slice(0, b"new data").is_ok());
        assert_eq!(&mem.data[..], b"new data");
    }

    #[test]
    fn test_write_accepts_any_bytes_like_object() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(8, false).unwrap()).unwrap();

            // A mutable source, which the caller can wipe afterwards.
            let src = pyo3::types::PyByteArray::new(py, b"secret!!");
            let buf = PyBuffer::<u8>::get(src.as_any()).unwrap();
            obj.borrow_mut().write(py, 0, buf).unwrap();
            assert_eq!(&obj.borrow().data[..], b"secret!!");

            // `bytes` keeps working.
            let src = PyBytes::new(py, b"abcd");
            let buf = PyBuffer::<u8>::get(src.as_any()).unwrap();
            obj.borrow_mut().write(py, 2, buf).unwrap();
            assert_eq!(&obj.borrow().data[..], b"seabcd!!");

            // Bounds are still enforced on this path, and a failed write
            // leaves the buffer alone.
            let src = PyBytes::new(py, b"abcd");
            let buf = PyBuffer::<u8>::get(src.as_any()).unwrap();
            let err = obj.borrow_mut().write(py, 6, buf).unwrap_err();
            assert!(err.is_instance_of::<PyValueError>(py));
            assert_eq!(&obj.borrow().data[..], b"seabcd!!");
        });
    }

    #[test]
    fn test_write_bounds() {
        let mut mem = SecureMemory::new(4, false).unwrap();
        assert!(mem.write_slice(0, b"abcd").is_ok());
        assert!(mem.write_slice(1, b"abcd").is_err()); // would overflow
    }

    #[test]
    fn test_read_bounds() {
        let mem = SecureMemory::new(4, false).unwrap();
        Python::attach(|py| {
            assert!(mem.read(py, 0, 4).is_ok());
            assert!(mem.read(py, 1, 4).is_err());
        });
    }

    #[test]
    fn test_buffer_is_zero_copy_and_readonly() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(8, false).unwrap()).unwrap();
            obj.borrow_mut().write_slice(0, b"secret!!").unwrap();

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
    fn test_write_refused_while_view_alive() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(8, false).unwrap()).unwrap();
            obj.borrow_mut().write_slice(0, b"original").unwrap();

            let mv = SecureMemory::memoryview(&obj).unwrap();
            assert_eq!(obj.borrow().exports(), 1);

            let err = obj.borrow_mut().write_slice(0, b"tampered").unwrap_err();
            assert!(err.is_instance_of::<PyBufferError>(py));
            assert_eq!(
                tobytes(&mv).unwrap(),
                b"original".to_vec(),
                "a refused write must not touch the buffer"
            );

            mv.call_method0("release").unwrap();
            drop(mv);
            assert_eq!(obj.borrow().exports(), 0);

            // Once the view is gone, writing works again.
            obj.borrow_mut().write_slice(0, b"tampered").unwrap();
            assert_eq!(&obj.borrow().data[..], b"tampered");
        });
    }

    #[test]
    fn test_retained_view_keeps_write_refused() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
            obj.borrow_mut().write_slice(0, b"abcd").unwrap();

            let mv = SecureMemory::memoryview(&obj).unwrap();
            // A derived view survives `mv.release()`.
            let derived = builtin(py, "memoryview").call1((&mv,)).unwrap();
            mv.call_method0("release").unwrap();
            assert_eq!(obj.borrow().exports(), 1);
            assert!(obj.borrow_mut().write_slice(0, b"wxyz").is_err());
            assert_eq!(&obj.borrow().data[..], b"abcd");

            drop(derived);
            assert_eq!(obj.borrow().exports(), 0);
            assert!(obj.borrow_mut().write_slice(0, b"wxyz").is_ok());
        });
    }

    #[test]
    fn test_clear_still_works_with_live_view() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
            obj.borrow_mut().write_slice(0, b"abcd").unwrap();

            let mv = SecureMemory::memoryview(&obj).unwrap();
            // A wipe is never blocked by a (possibly leaked) view.
            obj.borrow_mut().clear();
            assert_eq!(tobytes(&mv).unwrap(), vec![0u8; 4]);

            mv.call_method0("release").unwrap();
            drop(mv);
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_writable_buffer_request_rejected() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
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
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
            obj.borrow_mut().write_slice(0, b"abcd").unwrap();

            // `bytes(mv)` is an explicit, caller-chosen copy.
            let out = SecureMemory::with_buffer(&obj, &builtin(py, "bytes")).unwrap();
            assert_eq!(out.extract::<Vec<u8>>().unwrap(), b"abcd".to_vec());
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_with_buffer_releases_when_callback_raises() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
            // `getattr(memoryview)` raises TypeError: it needs at least two arguments.
            let err = SecureMemory::with_buffer(&obj, &builtin(py, "getattr")).unwrap_err();
            assert!(err.is_instance_of::<pyo3::exceptions::PyTypeError>(py));
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_with_buffer_detects_retained_view() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
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
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
            obj.borrow_mut().write_slice(0, b"abcd").unwrap();

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

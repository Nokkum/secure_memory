// Secure memory allocation with OS-level memory locking.

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

#[pymodule]
fn secure_memory(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<SecureMemory>()?;
    m.add_class::<SecureView>()?;
    Ok(())
}

#[pyclass]
pub struct SecureMemory {
    data: PageBuf,
    locked: bool,
    exports: AtomicUsize,
}

#[pymethods]
impl SecureMemory {
    #[new]
    #[pyo3(signature = (size, *, require_lock = false))]
    fn new(size: usize, require_lock: bool) -> PyResult<Self> {
        if size == 0 && require_lock {
            return Err(PyValueError::new_err("cannot lock an empty buffer"));
        }
        let data = PageBuf::new_zeroed(size).ok_or_else(|| {
            PyMemoryError::new_err(format!("cannot allocate a secure buffer of {size} bytes"))
        })?;
        let (ptr, region_len) = data.region();
        let locked = if region_len == 0 {
            false
        } else {
            match Self::lock_memory(ptr, region_len) {
                Ok(()) => true,
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

    fn write(&mut self, py: Python<'_>, offset: usize, data: PyBuffer<u8>) -> PyResult<()> {
        let dest = self.write_range(offset, data.item_count())?;
        data.copy_to_slice(py, dest)
    }

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

    fn clear(&mut self) {
        self.data.as_mut_slice().zeroize();
    }

    fn is_locked(&self) -> bool {
        self.locked
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn __len__(&self) -> usize {
        self.data.len()
    }

    fn __bool__(&self) -> bool {
        true
    }

    fn as_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.data)
    }

    fn with_buffer<'py>(
        slf: &Bound<'py, Self>,
        callback: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let view = Self::memoryview(slf)?;
        let result = callback.call1((&view,));
        let released = view.call_method0("release");
        drop(view);
        let value = result?;
        released?;
        if Self::exports_checked(slf)? != 0 {
            return Err(PyBufferError::new_err(LEAKED_VIEW_MSG));
        }
        Ok(value)
    }

    fn view(slf: &Bound<'_, Self>) -> SecureView {
        SecureView {
            owner: slf.clone().unbind(),
            view: None,
        }
    }

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

    unsafe fn __releasebuffer__(&self, _view: *mut ffi::Py_buffer) {
        self.exports.fetch_sub(1, Ordering::AcqRel);
    }
}

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
    fn memoryview<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        slf.py()
            .import("builtins")?
            .getattr("memoryview")?
            .call1((slf,))
    }

    fn exports(&self) -> usize {
        self.exports.load(Ordering::Acquire)
    }

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

    #[cfg(test)]
    fn write_slice(&mut self, offset: usize, src: &[u8]) -> PyResult<()> {
        self.write_range(offset, src.len())?.copy_from_slice(src);
        Ok(())
    }

    fn exports_checked(slf: &Bound<'_, Self>) -> PyResult<usize> {
        let this = slf
            .try_borrow()
            .map_err(|_| PyBufferError::new_err(BUSY_MSG))?;
        Ok(this.exports())
    }
}

fn page_size() -> usize {
    static PAGE_SIZE: OnceLock<usize> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        let size = query_page_size();
        if size.is_power_of_two() {
            size
        } else {
            4096
        }
    })
}

#[cfg(unix)]
fn query_page_size() -> usize {
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
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    info.dwPageSize as usize
}

#[cfg(not(any(unix, windows)))]
fn query_page_size() -> usize {
    4096
}

#[cfg(unix)]
fn os_alloc(size: usize) -> *mut u8 {
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        ptr::null_mut()
    } else {
        p.cast::<u8>()
    }
}

#[cfg(unix)]
unsafe fn os_free(ptr: *mut u8, size: usize) {
    libc::munmap(ptr.cast::<libc::c_void>(), size);
}

#[cfg(windows)]
fn os_alloc(size: usize) -> *mut u8 {
    use windows_sys::Win32::System::Memory as winmem;
    let p = unsafe {
        winmem::VirtualAlloc(
            ptr::null_mut(),
            size,
            winmem::MEM_COMMIT | winmem::MEM_RESERVE,
            winmem::PAGE_READWRITE,
        )
    };
    p.cast::<u8>()
}

#[cfg(windows)]
unsafe fn os_free(ptr: *mut u8, _size: usize) {
    use windows_sys::Win32::System::Memory::{VirtualFree, MEM_RELEASE};
    VirtualFree(ptr.cast(), 0, MEM_RELEASE);
}

#[cfg(not(any(unix, windows)))]
fn os_alloc(size: usize) -> *mut u8 {
    match std::alloc::Layout::from_size_align(size, page_size()) {
        Ok(layout) => unsafe { std::alloc::alloc_zeroed(layout) },
        Err(_) => ptr::null_mut(),
    }
}

#[cfg(not(any(unix, windows)))]
unsafe fn os_free(ptr: *mut u8, size: usize) {
    if let Ok(layout) = std::alloc::Layout::from_size_align(size, page_size()) {
        std::alloc::dealloc(ptr, layout);
    }
}

struct PageBuf {
    ptr: NonNull<u8>,
    len: usize,
    region_len: usize,
}

unsafe impl Send for PageBuf {}
unsafe impl Sync for PageBuf {}

impl PageBuf {
    fn new_zeroed(len: usize) -> Option<Self> {
        if len == 0 {
            return Some(PageBuf {
                ptr: NonNull::dangling(),
                len: 0,
                region_len: 0,
            });
        }
        let page = page_size();
        let region_len = len.checked_add(page - 1)? & !(page - 1);
        if region_len > isize::MAX as usize {
            return None;
        }
        let ptr = NonNull::new(os_alloc(region_len))?;
        Some(PageBuf {
            ptr,
            len,
            region_len,
        })
    }

    fn region(&self) -> (*mut u8, usize) {
        (self.ptr.as_ptr(), self.region_len)
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
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
        if self.region_len != 0 {
            unsafe { os_free(self.ptr.as_ptr(), self.region_len) };
        }
    }
}

impl SecureMemory {
    #[cfg(unix)]
    fn lock_memory(ptr: *mut u8, len: usize) -> std::io::Result<()> {
        if unsafe { mlock(ptr as *const libc::c_void, len) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(unix)]
    fn unlock_memory(ptr: *mut u8, len: usize) {
        unsafe {
            munlock(ptr as *const libc::c_void, len);
        }
    }

    #[cfg(windows)]
    fn lock_memory(ptr: *mut u8, len: usize) -> std::io::Result<()> {
        if unsafe { VirtualLock(ptr as *mut _, len) } != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(windows)]
    fn unlock_memory(ptr: *mut u8, len: usize) {
        unsafe {
            VirtualUnlock(ptr as *mut _, len);
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn lock_memory(_ptr: *mut u8, _len: usize) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "memory locking is not supported on this platform",
        ))
    }

    #[cfg(not(any(unix, windows)))]
    fn unlock_memory(_ptr: *mut u8, _len: usize) {}

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn exclude_from_core_dumps(ptr: *mut u8, len: usize) {
        unsafe {
            libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_DONTDUMP);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn include_in_core_dumps(ptr: *mut u8, len: usize) {
        unsafe {
            libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_DODUMP);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn exclude_from_core_dumps(_ptr: *mut u8, _len: usize) {}

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn include_in_core_dumps(_ptr: *mut u8, _len: usize) {}
}

impl Drop for SecureMemory {
    fn drop(&mut self) {
        self.data.as_mut_slice().zeroize();
        let (ptr, len) = self.data.region();
        if len != 0 {
            if self.locked {
                Self::unlock_memory(ptr, len);
            }
            Self::include_in_core_dumps(ptr, len);
        }
    }
}

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
        assert!(mem.__bool__());
        assert!(SecureMemory::new(0, false).unwrap().__bool__());
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
        let (_, region_len) = mem.data.region();
        assert!(region_len >= 5);
        assert_eq!(region_len % page_size(), 0);
        assert_eq!(mem.len(), 5);
        assert!(mem.write_slice(0, b"abcde").is_ok());
        assert!(mem.write_slice(1, b"abcde").is_err());
    }

    #[cfg(target_os = "linux")]
    fn vm_flags_of(addr: usize) -> Option<String> {
        let smaps = std::fs::read_to_string("/proc/self/smaps").ok()?;
        let mut in_mapping = false;
        for line in smaps.lines() {
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
        assert!(
            flags.split_whitespace().any(|f| f == "dd"),
            "VmFlags:{flags}"
        );
    }

    #[test]
    fn test_require_lock_on_empty_buffer_is_value_error() {
        Python::attach(|py| {
            let Err(err) = SecureMemory::new(0, true) else {
                panic!("an empty buffer cannot satisfy require_lock");
            };
            assert!(err.is_instance_of::<PyValueError>(py));
        });
        assert!(SecureMemory::new(0, false).is_ok());
    }

    #[test]
    fn test_require_lock_agrees_with_is_locked() {
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

            let src = pyo3::types::PyByteArray::new(py, b"secret!!");
            let buf = PyBuffer::<u8>::get(src.as_any()).unwrap();
            obj.borrow_mut().write(py, 0, buf).unwrap();
            assert_eq!(&obj.borrow().data[..], b"secret!!");

            let src = PyBytes::new(py, b"abcd");
            let buf = PyBuffer::<u8>::get(src.as_any()).unwrap();
            obj.borrow_mut().write(py, 2, buf).unwrap();
            assert_eq!(&obj.borrow().data[..], b"seabcd!!");

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
        assert!(mem.write_slice(1, b"abcd").is_err());
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

            let out = SecureMemory::with_buffer(&obj, &builtin(py, "bytes")).unwrap();
            assert_eq!(out.extract::<Vec<u8>>().unwrap(), b"abcd".to_vec());
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_with_buffer_releases_when_callback_raises() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
            let err = SecureMemory::with_buffer(&obj, &builtin(py, "getattr")).unwrap_err();
            assert!(err.is_instance_of::<pyo3::exceptions::PyTypeError>(py));
            assert_eq!(obj.borrow().exports(), 0);
        });
    }

    #[test]
    fn test_with_buffer_detects_retained_view() {
        Python::attach(|py| {
            let obj = Bound::new(py, SecureMemory::new(4, false).unwrap()).unwrap();
            let err = SecureMemory::with_buffer(&obj, &builtin(py, "memoryview")).unwrap_err();
            assert!(err.is_instance_of::<PyBufferError>(py));
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

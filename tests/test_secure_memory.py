"""Python-level tests for the secure_memory extension module.

Run after building the extension into the active environment, e.g.
``maturin develop && pytest``.
"""

import ctypes
import hashlib
import sys

import pytest

from secure_memory import SecureMemory


def test_len_builtin_and_method():
    sm = SecureMemory(16)
    assert len(sm) == 16 == sm.len()


def test_write_read_roundtrip():
    sm = SecureMemory(16)
    sm.write(2, b"abc")
    assert sm.read(2, 3) == b"abc"
    assert sm.read(0, 2) == b"\x00\x00"


@pytest.mark.parametrize("make", [bytes, bytearray, memoryview])
def test_write_accepts_bytes_like_objects(make):
    sm = SecureMemory(8)
    sm.write(0, make(b"secret!!"))
    assert sm.read(0, 8) == b"secret!!"


def test_source_bytearray_can_be_wiped_after_write():
    sm = SecureMemory(6)
    src = bytearray(b"secret")
    sm.write(0, src)
    src[:] = bytes(len(src))
    assert sm.read(0, 6) == b"secret"


def test_out_of_bounds_raises_value_error_and_leaves_buffer_alone():
    sm = SecureMemory(4)
    with pytest.raises(ValueError):
        sm.write(1, b"abcd")
    with pytest.raises(ValueError):
        sm.read(1, 4)
    assert sm.read(0, 4) == b"\x00" * 4


def test_write_refused_while_view_alive():
    sm = SecureMemory(4)
    sm.write(0, b"abcd")
    with sm.view() as mv:
        with pytest.raises(BufferError):
            sm.write(0, b"wxyz")
        assert bytes(mv) == b"abcd"
    sm.write(0, b"wxyz")
    assert sm.read(0, 4) == b"wxyz"


def test_view_exit_detects_kept_derived_view():
    sm = SecureMemory(4)
    with pytest.raises(BufferError):
        with sm.view() as mv:
            kept = mv[1:3]
    with pytest.raises(BufferError):
        sm.write(0, b"x")
    kept.release()
    sm.write(0, b"ok")


def test_with_buffer_returns_result_and_releases():
    sm = SecureMemory(4)
    sm.write(0, b"abcd")
    digest = sm.with_buffer(lambda mv: hashlib.sha256(mv).hexdigest())
    assert digest == hashlib.sha256(b"abcd").hexdigest()
    sm.write(0, b"wxyz")  # no view was left behind


def test_with_buffer_detects_retained_view():
    sm = SecureMemory(4)
    kept = []
    with pytest.raises(BufferError):
        sm.with_buffer(lambda mv: kept.append(mv[1:3]))
    with pytest.raises(BufferError):
        sm.write(0, b"x")
    kept.clear()
    sm.write(0, b"ok")


def test_with_buffer_releases_when_callback_raises():
    sm = SecureMemory(4)

    def boom(mv):
        raise RuntimeError("boom")

    with pytest.raises(RuntimeError):
        sm.with_buffer(boom)
    sm.write(0, b"ok")


def test_views_are_read_only():
    sm = SecureMemory(4)
    with sm.view() as mv:
        assert mv.readonly
        with pytest.raises(TypeError):
            mv[0] = 1


def test_writable_buffer_request_is_rejected():
    sm = SecureMemory(4)
    with pytest.raises((TypeError, BufferError)):
        (ctypes.c_char * 4).from_buffer(sm)


def test_clear_zeroizes_even_with_live_view_and_keeps_buffer_usable():
    sm = SecureMemory(4)
    sm.write(0, b"abcd")
    with sm.view() as mv:
        sm.clear()
        assert bytes(mv) == b"\x00" * 4
    assert len(sm) == 4
    sm.write(0, b"wxyz")
    assert sm.read(0, 4) == b"wxyz"


def test_zero_size_buffer():
    sm = SecureMemory(0)
    assert len(sm) == 0
    assert not sm.is_locked()
    assert sm.read(0, 0) == b""
    SecureMemory(0, require_lock=True)  # nothing to lock, so nothing to fail


@pytest.mark.skipif(sys.maxsize <= 2**32, reason="needs a 64-bit size_t")
def test_absurd_size_raises_memory_error():
    with pytest.raises(MemoryError):
        SecureMemory(2**64 - 1)


def test_require_lock_is_keyword_only():
    with pytest.raises(TypeError):
        SecureMemory(8, True)


def test_require_lock_agrees_with_is_locked():
    # Whether the OS allows locking depends on the environment
    # (RLIMIT_MEMLOCK, privileges), so accept both outcomes, but they must be
    # coherent with each other.
    try:
        sm = SecureMemory(64, require_lock=True)
    except OSError:
        assert not SecureMemory(64).is_locked()
    else:
        assert sm.is_locked()

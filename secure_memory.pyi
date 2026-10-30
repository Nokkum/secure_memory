from collections.abc import Callable
from types import TracebackType
from typing import TypeVar

from typing_extensions import Buffer

_T = TypeVar("_T")

class SecureMemory:
    """A fixed-size, zeroize-on-drop buffer on locked pages of its own."""

    def __init__(self, size: int, *, require_lock: bool = False) -> None:
        """Allocate ``size`` zeroed bytes and try to lock them into RAM.

        Raises ``MemoryError`` if the allocation fails, and ``OSError`` if
        ``require_lock`` is true and the OS refuses to lock the memory.
        """
    def write(self, offset: int, data: Buffer) -> None:
        """Copy any bytes-like object into the buffer at ``offset``.

        Raises ``BufferError`` while any view of the buffer is alive, and
        ``ValueError`` if the write would not fit.
        """
    def read(self, offset: int, length: int) -> bytes:
        """Copy bytes out. The copy lives outside the zeroize boundary."""
    def clear(self) -> None:
        """Zeroize in place; never refused because of live views."""
    def is_locked(self) -> bool: ...
    def len(self) -> int: ...
    def __len__(self) -> int: ...
    def as_bytes(self) -> bytes:
        """Copy the whole buffer out. The copy lives outside the zeroize boundary."""
    def with_buffer(self, callback: Callable[[memoryview], _T]) -> _T:
        """Call ``callback`` with a read-only memoryview, then release it.

        Raises ``BufferError`` if the callback kept a derived view alive.
        """
    def view(self) -> SecureView:
        """Context manager yielding a read-only memoryview."""
    def __buffer__(self, flags: int, /) -> memoryview: ...

class SecureView:
    def __enter__(self) -> memoryview: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None = None,
        exc_value: BaseException | None = None,
        traceback: TracebackType | None = None,
    ) -> bool: ...

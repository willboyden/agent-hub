"""Sliding-window rate limiter (per token id), in memory, monotonic clock injectable for tests."""
from __future__ import annotations

import time
from collections import deque
from collections.abc import Callable


class RateLimiter:
    def __init__(self, clock: Callable[[], float] = time.monotonic) -> None:
        self._clock = clock
        self._hits: dict[str, deque[float]] = {}

    def allow(self, key: str, per_minute: int) -> bool:
        now = self._clock()
        dq = self._hits.setdefault(key, deque())
        while dq and now - dq[0] > 60.0:
            dq.popleft()
        if len(dq) >= per_minute:
            return False
        dq.append(now)
        return True

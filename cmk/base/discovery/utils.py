#!/usr/bin/env python3
# -*- coding: utf-8 -*-
# Copyright (C) 2019 tribe29 GmbH - License: GNU General Public License v2
# This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
# conditions defined in the file COPYING, which is part of this source code package.

import signal
import time
from types import FrameType, TracebackType
from typing import (
    Final,
    Callable,
    Hashable,
    Iterable,
    Generic,
    NoReturn,
    Optional,
    Sequence,
    Type,
    TypeVar,
)

from cmk.utils.exceptions import MKException
from cmk.utils.log import console


class _Timeout(MKException):
    pass


class TimeLimitFilter:
    @classmethod
    def _raise_timeout(cls, signum: int, stack_frame: Optional[FrameType]) -> NoReturn:
        raise _Timeout()

    def __init__(
        self,
        *,
        limit: int,
        grace: int,
        label: str = "elements",
    ) -> None:
        self._start = int(time.monotonic())
        self._end = self._start + limit
        self.limit: Final = limit
        self.label: Final = label

        signal.signal(signal.SIGALRM, TimeLimitFilter._raise_timeout)
        signal.alarm(self.limit + grace)

    def __enter__(self) -> 'TimeLimitFilter':
        return self

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_val: Optional[BaseException],
        exc_tb: Optional[TracebackType],
    ) -> bool:
        signal.alarm(0)
        if isinstance(exc_val, _Timeout):
            console.verbose(f"  Timeout of {self.limit} seconds reached. "
                            f"Let's do the remaining {self.label} next time.")
            return True
        return False

    def __call__(self, iterable: Iterable) -> Iterable:
        for element in iterable:
            yield element
            if time.monotonic() > self._end:
                raise _Timeout()


_DiscoveredItem = TypeVar("_DiscoveredItem")


class QualifiedDiscovery(Generic[_DiscoveredItem]):
    """Classify items into "new", "old" and "vanished" ones.
    """
    def __init__(
        self,
        *,
        preexisting: Sequence[_DiscoveredItem],
        current: Sequence[_DiscoveredItem],
        key: Callable[[_DiscoveredItem], Hashable],
    ) -> None:
        current_dict = {key(v): v for v in current}
        preexisting_dict = {key(v): v for v in preexisting}

        self.vanished: Final = [v for k, v in preexisting_dict.items() if k not in current_dict]
        self.new: Final = [v for k, v in current_dict.items() if k not in preexisting_dict]
        self.old: Final = [v for k, v in current_dict.items() if k in preexisting_dict]
        self.present: Final = self.old + self.new

    @classmethod
    def empty(cls) -> 'QualifiedDiscovery':
        """create an empty instance"""
        return cls(preexisting=(), current=(), key=repr)

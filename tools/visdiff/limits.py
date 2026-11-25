# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A memory cap for the processes a run starts, and their end when the run ends.

On Windows each process goes in a job object with a per-process memory limit (an allocation past it
fails, so a page whose memory runs away stops there instead of taking the machine's) and kill-on-close
(the job's processes end with the run, even when the run itself is stopped). The job reports the peak
memory any of its processes used. Elsewhere the job does nothing and reports no peak.
"""

import ctypes
import sys

JOB_OBJECT_LIMIT_PROCESS_MEMORY = 0x100
JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000
JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS = 9
PROCESS_SET_QUOTA = 0x0100
PROCESS_TERMINATE = 0x0001


if sys.platform == "win32":
    from ctypes import wintypes

    class _IoCounters(ctypes.Structure):
        _fields_ = [(name, ctypes.c_ulonglong) for name in (
            "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
            "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]

    class _BasicLimit(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class _ExtendedLimit(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", _BasicLimit),
            ("IoInfo", _IoCounters),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    _k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _k32.CreateJobObjectW.restype = wintypes.HANDLE
    _k32.CreateJobObjectW.argtypes = [wintypes.LPVOID, wintypes.LPCWSTR]
    _k32.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD]
    _k32.QueryInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD,
                                               wintypes.LPVOID]
    _k32.OpenProcess.restype = wintypes.HANDLE
    _k32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    _k32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    _k32.CloseHandle.argtypes = [wintypes.HANDLE]


class Job:
    """A job object whose processes each may use at most `memory_bytes` (None: no cap)."""

    def __init__(self, memory_bytes=None):
        self.memory_bytes = memory_bytes
        self.handle = None
        if sys.platform != "win32":
            return
        self.handle = _k32.CreateJobObjectW(None, None)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        info = _ExtendedLimit()
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if memory_bytes:
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY
            info.ProcessMemoryLimit = int(memory_bytes)
        if not _k32.SetInformationJobObject(self.handle, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                                            ctypes.byref(info), ctypes.sizeof(info)):
            err = ctypes.get_last_error()
            self.close()
            raise ctypes.WinError(err)

    def add(self, pid):
        """Puts process `pid` (and the processes it starts from then on) in the job. False when it could not."""
        if not self.handle:
            return False
        h = _k32.OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, False, pid)
        if not h:
            return False
        try:
            return bool(_k32.AssignProcessToJobObject(self.handle, h))
        finally:
            _k32.CloseHandle(h)

    def peak_bytes(self):
        """The most memory any process in the job has used, or None."""
        if not self.handle:
            return None
        info = _ExtendedLimit()
        if not _k32.QueryInformationJobObject(self.handle, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                                              ctypes.byref(info), ctypes.sizeof(info), None):
            return None
        return info.PeakProcessMemoryUsed

    def over_cap(self):
        """Whether a process in the job reached the memory cap (its allocations then fail)."""
        peak = self.peak_bytes()
        return bool(self.memory_bytes and peak and peak >= 0.9 * self.memory_bytes)

    def close(self):
        """Ends the job's processes that are still running."""
        if self.handle:
            _k32.CloseHandle(self.handle)
            self.handle = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def run(cmd, timeout_s, memory_bytes=None, env=None):
    """Runs `cmd` in a job capped at `memory_bytes`, ending it after `timeout_s` seconds. Returns a dict:
    exit (the exit code, or "timeout"), out and err (bytes), peak (bytes, or None) and over_cap."""
    import subprocess

    with Job(memory_bytes) as job:
        proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        job.add(proc.pid)
        try:
            out, err = proc.communicate(timeout=timeout_s)
            code = proc.returncode
        except subprocess.TimeoutExpired:
            proc.kill()
            out, err = proc.communicate()
            code = "timeout"
        return {"exit": code, "out": out, "err": err, "peak": job.peak_bytes(), "over_cap": job.over_cap()}

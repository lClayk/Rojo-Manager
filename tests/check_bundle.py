"""Exercise the built executables with an isolated profile and no installed Python on PATH."""

import ctypes
import http.client
from ctypes import wintypes
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


class ProcessEntry(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD), ("th32ProcessID", wintypes.DWORD),
                ("th32DefaultHeapID", ctypes.c_void_p), ("th32ModuleID", wintypes.DWORD),
                ("cntThreads", wintypes.DWORD), ("th32ParentProcessID", wintypes.DWORD),
                ("pcPriClassBase", ctypes.c_long), ("dwFlags", wintypes.DWORD), ("szExeFile", ctypes.c_wchar * 260)]


def with_children(pid: int) -> set[int]:
    """The process and its children: the window runs as a child process of the tray app."""
    kernel32 = ctypes.windll.kernel32
    kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    snapshot = kernel32.CreateToolhelp32Snapshot(2, 0)  # TH32CS_SNAPPROCESS
    entry = ProcessEntry(dwSize=ctypes.sizeof(ProcessEntry))
    pids = {pid}
    found = kernel32.Process32FirstW(snapshot, ctypes.byref(entry))
    while found:
        if entry.th32ParentProcessID == pid:
            pids.add(entry.th32ProcessID)
        found = kernel32.Process32NextW(snapshot, ctypes.byref(entry))
    kernel32.CloseHandle(snapshot)
    return pids


def check(bundle: Path) -> None:
    user32 = ctypes.windll.user32
    callback_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
    user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]

    with tempfile.TemporaryDirectory(prefix="rojo-bundle-check-") as directory:
        profile = Path(directory)
        env = os.environ.copy()
        env["ROJO_MANAGER_DATA_DIR"] = str(profile)
        env["PATH"] = str(Path(os.environ["SystemRoot"]) / "System32")
        (profile / "config.json").write_text(json.dumps({"projects": [], "settings": {
            "tray": False, "version_check": False, "app_version_check": False}}), encoding="utf-8")
        process = subprocess.Popen([str(bundle / "RojoManager.exe")], env=env, cwd=profile,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        window = None
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and process.poll() is None:
                windows = []
                owners = with_children(process.pid)

                @callback_type
                def collect(hwnd, _):
                    owner = wintypes.DWORD()
                    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
                    title = ctypes.create_unicode_buffer(256)
                    user32.GetWindowTextW(hwnd, title, len(title))
                    if owner.value in owners and title.value == "Rojo Manager" and user32.IsWindowVisible(hwnd):
                        windows.append(hwnd)
                    return True
                user32.EnumWindows(collect, 0)
                if windows:
                    window = windows[0]
                    break
                time.sleep(.1)
            if window is None:
                raise AssertionError("Packaged app did not show its main window")
            control = json.loads((profile / "control.json").read_text(encoding="utf-8"))
            connection = http.client.HTTPConnection("127.0.0.1", control["port"], timeout=10)
            connection.request("GET", "/projects", headers={"Authorization": f"Bearer {control['token']}"})
            reply = connection.getresponse()
            assert reply.status == 200 and json.loads(reply.read()) == {"projects": []}, reply.status
            user32.PostMessageW(window, 0x0010, 0, 0)  # Normal close, with tray disabled.
            _, errors = process.communicate(timeout=15)
            if process.returncode != 0 or errors:
                raise AssertionError(f"Packaged app failed: {errors.decode(errors='replace')}")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
        assert json.loads((profile / "config.json").read_text(encoding="utf-8"))["projects"] == []
        assert not (profile / "control.json").exists(), "control.json outlived the app"

        # An unknown tool verifies the real console worker and result channel without installing anything.
        request = profile / "request.json"
        request.write_text('{"operation": "global", "tool": "unknown"}', encoding="utf-8")
        worker = subprocess.run([str(bundle / "ToolInstaller.exe"), str(request)],
                                env=env, cwd=profile, input="\n", text=True,
                                capture_output=True, timeout=20, creationflags=subprocess.CREATE_NO_WINDOW)
        assert worker.returncode == 0, worker.stderr
        result = json.loads((profile / "result.json").read_text(encoding="utf-8"))
        assert result == {"ok": False, "message": "Unknown installation action"}, result
        assert not (bundle / "config.json").exists()
        assert (bundle / "licenses" / "rust-crates.txt").stat().st_size > 10_000

        # The MCP server answers over stdio without needing the manager to run.
        requests = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}},
                    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}]
        mcp = subprocess.run([str(bundle / "RojoManagerMCP.exe")], env=env, cwd=profile,
                             input="".join(json.dumps(r) + "\n" for r in requests), text=True,
                             capture_output=True, timeout=30, creationflags=subprocess.CREATE_NO_WINDOW)
        assert mcp.returncode == 0, mcp.stderr
        responses = [json.loads(line) for line in mcp.stdout.splitlines()]
        assert responses[0]["result"]["serverInfo"]["name"] == "rojo-manager", responses
        assert {tool["name"] for tool in responses[1]["result"]["tools"]} >= {"rojo_server", "rojo_session"}
    print("Packaged GUI, control API, normal exit, private settings, console installer, and MCP server passed.")


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve())

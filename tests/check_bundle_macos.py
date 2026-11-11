"""Exercise the packaged macOS app with an isolated profile: its window, a second launch, the control
API, a clean exit, the tool installer, and the MCP server. Screenshots go to the folder in argv[2]."""

import http.client
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

# Lists on-screen windows by owning pid; window titles would need screen recording permission.
WINDOWS = """
import CoreGraphics
let windows = CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID) as? [[String: Any]] ?? []
for window in windows {
    let bounds = window[kCGWindowBounds as String] as? [String: Any] ?? [:]
    print(window[kCGWindowOwnerPID as String] ?? 0, bounds["Width"] ?? 0, bounds["Height"] ?? 0)
}
"""


def window_pids() -> set[int]:
    """Pids that own a large on-screen window, like Rojo Manager's 760x820 one."""
    listed = subprocess.run(["swift", "-e", WINDOWS], capture_output=True, text=True, timeout=120).stdout
    found = set()
    for line in listed.splitlines():
        pid, width, height = (int(float(part)) for part in line.split())
        if width >= 400 and height >= 300:
            found.add(pid)
    return found


def children(pid: int) -> set[int]:
    listed = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout
    return {int(child) for child in listed.split()}


def wait_for(condition, timeout: float, what: str):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = condition()
        if found:
            return found
        time.sleep(0.5)
    raise AssertionError(f"Timed out waiting for {what}")


def projects(profile: Path) -> dict:
    control = json.loads((profile / "control.json").read_text(encoding="utf-8"))
    connection = http.client.HTTPConnection("127.0.0.1", control["port"], timeout=10)
    connection.request("GET", "/projects", headers={"Authorization": f"Bearer {control['token']}"})
    reply = connection.getresponse()
    assert reply.status == 200, reply.status
    return json.loads(reply.read())


def screenshot(shots: Path, name: str) -> None:
    subprocess.run(["screencapture", "-x", str(shots / f"{name}.png")], timeout=30)


def check(app: Path, shots: Path) -> None:
    shots.mkdir(parents=True, exist_ok=True)
    macos = app / "Contents" / "MacOS"
    with tempfile.TemporaryDirectory(prefix="rojo-bundle-check-") as directory:
        profile = Path(directory)
        env = os.environ.copy()
        env["ROJO_MANAGER_DATA_DIR"] = str(profile)
        settings = {"tray": True, "version_check": False, "app_version_check": False}
        (profile / "config.json").write_text(json.dumps({"projects": [], "settings": settings}), encoding="utf-8")

        # Hidden in the menu bar, then a second launch shows the window and exits.
        host = subprocess.Popen([str(macos / "RojoManager"), "--hidden"], env=env, cwd=profile,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            wait_for(lambda: (profile / "control.json").exists(), 30, "the control API")
            assert projects(profile) == {"projects": []}
            assert not children(host.pid), "a hidden start opened the window"
            second = subprocess.run([str(macos / "RojoManager")], env=env, cwd=profile, timeout=30)
            assert second.returncode == 0, second.returncode
            window = wait_for(lambda: children(host.pid) & window_pids(), 60, "the window after a second launch")
            time.sleep(3)
            screenshot(shots, "window")
            # Closing the window keeps the menu bar app running.
            for pid in window:
                os.kill(pid, signal.SIGTERM)
            wait_for(lambda: not children(host.pid), 30, "the window process to exit")
            assert host.poll() is None, "the app quit when its window closed with the tray on"
            assert projects(profile) == {"projects": []}
        finally:
            if host.poll() is None:
                host.kill()
                host.wait()

        # With the tray off, closing the window quits the app cleanly.
        settings["tray"] = False
        (profile / "config.json").write_text(json.dumps({"projects": [], "settings": settings}), encoding="utf-8")
        (profile / "control.json").unlink(missing_ok=True)
        host = subprocess.Popen([str(macos / "RojoManager")], env=env, cwd=profile,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            window = wait_for(lambda: children(host.pid) & window_pids(), 60, "the window")
            for pid in window:
                os.kill(pid, signal.SIGTERM)
            _, errors = host.communicate(timeout=30)
            assert host.returncode == 0, (host.returncode, errors.decode(errors="replace"))
        finally:
            if host.poll() is None:
                host.kill()
                host.wait()
        assert not (profile / "control.json").exists(), "control.json outlived the app"

        # An unknown tool verifies the installer and its result channel without installing anything.
        request = profile / "request.json"
        request.write_text('{"operation": "global", "tool": "unknown"}', encoding="utf-8")
        worker = subprocess.run([str(macos / "ToolInstaller"), str(request)], env=env, cwd=profile,
                                input="\n", text=True, capture_output=True, timeout=20)
        assert worker.returncode == 0, worker.stderr
        result = json.loads((profile / "result.json").read_text(encoding="utf-8"))
        assert result == {"ok": False, "message": "Unknown installation action"}, result
        assert (app / "Contents" / "Resources" / "licenses" / "rust-crates.txt").stat().st_size > 10_000

        # The MCP server answers over stdio without needing the manager to run.
        requests = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}},
                    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}]
        mcp = subprocess.run([str(macos / "RojoManagerMCP")], env=env, cwd=profile,
                             input="".join(json.dumps(r) + "\n" for r in requests), text=True,
                             capture_output=True, timeout=30)
        assert mcp.returncode == 0, mcp.stderr
        responses = [json.loads(line) for line in mcp.stdout.splitlines()]
        assert responses[0]["result"]["serverInfo"]["name"] == "rojo-manager", responses
        assert {tool["name"] for tool in responses[1]["result"]["tools"]} >= {"rojo_server", "rojo_session"}
    print("Packaged app, window, second launch, control API, clean exit, installer, and MCP server passed.")


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())

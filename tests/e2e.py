"""End-to-end checks of a packaged Rojo Manager, driven the way an agent drives it: through the real
MCP server, with a real Rojo server, and with the real Studio bridge running under Lune
(tests/bridge_harness.luau) in place of Studio.

Usage: e2e.py <folder with the executables>
Needs rojo and lune on PATH. Uses an isolated profile; outside CI it skips the login item check,
which would change this machine's startup settings."""

import json
import os
from pathlib import Path
import queue
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

WINDOWS = sys.platform == "win32"
ROOT = Path(__file__).resolve().parent.parent
PLACE_ID = 123456
PORT = 35990
# Set ROJO_MANAGER_BRIDGE_PORT to run beside a live Rojo Manager, which holds 34871.
BRIDGE_PORT = os.environ.get("ROJO_MANAGER_BRIDGE_PORT", "34871")


def executable(folder: Path, name: str) -> Path:
    return folder / (f"{name}.exe" if WINDOWS else name)


def wait_for(condition, timeout: float, what: str):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = condition()
        if found:
            return found
        time.sleep(0.25)
    raise AssertionError(f"Timed out waiting for {what}")


class Lines:
    """A process's stdout lines, read on a thread."""

    def __init__(self, process: subprocess.Popen):
        self.queue: queue.Queue[str] = queue.Queue()
        threading.Thread(target=self._read, args=(process,), daemon=True).start()

    def _read(self, process: subprocess.Popen):
        for line in process.stdout:
            self.queue.put(line.rstrip("\n"))

    def expect(self, wanted: str, timeout: float = 30) -> None:
        deadline = time.monotonic() + timeout
        seen = []
        while time.monotonic() < deadline:
            try:
                line = self.queue.get(timeout=max(0.0, deadline - time.monotonic()))
            except queue.Empty:
                break
            seen.append(line)
            if line == wanted:
                return
        raise AssertionError(f"Expected {wanted!r} from the bridge; got {seen}")


class Mcp:
    """RojoManagerMCP over stdio, as an agent runs it."""

    def __init__(self, path: Path, env: dict):
        self.process = subprocess.Popen([str(path)], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        text=True, encoding="utf-8")
        self.lines = Lines(self.process)
        self.next_id = 0
        self.request("initialize", {"protocolVersion": "2025-06-18"})

    def request(self, method: str, params: dict) -> dict:
        self.next_id += 1
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.next_id, "method": method,
                                             "params": params}) + "\n")
        self.process.stdin.flush()
        while True:
            reply = json.loads(self.lines.queue.get(timeout=120))
            if reply.get("id") == self.next_id:
                return reply

    def call(self, tool: str, **arguments) -> dict:
        result = self.request("tools/call", {"name": tool, "arguments": arguments})["result"]
        text = result["content"][0]["text"]
        if result.get("isError"):
            raise AssertionError(f"{tool} failed: {text}")
        return json.loads(text)

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=30)


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def sourcemap_names(project: Path) -> str:
    try:
        return (project / "sourcemap.json").read_text(encoding="utf-8")
    except OSError:
        return ""


def answers(port: int) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.5):
            return True
    except OSError:
        return False


def shim_rojo(folder: Path) -> Path:
    """A rojo that runs the real one as its child, as Rokit's shim does on macOS, so stopping a server
    has to end both."""
    real = shutil.which("rojo")
    assert real, "rojo is not on PATH"
    shim = folder / "rojo"
    write(shim, f'#!/bin/sh\n"{real}" "$@"\n')
    shim.chmod(0o755)
    return folder


def run_bridge(place: str, mode: str = "") -> tuple[subprocess.Popen, Lines]:
    """The real bridge under Lune, in a stand-in Studio with this place ("<id>" or "0:<file name>").
    With mode "confirm", Rojo waits for its initial sync to be confirmed."""
    bridge = subprocess.Popen(["lune", "run", str(ROOT / "tests" / "bridge_harness.luau"),
                               str(ROOT / "rojo_bridge" / "RojoManagerBridge.luau"), place, BRIDGE_PORT, mode],
                              stdout=subprocess.PIPE, text=True, encoding="utf-8")
    events = Lines(bridge)
    events.expect("attached")
    return bridge, events


def check_login_item(mcp: Mcp, app_dir: Path) -> None:
    """Open at login writes and removes the system's startup entry for this executable."""
    def entry() -> str:
        if WINDOWS:
            import winreg
            try:
                with winreg.OpenKey(winreg.HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Run") as key:
                    return winreg.QueryValueEx(key, "RojoManager")[0]
            except OSError:
                return ""
        agent = Path.home() / "Library" / "LaunchAgents" / "com.lclayk.rojomanager.plist"
        return agent.read_text(encoding="utf-8") if agent.exists() else ""

    assert mcp.call("app_settings", changes={"launch_at_startup": True})["launch_at_startup"] is True
    assert str(executable(app_dir, "RojoManager")) in entry(), entry()
    assert mcp.call("app_settings", changes={"launch_at_startup": False})["launch_at_startup"] is False
    assert entry() == "", entry()


def check(app_dir: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="rojo-e2e-") as directory:
        profile, project = Path(directory) / "profile", Path(directory) / "MyGame"
        write(project / "default.project.json", json.dumps(
            {"name": "MyGame", "tree": {"$className": "DataModel", "ServerScriptService": {"$path": "src"}}}))
        write(project / "src" / "main.server.luau", 'print("hi")\n')
        write(profile / "config.json", json.dumps({
            "projects": [{"name": "MyGame", "path": str(project), "projectFile": "default.project.json",
                          "port": PORT, "autostart": False, "autoConnect": True,
                          "place": {"placeId": PLACE_ID, "universeId": 1}}],
            "settings": {"tray": True, "version_check": False, "app_version_check": False,
                         "notifications": False, "restart_crashed": False},
        }))
        env = os.environ.copy()
        env["ROJO_MANAGER_DATA_DIR"] = str(profile)
        if not WINDOWS:
            env["PATH"] = f"{shim_rojo(Path(directory) / 'shim')}{os.pathsep}{env['PATH']}"
        host = subprocess.Popen([str(executable(app_dir, "RojoManager")), "--hidden"], env=env, cwd=profile)
        bridge = lane_bridge = mcp = None
        try:
            wait_for(lambda: (profile / "control.json").exists(), 30, "the control API")
            mcp = Mcp(executable(app_dir, "RojoManagerMCP"), env)

            # Sourcemap: on while the server runs, following new files; gone with the server.
            updated = mcp.call("manage_project", action="update", project="MyGame", sourcemap=True)
            assert updated["project"]["sourcemap"] is True, updated
            started = mcp.call("rojo_server", project="MyGame", action="start")
            assert started["project"]["state"] in ("serving", "connected"), started
            wait_for(lambda: '"main"' in sourcemap_names(project), 30, "sourcemap.json")
            write(project / "src" / "other.server.luau", "print(1)\n")
            wait_for(lambda: '"other"' in sourcemap_names(project), 30, "the sourcemap to follow a new file")
            logs = mcp.call("server_logs", project="MyGame")["lines"]
            assert any(line.startswith("$ rojo sourcemap") for line in logs), logs

            # The bridge reports in, connects Rojo through auto-connect, and sends its Output.
            bridge, events = run_bridge(str(PLACE_ID))
            events.expect(f"connect {PORT}")
            instances = wait_for(lambda: mcp.call("get_connected_instances")["instances"], 30, "the bridge")
            assert [(i["placeId"], i.get("project")) for i in instances] == [(PLACE_ID, "MyGame")], instances
            instance = instances[0]["instanceId"]

            def connected():
                session = mcp.call("rojo_session", project="MyGame", action="status")
                return session if session["rojo"].get("status") == "Connected" else None
            session = wait_for(connected, 30, "Rojo to report Connected")
            assert session["rojo"]["address"] == f"localhost:{PORT}", session

            def sent():
                output = mcp.call("studio_output", project="MyGame")
                return output if any(line["text"] == "e2e error" for line in output["lines"]) else None
            output = wait_for(sent, 30, "Studio's Output")
            kinds = {line["text"]: (line["context"], line["kind"]) for line in output["lines"]}
            assert kinds["edit started"] == ("edit", "info"), kinds
            assert kinds["logged before the plugin loaded"] == ("edit", "output"), kinds
            assert kinds["e2e warning"] == ("edit", "warning") and kinds["e2e error"] == ("edit", "error"), kinds
            errors = mcp.call("studio_output", instance_id=instance, level="error")["lines"]
            assert [line["text"] for line in errors] == ["e2e error"], errors
            assert mcp.call("studio_output", instance_id=instance, since=output["next"])["lines"] == []

            # A copy of the project with a local place: the bridge in an unpublished place names it by its
            # file, so auto-connect finds that project's server, and its Output stays its own.
            lane = Path(directory) / "MyGame-lane"
            shutil.copytree(project, lane)
            added = mcp.call("manage_project", action="add", path=str(lane))["added"][0]
            assert added["name"] == "MyGame-lane", added
            linked = mcp.call("manage_project", action="update", project="MyGame-lane", place="local",
                              auto_connect=True)["project"]
            place_file = linked["placeFile"]
            assert place_file.startswith("MyGame-lane-") and place_file.endswith(".rbxl"), linked
            lane_port = mcp.call("rojo_server", project="MyGame-lane", action="start")["project"]["port"]

            # With the sync prompt left to the agent, status shows what accepting would do first.
            mcp.call("app_settings", changes={"auto_connect_accept": False})
            lane_bridge, lane_events = run_bridge(f"0:{place_file}", "confirm")
            lane_events.expect(f"confirming {lane_port}")
            session = mcp.call("rojo_session", project="MyGame-lane", action="status")
            assert session["rojo"]["pending"] == {
                "removed": {"count": 1, "paths": ["Workspace.Map"]},
                "added": {"count": 2, "paths": ["ServerScriptService.Tools"]},
                "changed": {"count": 1, "paths": ["ServerScriptService.main"]},
            }, session
            assert "deletes 1 instance found only in Studio" in session["next"], session

            def reported():
                listed = {p["name"]: p for p in mcp.call("list_projects")["projects"]}
                return listed["MyGame-lane"].get("studio", {}).get("rojo", {}).get("pending")
            assert wait_for(reported, 30, "the poll to report the pending sync") == session["rojo"]["pending"]
            mcp.call("rojo_session", project="MyGame-lane", action="accept")
            lane_events.expect("confirm Accept")
            lane_events.expect(f"connect {lane_port}")
            def lane_connected():
                listed = {p["name"]: p for p in mcp.call("list_projects")["projects"]}
                return listed["MyGame-lane"].get("studio", {}).get("rojo", {}).get("status") == "Connected"
            wait_for(lane_connected, 30, "the lane's Rojo to report Connected")
            lane_output = wait_for(lambda: mcp.call("studio_output", project="MyGame-lane")["lines"], 30, "lane Output")
            assert [line["text"] for line in lane_output].count("edit started") == 1, lane_output
            mcp.call("manage_project", action="remove", project="MyGame-lane")
            assert "MyGame-lane" not in [p["name"] for p in mcp.call("list_projects")["projects"]]

            # Connect waits in Studio until Rojo has connected, and answers with that.
            mcp.call("rojo_session", project="MyGame", action="disconnect")
            events.expect("disconnect")
            reconnected = mcp.call("rojo_session", project="MyGame", action="connect")
            assert reconnected["rojo"]["status"] == "Connected", reconnected
            events.expect(f"connect {PORT}")

            # Agent Luau runs in the bridge and comes back with what it printed and returned.
            ran = mcp.call("execute_luau", instance_id=instance, code="print('from agent') return 40 + 2")
            assert ran["returnValue"] == 42 and ran["output"] == ["from agent"], ran

            # Stopping disconnects Rojo first, ends the sourcemap watcher, and frees the port. On macOS
            # the real rojo runs behind a shim, so this also shows the whole process group ends.
            stopped = mcp.call("rojo_server", project="MyGame", action="stop")
            assert stopped["project"]["state"] == "off", stopped
            events.expect("disconnect")
            wait_for(lambda: not answers(PORT), 5, "the server's port to be free")
            write(project / "src" / "late.server.luau", "print(2)\n")
            time.sleep(3)
            assert '"late"' not in sourcemap_names(project), "the sourcemap watcher outlived its server"

            if os.environ.get("CI"):
                check_login_item(mcp, app_dir)
        finally:
            for process in (bridge, lane_bridge, host):
                if process and process.poll() is None:
                    process.kill()
                    process.wait()
            if mcp:
                mcp.close()
    print("Sourcemap, Rojo server, bridge auto-connect, local places, Studio Output, agent Luau, and login item "
          "passed.")


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve())

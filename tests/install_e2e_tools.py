"""Put pinned Rojo and Lune releases on PATH for tests/e2e.py in GitHub Actions.
Usage: install_e2e_tools.py <platform>, as their release assets name it (windows-x86_64, macos-aarch64)."""

import os
from pathlib import Path
import subprocess
import sys
import zipfile

TOOLS = [("rojo-rbx/rojo", "v7.7.1", "rojo-7.7.1"), ("lune-org/lune", "v0.11.1", "lune-0.11.1")]

platform = sys.argv[1]
downloads = Path(os.environ["RUNNER_TEMP"])
bin_dir = downloads / "e2e-bin"
bin_dir.mkdir(exist_ok=True)
for repository, tag, prefix in TOOLS:
    asset = f"{prefix}-{platform}.zip"
    subprocess.run(["gh", "release", "download", tag, "-R", repository, "-p", asset, "-D", str(downloads),
                    "--clobber"], check=True)
    with zipfile.ZipFile(downloads / asset) as archive:
        for name in archive.namelist():
            target = Path(archive.extract(name, bin_dir))
            target.chmod(0o755)
with open(os.environ["GITHUB_PATH"], "a", encoding="utf-8") as path:
    path.write(f"{bin_dir}\n")

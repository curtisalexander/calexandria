"""Allow running as `python -m calexandria`."""

import os
import shutil
import subprocess
import sys
import sysconfig


def _run():
    name = "calx.exe" if sys.platform == "win32" else "calx"
    # The installer puts calx next to this Python's other scripts, which may not be on
    # PATH, such as in a virtual environment that is not activated.
    scripts = [sysconfig.get_path("scripts"), sysconfig.get_path("scripts", f"{os.name}_user")]
    candidates = (os.path.join(d, name) for d in scripts)
    binary = next((c for c in candidates if os.path.isfile(c)), None) or shutil.which(name)
    if binary is None:
        print(f"Error: could not find '{name}' on PATH.", file=sys.stderr)
        print("Reinstall it: uv tool install calexandria", file=sys.stderr)
        sys.exit(1)
    if sys.platform == "win32":
        sys.exit(subprocess.call([binary, *sys.argv[1:]]))
    else:
        os.execvp(binary, [binary, *sys.argv[1:]])


if __name__ == "__main__":
    _run()

#!/usr/bin/env python3
"""Register every KG's MCP server with Claude Code (`claude mcp add ...`, user scope).

Each server is registered to run `serve_kg.py <key>` over stdio. After this, Claude
can call `mcp__<server>__<tool>` for any KG. MCP servers load at the start of a Claude
session, so start a new session (or restart) after registering.

Usage:
    python register.py            # register all KGs
    python register.py legal bank # register a subset

Environment:
    PYTHON   interpreter Claude should use to launch the servers (default: this one)
"""
import os
import subprocess
import sys
from pathlib import Path

import yaml

HERE = Path(__file__).resolve().parent
PYTHON = os.environ.get("PYTHON", sys.executable)


def main():
    kgs = yaml.safe_load((HERE / "kgs.yaml").read_text())
    wanted = set(sys.argv[1:])
    for e in kgs:
        if wanted and e["key"] not in wanted:
            continue
        cmd = ["claude", "mcp", "add", e["server"], "--scope", "user",
               "--", PYTHON, str(HERE / "serve_kg.py"), e["key"]]
        print("+", " ".join(cmd))
        subprocess.run(cmd, check=False)


if __name__ == "__main__":
    main()

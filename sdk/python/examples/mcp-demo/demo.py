#!/usr/bin/env python3
"""Run the live Claude <-> Samyama Graph MCP demo.

For each knowledge graph, asks Claude (headless, `claude -p`) a natural-language
question and lets it answer by calling that KG's MCP tools — a real round-trip to
the running engine, not a canned script. This is the sequence recorded in the demo
video.

Prerequisites:
  1. The engine is running with every tenant imported.
  2. The MCP servers are registered: `python register.py`
  3. You are in (or start) a Claude Code session so the MCP servers are loaded.

Usage:
    python demo.py               # all KGs, in order
    python demo.py legal cricket # a subset
"""
import subprocess
import sys
from pathlib import Path

import yaml

HERE = Path(__file__).resolve().parent


def main():
    kgs = yaml.safe_load((HERE / "kgs.yaml").read_text())
    wanted = set(sys.argv[1:])

    print("#" * 60)
    print("#  Claude  <->  Samyama Graph   via MCP   (live)")
    print("#  natural-language questions, answered from the graph")
    print("#" * 60)

    for e in kgs:
        if wanted and e["key"] not in wanted:
            continue
        allow = ",".join(f"mcp__{e['server']}__{t}" for t in e["allow"])
        print("\n" + "=" * 70)
        print(f"  KG:  {e['title']}")
        print(f"  ASK: {e['question']}")
        print("-" * 70)
        subprocess.run(
            ["claude", "-p", e["question"] + " Answer in ONE short line.",
             "--allowedTools", allow],
            stdin=subprocess.DEVNULL,
        )
        print()

    print("=" * 70)
    print("  DONE — every answer above came from a live MCP query on the graph")
    print("=" * 70)


if __name__ == "__main__":
    main()

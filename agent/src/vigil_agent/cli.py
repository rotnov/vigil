"""CLI entry point: `vigil-agent ask --snapshot FILE [--question TEXT]`."""

from __future__ import annotations

import argparse
import asyncio
import json
import sys

from .diagnose import ask


def main() -> None:
    parser = argparse.ArgumentParser(prog="vigil-agent")
    sub = parser.add_subparsers(dest="command", required=True)

    ask_parser = sub.add_parser("ask", help="Diagnose a snapshot and answer a question about it")
    ask_parser.add_argument("--snapshot", required=True, help="Path to a vigil snapshot JSON file")
    ask_parser.add_argument("--question", default=None, help="Question about the snapshot (optional)")

    args = parser.parse_args()

    if args.command == "ask":
        try:
            with open(args.snapshot, encoding="utf-8") as f:
                snapshot = json.load(f)
        except (OSError, json.JSONDecodeError) as e:
            print(f"failed to read snapshot {args.snapshot}: {e}", file=sys.stderr)
            sys.exit(1)

        answer = asyncio.run(ask(snapshot, args.question))
        print(answer)


if __name__ == "__main__":
    main()

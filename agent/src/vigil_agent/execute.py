"""Executes a small, pre-approved fix plan with narrowly, per-plan-scoped tool access.

Deliberately a separate module from `diagnose.py` rather than a shared config
with extra parameters -- the two configs' allowed blast radius is fundamentally
different (read-only investigation vs. a handful of unlocked destructive Bash
patterns), and conflating them risks a future edit to one accidentally loosening
the other. See docs/superpowers/specs/2026-08-09-fix-execution-workflow-design.md.
"""

from __future__ import annotations

from typing import Any

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    ResultMessage,
    TextBlock,
    query,
)

from .diagnose import format_usage_footer
from .prompts import EXECUTE_SYSTEM_PROMPT

# Same allowed set as investigation (diagnose.ALLOWED_TOOLS) -- the
# execute-agent re-verifies a step's target the same way an investigation
# checks a hypothesis (Bash/Read/Grep/Glob), before acting through whichever
# Bash patterns its plan's categories unlocked.
ALLOWED_TOOLS = ["Bash", "Read", "Grep", "Glob"]

# Blocked unconditionally, regardless of what's approved -- these fall
# outside all three fix categories below and there is no path to unlocking
# them via a plan. Write/Edit/NotebookEdit too: the execute-agent acts only
# through the unlocked Bash patterns, never by writing/editing files
# directly.
HARD_FLOOR_DISALLOWED_TOOLS = [
    "Write",
    "Edit",
    "NotebookEdit",
    "Bash(sudo *)",
    "Bash(su *)",
    "Bash(dd *)",
    "Bash(diskutil erase*)",
    "Bash(diskutil partition*)",
    "Bash(diskutil eraseVolume*)",
    "Bash(shutdown *)",
    "Bash(reboot *)",
    "Bash(halt *)",
    "Bash(chmod *)",
    "Bash(chown *)",
]

# Bash patterns each fix category unlocks -- only the categories actually
# present in the approved plan get removed from DISALLOWED_TOOLS; every
# other category's patterns stay blocked for this session even though
# they're not on the hard floor above.
CATEGORY_UNLOCKS: dict[str, list[str]] = {
    "kill_process": ["Bash(kill *)", "Bash(killall *)", "Bash(pkill *)"],
    "delete_path": ["Bash(rm *)", "Bash(rmdir *)", "Bash(mv *)"],
    "system_setting": [
        "Bash(defaults write*)",
        "Bash(defaults delete*)",
        "Bash(launchctl unload*)",
        "Bash(launchctl bootout*)",
        "Bash(launchctl remove*)",
    ],
}

MAX_EXECUTION_TURNS = 10


def build_categories(plan: list[dict[str, Any]]) -> set[str]:
    """The distinct `category` values present in an approved plan."""
    return {step["category"] for step in plan}


def disallowed_tools_for(categories: set[str]) -> list[str]:
    """The hard floor, plus every category-specific pattern for a category
    NOT in `categories` -- so a plan is scoped to exactly what it approved,
    nothing wider. Raises `ValueError` on an unrecognized category rather
    than silently ignoring it: silently ignoring here would either mean
    silently leaving that category's patterns blocked (safe, but hides a
    bug) or, worse, silently accepting a typo'd category that was meant to
    unlock something -- fail loud instead.
    """
    unknown = categories - CATEGORY_UNLOCKS.keys()
    if unknown:
        raise ValueError(f"unknown fix category/categories: {sorted(unknown)}")
    locked_patterns = [
        pattern
        for category, patterns in CATEGORY_UNLOCKS.items()
        if category not in categories
        for pattern in patterns
    ]
    return HARD_FLOOR_DISALLOWED_TOOLS + locked_patterns


def build_instruction(plan: list[dict[str, Any]]) -> str:
    """The literal prompt handed to the execute-agent: the approved steps,
    numbered, with nothing about any rejected step it should never see.
    """
    lines = [
        f"{i}. [{step['category']}] {step['description']} (target_hint: {step['target_hint']})"
        for i, step in enumerate(plan, start=1)
    ]
    return "Carry out this pre-approved plan:\n" + "\n".join(lines)


async def execute(plan: list[dict[str, Any]]) -> str:
    """Run the approved plan through a dedicated, narrowly-scoped agent
    session and return its report (steps done/aborted + a token footer).
    """
    categories = build_categories(plan)
    options = ClaudeAgentOptions(
        system_prompt=EXECUTE_SYSTEM_PROMPT,
        allowed_tools=ALLOWED_TOOLS,
        disallowed_tools=disallowed_tools_for(categories),
        max_turns=MAX_EXECUTION_TURNS,
    )

    chunks: list[str] = []
    usage: dict[str, Any] | None = None
    cost_usd: float | None = None
    async for message in query(prompt=build_instruction(plan), options=options):
        if isinstance(message, AssistantMessage):
            for block in message.content:
                if isinstance(block, TextBlock):
                    chunks.append(block.text)
        elif isinstance(message, ResultMessage):
            usage = message.usage
            cost_usd = message.total_cost_usd
            if message.subtype == "success" and message.result:
                return message.result.strip() + format_usage_footer(usage, cost_usd)

    answer = "".join(chunks).strip() or "The execute-agent returned no report."
    return answer + format_usage_footer(usage, cost_usd)

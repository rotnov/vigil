"""The only module that talks to Claude — a thin wrapper around `query()`."""

from __future__ import annotations

from typing import Any

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    ResultMessage,
    TextBlock,
    query,
)

from .prompts import SYSTEM_PROMPT, build_prompt

# Investigation tools: Bash/Read/Grep/Glob so the agent can actually dig
# (log show, sample <pid>, du -sh, docker system df, tmutil listlocalsnapshots,
# vm_stat, ps, cat a log file, ...) instead of only reasoning over the
# snapshot JSON. Deliberately excludes Write/Edit — the agent can inspect
# the system but never modify it.
ALLOWED_TOOLS = ["Bash", "Read", "Grep", "Glob"]

# Command-pattern denylist on top of the allowlist above. This runs for
# BOTH the interactive ('a' key) and the automatic background CPU-alert
# diagnosis (see agent.rs::maybe_diagnose_alert_async) — the background
# path has no one watching, so these rails apply unconditionally, not just
# when unattended. Investigation must stay read-only: no killing processes,
# no deleting/moving files, no privilege escalation, no power state changes.
DISALLOWED_TOOLS = [
    "Write",
    "Edit",
    "NotebookEdit",
    "Bash(sudo *)",
    "Bash(su *)",
    "Bash(rm *)",
    "Bash(rmdir *)",
    "Bash(mv *)",
    "Bash(dd *)",
    "Bash(kill *)",
    "Bash(killall *)",
    "Bash(pkill *)",
    "Bash(diskutil erase*)",
    "Bash(diskutil partition*)",
    "Bash(diskutil eraseVolume*)",
    "Bash(launchctl unload*)",
    "Bash(launchctl bootout*)",
    "Bash(launchctl remove*)",
    "Bash(chmod *)",
    "Bash(chown *)",
    "Bash(shutdown *)",
    "Bash(reboot *)",
    "Bash(halt *)",
    "Bash(defaults write*)",
    "Bash(defaults delete*)",
]

MAX_INVESTIGATION_TURNS = 15


def format_usage_footer(usage: dict[str, Any] | None, cost_usd: float | None) -> str:
    """A short, appended note on the investigation's own token spend.

    Goes straight into whatever consumes `ask()`'s return value — the
    incident journal file, the UI's answer popup — rather than a separate
    side channel, so it's visible wherever the answer itself is. Matches
    this project's governing design goal (vigil's own overhead, including
    the agent's token spend, should be visible, not hidden). Returns ""
    if no usage was ever reported (e.g. the query errored before any
    `ResultMessage` arrived) — nothing to show is different from a zero.
    """
    if usage is None:
        return ""
    input_tokens = usage.get("input_tokens", 0)
    output_tokens = usage.get("output_tokens", 0)
    cache_read = usage.get("cache_read_input_tokens", 0)
    cost_note = f" — ~${cost_usd:.4f}" if cost_usd is not None else ""
    return f"\n\n---\n_Tokens: {input_tokens} in / {output_tokens} out (+{cache_read} cache read){cost_note}_"


async def ask(snapshot: dict[str, Any], question: str | None) -> str:
    """Investigate a snapshot (and, if needed, the live system) and answer a question.

    The agent can read files and run informational shell commands to dig
    past the snapshot's numbers (e.g. `log show`, `sample <pid>`, `du -sh`),
    but Write/Edit are excluded and a denylist blocks destructive or
    privilege-escalating Bash patterns — see DISALLOWED_TOOLS. It can only
    ever produce text: it never kills a process or deletes anything itself.
    """
    prompt = build_prompt(snapshot, question)
    options = ClaudeAgentOptions(
        system_prompt=SYSTEM_PROMPT,
        allowed_tools=ALLOWED_TOOLS,
        disallowed_tools=DISALLOWED_TOOLS,
        max_turns=MAX_INVESTIGATION_TURNS,
    )

    chunks: list[str] = []
    usage: dict[str, Any] | None = None
    cost_usd: float | None = None
    async for message in query(prompt=prompt, options=options):
        if isinstance(message, AssistantMessage):
            for block in message.content:
                if isinstance(block, TextBlock):
                    chunks.append(block.text)
        elif isinstance(message, ResultMessage):
            usage = message.usage
            cost_usd = message.total_cost_usd
            if message.subtype == "success" and message.result:
                return message.result.strip() + format_usage_footer(usage, cost_usd)

    answer = "".join(chunks).strip() or "The agent returned no answer."
    return answer + format_usage_footer(usage, cost_usd)

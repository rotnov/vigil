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


async def ask(snapshot: dict[str, Any], question: str | None) -> str:
    """Reason over a snapshot and answer a question about it.

    Read-only by design: allowed_tools=[] means the agent has no filesystem
    or bash access in v1, it can only reason over the snapshot it was given.
    """
    prompt = build_prompt(snapshot, question)
    options = ClaudeAgentOptions(
        system_prompt=SYSTEM_PROMPT,
        allowed_tools=[],
        max_turns=1,
    )

    chunks: list[str] = []
    async for message in query(prompt=prompt, options=options):
        if isinstance(message, AssistantMessage):
            for block in message.content:
                if isinstance(block, TextBlock):
                    chunks.append(block.text)
        elif isinstance(message, ResultMessage) and message.subtype == "success" and message.result:
            return message.result

    return "".join(chunks).strip() or "The agent returned no answer."

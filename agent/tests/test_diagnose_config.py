"""Guards the safety rails around the agent's investigation tool access.

These don't call the SDK — they just assert the config we pass to
ClaudeAgentOptions actually blocks what it's supposed to, so a future edit
can't silently loosen it without a test failing.
"""

from vigil_agent.diagnose import ALLOWED_TOOLS, DISALLOWED_TOOLS


def test_write_and_edit_are_never_allowed():
    assert "Write" not in ALLOWED_TOOLS
    assert "Edit" not in ALLOWED_TOOLS
    assert "Write" in DISALLOWED_TOOLS
    assert "Edit" in DISALLOWED_TOOLS


def test_investigation_tools_are_allowed():
    for tool in ("Bash", "Read", "Grep", "Glob"):
        assert tool in ALLOWED_TOOLS


def test_destructive_and_privilege_escalating_bash_patterns_are_blocked():
    must_block = [
        "Bash(sudo *)",
        "Bash(rm *)",
        "Bash(mv *)",
        "Bash(kill *)",
        "Bash(killall *)",
        "Bash(pkill *)",
        "Bash(shutdown *)",
        "Bash(diskutil erase*)",
    ]
    for pattern in must_block:
        assert pattern in DISALLOWED_TOOLS

"""Guards the safety rails around the execute-agent's tool access -- same
spirit as test_diagnose_config.py, but for the narrower, per-plan-scoped
execute path.
"""

import pytest

from vigil_agent.execute import (
    ALLOWED_TOOLS,
    CATEGORY_UNLOCKS,
    HARD_FLOOR_DISALLOWED_TOOLS,
    build_categories,
    build_instruction,
    disallowed_tools_for,
)


def test_write_and_edit_are_always_blocked():
    assert "Write" in HARD_FLOOR_DISALLOWED_TOOLS
    assert "Edit" in HARD_FLOOR_DISALLOWED_TOOLS
    assert "NotebookEdit" in HARD_FLOOR_DISALLOWED_TOOLS


def test_hard_floor_is_present_regardless_of_approved_categories():
    disallowed = disallowed_tools_for({"kill_process", "delete_path", "system_setting"})
    for pattern in HARD_FLOOR_DISALLOWED_TOOLS:
        assert pattern in disallowed


def test_hard_floor_patterns_cannot_be_unlocked_by_any_category():
    hard_floor_set = set(HARD_FLOOR_DISALLOWED_TOOLS)
    for patterns in CATEGORY_UNLOCKS.values():
        assert hard_floor_set.isdisjoint(patterns)


def test_approving_kill_process_only_unlocks_kill_patterns():
    disallowed = disallowed_tools_for({"kill_process"})
    for pattern in CATEGORY_UNLOCKS["kill_process"]:
        assert pattern not in disallowed
    for pattern in CATEGORY_UNLOCKS["delete_path"]:
        assert pattern in disallowed
    for pattern in CATEGORY_UNLOCKS["system_setting"]:
        assert pattern in disallowed


def test_approving_no_categories_blocks_all_category_patterns():
    disallowed = disallowed_tools_for(set())
    for patterns in CATEGORY_UNLOCKS.values():
        for pattern in patterns:
            assert pattern in disallowed


def test_unknown_category_raises():
    with pytest.raises(ValueError):
        disallowed_tools_for({"reboot_machine"})


def test_build_categories_collects_distinct_categories():
    plan = [
        {"category": "kill_process", "description": "d1", "target_hint": "h1"},
        {"category": "delete_path", "description": "d2", "target_hint": "h2"},
        {"category": "kill_process", "description": "d3", "target_hint": "h3"},
    ]
    assert build_categories(plan) == {"kill_process", "delete_path"}


def test_build_instruction_numbers_steps_and_includes_target_hints():
    plan = [{"category": "kill_process", "description": "Kill the stale session", "target_hint": "pid 72837"}]
    instruction = build_instruction(plan)
    assert "1. [kill_process] Kill the stale session" in instruction
    assert "pid 72837" in instruction


def test_investigation_tools_are_allowed_for_reverification():
    for tool in ("Bash", "Read", "Grep", "Glob"):
        assert tool in ALLOWED_TOOLS

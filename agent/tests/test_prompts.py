from vigil_agent.prompts import EXECUTE_SYSTEM_PROMPT, SYSTEM_PROMPT, build_prompt


def test_build_prompt_embeds_snapshot_and_question():
    snapshot = {"load_avg": {"one": 12.0}, "disks": []}
    prompt = build_prompt(snapshot, "why is it slow?")
    assert "why is it slow?" in prompt
    assert "12.0" in prompt


def test_build_prompt_defaults_question_when_missing():
    prompt = build_prompt({"a": 1}, None)
    assert "Analyze the system" in prompt


def test_build_prompt_defaults_question_when_blank():
    prompt = build_prompt({"a": 1}, "   ")
    assert "Analyze the system" in prompt


def test_build_prompt_preserves_custom_question_untouched():
    prompt = build_prompt({"a": 1}, "  why is disk space low?  ")
    assert prompt.endswith("why is disk space low?")


def test_build_prompt_passes_through_non_ascii_questions_untouched():
    prompt = build_prompt({"a": 1}, "почему мало места на диске?")
    assert prompt.endswith("почему мало места на диске?")


def test_system_prompt_forbids_fabricating_files():
    assert "do not invent" in SYSTEM_PROMPT.lower()


def test_system_prompt_requires_confirmation_before_risky_actions():
    assert "confirmation" in SYSTEM_PROMPT.lower()


def test_system_prompt_forbids_modifying_the_system_despite_shell_access():
    lowered = SYSTEM_PROMPT.lower()
    assert "inspect the system, never change it" in lowered
    assert "kill" in lowered
    assert "sudo" in lowered


def test_system_prompt_documents_the_proposed_fix_json_schema():
    assert "## Proposed fix" in SYSTEM_PROMPT
    for category in ("kill_process", "delete_path", "system_setting"):
        assert category in SYSTEM_PROMPT
    assert '"plan"' in SYSTEM_PROMPT


def test_execute_system_prompt_requires_reverification_and_abort_on_failure():
    lowered = EXECUTE_SYSTEM_PROMPT.lower()
    assert "re-verify" in lowered
    assert "stop" in lowered
    assert "target_hint" in EXECUTE_SYSTEM_PROMPT

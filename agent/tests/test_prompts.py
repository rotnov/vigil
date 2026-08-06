from vigil_agent.prompts import SYSTEM_PROMPT, build_prompt


def test_build_prompt_embeds_snapshot_and_question():
    snapshot = {"load_avg": {"one": 12.0}, "disks": []}
    prompt = build_prompt(snapshot, "почему грузится?")
    assert "почему грузится?" in prompt
    assert "12.0" in prompt


def test_build_prompt_defaults_question_when_missing():
    prompt = build_prompt({"a": 1}, None)
    assert "Проанализируй систему" in prompt


def test_build_prompt_defaults_question_when_blank():
    prompt = build_prompt({"a": 1}, "   ")
    assert "Проанализируй систему" in prompt


def test_build_prompt_preserves_custom_question_untouched():
    prompt = build_prompt({"a": 1}, "  почему мало места на диске?  ")
    assert prompt.endswith("почему мало места на диске?")


def test_system_prompt_forbids_fabricating_files():
    assert "не выдумывай" in SYSTEM_PROMPT.lower()


def test_system_prompt_requires_confirmation_before_risky_actions():
    assert "подтверждени" in SYSTEM_PROMPT.lower()

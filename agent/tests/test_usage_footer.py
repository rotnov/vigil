"""format_usage_footer is pure -- no network, no query() call."""

from vigil_agent.diagnose import format_usage_footer


def test_returns_empty_string_when_no_usage_was_reported():
    assert format_usage_footer(None, None) == ""


def test_includes_input_output_and_cache_read_tokens():
    usage = {"input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 5}
    footer = format_usage_footer(usage, None)
    assert "10 in" in footer
    assert "20 out" in footer
    assert "5 cache read" in footer


def test_includes_cost_when_present():
    usage = {"input_tokens": 1, "output_tokens": 1}
    footer = format_usage_footer(usage, 0.29016)
    assert "$0.2902" in footer


def test_omits_cost_when_absent():
    usage = {"input_tokens": 1, "output_tokens": 1}
    footer = format_usage_footer(usage, None)
    assert "$" not in footer


def test_defaults_missing_keys_to_zero():
    footer = format_usage_footer({}, None)
    assert "0 in" in footer
    assert "0 out" in footer

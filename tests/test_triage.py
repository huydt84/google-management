from conftest import ROOT, gmail_message

from gm.gmail.message import parse_message
from gm.triage import RuleFile, RuleSet, evaluate


def _m(**kw):
    return parse_message(gmail_message("x", **kw))


def test_default_rules_file():
    rules = RuleFile(ROOT / "rules.yaml").get()
    assert evaluate(rules, _m(labels=["INBOX", "CATEGORY_PROMOTIONS"])).action == "ignore"
    newsletter = _m(extra_headers=[{"name": "List-Unsubscribe", "value": "<x>"}])
    assert evaluate(rules, newsletter).action == "log"
    assert evaluate(rules, _m(labels=["INBOX", "CATEGORY_UPDATES"])).action == "log"
    v = evaluate(rules, _m())
    assert (v.action, v.rule) == ("notify", "default")


def test_conditions_are_anded_and_first_match_wins():
    rules = RuleSet.model_validate({
        "default": "log",
        "rules": [
            {"name": "bank-otp", "when": {"from_contains": ["@bank.vn"],
                                           "subject_regex": r"\bOTP\b"}, "action": "ignore"},
            {"name": "bank", "when": {"from_contains": ["@bank.vn"]}, "action": "notify"},
        ],
    })
    assert evaluate(rules, _m(sender="x@bank.vn", subject="Your OTP code")).rule == "bank-otp"
    assert evaluate(rules, _m(sender="x@bank.vn", subject="Statement")).rule == "bank"
    assert evaluate(rules, _m(sender="y@other.vn")).action == "log"


def test_empty_when_never_matches():
    rules = RuleSet.model_validate({"rules": [{"name": "x", "when": {}, "action": "ignore"}]})
    assert evaluate(rules, _m()).action == "notify"


def test_rule_file_reloads_and_survives_bad_yaml(tmp_path):
    p = tmp_path / "rules.yaml"
    p.write_text("default: log\n")
    rf = RuleFile(p)
    assert rf.get().default == "log"
    import os
    p.write_text("default: [broken\n")
    os.utime(p, (1, 1))
    assert rf.get().default == "log"  # previous rules kept

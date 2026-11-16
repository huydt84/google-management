"""Rule-based triage (Phase 0). The decision model plugs in after these rules in Phase 2.

rules.yaml: first matching rule wins; every condition inside one `when` must match.
"""

import logging
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

import yaml
from pydantic import BaseModel, Field

from gm.gmail.message import ParsedMessage

log = logging.getLogger(__name__)

Action = Literal["ignore", "log", "notify"]


class When(BaseModel):
    labels_any: list[str] = Field(default_factory=list)
    from_contains: list[str] = Field(default_factory=list)
    from_regex: str | None = None
    subject_contains: list[str] = Field(default_factory=list)
    subject_regex: str | None = None
    has_list_unsubscribe: bool | None = None

    def matches(self, m: ParsedMessage) -> bool:
        sender = f"{m.from_name} {m.from_addr}".lower()
        subject = m.subject.lower()
        checks = []
        if self.labels_any:
            checks.append(bool(set(self.labels_any) & set(m.label_ids)))
        if self.from_contains:
            checks.append(any(s.lower() in sender for s in self.from_contains))
        if self.from_regex:
            checks.append(re.search(self.from_regex, sender, re.I) is not None)
        if self.subject_contains:
            checks.append(any(s.lower() in subject for s in self.subject_contains))
        if self.subject_regex:
            checks.append(re.search(self.subject_regex, m.subject, re.I) is not None)
        if self.has_list_unsubscribe is not None:
            checks.append(m.has_list_unsubscribe == self.has_list_unsubscribe)
        return bool(checks) and all(checks)


class Rule(BaseModel):
    name: str
    when: When
    action: Action


class RuleSet(BaseModel):
    default: Action = "notify"
    rules: list[Rule] = Field(default_factory=list)


@dataclass(frozen=True)
class Verdict:
    action: Action
    rule: str


def evaluate(rules: RuleSet, m: ParsedMessage) -> Verdict:
    for r in rules.rules:
        if r.when.matches(m):
            return Verdict(r.action, r.name)
    return Verdict(rules.default, "default")


class RuleFile:
    """Loads rules.yaml and reloads it when the file changes (edit rules without restarting)."""

    def __init__(self, path: Path):
        self._path = path
        self._mtime: float | None = None
        self._rules = RuleSet()

    def get(self) -> RuleSet:
        try:
            mtime = self._path.stat().st_mtime
        except FileNotFoundError:
            if self._mtime is not None or not hasattr(self, "_warned"):
                log.warning("rules file %s not found; using default action", self._path)
                self._warned = True
            return self._rules
        if mtime != self._mtime:
            try:
                self._rules = RuleSet.model_validate(yaml.safe_load(self._path.read_text()) or {})
                self._mtime = mtime
                log.info("loaded %d triage rules from %s", len(self._rules.rules), self._path)
            except Exception:
                log.exception("invalid rules file %s; keeping previous rules", self._path)
        return self._rules

#!/usr/bin/env python3
"""Prepare a local Bonsai checkpoint with an empty reasoning block in the chat template."""

import os
from pathlib import Path
from tempfile import TemporaryDirectory

MODEL = "Bonsai-27B-mlx-1bit"
DIRECT = MODEL + "-direct"
THINKING_GUARD = "if enable_thinking is defined and enable_thinking is false"


def cached_checkpoint() -> Path:
    hf_home = Path(os.environ.get("HF_HOME", Path.home() / ".cache" / "huggingface"))
    hub = Path(os.environ.get("HF_HUB_CACHE", hf_home / "hub"))
    repo = hub.expanduser() / f"models--prism-ml--{MODEL}"
    revision = (repo / "refs" / "main").read_text().strip()
    return repo / "snapshots" / revision


def prepare() -> Path:
    source = cached_checkpoint()
    template_path = source / "chat_template.jinja"
    template = template_path.read_text()
    if template.count(THINKING_GUARD) != 1:
        raise ValueError(f"Unexpected thinking guard in {template_path}")
    direct_template = template.replace(THINKING_GUARD, "if true")
    assets = {file.name: file.resolve(strict=True) for file in source.iterdir() if file != template_path}
    target = Path.home() / ".local" / "share" / "gm" / "models" / DIRECT
    if target.exists():
        if (target / template_path.name).read_text() != direct_template or any(
            not (target / name).is_symlink() or (target / name).resolve() != asset
            for name, asset in assets.items()
        ):
            raise ValueError(f"{target} already exists with different contents")
        return target

    target.parent.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=target.parent) as temporary:
        staged = Path(temporary) / DIRECT
        staged.mkdir()
        for name, asset in assets.items():
            (staged / name).symlink_to(asset)
        (staged / template_path.name).write_text(direct_template)
        staged.rename(target)
    return target


if __name__ == "__main__":
    try:
        print(prepare())
    except (OSError, ValueError) as err:
        raise SystemExit(str(err)) from err

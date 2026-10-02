# Copyright (C) 2026 Philip Eriksson. All rights reserved.

import html
import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CAST = ROOT / "docs/demo.cast"
PAGE = ROOT / "docs/index.html"
PLACEHOLDER = "<!-- DEMO_TRANSCRIPT -->"
ANSI_CSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def main():
    output = []
    for line in CAST.read_text(encoding="utf-8").splitlines():
        event = json.loads(line)
        if isinstance(event, list) and len(event) >= 3 and event[1] == "o":
            if not isinstance(event[2], str):
                raise ValueError("asciicast output event must contain text")
            output.append(event[2])

    transcript = "".join(output).replace("\x1b[H\x1b[2J", "\n[Dashboard redraw]\n")
    transcript = ANSI_CSI.sub("", transcript).replace("\r\n", "\n").replace("\r", "\n")
    if not transcript:
        raise ValueError("asciicast contains no terminal output")

    page = PAGE.read_text(encoding="utf-8")
    if page.count(PLACEHOLDER) != 1:
        raise ValueError(f"expected exactly one {PLACEHOLDER} placeholder in {PAGE}")

    accessible_transcript = (
        '<pre class="sr-only" aria-label="Transcript of the terminal demo">'
        f"{html.escape(transcript, quote=False)}</pre>"
    )
    PAGE.write_text(page.replace(PLACEHOLDER, accessible_transcript), encoding="utf-8")


if __name__ == "__main__":
    main()

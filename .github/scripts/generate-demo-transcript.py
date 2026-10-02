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


def terminal_screen(output, cols, rows):
    screen = [[" "] * cols for _ in range(rows)]
    row = column = 0
    offset = 0
    while offset < len(output):
        if output[offset] == "\x1b":
            match = ANSI_CSI.match(output, offset)
            if match:
                params, final = match.group()[2:-1], match.group()[-1]
                values = params.lstrip("?").split(";") if params else []
                numbers = [int(value) if value.isdigit() else 0 for value in values]
                if final in "Hf":
                    row = max(0, (numbers[0] if numbers else 1) - 1)
                    column = max(0, (numbers[1] if len(numbers) > 1 else 1) - 1)
                elif final == "J" and numbers and numbers[0] == 2:
                    screen = [[" "] * cols for _ in range(rows)]
                offset = match.end()
                continue
            offset += 1
            continue
        character = output[offset]
        if character == "\r":
            column = 0
        elif character == "\n":
            row = min(rows - 1, row + 1)
        elif character == "\b":
            column = max(0, column - 1)
        elif character >= " " and character != "\x7f":
            if row < rows and column < cols:
                screen[row][column] = character
            column += 1
        offset += 1
    return "\n".join("".join(line).rstrip() for line in screen).rstrip()


def main():
    events = []
    for line in CAST.read_text(encoding="utf-8").splitlines():
        event = json.loads(line)
        if isinstance(event, list) and len(event) >= 3 and event[1] == "o":
            if not isinstance(event[2], str):
                raise ValueError("asciicast output event must contain text")
            events.append(event[2])

    transcript = "".join(events)
    alt_screen_start = "\x1b[?1049h"
    alt_screen_end = "\x1b[?1049l"
    if alt_screen_start in transcript and alt_screen_end in transcript:
        before, terminal = transcript.split(alt_screen_start, 1)
        terminal, after = terminal.split(alt_screen_end, 1)
        header = json.loads(CAST.read_text(encoding="utf-8").splitlines()[0])
        cols = header["term"]["cols"]
        rows = header["term"]["rows"]
        transcript = (
            before
            + "\n[Terminal control panel]\n"
            + terminal_screen(terminal, cols, rows)
            + "\n"
            + after
        )
    transcript = transcript.replace("\x1b[H\x1b[2J", "\n[Dashboard redraw]\n")
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

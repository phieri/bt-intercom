---
name: asciinema
description: Create and edit asciicast v3 recordings.
---

# Creating and editing asciinema recordings

Use this skill when creating or updating `.cast` recordings. Preserve the
recording's existing asciicast v3 JSON format and follow the target file's event
style rather than replacing it with a different recording pattern.

## Event format and typing

- Keep the first line as the v3 header with the intended terminal dimensions.
- Each following line is one JSON event: elapsed time, event type (`i` for input
  or `o` for output), and text.
- Simulate typing with short input chunks, usually around 2–4 characters each.
  Put each `i` event immediately before its matching `o` terminal echo event.
  Do not group all input chunks together and put one full-command echo afterward.
- Input adresses like IP or MAC in one chunk to simulate copy-and-paste.
- Echo exactly what the terminal would display for each chunk. In this repository,
  an entered carriage return (`\r`) is echoed as a newline (`\r\n`). Preserve
  backslash continuations and their following spaces in multi-line commands.
- Keep a visible pause between input chunks so the player presents deliberate
  typing, not a command pasted at speed. For `docs/demo.cast`, use about `0.15`
  seconds per short input chunk; do not reduce existing pauses or shorten
  non-input events unless specifically requested.
- Make command output and dashboard states plausible and consistent with the
  current CLI and README. When recording CLI help, verify it against the current
  binary rather than relying on an older recording.

## Editing and validation

- Make targeted edits and preserve unrelated event contents and timing.
- Parse every line as JSON; check the header, event shape, valid event type, and
  non-negative delays.
- Check that each input event is followed by an output event containing its
  matching terminal echo, including carriage-return/newline behavior. Reassemble
  the input and verify that each complete command is exactly the intended
  command.
- Confirm output events still contain the expected response and that input delays
  retain the intended pace.
- The Pages workflow generates the accessible transcript from
  `.github/scripts/generate-demo-transcript.py`. Run it when validating a changed
  recording, but restore the original `docs/index.html` placeholder afterward;
  do not commit the locally generated transcript or files under `docs/vendor/`.

---
name: asciinema
description: Create, edit, validate, and test asciicast v3 recordings and terminal demos (.cast files) with precise typing simulation and timing.
---

# Creating and editing asciinema recordings

Use this skill when creating, editing, or validating `.cast` recordings. Preserve the recording's existing asciicast v3 JSON format and follow the target file's event style rather than replacing it with a different recording pattern.

## Header & Format Rules

- Keep the first line as the v3 header with the intended terminal dimensions and metadata.
- Each following line must be a valid JSON event array: `[elapsed_time, event_type, text]`.
- Event types: `i` for input or `o` for output.
- Make targeted edits while preserving unrelated event contents and timing.

## Terminal Simulation & Typing Rhythm

- **Simulate realistic typing:** Use short input chunks (usually around 3 characters each). Put each `i` event immediately before its matching `o` terminal echo event. Do not group all input chunks together and put one full-command echo afterward.
- **Copy-and-paste handling:** Print addresses like IP or MAC in a single chunk to simulate copy-and-paste, and add a bit of extra pause around these events to represent the copying.
- **Carriage returns & continuations:** Echo exactly what the terminal would display for each chunk. An entered carriage return (`\r`) is echoed as a newline (`\r\n`). Preserve backslash continuations and their following spaces in multi-line commands.
- **Pacing:** Keep a visible pause between input chunks so the player presents deliberate typing, not a command pasted at speed. For `docs/demo.cast`, use about `0.15` seconds per short input chunk; do not reduce existing pauses or shorten non-input events unless specifically requested.
- **Pauses:** Linger for 0.85 second on the empty shell to give the viewer some air.
- **Content Accuracy:** Make command output and dashboard states plausible and consistent with the current CLI and README. When recording CLI help, verify it against the current program source code rather than relying on an older recording.

## Editing & Validation Workflow

1. **Parse & Verify:** Parse every line as JSON; check the header, event shape, valid event types, and non-negative delays.
2. **Echo & Command Check:** Check that each input event is followed by an output event containing its matching terminal echo, including carriage-return/newline behavior. Reassemble the input and verify that each complete command is exactly the intended command.
3. **Pace & Response Check:** Confirm output events contain the expected response and input delays retain the intended pace.
4. **Transcript Generation:** The Pages workflow generates the accessible transcript via `.github/scripts/generate-demo-transcript.py`. Run it when validating a changed recording, but restore the original `docs/index.html` placeholder afterward. Do not commit the locally generated transcript or files under `docs/vendor/`.
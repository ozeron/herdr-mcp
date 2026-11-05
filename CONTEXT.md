# Herdr MCP file reads

This context names local file access for pane artifacts and agent session transcripts; each reader owns its read permissions and output rules.

## Language

**Allowed root**:
The canonical directory within which a reader permits an open file to reside.
_Avoid_: working directory when referring to a provider's transcript directory.

**Verified file**:
An open regular file whose actual location has been checked against its **Allowed root** and which has no additional hard links.
_Avoid_: allowed file when referring only to these filesystem checks, which do not grant permission to read its contents.

**Pane artifact**:
A file under the directory a pane was opened in, read through the control-only file reader.
_Avoid_: session transcript when the file was written as a report or other output in the pane's directory.

**Session transcript**:
A provider-owned conversation file selected through the live pane's reported agent session ID, from which the session reader extracts saved assistant text.
_Avoid_: current model context when describing recorded output, which may lag and does not reconstruct branch history.

## Relationships

- A **Pane artifact** and a **Session transcript** each have an **Allowed root** selected by their reader.
- Both readers open a **Verified file** before reading bytes.
- The pane artifact reader also applies credential-path restrictions; the session reader also validates provider/session identity and requires a linked transcript.

## Example dialogue

> **Dev:** "Does opening a verified file make an arbitrary pane artifact available in read mode?"
> **Domain expert:** "No. Verification checks the file; the artifact reader still requires control mode, while the session reader permits only assistant text from the pane's session transcript."

## Flagged ambiguities

- "Verified" can mean a filesystem check or permission to expose content; resolved here: use **Verified file** for filesystem checks, and keep content permissions with each reader.

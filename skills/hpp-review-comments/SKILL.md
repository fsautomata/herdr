---
name: hpp-review-comments
description: Read and act on hc: inline review comments that a human left in markdown files with the hpp file viewer. Use when asked to address, answer, or review "hc comments", "review comments", or "comments in <file>.md", or when a markdown file you are editing contains "<!-- hc:body".
---

# hc: review comments

Humans review specs and plans in the hpp file viewer by selecting text and leaving comments. Each
comment is stored in the markdown file itself as HTML comments and carries a **directive** telling
you what to do.

## Steps

1. Load the format and rules: run `hpp protocol` (or read `hc-comments.md` next to this skill in the hpp
   repository). Follow those rules exactly.
2. Find the threads: `grep -n "hc:body" <file>` for one file, or
   `grep -rln "hc:body" --include='*.md' <dir>` to find files with comments.
3. For each thread, read its anchored text (between `<!--hc:a id=X-->` and `<!--hc:/ id=X-->`, or
   the `quote=` / `cell=` of its first body) and all of its bodies, then act on the first body's
   `directive`:
   - `reply` or no directive → add a reply body; do not change the document text.
   - `fix` → make the change, then remove the whole thread (markers and every body).
   - `discuss` → add a reply body proposing an approach; change nothing else.
4. Write replies directly beneath the thread's last body, as `author=agent`, with the current UTC
   time as `ts` (`YYYY-MM-DDTHH:MMZ`), `reply-to=<id>`, and `status=resolved` when nothing is left
   for the human.
5. Summarize for the human: which threads you answered, fixed (removed), or left open, and why.

## Never

- Delete or move markers or bodies of threads you did not complete with a `fix`.
- Edit the human's comment text.
- Put `-->` inside a comment body (write `--\>`).

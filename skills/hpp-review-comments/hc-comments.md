# hc: inline review comments

Markdown files can carry review comments written by a human in the hpp file viewer. They are HTML
comments, so rendered markdown is unchanged, and they sit next to the text they discuss, so you
read them in place while editing the file.

## What a thread looks like

```markdown
The system MUST <!--hc:a id=c1-->retry on 5xx<!--hc:/ id=c1--> within 30s.
<!-- hc:body id=c1 author=elio ts=2026-09-15T10:12Z directive=reply
     quote="retry on 5xx"
     ctx="The system MUST|within 30s."
     : Should this also cover 429? And is 30s per-attempt or total? -->
<!-- hc:body id=c1 author=agent ts=2026-09-15T10:20Z reply-to=c1
     : 429 is covered by the rate-limit path; 30s is total. -->
```

- **Anchor**: `<!--hc:a id=c1-->` … `<!--hc:/ id=c1-->` wraps the commented text. Tables and code
  blocks never contain markers; their comments use `cell=` or `quote=` instead.
- **Body**: `<!-- hc:body … -->` on its own lines, right after the block that holds the anchor.
  Bodies that share an `id` form one thread, in file order.
- **Attributes**: `id`, `author`, `ts` (UTC), `directive`, `reply-to`, `status`, and on the first
  body `quote` (the anchored text), `ctx` (text before `|` after), or `cell="<row key>|<column
  header>"` for a table cell (`key#2` for a repeated row key). The comment text follows the `:`.

## Directives: what the human wants from you

| `directive` | Do this |
|---|---|
| `reply` (also when omitted) | Answer with a reply body. **Do not change the file's content.** |
| `fix` | Change the file to address the comment, then **remove the whole thread** (both markers and every body). Its disappearance is the done signal. |
| `discuss` | Answer with a reply body that proposes an approach. **Change nothing** until the human agrees in a later comment. |

## Rules

1. Before editing a markdown file, read every `hc:body` in it and act on each open thread by its
   directive.
2. Reply by adding a body **directly beneath the thread's last body**:
   `<!-- hc:body id=<id> author=agent ts=<UTC now> reply-to=<id>` then `     : <answer> -->`.
3. Mark a thread done on your reply with `status=resolved` (for `reply` and `discuss` threads).
4. Never delete or move `hc:` markers or bodies, except removing a `fix` thread you completed.
   Never edit a human's comment text.
5. When you rewrite a sentence that holds an anchor, keep the anchored words and the marker pair
   around them. If the words must go, leave the bodies; the viewer re-anchors by `quote`/`ctx` or
   shows the thread as orphaned.
6. Inside a comment, write `--\>` instead of `-->`.

## Finding open comments

```bash
grep -n "hc:body" path/to/file.md          # every comment line
grep -rln "hc:body" --include='*.md' .     # files with comments
```

Instruction that is enough after this document is loaded: *"address the hc comments in `<file>`"*.

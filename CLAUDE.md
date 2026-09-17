@AGENTS.md

# hpp fork (fsautomata/herdr, branch `hpp`)

This checkout is **hpp** ("herdr++"), a personal fork of herdr. Upstream does not accept outside
pull requests; never push to or open PRs against `herdrdev/herdr` (its push URL is disabled).
AGENTS.md above still applies, except where this section overrides it.

## What the fork adds

| Area | Where |
|---|---|
| Binary `hpp`, app dir `hpp`/`hpp-dev`, `hpp --version` = `<upstream>+hpp.<rev>`, self-update and remote release downloads disabled | `src/fork.rs`, `Cargo.toml` `[[bin]]`, `src/update.rs`, `src/remote/attach.rs` |
| File API: `file.list`, `file.read`, `file.write` (sha256 precondition, atomic), `git.diff` | `src/api/schema/files.rs`, `src/file_access.rs` |
| File viewer overlay (`prefix+f`): browse, view, markdown render, diff | `src/client/shell/file_viewer*.rs` |
| Source-mapped document model and markdown renderer | `src/ui/document.rs`, `src/ui/markdown.rs` |
| `hc:` inline review comments (codec, anchoring, re-anchoring) | `src/hc.rs` |
| Agent protocol + skill (`hpp protocol` prints it) | `skills/hpp-review-comments/` |

Keep fork changes in new files with small hook points into upstream files, so rebases stay cheap.
Mark hook points with an `hpp fork:` comment when the reason is not obvious.

## Build and test (inside the `claude-dev` container)

The clone lives at `/workspace/herdr`. Build output and Zig caches must stay off the `/workspace`
share (Unraid FUSE breaks Zig's atomic renames):

```bash
cd /workspace/herdr
export CARGO_TARGET_DIR=/root/herdr-target
cargo build --release                         # /root/herdr-target/release/hpp
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings
```

The test gate is upstream's `just test` minus three tests that cannot pass on the Unraid kernel or
as root, and minus the bun-based recipes. It lives in the infra repo as
`plans/claude-code-infra/scripts/hpp-test-gate.sh`; run it from the laptop with
`ssh unraid 'docker exec -i claude-dev bash -s' < scripts/hpp-test-gate.sh`. Every commit on `hpp`
must pass it plus fmt and clippy.

Contract tests you will meet:
- New API methods: add to `CLIENT_SHELL_METHODS` (sorted), pin their shape digest in
  `advertised_client_shell_method_shapes_stay_at_the_v1_contract` (never edit the v1 fixture), and
  regenerate `docs/next/api/herdr-api.schema.json` with
  `HERDR_UPDATE_API_SCHEMA=1 cargo nextest run -E 'test(generated_protocol_schema_artifact_is_current)'`.
- New config keys: `docs/next/website/src/data/config-reference.json` must list them.
- `/docs/*` is gitignored upstream except whitelisted paths; put fork docs elsewhere.

## Commits and rebases

- Lowercase conventional commits (`feat(viewer): ...`), as upstream.
- Rebase `hpp` onto the latest upstream **release tag** monthly, not onto every upstream commit;
  resolve conflicts at the hook points, rerun the gate, then bump `FORK_REVISION` in `src/fork.rs`
  when shipping a new build.

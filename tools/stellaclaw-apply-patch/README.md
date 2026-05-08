# stellaclaw-apply-patch

Standalone patch applier used by Stellaclaw local and remote tool runtimes.

It reads a patch from stdin by default, applies it under a workspace directory,
and writes a JSON result to stdout.

```bash
stellaclaw-apply-patch --workspace /path/to/workspace --format auto < patch.txt
```

Codex patch format is implemented without external dependencies. Unified diff
format delegates to `git apply`.

This package has its own version in `tools/stellaclaw-apply-patch/VERSION`.

# stellaclaw-apply-patch

Standalone patch applier used by Stellaclaw local and remote tool runtimes.

It reads a patch from stdin by default, applies it under a workspace directory,
and writes a JSON result to stdout.

```bash
stellaclaw-apply-patch --workspace /path/to/workspace --format auto < patch.txt
```

Codex patch format is implemented without external dependencies. Unified diff
format delegates to `git apply`.

Linux release assets use musl targets and keep the existing `linux-x64` /
`linux-arm64` platform names, so the binaries are statically linked and do not
depend on the remote host glibc version.

This package has its own version in `tools/stellaclaw-apply-patch/VERSION`.

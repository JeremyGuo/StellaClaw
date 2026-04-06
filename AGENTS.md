# AGENTS.md

This file defines project-specific rules for LLM-assisted development in this repository.

## Versioning Responsibilities

There are two independent version tracks in this project:

- `config` version
  Managed in `agent_host/src/config.rs` and the corresponding `agent_host/src/config/v0_x.rs` loaders.
- `workdir` version
  Managed in `agent_host/src/upgrade/mod.rs` and the corresponding `agent_host/src/upgrade/v0_x.rs` upgrade steps.

Do not assume the `config` minor version and the `workdir` minor version must always match. They are logically independent even if they currently share the same visible number.

## Rule 1: Workdir Schema Changes

If a change affects the schema or layout of persisted files inside the runtime workdir:

- add or update the appropriate upgrade step in `agent_host/src/upgrade/v0_x.rs`
- register it in `agent_host/src/upgrade/mod.rs`
- preserve sequential upgrade behavior

Workdir upgrades must run from old to new in order. Do not introduce a migration that assumes skipping intermediate versions is safe unless the ordered chain still works.

## Rule 2: Config Schema Changes

If a change affects config structure or config serialization format:

- update the config version in `agent_host/src/config.rs`
- add or update the relevant loader in `agent_host/src/config/v0_x.rs`
- bump the config `MINOR` version

Keep old config loaders working so existing saved configs can still be loaded and upgraded.

## Rule 3: Top-Level VERSION Bump Policy

The repository root `VERSION` file must follow this policy:

- if Rule 2 applies, bump the repository `MINOR` version and reset `PATCH` to `0`
- otherwise, if only Rule 1 applies, bump `PATCH` by `1`

In short:

- config change => `MINOR + 1`, `PATCH = 0`
- workdir-only change => `PATCH + 1`

## Rule 4: Changelog Maintenance

Whenever Rule 1 or Rule 2 applies:

- update the root `VERSION` changelog
- describe the schema/upgrade impact clearly
- mention whether the change affects `config`, `workdir`, or both

Do not leave schema-affecting changes undocumented.

## Practical Checklist

Before pushing schema-related changes, verify:

- whether `config` changed
- whether `workdir` changed
- whether upgrade code was added where required
- whether `VERSION` was updated according to the policy above
- whether the changelog explains the migration clearly

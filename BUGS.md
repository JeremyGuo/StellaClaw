# BUGS.md

Confirmed project bugs found by code inspection and targeted test runs. These are intentionally documented only; no implementation changes are included here.

## 2026-04-21

### Web `/api/send` accepts conversation keys that create/delete reject

- Status: open
- Severity: medium
- Area: Web channel conversation routing
- Evidence:
  - `agent_host/src/channels/web.rs:395` normalizes conversation keys for `create_conversation`, and `agent_host/src/channels/web.rs:411` requires the same normalization for `delete_conversation`.
  - `agent_host/src/channels/web.rs:804` rejects empty, over-128-byte, and control-character conversation keys.
  - `agent_host/src/channels/web.rs:471` handles `/api/send` by filtering only blank values and otherwise using `body.conversation_key` directly in the `ChannelAddress` and `create_web_conversation` call.
  - The bundled client initializes `currentConversation` directly from the URL or local storage at `agent_host/src/channels/web_static/app.js:20`, then posts it to `/api/send` at `agent_host/src/channels/web_static/app.js:676`.
  - Persisted session directories are derived from the raw `conversation_id` by `agent_host/src/session.rs:1501` and `agent_host/src/session.rs:1525`, so an invalid key accepted by `/api/send` can become a real persisted conversation/session identity.
- Impact:
  - A crafted Web request or URL can create conversations that cannot be created or deleted through the normal validated conversation mutation endpoint.
  - Long or control-character conversation ids can reach persisted session metadata and path generation despite the channel already having a validator intended to block them.
- Suggested regression test:
  - POST `/api/send` with a 129-character `conversation_key` and assert it returns `400 Bad Request`, matching `/api/conversations`.

### Web attachment delivery can drop valid attachments when multiple session roots share a conversation

- Status: open
- Severity: medium
- Area: Web channel attachment rendering
- Evidence:
  - `agent_host/src/server/messaging.rs:986` persists assistant attachment tags under the current session's `root_dir/outgoing` directory.
  - `agent_host/src/session.rs:1525` builds separate session roots for the same conversation, and the test at `agent_host/src/session.rs:3230` confirms foreground and background sessions live under sibling `foreground` and `background` directories.
  - `agent_host/src/channels/web.rs:843` asks `find_session_root` for a single session root, then only tries `strip_prefix` against that one root.
  - `agent_host/src/channels/web.rs:1004` implements `find_session_root` by returning the first persisted session whose channel and conversation match, while `agent_host/src/session.rs:1537` sorts all roots before iteration.
  - `agent_host/src/channels/web.rs:186` and `agent_host/src/channels/web.rs:191` silently drop attachment refs when `web_attachment_ref` fails, while `agent_host/src/channels/web.rs:207` and `agent_host/src/channels/web.rs:208` still report counts from the original outgoing message.
- Impact:
  - A foreground reply with attachments can lose its Web attachment links after any background session root for the same conversation sorts first.
  - A background reply with attachments can also lose links when another matching session root is returned first.
  - The WebSocket event can say attachments/images exist while sending empty `images`/`attachments` arrays, leaving the browser with nothing to render.
- Suggested regression test:
  - Create foreground and background session roots for one Web conversation, place an outgoing attachment under the non-first root, and assert `WebChannel::send` emits a `WebAttachmentRef` instead of dropping it.

### Web command and helper replies are live-only and disappear from transcript history

- Status: open
- Severity: medium
- Area: Web channel transcript persistence
- Evidence:
  - `agent_host/src/transcript.rs:64` defines an `AssistantMessage` transcript entry type, and `agent_host/src/transcript.rs:373` exposes `record_assistant_message` with `ShowOptions` support.
  - The test at `agent_host/src/transcript.rs:601` confirms assistant-message skeletons are expected to preserve Web option buttons.
  - Production command/help/status/model-selection replies are sent through `send_channel_message`, for example `agent_host/src/server/command_routing.rs:23`, `agent_host/src/server/command_routing.rs:121`, and `agent_host/src/server/command_routing.rs:392`.
  - `agent_host/src/server/security.rs:527` implements `send_channel_message` as a direct `channel.send(address, message)` call and does not record `TranscriptEntry::AssistantMessage`.
  - `agent_host/src/channels/web.rs:185` emits these replies as live `OutgoingMessage` WebSocket events, while `agent_host/src/channels/web.rs:714` reloads history only from `list_web_transcript`.
  - The bundled Web client renders live `outgoing_message` events at `agent_host/src/channels/web_static/app.js:1066`, but after refresh it renders only transcript skeletons from `/api/transcript` at `agent_host/src/channels/web_static/app.js:711`.
- Impact:
  - Web users can see `/help`, `/status`, model selection prompts, unknown-command replies, and other Host helper messages live, but those assistant messages are absent after browser refresh or transcript reload.
  - `ShowOptions` buttons sent outside the model-call transcript path are particularly fragile: they render live but are not recoverable from Web history despite the transcript schema supporting them.
- Suggested regression test:
  - Send a Web `/agent` or `/help` command, then reload `/api/transcript` for that conversation and assert an `assistant_message` entry with the sent text/options is present.

### `exec_start` manual SSH and direct-read policy can be bypassed with compound shell commands

- Status: open
- Severity: high
- Area: AgentFrame tool policy enforcement
- Evidence:
  - `FEATURES.md:39` says direct shell read/search commands are rejected on the simple-command path, and `FEATURES.md:42` says `exec_start` rejects manual `ssh ...` command strings.
  - `agent_frame/src/tooling/exec.rs:86` implements `shell_command_head` by trimming and taking only the first whitespace-separated token.
  - `agent_frame/src/tooling/exec.rs:94` then checks only that token against blocked commands including `cat`, `grep`, `find`, `head`, `tail`, `ls`, and `ssh`.
  - `agent_frame/src/tooling/exec.rs:1031` applies this guard once to the raw command string before the command is accepted.
  - The worker executes the full string through `sh -c 'eval "$AGENT_FRAME_EXEC_COMMAND"'` at `agent_frame/src/tool_worker.rs:982`, so commands such as `cd /tmp && ssh host uptime`, `env ssh host uptime`, `sh -c 'ssh host uptime'`, or `true; cat README.md` run even though their effective operation is blocked when it appears as the first token.
  - Existing coverage at `agent_frame/src/tooling/exec.rs:1287` only asserts simple first-token cases such as `cat ...`, `/usr/bin/grep ...`, `find ...`, and `ssh ...`.
- Impact:
  - The remote execution policy can be bypassed by wrapping manual SSH in ordinary shell syntax.
  - The direct filesystem/search tool policy can also be bypassed by putting a harmless command before the blocked command.
  - Tool schemas and prompts tell models that these operations are rejected, but the runtime only rejects a narrow first-token shape.
- Suggested regression test:
  - Assert `exec_start` rejects compound/wrapped forms such as `cd /tmp && ssh dev uptime`, `env ssh dev uptime`, `sh -c 'ssh dev uptime'`, `true; cat README.md`, and `cd src && grep foo lib.rs`.

### Background replies with attachments are not inserted into foreground history or context

- Status: open
- Severity: medium
- Area: Background agent delivery / attachment persistence
- Evidence:
  - `FEATURES.md:89` says a main background agent final user-facing reply is delivered to the foreground conversation, and `FEATURES.md:90` says the same final reply is inserted into the Main Foreground Agent stable context.
  - `agent_host/src/server/messaging.rs:958` extracts `<attachment>...</attachment>` tags from assistant text, and `agent_host/src/server/messaging.rs:971` persists each outgoing attachment before adding it to `outgoing.images` or `outgoing.attachments`.
  - `agent_host/src/server/session_runner.rs:1607` logs background reply attachment counts from `outgoing.attachments.len() + outgoing.images.len()`, confirming background final replies can carry attachments.
  - `agent_host/src/server/session_runner.rs:1792` delivers the background reply to the foreground actor, but the `SessionActorMessage` uses `text: outgoing.text.clone()` at `agent_host/src/server/session_runner.rs:1805` and `attachments: Vec::new()` at `agent_host/src/server/session_runner.rs:1806`.
  - `agent_host/src/session.rs:1116` inserts actor-message text into stable context only as a text `ChatMessage`, and `agent_host/src/session.rs:1135` pushes the actor-message attachments into visible history; because delivery passed an empty vector, those attachments are lost from foreground persistence.
  - `agent_host/src/server/session_runner.rs:1814` still sends the original `OutgoingMessage` to the live channel, so the live delivery and the foreground persisted view diverge.
- Impact:
  - A Web/Telegram user can receive a background-generated file or image live, but later foreground turns do not have the attachment represented in their stable context.
  - The foreground visible history/checkpoint also lacks the attachment metadata, so refresh/reload and later session inspection can miss what the background agent actually delivered.
  - If the background final answer consists only of attachment tags, `outgoing.text` can be empty and nothing meaningful is inserted into foreground stable context at all.
- Suggested regression test:
  - Complete a background turn whose final assistant text contains an attachment tag, deliver it to the foreground actor, and assert the foreground history/checkpoint preserves the attachment metadata and a stable context representation.

### `ChannelAddress::session_key` can collide when ids contain the `::` delimiter

- Status: open
- Severity: high
- Area: Conversation/session routing keys
- Evidence:
  - `agent_host/src/domain.rs:16` builds the logical key as `format!("{}::{}", self.channel_id, self.conversation_id)`.
  - `ConversationManager` uses that string key for creation and lookup at `agent_host/src/conversation.rs:140` and `agent_host/src/conversation.rs:163`.
  - `SessionManager` uses the same string key for foreground actor creation and lookup at `agent_host/src/session.rs:2129` and `agent_host/src/session.rs:2142`.
  - Web conversation keys are only trimmed and checked for length/control characters at `agent_host/src/channels/web.rs:804`; the delimiter `::` is allowed.
  - Channel ids come from config strings such as `agent_host/src/config.rs:70` and `agent_host/src/config.rs:78`, and runtime config validation at `agent_host/src/config.rs:1289` does not reject ids containing `::`.
- Impact:
  - Distinct addresses can map to the same routing key, for example `{channel_id: "web", conversation_id: "a::b"}` and `{channel_id: "web::a", conversation_id: "b"}` both become `web::a::b`.
  - Conversation settings, foreground session actors, pending restart notices, cron matching, and persisted session restore can be read from or overwritten by the wrong address.
  - A Web conversation key crafted with `::` can participate in collisions with another configured channel whose id contains the same delimiter.
- Suggested regression test:
  - Construct two distinct `ChannelAddress` values whose current `session_key()` strings collide, then assert conversation/session managers keep them separate after the key representation is fixed.

### Duplicate channel ids in config silently overwrite earlier channels at startup

- Status: open
- Severity: medium
- Area: Host config validation / channel startup
- Evidence:
  - `agent_host/src/config.rs:1289` validates that at least one channel exists but does not check for duplicate channel ids.
  - Server initialization builds `channels`, `web_channels`, `telegram_channel_ids`, and `command_catalog` as maps/sets keyed by channel id at `agent_host/src/server.rs:2102`.
  - Each channel branch inserts directly with `channels.insert(id, ...)` at `agent_host/src/server.rs:2111`, `agent_host/src/server.rs:2117`, `agent_host/src/server.rs:2122`, `agent_host/src/server.rs:2127`, and `agent_host/src/server.rs:2147`.
  - The TUI config editor does reject duplicate channel ids at `agent_host/src/config_editor.rs:2377` and `agent_host/src/config_editor.rs:2388`, so the invariant is known at the editor layer but not enforced for hand-written or migrated configs.
- Impact:
  - A config file with two channels using the same id loads without validation failure, but only the later channel remains reachable through the runtime maps.
  - The overwritten channel can still have side effects during construction before being dropped, while command catalogs and channel-specific sets may represent a mix of old/new channel intent.
  - Users can lose a channel silently instead of getting a clear startup error.
- Suggested regression test:
  - Load a config containing two channels with the same `id` and assert config validation fails before `Server::new` starts inserting channels into runtime maps.

### Outgoing attachments with the same basename overwrite each other

- Status: open
- Severity: medium
- Area: Assistant attachment persistence
- Evidence:
  - `agent_host/src/server/messaging.rs:899` preserves every `<attachment>...</attachment>` reference as a separate outgoing attachment candidate.
  - `agent_host/src/server/messaging.rs:970` persists each candidate in order.
  - `agent_host/src/server/messaging.rs:993` chooses the persisted filename with `attachment.path.file_name()` only.
  - `agent_host/src/server/messaging.rs:998` writes every persisted attachment to `session.root_dir/outgoing/<file_name>`, and `agent_host/src/server/messaging.rs:999` copies without checking whether that destination was already used in the same reply.
  - `agent_host/src/server/messaging.rs:1006` returns the persisted path, so two source files like `dir-a/report.pdf` and `dir-b/report.pdf` both become outgoing attachments pointing at the same `outgoing/report.pdf` path after the second copy overwrites the first.
- Impact:
  - A single assistant reply containing multiple files/images with the same basename can deliver duplicate links to the last copied file while silently losing the earlier file contents.
  - This affects both foreground replies and background/user_tell paths that use `build_outgoing_message_for_session`.
  - The transcript/live message can show multiple attachments, but users receive corrupted attachment identity.
- Suggested regression test:
  - Build an outgoing message with `<attachment>a/report.txt</attachment>` and `<attachment>b/report.txt</attachment>` containing different bytes, then assert the resulting outgoing attachment paths are distinct and preserve both contents.

### `workspace_mount` can place mounts outside the workspace `mounts/` directory

- Status: open
- Severity: high
- Area: Workspace mount path handling
- Evidence:
  - The `workspace_mount` tool exposes an optional free-form `mount_name` string at `agent_host/src/server/extra_tools.rs:81`.
  - `AgentRuntimeView::mount_workspace` only trims the provided value and checks for emptiness at `agent_host/src/server.rs:918`; it does not reject path separators, absolute paths, `.` segments, or `..` segments.
  - `WorkspaceManager::mount_workspace_snapshot` joins the unchecked mount name directly under `owner.mounts_dir` at `agent_host/src/workspace.rs:454`.
  - The materializers then remove any existing file/symlink/directory at that joined path before creating a symlink/copy/placeholder at `agent_host/src/workspace.rs:787`, `agent_host/src/workspace.rs:791`, `agent_host/src/workspace.rs:827`, and `agent_host/src/workspace.rs:831`.
  - The returned display path strips against `session.workspace_root` only after the mount is created at `agent_host/src/server.rs:939`, so it does not prevent the write from escaping `files/mounts/`.
- Impact:
  - A mount name such as `../notes` places or replaces content under the workspace root instead of under `files/mounts/`.
  - A deeper traversal can target paths outside the workspace root, depending on the relative depth and filesystem permissions.
  - Because existing paths are removed before materialization, this can delete or replace user workspace content even though the tool is documented as creating a read-only historical workspace mount.
- Suggested regression test:
  - Call `workspace_mount` with a `mount_name` containing `../` and assert it is rejected before any filesystem path is removed or created outside `workspace.files_dir/mounts`.

### Moving workspace content with duplicate basenames overwrites earlier files

- Status: open
- Severity: medium
- Area: Workspace content move
- Evidence:
  - `WorkspaceManager::move_contents_between_workspaces` resolves each requested source path independently at `agent_host/src/workspace.rs:578`.
  - For every source, it builds the target as `target_base.join(source_path.file_name())` at `agent_host/src/workspace.rs:585`, discarding the source's parent directory structure.
  - `copy_path_recursive` ultimately uses `fs::copy(source, target)` for files at `agent_host/src/workspace.rs:992` without checking whether `target` already exists.
  - After each copy, the original source is removed at `agent_host/src/workspace.rs:591` and `agent_host/src/workspace.rs:595`.
- Impact:
  - Moving `a/report.txt` and `b/report.txt` into the same target directory silently overwrites the first copied file with the second.
  - Both source files are then deleted, leaving only one target file and losing the other file's contents.
  - The returned `moved_paths` can report two successful moves even though only one basename survived in the destination.
- Suggested regression test:
  - Move two source files with the same basename and different contents into the same target directory, then assert the operation either rejects the collision or creates distinct target paths without data loss.

### `workpath_add` can persist remote hosts that later tools reject and cannot remove

- Status: open
- Severity: medium
- Area: Remote workpath validation
- Evidence:
  - The `workpath_add` tool accepts a free-form `host` at `agent_host/src/server/extra_tools.rs:155` and routes it to `AgentRuntimeView::add_remote_workpath` at `agent_host/src/server/extra_tools.rs:166`.
  - `ConversationManager::add_remote_workpath` validates with `validate_remote_workpath` at `agent_host/src/conversation.rs:338`.
  - `validate_remote_workpath` trims host/path/description and rejects only empty or `local` hosts at `agent_host/src/workpath.rs:32`; it does not call `validate_remote_workpath_host`.
  - The stricter validator rejects placeholders, leading dashes, whitespace, shell metacharacters, and path separators at `agent_host/src/workpath.rs:67`.
  - `workpath_modify` and `workpath_remove` use that stricter validator through `ConversationManager::modify_remote_workpath` and `ConversationManager::remove_remote_workpath` at `agent_host/src/conversation.rs:358` and `agent_host/src/conversation.rs:372`.
  - AgentFrame remote tool execution also rejects unsafe remote hosts at `agent_frame/src/tooling/remote.rs:25`.
- Impact:
  - `workpath_add` can persist hosts such as `dev host`, `dev/repo`, or `-oProxyCommand=bad` into conversation settings.
  - The immediately attempted AGENTS.md load fails, future remote-capable tools reject the stored alias, and the prompt can still include the invalid workpath as durable context.
  - The same invalid host cannot be modified or removed with the normal `workpath_modify`/`workpath_remove` tools because those paths use the stricter validator.
- Suggested regression test:
  - Assert `validate_remote_workpath` rejects the same unsafe host examples already covered by `validate_remote_workpath_host`, and assert `workpath_add` cannot persist them.

## Verification Run

- `cargo test channels::telegram --lib` in `agent_host`: passed.
- `cargo test tooling::tests --lib` in `agent_frame`: passed.
- `cargo test --lib` in `agent_host`: passed.
- `cargo test --lib` in `agent_frame`: passed.
- `cargo test --test integration` in `agent_host`: passed.
- `cargo test --test integration` in `agent_frame`: passed.
- `cargo test main_agent_timeout --lib` in `agent_host`: passed.
- `cargo test channels::web --lib` in `agent_host`: passed.
- `cargo test parses_set_api_timeout_command_argument --lib` in `agent_host`: passed.
- `cargo test latest_config_loads_chat_web_search_alias --lib` in `agent_host`: passed.
- `cargo test assistant_message_skeleton_preserves_options_without_detail_payloads --lib` in `agent_host`: passed.
- `cargo test outgoing_message_serializes_show_options --lib` in `agent_host`: passed.
- `cargo test bundled_web_client_handles_ime_and_authoritative_user_echo --lib` in `agent_host`: passed.
- `cargo test disabled_agent_models_do_not_block_config_loading --lib` in `agent_host`: passed.
- `cargo test idle_compaction_pauses_and_opens_agent_selection_when_model_disappears --lib` in `agent_host`: passed.
- `cargo test main_agent_config_normalizes_legacy_file_tool_names --lib` in `agent_host`: passed.
- `cargo test direct_read_command_guard_catches_simple_shell_reads --lib` in `agent_frame`: passed.
- `cargo test foreground_and_background_session_roots_are_nested_by_conversation_and_kind --lib` in `agent_host`: passed.
- `cargo test idle_actor_tell_persists_and_drains_mailbox --lib` in `agent_host`: passed.
- `cargo test conversation::tests --lib` in `agent_host`: passed.
- `cargo test extracts_multiple_attachments_and_strips_tags --lib` in `agent_host`: passed.
- `cargo test web_channel_resolves_literal_auth_and_ignores_blank_values --lib` in `agent_host`: passed.
- `cargo test telegram_channel_config_loads_without_commands_field --lib` in `agent_host`: passed.
- `cargo test repository_telegram_templates_omit_commands_array --lib` in `agent_host`: passed.
- `cargo test legacy_external_web_search_is_ignored_during_upgrade --lib` in `agent_host`: passed.
- `cargo test main_agent_config_ignores_legacy_model_field --lib` in `agent_host`: passed.
- `cargo test latest_templates_omit_legacy_model_and_external_search_fields --lib` in `agent_host`: passed.
- `cargo test workspace --lib` in `agent_host`: passed.
- `cargo test workspace_content --lib` in `agent_host`: passed.
- `cargo test workpath --lib` in `agent_host`: passed.

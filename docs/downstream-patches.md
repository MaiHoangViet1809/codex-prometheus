# Downstream app-server patches

This fork keeps app-server changes independently reviewable. The combined
integration branch carries them together; each proposed fix has a separate branch.
Upstream remains `openai/codex`. Existing fork `main` is preserved.

Upstream currently requests issue reports rather than external code PRs; see
[`docs/contributing.md`](contributing.md). The catalog-refresh request is tracked
in [openai/codex#49930](https://github.com/openai/codex/issues/49930), with a
[current router reproduction](https://github.com/openai/codex/issues/49930#issuecomment-6451969496).
Separate feature branches remain useful for review and future upstream adoption.

## Idle dynamic-tool declarations

Embedded hosts can change which tools they expose while retaining a conversation.
The host must be able to replace declarations when resuming an idle thread, then
verify what the server actually applied. A process restart must preserve that
selection rather than silently reverting to the thread's initial declarations.

The patch adds optional `thread/resume.dynamicTools`: omission preserves the
current declarations, an empty list clears them, and a nonempty list replaces
them. Active-thread mutations are refused. The response reports a canonical
SHA-256 `dynamicToolsDigest`. Applied declarations persist through cold resume,
compaction, frozen-boundary fork and revert semantics. This is an additive wire
and persistence change; generated protocol fixtures must match source.

## Custom model-catalog refresh

External catalog publishers update `model_catalog_json` as available models or
visibility settings change. A running app-server previously retained its startup
snapshot until an account transition, even when a publisher had already written
the new file successfully.

The patch retains the resolved source path and rereads that file at the shared
app-server catalog boundary. It validates the complete document before replacing
the catalog/manager pair under the existing serialized lock. Unchanged content
keeps the manager; invalid, empty or unavailable files keep the last valid
snapshot. No credential cycling or unrelated config reload is required. Existing
thread snapshots retain their original routes. Provider-policy checks still run.

This makes a new `model/list` call see the current valid catalog. It does not
force a desktop renderer to issue that call. Renderer polling, window-focus
refresh or a future catalog-change notification remains a client responsibility.

## Ownership and updates

Provider routing, native WebSocket forwarding and foreign-history normalization
remain in the separate router repository. They are not Codex app-server patches.

Track each app-server change as ordinary commits. Fetch upstream, replay the
feature branches onto a pinned upstream revision, resolve conflicts and run the
relevant protocol, persistence and catalog tests. Combine verified branches for
integration; remove an individual downstream commit when upstream includes it.
An upstream PR being open does not imply that a future release contains it.

Source updates and runtime activation are distinct operations. Build one shared
artifact and activate it explicitly for each consumer, recording its commit and
checksum. Preserve each consumer's auth/state directories. Keep the previously
verified binary for rollback. An application update may replace its bundled
binary; do not patch the application bundle or assume an override survives until
that consumer's supported launch path has been verified.

# xrm fork of gridcraft

This is `andjil/gridcraft`, a fork of [storytold/gridcraft](https://github.com/storytold/gridcraft) that is
**not** meant to be merged upstream. Branch `main` tracks upstream untouched; branch `xrm-host`
carries the fork's change on top of it and is what gets built.

## What the fork changes

Upstream already ships the bridge (`apps/gridcraft-web/src/host.rs`, `?host=parent`). This fork adds the command channel and the save confirmations to it: a self-contained section at the end of that file marked `xrm host extension`, plus one field, one line in `new`, one in `post_ready`, one in `sync` and five in `listen`.

Nothing else: no asset, no crate below the web app, no behaviour outside `?host=parent`. Without
that flag the app is upstream's.

## Why

The xrm platform embeds the web build in a sandboxed same-origin iframe and stores documents
itself. The bridge lets the page open a file into the editor, receive every file the editor would
download, drive the editor through its own commands (`<app>:command` → `<app>:result`) and confirm
or refuse a stored save (`<app>:saved` / `<app>:save-failed`), which is what a host-driven
autosave needs. The protocol: `docs/craft-host-protocol.md` in the xrm repository (rxrmserver).

## Pulling upstream

From the rxrmserver repository: `scripts/craft/sync.sh fetch`, `rebase [tag]`, `check`, `build`,
`patches`, `push`. The fork touches upstream files only in the few places listed above, so a
rebase conflict can only be there; the bridge itself is in files upstream does not have (or, in
GridCraft, in a separate section). The bridge uses only the public API the app's own control
channel, CLI and MCP use (`app.run`, the `Services` hooks), the most stable surface the project has.

## Licence and marks

MIT OR Apache-2.0, as upstream. The ArtCraft and gridcraft marks are upstream's trademarks; a build
of this fork must not use them (the build step drops the brand icon).

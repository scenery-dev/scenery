---
name: authoring-with-scenery
description: Use when the user asks to create, write, check or share a skill, or to set up, add or install an MCP server. Routes the work through Scenery's tools so the skill or server is governed from its first save instead of being hand-built.
license: Proprietary. Provided by the Scenery runner.
---

# Authoring with Scenery

Scenery governs the AI tools on this machine. When the user wants a new skill
or a new MCP server, use Scenery's tools instead of writing files or editing
client config by hand. This is guidance, not a lock: if a tool is missing or
fails, tell the user what happened before falling back to a manual path.

## When the user asks you to make a skill

1. Agree the name (a short lowercase folder name such as `meeting-notes`), a
   one-sentence description of when the skill should be used, and the
   instructions the skill should follow.
2. Call `scenery_create_skill` with `name`, `description` and `instructions`.
   It writes the skill into `~/.agents/skills/<name>/`, registers it as the
   user's own private skill and links it for Claude Code. It is managed from
   its first save. It is private: nobody else can see it.
3. Edit the files in that folder as the user refines the skill. Keep
   `SKILL.md` frontmatter limited to `name`, `description` and `license`.
   Never put secrets, API keys, tokens or `.env` files inside a skill folder.
4. Call `scenery_check_skill` with the skill name before telling the user the
   skill is done. It checks the layout, looks for secrets and previews the
   security scan. Fix everything it reports.

## When the user wants to share a skill with their company

Sharing is the user's choice, never yours. Only when they ask:

1. Call `scenery_share_skill` with the skill name. It checks the skill,
   uploads the current version privately and returns a preview: the catalog
   name, who will see it and the scan result. It also returns a one-time
   confirmation token. A share made from an AI client always goes to IT for
   review first and stays private to the user until IT approves it; say so.
2. Show the preview to the user in plain words and ask whether to go ahead.
   Do not call the second step on your own.
3. Only after the user says yes, call `scenery_share_skill` again with the
   same name and the `confirm_token`. If the skill changed since the preview,
   the token is refused: run the first step again.
4. If the answer is `name_clash`, ask the user for a different catalog name
   and call the first step again with `catalog_name`.

## When the user wants an MCP server

Do not edit `~/.claude.json` or any client config by hand, and do not run
`claude mcp add` for a new server.

1. Call `scenery_mcp_catalog` to see whether the organization already offers
   the server.
2. Call `scenery_mcp_request` with what the user gave you (a package name, a
   command, a URL or a JSON snippet). If the server is already approved it
   becomes available through Scenery; otherwise the request goes to IT for
   review. Tell the user which one happened and, if it is waiting, that it
   needs approval.
3. If the user is not signed in, call `scenery_login` first.
4. Never ask the user to paste secrets into the chat.

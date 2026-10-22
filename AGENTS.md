## Issue Tracking (Multica)

When you discover a problem while working in this repo -- a bug, security issue, or performance issue that is not what the user directly asked you to fix right now -- record it as an issue via the `multica` CLI in the **geometry_pyramid** project (project id `07ece43a-f577-4739-a33f-dbc7ad7afcdd`, workspace id `6265acfe-9f1b-4902-a197-16ea5437f872`). Do not just leave it in chat; chat context does not survive between sessions, Multica issues do.

Check for an existing issue before creating one:

```bash
multica issue list --project 07ece43a-f577-4739-a33f-dbc7ad7afcdd --output json
multica issue search "<keyword>" --output json
```

Filter search results by `project_id`. If an issue already covers the same root cause or same file:line, do not create a duplicate; add a comment instead when there is new detail:

```bash
multica issue comment add <issue-id> --content-file <path-to-scratch-file> --output json
```

Only create a new issue when nothing existing already covers it:

```bash
multica issue create \
  --project 07ece43a-f577-4739-a33f-dbc7ad7afcdd \
  --title "<short, specific summary; include file:line when available>" \
  --description-file <path-to-scratch-file> \
  --priority <low|medium|high> \
  --output json
```

- Use `--description-file` or `--content-file`, not inline `--description` / `--content`, for anything multi-line or containing non-ASCII text.
- The description should cover what is wrong, why it matters, concrete file:line references, and a suggested fix.
- Check `multica label list --output json` and reuse an existing label (`安全`, `性能优化`, `bug`, `新需求`) with `multica issue label add <issue-id> <label-id>`. Only create a new label when none fit.
- This is for incidental findings surfaced during audits, review, or debugging; it does not replace completing the task the user actually asked for.



<!-- OPENWIKI:START -->

## OpenWiki

This repository uses OpenWiki for recurring code documentation. Start with `openwiki/quickstart.md`, then follow its links to architecture, workflows, domain concepts, operations, integrations, testing guidance, and source maps.

The scheduled OpenWiki GitHub Actions workflow refreshes the repository wiki. Do not hand-edit generated OpenWiki pages unless explicitly asked; prefer updating source code/docs and letting OpenWiki regenerate.

<!-- OPENWIKI:END -->

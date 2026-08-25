<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **geometry_pyramid** (586 symbols, 1376 relationships, 50 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> Index stale? Run `node .gitnexus/run.cjs analyze` from the project root — it auto-selects an available runner. No `.gitnexus/run.cjs` yet? `npx gitnexus analyze` (npm 11 crash → `npm i -g gitnexus`; #1939).

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows. For regression review, compare against the default branch: `detect_changes({scope: "compare", base_ref: "main"})`.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `context({name: "symbolName"})`.

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

## Never Do

- NEVER edit a function, class, or method without first running `impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `rename` which understands the call graph.
- NEVER commit changes without running `detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/geometry_pyramid/context` | Codebase overview, check index freshness |
| `gitnexus://repo/geometry_pyramid/clusters` | All functional areas |
| `gitnexus://repo/geometry_pyramid/processes` | All execution flows |
| `gitnexus://repo/geometry_pyramid/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->

<!-- OPENWIKI:START -->

## OpenWiki

This repository uses OpenWiki for recurring code documentation. Start with `openwiki/quickstart.md`, then follow its links to architecture, workflows, domain concepts, operations, integrations, testing guidance, and source maps.

The scheduled OpenWiki GitHub Actions workflow refreshes the repository wiki. Do not hand-edit generated OpenWiki pages unless explicitly asked; prefer updating source code/docs and letting OpenWiki regenerate.

<!-- OPENWIKI:END -->

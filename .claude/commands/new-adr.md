---
description: Scaffold a new ADR in the Arkiv `wiremirage` workspace.
argument-hint: <short-title>
---

Scaffold a new ADR following the WireMirage conventions.

Title: $ARGUMENTS

Workflow:

1. Find the workspace ID by calling `mcp__claude_ai_Arkiv__search_workspaces`
   with query `wiremirage`. Cache the ID for the rest of this command.
2. Read `adrs/index.md` for the conventions and the list of existing ADRs.
3. List `adrs/` (`mcp__claude_ai_Arkiv__list_files` with `prefix: "adrs/"`)
   to determine the next sequential number. Numbers are never reused, even
   for superseded ADRs.
4. Read the most recent ADR (highest number) for an up-to-date format
   example.
5. Slugify the title (lowercase, hyphens, no punctuation) and create
   `adrs/{NNNN}-{slug}.md` with this structure:

   - `# ADR-{NNNN}: {Title}`
   - `**Status:** Proposed`
   - `**Context:**` — what motivated this decision
   - `**Decision:**` — the decision itself
   - `**Consequences:**` — bullet list, intended and accepted
   - `**Alternatives considered:**` — bullets, what was rejected and why
   - `See also:` — pointers to related ADRs and design docs

6. Update `adrs/index.md` to add the new ADR to the Decisions list,
   matching the existing wikilink format.
7. Update the workspace root `index.md` ("Decisions made and why" section)
   to add the new ADR to its per-ADR list, matching the existing wikilink
   format there. Keep both indices in sync — the root list is the
   document-map entry point and shouldn't lag behind `adrs/index.md`.

Do not finalize Status: leave as `Proposed` until the user confirms the
decision is accepted.

If the title is missing or vague, ask the user for the Context and Decision
content before creating the file.

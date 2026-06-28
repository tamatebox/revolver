---
name: create-issue
description: Use when the user asks to open / file / create a GitHub issue on this repo (e.g. "issue を立てて", "open an issue for X"). Drafts in revolver's house style, confirms with the user, then posts via gh.
---

# Create a revolver issue

House style for `gh issue create` in this repo. Always: learn → draft → confirm → post.

## 1. Learn the house style (only if uncertain)

If you have not seen a recent revolver issue this session, sample one:

```sh
gh issue list --limit 10 --state all
gh issue view <recent-number> --json title,body,labels
```

Skip if the conversation already shows the format.

## 2. Draft

Write the body in **English** (matches every existing issue) using these five sections, in order. Omit a section only when it would be empty — do not invent filler.

```markdown
## Problem

What is wrong / missing today. Reference offending code with
[path/to/file.rs:42](src/path/to/file.rs#L42) markdown links so reviewers can
click through. Cite the rule being violated (SPEC.md §x.y, CLAUDE.md bullet,
docs/*.md) when relevant.

## Background

Why this matters and what context the reader needs. Mention related issues
with `#NN`. State explicit unknowns ("unverified on Linn") rather than
hand-waving.

## Proposed change

Concrete edits. Bullet list, each item naming a file or behaviour. If the
change spans assets + docs + code, group them under sub-headings
(`### Asset edits`, `### Doc edits`, etc.).

## Verification

How the change will be confirmed. Distinguish:
- **Unit / cargo test** — the deterministic checks
- **Manual on Linn DSM/2** (or Sonos, per [project_verification_hardware])
  — when real-device observation is the only proof

## Out of scope

What this issue deliberately does **not** cover, so the PR stays bounded.
List sibling concerns + the issue/PR that owns them, if any.
```

### Title

- Under ~80 chars, no trailing period.
- Form: `<area>: <imperative summary>` — e.g. `Search: typo-tolerant fuzzy matching via FTS5 trigram tokenizer`, `Icon set cleanup: remove decorative orange on cat-ar`.
- `area` is the user-visible feature (Search, Browse, Scan, Icon set, Admin UI, …), not the module path.

### Conventions to preserve

- Reference code as `[filename.rs:LINE](src/filename.rs#LLINE)` (relative paths from repo root).
- Reference rules from `CLAUDE.md` / `SPEC.md` / `docs/*.md` by quoting the bullet's key phrase, then linking the file.
- Cross-link prior issues with `#NN`. If unsure of the number, run `gh issue list --search "<keyword>" --state all`.
- Use real-device terminology consistent with CLAUDE.md ("Linn DSM/2", "Linn App", not "Kazoo" — see [project_kazoo_deprecated]).
- Do **not** include AI attribution lines.

## 3. Confirm with the user

Before posting, show the **title + full body + labels** as a markdown block and ask for approval. Issues are public-facing; posting without confirmation is destructive in the "hard to retract cleanly" sense.

Default label: `enhancement` (every recent revolver issue uses it). Use `bug` only when the title clearly describes a defect in shipped behaviour.

## 4. Post

Pass the body via HEREDOC so newlines and backticks survive:

```sh
gh issue create --title "<title>" --label enhancement --body "$(cat <<'EOF'
## Problem
...
EOF
)"
```

Return the issue URL from gh's stdout. Do not follow up with a "summary of what was created" — the URL is the receipt.

## Anti-patterns

- Drafting in Japanese (every existing issue is English; mixing languages breaks search and skim).
- Skipping the confirmation step because the user already approved the *idea*. Approve the *text*.
- Inventing section headings beyond the five above. The shape is the contract.
- Padding `Out of scope` with hypotheticals — list only real adjacent concerns.
- Including ASCII diagrams, tables of file trees, or full code dumps. Link to the file instead.

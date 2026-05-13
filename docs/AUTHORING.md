# VectorHawk Skill Authoring Guide

SKILL.md is the canonical authoring format for VectorHawk skills. A skill is a
Markdown file with a YAML frontmatter block that defines metadata, permissions,
and execution settings. The Markdown body becomes the system prompt.

## Quick Start

```bash
# Scaffold a new skill
vectorhawk skill init my-skill

# Edit the scaffold
$EDITOR my-skill/SKILL.md

# Validate before publishing
vectorhawk skill validate my-skill/

# Publish to the registry
vectorhawk skill publish my-skill/ --registry-url https://app.vectorhawk.ai
```

## File Format

```markdown
---
name: my-skill
description: One sentence describing what this skill does.
version: 0.1.0
publisher: your-publisher-id
vh_permissions:
  network: none
  filesystem: none
  clipboard: none
vh_execution:
  timeout_ms: 30000
  memory_mb: 256
  sandbox: strict
---

# My Skill

Your system prompt goes here.
```

## Required Fields

| Field | Description |
|-------|-------------|
| `name` | Lowercase alphanumeric with hyphens (e.g. `contract-compare`). Must be unique within your publisher namespace. |
| `description` | 10–280 character description shown in the registry and as the MCP tool description. |

## Optional `vh_*` Fields

### `vh_version`
Semver 2.0 string. Defaults to `0.1.0`. Increment when publishing an update.

### `vh_publisher`
Your publisher handle in the VectorHawk registry. Required for `skill publish`.
Authenticate first with `vectorhawk auth login`.

### `vh_permissions`

Controls what the skill runtime is allowed to do:

```yaml
vh_permissions:
  network: none        # none | restricted | full
  filesystem: none     # none | read-only | full
  clipboard: none      # none | read | write | full
```

Default for all fields is `none`. Org policy may block skills that request more
than allowed.

### `vh_execution`

```yaml
vh_execution:
  timeout_ms: 30000    # 1000–600000 ms (default: 30000)
  memory_mb: 256       # 64–8192 MiB (default: 256)
  sandbox: strict      # strict | relaxed | unrestricted
```

### `vh_triggers`

Natural-language phrases the MCP dispatcher uses to route requests to this skill:

```yaml
vh_triggers:
  - compare contracts
  - diff legal documents
  - review contract changes
```

Up to 10 triggers, 3–200 characters each. Case-insensitive; duplicates are
removed automatically.

### `vh_workflow_ref`

Reference an external `workflow.yaml` for multi-step skills:

```yaml
vh_workflow_ref: ./workflow.yaml
```

When omitted, the runtime generates a single `llm` step from the Markdown body.

### `vh_model`

By default (when `vh_model` is omitted), the runner uses MCP sampling — the AI
client's own model (Claude Sonnet, GPT-4o, etc.). Only set `vh_model` when the
skill requires local inference, e.g. for sensitive data that must not leave the
machine:

```yaml
vh_model:
  provider: ollama        # ollama | mcp-sampling
  name: llama3.2
  prefer_local: true      # try Ollama first, fall back to MCP sampling
  min_context_tokens: 8192
```

`prefer_local: false` (the default) means: try MCP sampling first, fall back to
Ollama if the client has no sampling capability.

### `vh_schemas`

Attach JSON Schema files for input/output validation:

```yaml
vh_schemas:
  input: ./schemas/input.json
  output: ./schemas/output.json
```

## The System Prompt

Everything after the `---` closing delimiter is the skill's system prompt.
Keep it focused: describe the task, the expected input, and the required output
format. Reference `vh_schemas.output` for structured output.

## Multi-Step Skills

For skills that need more than one LLM call or transformation, add a
`workflow.yaml` alongside your `SKILL.md`:

```yaml
# workflow.yaml
steps:
  - name: extract
    type: llm
    params:
      prompt_file: prompts/extract.txt
  - name: summarize
    type: llm
    params:
      prompt_file: prompts/summarize.txt
      output_schema: schemas/output.json
```

Then reference it in your frontmatter with `vh_workflow_ref: ./workflow.yaml`.

## Examples

| Directory | What it shows |
|-----------|---------------|
| `examples/skills/minimal/` | Bare minimum — name, description, and a body prompt |
| `examples/skills/medium/` | Permissions, execution limits, triggers, external prompt file |
| `examples/skills/complex/` | Multi-step workflow, full metadata, output schema reference |
| `examples/skills/contract-compare/` | Production-grade skill with workflow and schemas |

## Converting a Legacy Bundle

If you have an existing skill bundle (a directory with `manifest.json`), convert
it to SKILL.md format:

```bash
vectorhawk skill convert ./my-legacy-bundle/
# writes ./my-legacy-bundle-skill-md/SKILL.md
```

Review and edit the generated file before publishing.

## Security Scanning

When `--registry-url` is provided, `skill import` scans the content before
creating a local bundle:

```
✓ Clean   (0 findings)
```

If the scan returns a risky verdict, the import is blocked unless you pass
`--confirm-risky` after reviewing the findings.

## Schema Reference

The full JSON Schema for SKILL.md frontmatter is at:
`crates/vectorhawkd-manifest/schemas/skill_md_frontmatter.json`

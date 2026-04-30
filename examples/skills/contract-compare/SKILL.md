---
name: contract-compare
description: Compare two contracts, summarize changes, and assess risk level.
license: Apache-2.0
vh_version: 0.3.0
vh_publisher: skillclub
vh_permissions:
  network: none
  filesystem: read-only
  clipboard: none
vh_execution:
  sandbox: strict
  timeout_ms: 90000
  memory_mb: 1024
vh_triggers:
  - compare contracts
  - diff legal documents
  - review contract changes
vh_workflow_ref: workflow.yaml
---

# Contract Compare

Compare two contracts, summarize the material differences between them, and
assess the overall risk level of the changes.

This skill is used when a user wants to review a proposed contract revision
against the prior version, understand what changed, and decide whether the
changes are safe to accept. Typical use cases include legal review of
redlines, vendor agreement renewals, and M&A due diligence.

The workflow extracts text from both documents, then runs an LLM comparison
step that produces a structured JSON verdict.

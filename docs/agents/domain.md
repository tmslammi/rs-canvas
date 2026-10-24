# Domain docs

This repository uses a single context.

## Before exploring

- Read [`CONTEXT.md`](../../CONTEXT.md) for the domain vocabulary.
- Read the ADRs in [`docs/adr/`](../adr/) that touch the area you are about to work in.

## Using the vocabulary

When naming a concept in code, an issue, a refactor proposal or a test, use the term defined in `CONTEXT.md`. Do not drift to a synonym the glossary explicitly avoids. If a concept is missing, decide whether it is new language the project needs or a term borrowed from elsewhere.

## ADR conflicts

If a proposed change contradicts an accepted ADR, say so explicitly instead of silently overriding it:

> This contradicts ADR-0007 (workspaces isolate concurrent users), but it is worth reopening because…

# Documentation Site

This directory contains the VitePress documentation website for ccusage.

This fork has no hosted docs site; preview locally with `just docs::dev` and build with `just docs::build`.

## Structure

- `guide/` - user guides and tutorials.
- `public/` - screenshots, static assets, and generated config schema.
- `.vitepress/` - VitePress configuration and theme customization.

The docs build copies `apps/ccusage/config-schema.json` to
`docs/public/config-schema.json` before running VitePress.

## Commands

```sh
just docs::dev
just docs::build
just docs::preview
just docs::typecheck
just fmt
```

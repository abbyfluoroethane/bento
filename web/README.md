# Bento dashboard

The web dashboard (SPEC section 14): Vite + React + TypeScript + Tailwind
CSS, shadcn/ui-style components over Radix primitives, Catppuccin
Latte/Mocha, IBM Plex Mono and IBM Plex Sans self-hosted via @fontsource
(latin subset, woff2, `font-display: swap`).

## Build

```
npm install
npm run build     # tsc --noEmit && vite build → dist/
```

Assets are built here (Node build step) and embedded into `bentod` with
`rust-embed`, served by `crates/dashboard`. The build output directory
(`web/dist`) is committed, not gitignored, so the Rust build never needs
a Node runtime. A build with no assets serves a placeholder that says how
to produce them; the real build always embeds real assets.

## Design tokens

SPEC 14.1 names the shadcn/ui preset `b3DooLR16I`. The hosted preset is
not reachable from this repository's build environment, so
`src/styles/tokens.css` is the committed token file standing in for it
(SPEC 19 requires the tokens to be committed either way). The preset
identifier is recorded there and here. The accent color (Mauve) is
defined in exactly one place: the `--accent` variables in that file.

## Development

`npm run dev` starts Vite with `/api` proxied to `http://localhost:8080`
(a running `bentod`).

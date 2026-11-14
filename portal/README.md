# AI Factory portal

React + TypeScript single-page app for the AI Factory control plane. It talks to the plane only through the HTTP API in `../docs/API.md`.

## Develop

```sh
npm install
npm run dev          # http://localhost:5173, proxies /api, /auth, /healthz to http://localhost:8080
```

Set `FACTORY_PLANE_URL` to proxy to a plane on another address, e.g. `FACTORY_PLANE_URL=http://localhost:9090 npm run dev`.

## Build and test

```sh
npm run build        # type-checks, then writes static assets to dist/ (base '/')
npm test             # vitest run (event merge / paging logic)
```

The plane serves `dist/` as static files with an SPA fallback to `index.html`.

## Layout

- `src/api.ts`: typed API client and types that mirror `docs/API.md`. A 401 redirects to sign-in, and errors show as toasts.
- `src/lib/`: event merging and paging (`events.ts`, unit-tested), the run event stream hook (`useRunEvents.ts`), polling, auth context and formatting.
- `src/components/`: the virtualised event list, question cards, modal, toasts and badges.
- `src/pages/`: sign-in, dashboard, run view and the admin screens (connectors, projects, users, settings, audit, outcomes).

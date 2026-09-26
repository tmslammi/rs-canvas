# Deployment

rs-canvas ships as a static site: the wasm bundle and Vite output are plain files, so any static host works. CI deploys to Cloudflare Pages (`.github/workflows/deploy.yml`).

## Human provisioning steps (one-time)

These steps need a person with a Cloudflare account; the agent cannot do them.

1. **Create a Cloudflare account** (or use an existing one) at <https://dash.cloudflare.com/sign-up>.

2. **Create a Pages project** named `rs-canvas`:
   - Dashboard → Workers & Pages → Create → Pages → *Direct Upload*.
   - Name it exactly `rs-canvas` (the workflow passes `--project-name=rs-canvas`).

3. **Create an API token** with Pages edit permission:
   - Dashboard → My Profile → API Tokens → Create Token → *Custom token*.
   - Permissions: `Account` → `Cloudflare Pages` → `Edit`.
   - Account resources: include the account that owns the Pages project.
   - Copy the token.

4. **Find the Account ID**: Dashboard → Workers & Pages → your account ID is shown in the right sidebar (also in the URL).

5. **Add GitHub repository secrets** (repo → Settings → Secrets and variables → Actions → New repository secret):
   - `CLOUDFLARE_API_TOKEN` — the token from step 3.
   - `CLOUDFLARE_ACCOUNT_ID` — the ID from step 4.

6. **Trigger a deploy**: push to `master` (or run the *Deploy* workflow manually). Pull requests get preview deployments on their own branch URLs.

Without the secrets the workflow fails immediately with a message pointing back here.

## What gets deployed

- `web/dist` produced by `npm run build` (wasm via `wasm-bindgen`, then Vite).
- Production deploys from `master` use the `main` branch on Pages (the production branch of the Pages project).
- Custom domain: Pages → project → Custom domains → add yours once the project exists.

## Verifying a deployment

The deployed page must boot the editor. The CI e2e suite (`web/e2e/smoke.spec.ts`) covers the same behavior against a local preview build; after adding a custom domain you can point the Playwright `baseURL` at it to smoke-test production.

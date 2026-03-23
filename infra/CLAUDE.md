# CLAUDE.md — infra

Terraform configurations for provisioning Forgejo resources.

## Structure

- `modules/forgejo-init/` — Reusable module: creates users, repos, labels, collaborator permissions
- `test/` — Test environment config, references the module with test-specific variables (`test.tfvars`)
- `dev/` — Dev environment config

## Provider

Uses **Lerentis/gitea** Terraform provider. This provider requires username+password auth (not token-only) for the admin user.

## Labels are created via `curl`

The gitea provider has no label resource. Labels are provisioned via `null_resource` + `local-exec` curl commands. Pre-provisioned labels include:
- **Status**: `on-deck`, `blocked`, `on-the-stack`, `in-review`, `changes-requested`, `done`, `failed`, `revoked`, `on-ice`
- **Worker type**: `worker:action`, `worker:interactive`
- **Platform**: `platform:linux`, `platform:macos`, `platform:arm64`
- **Review**: `review:human`, `review:low`, `review:medium`, `review:high`
- **Capability**: `capability:rust`

If you change label names, also update `JobState` string conversions in `crates/types/src/lib.rs`.

## Forgejo app.ini

`forgejo/app.ini` is the base Forgejo configuration. It's copied into `.data/forgejo/gitea/conf/` by `init.sh` before first boot. Includes CORS configuration so the graph viewer (served at `:8080`) can make cross-origin API calls to Forgejo (`:3000`). Also enables Forgejo Actions (`[actions]` section).

## Three service accounts

The system uses three Forgejo identities for audit trail clarity:
- **`workflow-sync`** (variable: `sidecar_login`) — label/dep management (sidecar process)
- **`workflow-dispatcher`** (variable: `dispatcher_login`) — assignee/comment management (sidecar process)
- **`workflow-reviewer`** (variable: `reviewer_login`) — PR reviews, merge, escalation comments (reviewer process — separate container)

All are provisioned by the `forgejo-init` module and added as repo collaborators with write permission.

## Shared runner

A shared `act_runner` container is started by `init.sh` as part of the core infrastructure (alongside CDC, sidecar, reviewer). It handles system-level Forgejo Actions like review workflows (`review-work.yml`). The runner registration token is obtained from the Forgejo admin API during init and written to `.sidecar.env`. Scale with `docker compose up --scale runner=N`.

## Human reviewer account

A **`you`** user (variable: `human_login`) is provisioned as a regular Forgejo account with write collaborator access. This is the escalation target for the automated reviewer — when it decides a PR needs human attention, it adds `you` as a PR reviewer. Log in at `localhost:3000` with the configured password to manually review and merge PRs.

## No webhooks

Webhooks were removed in favor of the CDC pipeline. The sidecar learns about Forgejo changes via the CDC process polling Forgejo's PostgreSQL database, not via webhook callbacks. The CDC also detects merged PRs via PostgreSQL queries (`pull_request` table joined with `issue` to find merged PRs referencing `Closes #N`).

## Worker permissions

Workers are created as Forgejo users with `write` collaborator access on the repo. This gives them permission to push branches, create PRs, and comment on issues, but NOT admin/merge access. Three workers are provisioned by default: `worker-aria`, `worker-blake`, `worker-casey`.

## Running Terraform

Always use `scripts/init.sh` rather than running Terraform directly — the script handles admin user/token creation, fresh Terraform state, worker password setup, sidecar token provisioning, and `.sidecar.env` generation.

## Variables requiring secrets

- `forgejo_admin_token` — passed via `TF_VAR_forgejo_admin_token` env var (created by init.sh)
- `forgejo_admin_password` — passed via `TF_VAR_forgejo_admin_password`
- `sidecar_password` — passed via `TF_VAR_sidecar_password`
- `dispatcher_password` — passed via `TF_VAR_dispatcher_password`
- `reviewer_password` — passed via `TF_VAR_reviewer_password`
- `human_password` — passed via `TF_VAR_human_password`

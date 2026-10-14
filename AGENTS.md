# Agent Guidelines

## Recording project guidelines

When the user gives a generic direction about design, naming, preferences,
where to store things, or similar project-wide conventions:

1. Check whether it is already in this file.
2. If it is not, add it, unless it contradicts an existing guideline.
3. If it contradicts an existing guideline, do not change either one. Ask the
   user which one should apply.

## Language and design

- Implement the project in Rust, using the latest stable edition
  (currently Rust 2024).
- The user interface is a terminal UI (TUI) modelled on mutt.
- Keep the design clean and built on well-established design patterns.
- The design document lives in `docs/DESIGN.md`. Keep it up to date with
  design decisions.
- Project documentation lives in `docs/`.
- Configuration uses mutt's muttrc format. Extend it for Office365 only where
  the existing format is not enough (see `docs/CONFIG-OFFICE365.md`).
- mett reads its own `mettrc` files, never mutt's muttrc files.
- mett ships no Azure AD app registration. Users provide their own app
  (`msgraph_client_id`).

## Naming

- Workspace crates are prefixed with `mett-`.
- Name crates after the concrete technology or service they wrap (for
  example `mett-msgraph` for the Microsoft Graph adapter, not a generic
  name such as `mett-graph`).

## Git workflow

1. Develop each feature on its own branch named `<username>/<feature>` (for
   example `igelado/msgraph-auth`), created from `main`. Commit to that
   branch, never directly to `main`.
2. When the feature is complete, push the branch and open a merge request
   (MR) on GitLab.
3. Make sure all tests pass, and wait for code review.
4. Address every error and review concern on the same branch.
5. Then merge the MR into `main` and delete the feature branch.

GitLab enforces this workflow:

- `main` is protected. Nobody can push to it directly or force-push, and only
  Maintainers can merge.
- An MR can be merged only if its pipeline succeeds. Skipped pipelines don't
  count as successful.
- The source branch is deleted after merge by default.
- An MR can be merged only when all review threads are resolved.
- CI is defined in `.gitlab-ci.yml`. Its jobs run on the shared Linux Docker
  runners (`os/linux`, `type/docker`, `size/medium`). Add new checks there,
  so that they gate merging.
- CodeRabbit reviews every MR automatically, configured in
  `.coderabbit.yaml`. Its comments are review threads, so they must be
  resolved before merging.

## Locally generated files

Do not commit locally generated files. Keep `.gitignore` up to date: whenever
a new tool or skill is used, or temporary or intermediate files or
directories are produced, add matching entries to `.gitignore`.

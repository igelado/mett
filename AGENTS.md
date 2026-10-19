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
- Each crate has a `README.md` describing what it does and how to test it,
  including any manual testing (for example, against real services).
- Configuration uses mutt's muttrc format. Extend it for Office365 only where
  the existing format is not enough (see `docs/CONFIG-OFFICE365.md`).
- mett reads its own `mettrc` files, never mutt's muttrc files.
- mett ships no Azure AD app registration. Users provide their own app
  (`msgraph_client_id`).

## Naming

- Workspace crates are prefixed with `mett-` and live in `crates/<name>/`.
- Name crates after the concrete technology or service they wrap (for
  example `mett-msgraph` for the Microsoft Graph adapter, not a generic
  name such as `mett-graph`).

## Git workflow

The repository lives on Forgejo at
`ssh://git@git.gelado.org:12500/igelado/mett.git`
(web: https://git.gelado.org/igelado/mett).

1. Develop each feature on its own branch named `<username>/<feature>` (for
   example `igelado/msgraph-auth`), created from `main`. Commit to that
   branch, never directly to `main`.
2. After each commit, run a Codex review against `main` and fix every valid
   finding (explain why any finding doesn't apply) until a review comes back
   clean.
3. When the feature is complete, push the branch and open a pull request
   (PR) into `main`. Post the clean Codex review result as a PR comment.
4. Make sure all checks pass, and wait for code review.
5. Address every error and review comment on the same branch. Reply to each
   review comment and resolve its conversation.
6. Then merge the PR into `main` and delete the feature branch.

While a PR waits for checks or review, start the next feature on its own
branch instead of waiting. Return to the waiting PR when its results arrive,
and don't merge it before its review is complete. If review comments arrive
after a PR was merged, fix them in a follow-up PR and reply in each
conversation with the fix.

Forgejo enforces this workflow:

- `main` is protected. Nobody can push to it directly or force-push.
- A PR can be merged only if its required checks (the `docs` and `rust` jobs)
  succeed.
- The head branch is deleted after merge by default.
- CI is defined in `.forgejo/workflows/ci.yml` and runs with Forgejo Actions
  on every push. Add new checks there, and add them to the required checks
  of the `main` protection rule, so that they gate merging.

Forgejo can't require resolved conversations, so check by hand that every
review conversation is resolved before merging.

## Locally generated files

Do not commit locally generated files. Keep `.gitignore` up to date: whenever
a new tool or skill is used, or temporary or intermediate files or
directories are produced, add matching entries to `.gitignore`.

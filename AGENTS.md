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
  (`office365_client_id`).

## Naming

- Workspace crates are prefixed with `mett-` and live in `crates/<name>/`.
- Name crates after the concrete technology or service they wrap (for
  example `mett-msgraph` for the Microsoft Graph adapter, not a generic
  name such as `mett-graph`).

## Git workflow

The repository lives on GitHub at `git@github.com:igelado/mett.git`
(web: https://github.com/igelado/mett). Manage it with the `gh` CLI:
pull requests, comments, checks, merges and releases.

1. Develop each feature on its own branch named `<username>/<feature>` (for
   example `igelado/msgraph-auth`), created from `main`. Commit to that
   branch, never directly to `main`.
2. After each commit, run a Codex review against `main` and fix every valid
   finding (explain why any finding doesn't apply) until a review comes back
   clean.
3. When the feature is complete, push the branch and open a pull request
   (PR) into `main` (`gh pr create`). Post the clean Codex review result as
   a PR comment (`gh pr comment`).
4. Make sure all checks pass. The clean Codex review (step 2) is the
   review a PR needs; no approval is required.
5. Address every error and review comment on the same branch. Reply to each
   review comment and resolve its conversation.
6. Don't merge by hand. When a PR opens (or leaves draft), the `automerge`
   workflow turns on its auto-merge (with a merge commit), and GitHub
   merges it into `main` as soon as every required check passed and every
   review conversation is resolved. To hold a PR back, open it as a draft,
   or turn auto-merge off (`gh pr merge --disable-auto`); turn it back on
   with `gh pr merge --auto --merge`.
7. Every PR removes its branch after merge. The repository's "Automatically
   delete head branches" setting does this for GitHub's merges; keep it on
   (`gh api repos/igelado/mett --jq .delete_branch_on_merge` prints `true`;
   `gh repo edit --delete-branch-on-merge` turns it back on). A PR merged
   any other way is merged with `gh pr merge --delete-branch`. Once merged,
   delete the local branch too (`git branch -d`).

While a PR waits for checks or review, start the next feature on its own
branch instead of waiting. Return to the waiting PR when its results arrive.
A PR that needs more than CI and the Codex review (a look from someone
else, say) is opened as a draft, so it doesn't merge before then. If review comments arrive
after a PR was merged, fix them in a follow-up PR and reply in each
conversation with the fix.

GitHub enforces this workflow:

- `main` is protected, for administrators too. Nobody can push to it
  directly or force-push; changes go in through PRs.
- A PR can be merged only if its required checks (the `docs`, `rust`,
  `macos` and `windows` jobs)
  succeed, and once every review conversation is resolved.
- The head branch is deleted after merge (see step 7).
- CI is defined in `.github/workflows/ci.yml` and runs with GitHub Actions
  on every pull request (and each push to one), and on `main`. A branch
  without a PR gets no CI, so open the PR to have it checked. Every CI job
  must be a required check of the `main` protection rule: when adding a
  job, add its name to the required checks (from GitHub Actions), so that
  it gates merging.

If a new review comment arrives while the merge is scheduled, GitHub holds
the merge until its conversation is resolved. A comment only holds the
merge when it opens a conversation (a review comment on the diff), not a
plain PR comment.

A merge made by auto-merge that the `automerge` workflow turned on is made
with the workflow's token, and GitHub starts no workflows for it, so CI
doesn't run again on `main` after such a merge. What was merged had passed
CI on the PR.

## Locally generated files

Do not commit locally generated files. Keep `.gitignore` up to date: whenever
a new tool or skill is used, or temporary or intermediate files or
directories are produced, add matching entries to `.gitignore`.

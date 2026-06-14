# Cerberus — agent guide

## Commit & PR identity — MANDATORY, never violate

Every git commit and every pull request in this repository MUST be authored as
one of the owner's identities:

- `Ben Barker <benz.benbarker@gmail.com>`  (default)
- `Ben Barker <bbarker@barkbite.org>`

At the start of every session, **before committing anything**, set the identity
(the ephemeral container otherwise defaults to the wrong one):

    git config user.name  "Ben Barker"
    git config user.email "benz.benbarker@gmail.com"

NEVER author a commit as `Claude <noreply@anthropic.com>` or any other identity.
After committing, verify with `git log -1 --format='%an <%ae>'`. This applies to
every branch and every session, without exception.

# astroarch-updates

A small Rust CLI that looks up AstroArch's versioned migration scripts,
runs them, and records the outcome (success, failure, or missing script)
in a local SQLite database — instead of running them silently as
`update-astroarch` does today.

## How AstroArch runs migrations today

The `update-astroarch` shell function
([source](https://github.com/devDucks/astroarch/blob/main/configs/.zshrc))
pulls the latest AstroArch repo, compares the old and new version
strings, and if they differ, runs a single script matching the new
version:

```sh
zsh /home/$USER/.astroarch/scripts/$NEW_VER.sh
if [[ $? -ne 0 ]]; then
    git reset --hard "$CURRENT_COMMIT"
fi
```

There is no record of what ran, when, or why it failed beyond that
in-the-moment rollback.

## What this does instead

`update-astroarch` should only ever invoke this binary; everything else —
scanning the scripts directory, sorting versions, deciding what's already
been applied, running the pending scripts — lives here, not in `.zshrc`.

```
astroarch-updates update [--scripts-dir DIR]
```

- Lists `<scripts-dir>/*.sh` (default: `~/.astroarch/scripts/migrations`, only the
  migrations: `scripts/` also holds tools that must never run as one) and parses
  each file stem as a version, sorted numerically (so `2.9.0` sorts before
  `2.10.0`, unlike a plain string sort).
- Skips any version that already has a successful execution recorded.
- Runs each remaining version in ascending order with `zsh`, stdin/stdout/
  stderr inherited so interactive steps (sudo prompts, `kdialog`,
  `notify-send`) behave exactly as before.
- Records version, script path, status, exit code, start/finish time and
  duration in SQLite for every attempt.
- Stops at the first failure and exits non-zero, so the caller (e.g. the
  zsh function) can still detect failure and roll back. Re-running `update`
  later retries the failed version before moving on to later ones.

```
astroarch-updates run <version> [--scripts-dir DIR] [--force]
```

Runs a single version's script directly, bypassing discovery. Mainly
useful for manually re-applying or forcing a specific version.

```
astroarch-updates status [VERSION] [--limit N]
```

Lists past executions, most recent first.

## Database location

`~/.local/share/astroarch-updates/astroarch.db` (respects
`XDG_DATA_HOME`). This is deliberately outside `~/.astroarch`, since
that directory is a symlinked folder and not a safe place for a
database file.

## Wiring it into `update-astroarch`

The `.zshrc` in the `astroarch` repo currently reads the old and new
version strings itself and runs a single script:

```sh
NEW_VER=$(cat /home/$USER/.astroarch/configs/.astroarch.version)

if [[ "$OLD_VER" != "$NEW_VER" ]]; then
    zsh /home/$USER/.astroarch/scripts/$NEW_VER.sh
    if [[ $? -ne 0 ]]; then
        git reset --hard "$CURRENT_COMMIT"
        ...
    fi
fi
```

That whole block is replaced by a single call, after the `git pull`:

```sh
astroarch-updates update --scripts-dir /home/$USER/.astroarch/scripts/migrations
if [[ $? -ne 0 ]]; then
    git reset --hard "$CURRENT_COMMIT"
    ...
fi
```

`.astroarch.version` files and the `OLD_VER`/`NEW_VER` comparison are no
longer needed — the SQLite database is now the source of truth for which
versions have been applied. That change lives in the `astroarch` repo,
not here.

## Building

```sh
cargo build --release
```

Produces `target/release/astroarch-updates`.

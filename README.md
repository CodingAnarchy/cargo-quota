# cargo-quota

Keeps a cargo target directory under a size budget, expressed as a percentage
of total disk space, by removing the least recently used build artifacts before
each build.

`cargo clean` throws everything away. A normal build keeps everything forever.
`cargo-quota` sits in between: when the target directory goes over budget, it
removes artifacts oldest-first until the directory is back under, and it never
touches anything you've used recently.

```console
$ cargo quota build --release
     Pruning /home/me/proj/target is 41.2 GiB, over its 37.3 GiB budget (2% of 1.8 TiB)
     Removed 812 artifacts (11.7 GiB) unused for 19d or longer, leaving 29.5 GiB
   Compiling proj v0.1.0 (/home/me/proj)
    Finished `release` profile [optimized] target(s) in 4.12s
```

## Install

```sh
cargo install --path .
```

## Usage

```sh
cargo quota build --release   # prune, then `cargo build --release` (any cargo subcommand works)
cargo quota prune             # prune only
cargo quota status            # show size and budget, and what a prune would remove
cargo quota status -v         # ...listing each artifact
```

Pruning before a build prints nothing unless it removes something. If pruning
fails for any reason, the build still runs.

### Running it automatically

Cargo has no pre-build hook, and it won't let an alias shadow `build`. There
are two ways around that.

**Shell wrapper (recommended).** This routes `cargo build`, `test`, and the
other build commands through `cargo quota`:

```fish
# ~/.config/fish/functions/cargo.fish
function cargo --wraps cargo --description 'cargo, pruning target/ to its quota before builds'
    set -l toolchain
    if string match -q -- '+*' $argv[1]
        set toolchain $argv[1]
        set -e argv[1]
    end
    if contains -- "$argv[1]" build b check c test t run r bench clippy
        command cargo $toolchain quota $argv
    else
        command cargo $toolchain $argv
    end
end
```

```sh
# bash / zsh
cargo() {
  local tc=()
  if [[ $1 == +* ]]; then tc=("$1"); shift; fi
  case $1 in
    build|b|check|c|test|t|run|r|bench|clippy) command cargo "${tc[@]}" quota "$@" ;;
    *) command cargo "${tc[@]}" "$@" ;;
  esac
}
```

**Cargo aliases.** The short built-in aliases *can* be overridden, in
`~/.cargo/config.toml` or a project's `.cargo/config.toml`:

```toml
[alias]
b = "quota build"
t = "quota test"
r = "quota run"
c = "quota check"
```

Tools that call cargo directly, such as rust-analyzer and IDEs, bypass both
approaches. That's fine: the next build from your shell catches up.

## Configuration

| Setting     | Flag           | Cargo.toml key | Env var                 | Default  |
|-------------|----------------|----------------|-------------------------|----------|
| Budget      | `--percent`    | `percent`      | `CARGO_QUOTA_PERCENT`   | `5`      |
| Basis       | `--basis`      | `basis`        | `CARGO_QUOTA_BASIS`     | `volume` |
| Low water   | `--low-water`  | `low-water`    | `CARGO_QUOTA_LOW_WATER` | `0.8`    |
| Keep recent | `--min-age`    | `min-age`      | `CARGO_QUOTA_MIN_AGE`   | `1h`     |
| Worktrees   | `--worktrees`  | `worktrees`    | `CARGO_QUOTA_WORKTREES` | `false`  |

- **Budget** is the target directory's size limit, as a percentage of total
  disk space.
- **Basis** sets what "total disk space" means. `volume` is the capacity of the
  disk the target directory is on. `system` adds up all non-removable disks.
  Volumes that share a pool (APFS containers, btrfs, ZFS) are counted once,
  identified by filesystem type and size.
- **Low water**: once over budget, pruning continues down to this fraction of
  the budget. That way the next few builds don't each trigger a small prune.
- **Keep recent**: anything used within this window is never removed, even if
  that leaves the directory over budget. This keeps a too-small budget from
  deleting the artifacts the current build is about to need.
- **Worktrees**: share one budget across all of the repository's git
  worktrees. See [Worktrees](#worktrees) below.

Flags go before the cargo subcommand: `cargo quota -p 2 build`.

In Cargo.toml, use `[workspace.metadata.quota]` or `[package.metadata.quota]`.
Cargo ignores both tables:

```toml
[workspace.metadata.quota]
percent = 2
min-age = "1d"
worktrees = true
```

Precedence, highest first: flags, then `[package.metadata.quota]`, then
`[workspace.metadata.quota]`, then environment variables, then defaults. Project
config deliberately outranks the environment, so you can set a global default in
your shell profile (`set -gx CARGO_QUOTA_PERCENT 3`) and still override it per
project.

The `--worktrees` flag can only turn sharing on. To turn it off for one project
when the environment turns it on, set `worktrees = false` in Cargo.toml.

## Worktrees

Each git worktree normally has its own `target/` directory. Without
`--worktrees`, each one gets its own budget, so five worktrees can use five
times the budget between them. A worktree you've stopped building in is never
pruned at all, because pruning only happens when you build there.

With `--worktrees`, `cargo-quota` finds the repository's other worktrees with
`git worktree list`, asks `cargo metadata` where each one builds, and treats all
of those target dirs as one pool under one budget. Pruning is least recently
used across the whole pool, so an abandoned worktree's artifacts go first even
if you never build there again:

```console
$ cargo quota status --worktrees
  Target dir /src/proj-abandoned/target (34.6 GiB)
  Target dir /src/proj-feature/target (34.6 GiB)
  Target dir /src/proj/target (34.6 GiB)
        Size 103.8 GiB in 3006 artifacts across 3 profile dir(s), oldest unused for 70d
      Budget 92.9 GiB (5% of this volume, 1.8 TiB); prunes down to 74.3 GiB, keeping anything used within 1h
  Over quota 3 target dirs total 103.8 GiB, over their shared 92.9 GiB budget (5% of 1.8 TiB)
Would remove 1002 artifacts (34.4 GiB) unused for 70d or longer, leaving 69.4 GiB
```

The settings (budget, min-age, and so on) come from the worktree you run it
in. Worktrees that share a target dir, or that put theirs inside another's
(`CARGO_TARGET_DIR=../proj/target/feature`), are counted once.

## How it works

Cargo gives every compilation unit (one crate, built one way, with one set of
features and flags) a 16-hex-digit hash, and names all of that unit's outputs
with it:

```text
debug/.fingerprint/serde-0123456789abcdef/
debug/build/serde-0123456789abcdef/
debug/deps/libserde-0123456789abcdef.rlib  (and .rmeta, .d, .rcgu.o, test binaries, ...)
```

`cargo-quota` groups files by that hash and removes each unit as a whole. Cargo
then sees no fingerprint for the unit and rebuilds it if it's ever needed again.
Each incremental-compilation cache under `incremental/` is its own unit.

- **Last used** is the newest access or modification time of any file in the
  unit. Cargo reads fingerprint files on every build, including no-op builds,
  so units that are still in use stay fresh. On `noatime` mounts, only
  modification times are available, and "last used" means "last rebuilt".
- **Speed**: when the pool is under budget, the only cost is measuring its
  size, which is spread across all cores. On a 19 GiB target dir with 190k
  files, that takes about 0.9s, close to `du -s`.
- **Profile directories** are found by their `.fingerprint/` subdirectory at any
  depth. This covers `target/debug`, `target/<triple>/release`, and target dirs
  nested inside `target/` by `CARGO_TARGET_DIR`.
- **Locking**: before removing anything from a profile directory,
  `cargo-quota` takes cargo's own `.cargo-lock` for it. If a build already
  holds the lock, that profile is skipped.
- **Never removed**: `doc/`, `package/`, the final binaries cargo copies into
  `target/debug/`, and anything else outside the per-unit layout. These still
  count toward the total.
- The target and build directories come from `cargo metadata`, so
  `CARGO_TARGET_DIR`, `build.target-dir`, `build.build-dir`, `--target-dir`,
  and `--manifest-path` are all honored.

## License

MIT. See [LICENSE](LICENSE).

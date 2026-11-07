# Porting This Fork to a New Upstream Release

This fork carries 27 commits on top of upstream `main`. This document is how to move them onto a
new upstream release without re-introducing work upstream has since done itself.

## Strategy: fresh branch, replay selectively

**Do not merge or rebase the old branch forward.** Upstream absorbs features continuously, and a
rebase replays every commit whether or not it is still needed — so superseded code comes back,
usually as a worse duplicate of what upstream now does, and usually without a conflict to warn you.

Instead:

```bash
git fetch origin                                   # origin = upstream (LLukas22/Jellyswarrm)
git checkout -b feature/plugin-federation-vNEXT origin/main
```

Then walk the old branch's commits oldest-first and, for each one, **read upstream's current code
before deciding**. The question is never "does this still apply cleanly" but "does upstream already
solve this, and better".

Last time this was done (upstream `main` at Jellyfin v12, from a v10.11 fork) **8 of 42 commits
were dropped as superseded**, plus two pairs that were net-zero reverts of each other. Replaying
blindly would have shipped all ten.

### What upstream absorbed last time

Recorded so the same ground is not re-checked from scratch. Upstream now does all of this itself:

| Dropped | Why |
| --- | --- |
| Bare-token authentication | `allow_partial` + `has_complete_metadata()` + `get_user_sessions_for_partial_device` is a better solution; down to `Token=""` → `None` |
| Config parse error handling | `build()?` / `try_deserialize()?` propagate properly |
| 503 on no healthy server | Handled |
| Interleaved result totals | Handled in federated post-processing |
| Playlist/collection path ids | Both now in `MEDIA_ID_PATH_TAGS` |
| Sort field remapping | Replaced by `SortPolicy` |
| My Media section | `get_media_folders` covers it |
| Remote control | Handled in `sessions/` |

Library merging, media deduplication and bare-token auth are all upstream features now. If a future
release absorbs the plugin work too, drop it here rather than carrying it.

## The commit inventory

Grouped by what each group is for, so a group can be evaluated as a unit.

**Carry-overs that are not plugin work** — check whether upstream fixed these first.

| | |
| --- | --- |
| `29572a4` | Fix two Quick Connect authentication bypasses |
| `0353927` | Carry the display-preferences migration so existing databases still boot |

**Build** — required to build the image straight from a git URL; see
[Deploying the result](#deploying-the-result).

| | |
| --- | --- |
| `3787b0e` | Build the UI when the image is built from a git URL |
| `9d3587c` | Bump the embedded web client to 12.4 |

**The plugin foundation.** Nothing client-side works without `web_client_host`; see
[`plugin-federation.md`](plugin-federation.md).

| | |
| --- | --- |
| `4d1d617` | Serve the web client from one pinned origin |
| `6e9a3b8` | Serve `/web/` from the pinned origin like every other client path |
| `ee8ec3b` | Give the bundled client a validator so pinning a host can take effect |
| `9f624e8` | Validate the reported server version before advertising it |

**Id translation for plugin traffic.**

| | |
| --- | --- |
| `0ec6f80` | Translate the proxy's user id inside plugin paths |
| `0a01fbb` | Translate media ids a plugin uses as object keys |
| `ec14d88` | Remap plural item-id query parameters (`ItemIds`, `AncestorIds`) |

**Routing and policy.**

| | |
| --- | --- |
| `f59440c` | Send a plugin's own API calls to the server that injected it |
| `d28f263` | Ask every server for a plugin's per-item data |
| `67c43e2` | Merge Home Screen Sections data across servers |
| `c492f83` | Give each federated fan-out leg its own timeout |
| `0c2a2f7` | Let an operator choose whose custom tabs a browser sees |
| `a005e20` | Add a Plugins page for managing plugins across servers |
| `3a0cd51` | Make the plugin federation policy editable from the Plugins page |

**Discovery and routing.** Built on the foundation above; evaluate as a unit, and only after
deciding the foundation still applies.

| | |
| --- | --- |
| `65efaf9` | Make the plugin asset policy govern what it describes |
| `9ee38d1` | Discover plugin surfaces from the client the pinned host serves |
| `2ef0811` | Show what the pinned client serves, and ask about the rest |
| `81b1afb` | Let one plugin surface be answered by one named server |

`65efaf9` is a prerequisite for the three after it: until the asset policy was consulted from the
catch-all, a discovered prefix had no route to reach the handler that reads the policy, so nothing
discovered at runtime could be served. Port it first or the rest does nothing.

**Engine fixes upstream does not have.** These are not plugin work and each one is a silent
correctness bug — check whether upstream fixed them, but do not drop them without checking.

| | |
| --- | --- |
| `b975a82` | Stop forwarding the client's `Accept-Encoding` upstream |
| `240619e` | Serve the session whose device id the client actually uses |
| `e230adf` | Distinguish an unprobed server from a confirmed-offline one |
| `a6f600a` | Report unusable credentials as 401 rather than 400 |

`54df2c3` is this document and `docs/plugin-federation.md`; it carries no code.

Each commit message states the mechanism and the symptom; read it before deciding the commit is
cosmetic. Three of the four engine fixes produce no error of any kind when they regress.

## Hazards

**The `ui` gitlink and `JELLYFIN_WEB_COMMIT` must move together.** The Dockerfile falls back to
fetching `jellyfin-web` directly when `ui/package.json` is absent, so the `ARG` and the submodule
gitlink must name the same commit — otherwise the two build paths produce different clients and
which one you get depends on how the image was built. Verify:

```bash
git ls-tree HEAD -- ui                       # the gitlink
grep JELLYFIN_WEB_COMMIT= Dockerfile         # must match
```

(BuildKit *does* recurse submodules for a git context, so the fallback normally does not fire. Keep
it for contexts without submodules, but do not rely on it to be exercised.)

**Migration checksums must stay byte-identical.** `sqlx` validates the checksum of every applied
migration and `MIGRATOR.run()` calls `process::exit(1)` on a mismatch — with `restart: unless-stopped`
that is a crash loop, on a database that has no down path. Before deploying to an existing database:

```bash
# every migration the running image already applied must be byte-identical on the new branch
for m in $(git ls-tree --name-only OLD_REF -- crates/jellyswarrm-proxy/migrations/ | grep '\.up\.sql$'); do
  a=$(git show "OLD_REF:$m" | git hash-object --stdin)
  b=$(git show "HEAD:$m"    | git hash-object --stdin)
  [ "$a" = "$b" ] || echo "DIFFERS: $m"
done
```

A *new* migration dated earlier than one already applied is fine — `sqlx`'s `run_direct` performs no
ordering check; it only compares checksums for migrations already applied and applies anything it
has not seen. Back up the data directory regardless.

**The checkout is CRLF.** Scripted edits must read with universal newlines and write back with the
platform default, or the diff becomes the whole file.

**The Windows build cannot run the UI step** — `build.rs` calls `Command::new("npm")`, which does
not resolve `npm.cmd`. Use `JELLYSWARRM_SKIP_UI=1` for all local verification. Tests that need the
embedded client skip themselves when it is absent.

## Verifying

```bash
export JELLYSWARRM_SKIP_UI=1
cargo fmt --all --check
cargo clippy --all-targets --all-features
cargo test -p jellyswarrm-proxy
```

At the time of writing: **513 tests**, clippy clean, fmt clean.

Two practices worth keeping:

**Prove each commit standalone.** For work that landed intertwined in one tree, reconstruct each
intermediate file, stage it with `git hash-object -w` plus `git update-index --cacheinfo`, then
`git stash push --keep-index` and build — so every commit compiles and tests on its own. Note that
`git stash pop` will conflict afterwards if the committed version differs from the stash base;
resetting the few shared files to `HEAD` and re-applying the remainder is quicker than resolving it.

**Reinstate the bug and watch the test fail.** Every behavioural fix here was verified that way, and
it has caught two tests of mine that passed either way — one asserted priority ordering that
`list_servers` already guaranteed, the other grepped `main.rs` for a route literal that also appears
in the test's own fixture and in a comment. A test that cannot fail is worse than no test, because it
reads as coverage.

## Deploying the result

A brand-new branch ref has never been seen by BuildKit, so the git-URL build always fetches fresh —
which sidesteps the `[internal] load git source` cache that `--no-cache` does not bust.

```bash
docker build -t jellyswarrm:TAG https://github.com/OWNER/Jellyswarrm.git#BRANCH
```

Then point the stack at the new tag. Use a **new tag each time**: with `pull_policy: never` and an
unchanged compose file there is nothing for the orchestrator to notice, and the old tag remains as a
rollback. Expect ~19 minutes cold (`jellyfin-web` npm build, then the Rust release build).

After deploying, set `web_client_host` on the Settings page — otherwise the bundled client is served
and every plugin UI is silently absent.

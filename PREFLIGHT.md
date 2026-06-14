# Upgrade Pre-Flight Checklist (v10.11.10)

Run this **before** pointing your real deployment at the upgraded binary. It guards the
upgrade against three real failure modes uncovered during planning, and gives you a clean
rollback. Everything here operates on **your deployment's database** — the DB does not live
in this repo.

## Where the database lives
- Path: `<DATA_DIR>/jellyswarrm.db` (plus `-wal` and `-shm` sidecars — WAL mode is on).
- `DATA_DIR` = env `JELLYSWARRM_DATA_DIR`, else `./data` relative to the proxy's working dir
  (`crates/jellyswarrm-proxy/src/config.rs:67`). For the Docker compose in the README it's the
  mounted `./data`.

## What the upgrade does to the DB (so you know the stakes)
On first boot the new binary runs, **in this order** (`main.rs:326` then `:336`):
1. `canonicalize_legacy_server_identity` — assigns a numeric `server_id`, and **dedups/merges**
   servers, server-mappings, and auth-sessions that share a canonical URL. Runs in one
   transaction. **On any failure it calls `std::process::exit(1)` — the server won't boot.**
2. `sqlx` migrations (`20260508120000_server_id_identity`, `20260521120000_add_merged_libraries`).
   **Forward-only.** The `.down.sql` files only drop the new columns/tables — they cannot
   resurrect rows deleted by the dedup. **Your backup is the only real rollback.**

No migration files that already existed were modified upstream, so there is **no checksum
mismatch risk** — only the two new migrations apply.

---

## Step 0 — Back up the DB (this is your rollback)
Stop the proxy first, then copy all three files (PowerShell):
```powershell
Stop-Service jellyswarrm  # or stop your container / Ctrl-C the process
$db = "$env:JELLYSWARRM_DATA_DIR\jellyswarrm.db"   # or .\data\jellyswarrm.db
Copy-Item "$db"      "$db.bak"
Copy-Item "$db-wal"  "$db-wal.bak" -ErrorAction SilentlyContinue
Copy-Item "$db-shm"  "$db-shm.bak" -ErrorAction SilentlyContinue
```
To roll back later: stop the proxy, restore the three `.bak` files over the originals, and run
the **old** binary again.

---

## Step 1 — URL parseability audit  ⚠️ BLOCKER if it fails
The boot-time canonicalizer parses every stored server URL as an absolute URL. A scheme-less
or empty value (which the old code tolerated) makes it `exit(1)`. Every URL **must** contain a
scheme (`https://…`). Run against a copy or the live DB (read-only):
```sql
SELECT id, name, url            FROM servers          WHERE url        NOT LIKE '%://%';
SELECT DISTINCT server_url      FROM server_mappings  WHERE server_url NOT LIKE '%://%';
SELECT DISTINCT server_url      FROM media_mappings   WHERE server_url NOT LIKE '%://%';
```
All three should return **0 rows**. If any row appears, fix it (e.g. `stream.sosiagaming.com`
→ `https://stream.sosiagaming.com`) before upgrading:
```sql
-- example fix; adjust to your value
UPDATE servers SET url = 'https://' || url WHERE url NOT LIKE '%://%';
```

## Step 2 — Duplicate audit  (DATA-LOSS only if non-empty)
The dedup collapses servers registered under two URL spellings, and media mappings with the
same original id, keeping the oldest and **deleting** the rest. For a clean single-server setup
this is a no-op. Confirm:
```sql
-- same backend registered twice? (expect 0 rows)
SELECT RTRIM(TRIM(url),'/') AS u, COUNT(*) c FROM servers GROUP BY u HAVING c > 1;

-- media mappings pointing at a server_url that matches no configured server -> these get DELETED
SELECT DISTINCT m.server_url
FROM media_mappings m
LEFT JOIN servers s ON RTRIM(TRIM(s.url),'/') = RTRIM(TRIM(m.server_url),'/')
WHERE s.id IS NULL;                                  -- expect 0 rows

-- duplicate originals (the newer twin's virtual id is dropped)
SELECT original_media_id, COUNT(*) c FROM media_mappings GROUP BY original_media_id HAVING c > 1;
```
If these are empty, your cached item IDs (resume points, favorites, watched) survive intact —
the migration never rewrites surviving `virtual_media_id`s.

## Step 3 — Decide library-merging / labels  (UX choice)
Your old config file predates these keys, so absent → defaults apply. In `jellyswarrm.toml`
(or env `JELLYSWARRM_<UPPER>`):
- `merge_libraries` (default **true**): merges same-typed/same-named libraries **across
  servers** into one. For a **single** upstream this is a no-op (library IDs don't change). It
  only reshapes views once you add a second server with overlapping libraries.
- `include_server_name_in_media` (default **true**): appends ` [ServerName]` to library/media
  names. If your "mislabeled libraries" complaint is really "I don't want the `[stream]`
  suffix," set this to `false`.

Pick consciously rather than inheriting the default; we'll eyeball the rendered labels during
verification and adjust.

## Step 4 — Dry-run on a COPY (the single best safety step)
Boot the new binary once against a throwaway copy of the data dir and watch it migrate, before
touching production:
```powershell
Copy-Item -Recurse .\data .\data-dryrun
$env:JELLYSWARRM_DATA_DIR = (Resolve-Path .\data-dryrun)
$env:RUST_LOG = "info"
.\target\release\jellyswarrm-proxy        # or: cargo run --release -p jellyswarrm-proxy
```
**Pass = it boots to "listening" with no exit.** Then stop it (Ctrl-C) and delete `data-dryrun`.

### Boot log lines that mean STOP (restore backup, fix the offending rows):
- `Failed to canonicalize legacy server identity …`  → exits 1 (usually a Step-1 URL problem)
- `Failed to run database migrations …`              → exits 1

---

## Quick summary for your (single-server) setup
You have one upstream (`stream.sosiagaming.com`). If Step 1 returns 0 rows and Step 2 is empty,
the upgrade is low-risk: IDs are preserved, no rows are deleted, and merging is a no-op. The
backup (Step 0) plus the dry-run (Step 4) cover the rest.

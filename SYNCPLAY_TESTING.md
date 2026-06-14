# SyncPlay Test Matrix (v10.11.10 branch)

This upgrade **is** the SyncPlay upgrade — the fork carried none of the SyncPlay fixes; they
all live in the commits this branch pulls in. Use this matrix to confirm SyncPlay works and to
exercise the four failure modes the upgrade fixes.

## Mental model (so logs make sense)
- **Jellyswarrm itself is the SyncPlay coordinator.** Group state is in-memory in the proxy
  (`handlers/syncplay/service.rs`); nothing SyncPlay-related is forwarded to upstream Jellyfin.
- The `/socket` (and `/websocket`) WebSocket is terminated **locally**. Commands (Play/Pause/
  Seek) fan out over each member's WS channel.
- A participant's key embeds `user_id:device_id:token`. Item IDs in the queue are the proxy's
  **virtual** IDs (remapped to real IDs only on the separate playback path).

## What the upgrade fixes (verify each)
1. **30s reconnect grace** — a brief WS drop no longer instantly evicts you (test C1).
2. **No group freeze on disconnect** — one member leaving/closing doesn't hang everyone (A8/C4).
3. **Lax request models** — clients omitting `PlaylistItemId`, sending no NextItem body, or
   fractional pings no longer 400 (A5/A9).
4. **Corrective pause on buffering** — one member stalling pauses the group instead of letting
   it drift; resumes together when ready (A7). This is standard Jellyfin behavior; the per-member
   **Ignore Wait** toggle (`/SyncPlay/SetIgnoreWait`) opts a flaky member out.

## Expected behaviors that look like bugs but aren't
- **Cross-server library-access denial** returns a quiet `HTTP 204` + a `LibraryAccessDenied`
  WS frame — not a 5xx. Watch for it (B3).
- **A proxy restart wipes all groups** (in-memory). Clients' next command gets `NotInGroup` /
  `GroupDoesNotExist`; re-create the group. Expected (C5).

---

## Logging
```powershell
$env:RUST_LOG = "jellyswarrm_proxy::handlers::syncplay=debug,jellyswarrm_proxy::request_preprocessing=debug,jellyswarrm_proxy=info"
```
Key log strings to watch (all in `handlers/syncplay/`):
| String | Meaning |
|---|---|
| `Registered SyncPlay websocket` | a client's WS connected (registered under its session key) |
| `Unregistered SyncPlay websocket with reconnect grace` | WS dropped; 30s grace timer started |
| `SyncPlay websocket grace expired; leaving group` | grace elapsed; member removed |
| `SyncPlay group created` / `member joined group` / `member left group` | group lifecycle |
| `SyncPlay unpause`/`pause`/`seek`/`next item` requested | command received |
| `Ignoring stale SyncPlay buffering/ready update` | out-of-order client event dropped |
| `denied due to library access` | cross-server access gate hit (paired with the 204) |
| `Replacing media ID in path/query` (request_preprocessing) | virtual→real ID swap on playback |
| `Android TV` (request_preprocessing) | device-id rebind on the playback path (see C6 caveat) |

---

## Phase A — two clients on the SAME upstream server (core)
Run these first; they prove the coordinator and the lax-model/skip fixes.

| # | Scenario | Expected | Watch |
|---|---|---|---|
| A1 | Create + join | C1 `New`; C2 `List` then `Join`. Both see one group; C2 gets `GroupJoined`+queue; C1 gets `UserJoined` | `group created`, `member joined` |
| A2 | Set queue / Play | Both go `Waiting`→`Playing` after both `Ready`; start together (delay ≈ max(2×ping,500ms)) | `set new queue`, resolve→`Unpause` |
| A3 | Pause / Unpause | C2 pause then C1 unpause; both pause/resume at the same tick | identical `PositionTicks` |
| A4 | Seek | C1 seeks to T; both jump to T, re-buffer, resume in sync | `seek requested`; Buffer→Ready→Unpause |
| A5 | Skip-next/prev (**send with body, with empty body, and with null `PlaylistItemId`**) | Advances in **all three** cases (the lax-model fix); **no 400** | `next item from→to`; no parse error |
| A6 | Late join mid-playback | New joiner gets current queue + frozen position; group re-`Waiting` until they're `Ready`, then all resume | `member joined`, freeze position |
| A7 | Buffering barrier | Throttle C2 mid-play → group pauses everyone (corrective pause), resumes when C2 `Ready` | a broadcast `Pause` on entering Buffer |
| A8 | Leave while waiting | C1 leaves while C2 mid-buffer → C2 gets `UserLeft`; group does **not** hang | `member left`, immediate resolve |
| A9 | Fractional ping | Browser client sends `Ping: <float>` → no 400; sane delay math | no error on `/SyncPlay/Ping` |

## Phase B — two clients on DIFFERENT upstream servers (cross-server)
| # | Scenario | Expected | Watch |
|---|---|---|---|
| B1 | Create empty group across servers, both join before any queue | Works (empty queue short-circuits the access check) | no `LibraryAccessDenied` |
| B2 | Queue content present on **both** backends, both authenticated | Plays in sync across backends | access check passes on the item's `server_id` |
| B3 | Queue content present on **only one** backend | Other member (and the action) **denied**: `204` + `LibraryAccessDenied`; queue not set | `denied due to library access` |
| B4 | B2, but one backend marked unhealthy | Denial/success tracks the health-filtered sessions; healthy side keeps playing | `server_status … is_healthy` |

> Note: B-series depends on Phase B (logout fix) for stability — an unhealthy backend currently
> drops sessions. Re-run B4 after the logout fix lands.

## Phase C — resilience
| # | Scenario | Expected | Watch |
|---|---|---|---|
| C1 | Blip **< 30s** | Kill C2's WS ~10s, reconnect | C2 **stays** in group; on reconnect gets queue snapshot; others never saw it leave | `reconnect grace`, then `Registered …`, **no** `grace expired` |
| C2 | Blip **> 30s** | Kill C2's WS ~40s | C2 removed at 30s; others get `UserLeft`; C2 must rejoin | `grace expired; leaving group` |
| C3 | Rapid reconnect race | New WS connects, then old socket's close lands | Old close ignored (stale id); no spurious `UserLeft` | no double-leave |
| C4 | Disconnect while `Waiting` | Group resumes once remaining members ready (doesn't freeze) | resolve on `Disconnect` |
| C5 | **Proxy restart mid-group** | All groups gone; next command → `NotInGroup`; re-create works | `NotInGroup` / `GroupDoesNotExist` |
| C6 | Android-TV client | If it stalls in `Waiting`, you've hit the device-id split (the SyncPlay path does **not** run the AndroidTV rebind) — look for **two participant entries sharing one user_name** | duplicate participant under one user |

**Recommended order:** A1–A9 → C1–C5 (the headline reconnect-grace work) → B1–B4 (most
environment-dependent). C6 only if Android TV is in scope.

---

## Results log (fill in)
| Case | Pass/Fail | Notes / log evidence |
|------|-----------|----------------------|
| A1 | | |
| A2 | | |
| A3 | | |
| A4 | | |
| A5 | | |
| A6 | | |
| A7 | | |
| A8 | | |
| A9 | | |
| B1 | | |
| B2 | | |
| B3 | | |
| B4 | | |
| C1 | | |
| C2 | | |
| C3 | | |
| C4 | | |
| C5 | | |
| C6 | | |

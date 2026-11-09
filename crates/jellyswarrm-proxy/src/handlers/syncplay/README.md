# SyncPlay

SyncPlay coordinates group playback inside Jellyswarrm, allowing a group to
play media from different backends. `/SyncPlay/*` handlers update group state
and send commands to members through the shared client-session transport.

Group membership and synchronization live here. Client identity, WebSockets,
and delivery are shared with remote control in the
[sessions module](../../sessions/README.md).

## Liveness is the hub's to answer

A group resumes only once every member it is waiting on reports ready, so
anything that makes a member look present when it is not stops the group dead.
`GroupParticipant::is_connected` is a cache, and the socket task clears it on
its way out — which a cancelled task never reaches. Treat `ConnectionHub` as
the authority whenever the answer decides whether the group waits, and keep a
graceful disconnect distinguishable from a lost one: the first is owed its
reconnect grace, the second has to be reaped or it keeps the group alive and
listed forever.

Delivery is best effort in both directions of that relationship. A full
outbound queue drops the message and leaves the member registered, so a lost
`SyncPlayCommand` means the group records `Playing` for a client that never
heard it. Nothing here can re-deliver one; the least it can do is not fail
silently.

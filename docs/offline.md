# Working offline

Some apps keep working without a connection: Point of Sale sells from its price book, Docs keeps
what was typed. What a device keeps for that follows one policy, whatever the app.

## What a device keeps

* **Cached reads**: what someone works from (a till's price book and its register). They are kept
  for the person under the workspace's *lease* and forgotten, every app's at once, when the lease
  ends, when another person signs in on the device, or when the person is signed out everywhere,
  turned off or their password changes.
* **Work not sent yet**: a sale made offline, changes not yet stored. It is kept for its person until
  it is sent, whatever the lease says, since forgetting it would lose what they did; nobody else
  signed in on the device is shown it or sends it.

Guests and portal people never keep anything. A page that cannot store anything (a private window,
a full disk) works online only, and says so where it matters.

## The workspace's policy

Administrators set it under Administration → Workspace → Working offline, or with
`PUT /api/admin/settings {"offline": {"allowed": true, "hours": 72}}`:

| Setting | Meaning |
|---|---|
| `allowed` | whether devices may keep cached reads at all (default `true`); off, a till cannot price a sale offline and sells online only |
| `hours` | how long a lease lasts from the page's load, or its last renewal (1 to 720, default 72) |

## How apps follow it

Each page's bootstrap carries the person's lease, `offline`:

```json
{"allowed": true, "until": "2026-11-09T10:00:00Z", "hours": 72, "key": "3f9c1e7a2b8d4c60", "person": "…"}
```

`key` stands for the person and their sign-in, so a new one means what was cached for the old one
goes. `GET /api/shell/offline` renews the lease for a page that stays open for days.

Apps name what they keep with `@ferrum/client`: `cacheName(env, app, name)` for cached reads (null
when nothing may be kept now), `pendingName(env, app, name)` for work not sent yet, and `sweep()` at
start to forget the cached reads the lease no longer covers, in storage and IndexedDB.

## What it cannot do

A lease is kept by the browser, and the server cannot reach into a device. Turning a person off or
signing them out everywhere changes their key, so what a device cached for them goes the next time
one of their pages loads there with a connection; a device that stays offline goes on working from
it until the lease ends, which is what `hours` bounds. A device lost while offline keeps its cached
reads until the lease ends and someone opens the app there, or its storage is cleared. Encrypting
what a device keeps is the device's (disk encryption), not the app's.

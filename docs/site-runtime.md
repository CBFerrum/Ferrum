# Running a site

`ferrum-site` runs next to the equipment and keeps working without the platform, inside what was
authorized before the link went down. One binary, one SQLite journal, no broker of its own.

**Nothing here has run on physical equipment** ([compatibility](compatibility.md)). Rehearse on the
simulator first.

## Install

Build or take `ferrum-site` for the gateway, then:

```bash
sudo useradd --system --home /var/lib/ferrum-site --create-home ferrum-site
sudo install -d -o ferrum-site -m 0750 /var/lib/ferrum-site/bin
sudo install -o ferrum-site -m 0755 ferrum-site /var/lib/ferrum-site/bin/
sudo ln -sf /var/lib/ferrum-site/bin/ferrum-site /usr/local/bin/ferrum-site
sudo install -d -o ferrum-site -m 0750 /etc/ferrum-site
sudo install -m 0640 -g ferrum-site deploy/site/site.yaml /etc/ferrum-site/
sudo install -m 0644 deploy/site/ferrum-site.service /etc/systemd/system/
```

Edit `site.yaml`. Keep `state_dir` on storage that survives power loss and is encrypted at rest: it
holds this gateway's private key and the operators' PIN hashes. The console binds to `127.0.0.1`
unless you say otherwise; anything wider belongs behind a segmented network.

The binary lives under `/var/lib` because that is the one place the service may write, and taking a
signed release (see [Updating](#updating)) means replacing it. With `update_keys` empty, which is the
default, it never does.

## Enroll

In Operations: add the site, add the gateway as a device with kind Gateway, claim it, and choose
**Enroll a key** for a one-time code. Then, on the gateway:

```bash
sudo -u ferrum-site ferrum-site enroll --config /etc/ferrum-site/site.yaml --code <code>
sudo systemctl enable --now ferrum-site
```

The private key never leaves the gateway. Back in Operations, commission it and put it in service.
Equipment behind it needs no key of its own: set its Gateway field, then choose **It is reached
through its gateway**, commission it, and list what it may be asked to do.

## Configuration

| Setting | Default | What it decides |
|---|---|---|
| `sync_secs` | 5 | How often it talks to the platform |
| `read_secs` | 5 | How often it reads equipment |
| `work_millis` | 250 | How often it looks for work |
| `adapter_timeout_secs` | 10 | How long one call to equipment may take |
| `unknown_after_secs` | 300 | How long silence lasts before a command's outcome is unknown |
| `telemetry_budget_mb` | 1024 | Space for measurements. Oldest and least important go first |
| `reserve_mb` | 256 | Disk kept for commands and evidence. Below it, nothing new starts |
| `max_queued` | 2000 | Commands it will hold |
| `console` | off | Where the console listens. `/metrics` is served there too |
| `managed` | false | Whether to become what the platform says: definitions, devices, rules, flows, exports, releases. What is in this file wins |
| `definitions_dir` | none | Definitions kept on this machine, as `*.yaml` |
| `devices` | none | Devices kept on this machine, each with its definition |
| `allow` | any | What hosts a definition may reach, by how they start |
| `listen` | none | Addresses a definition may listen on, exactly |
| `drivers_dir` | none | The only place an external driver can run from |
| `secrets` | none | What `{secret.x}` stands for. They stay here |
| `peers` | none | Boxes this one works through: `{name, url, devices, concurrency}` |
| `peer_only`, `listen_peers`, `trusted_peers` | off | For such a box: no platform of its own, where to answer, and whose keys to believe |
| `relay` | off | Passes gateways' requests on to `server`, for a network with no other way out |
| `plain_link` | false | Lets `server` be plain HTTP, for a relay on the plant's own network. Requests cannot be changed, and can be read |
| `update_keys` | none | Public keys that may sign a release. Without one it never updates itself |

## Day to day

Operations shows each site: whether it is heard, its runtime, its disk, what is waiting, and how far
behind its reports are. A silent gateway may be working perfectly well; the screen says what it does
not know.

The console, at the address you chose, needs nothing but this process. People sign in with a PIN set
in Operations (**Console PIN…**). It shows what is waiting, unknown outcomes, tasks for people, and
takes floor reports by the run number on the job sheet. Those go up when the link allows, and are
applied then: one a run cannot take by that time is noted on the run, and stays in `op_fact`.

## When something goes wrong

| What happened | What it does | What you do |
|---|---|---|
| The link goes down | Keeps working within the lease. The console says **Working offline** | Nothing, until the lease nears its end |
| The platform hangs | The same, once it has been silent for three exchanges | Nothing |
| The lease ends | Starts nothing new; follows what is running. Operations shows why | Restore the link, or extend `max_offline_hours` knowing what that allows |
| The gateway crashes | On restart, asks the equipment about whatever was in flight. What cannot be answered is `OutcomeUnknown`, never resent | Look at the equipment, then record what you found |
| A command is unknown | Stops counting on it. Nothing retries it | Look, then **Say what happened** in Operations or at the console. In a warehouse, **I found it** on the task settles both |
| Two different endings | The first stands, the command is flagged | Read both, in **Needs a person** |
| Disk nearly full | Sheds telemetry first; below the reserve starts nothing new | Free space. Shed measurements are counted, not hidden |
| Gateway replaced | The new one enrolls and a person moves authority. The old one is fenced | Settle what the move made unknown |
| Key suspected stolen | Quarantine the gateway in Operations. Its epoch ends at once | Investigate, then enroll a new key. There is no rotation in place |

## Backup and restore

```bash
ferrum-site backup --config /etc/ferrum-site/site.yaml --to /backup/journal.db
ferrum-site restore --config /etc/ferrum-site/site.yaml --from /backup/journal.db
```

A backup is consistent while running. After a restore the equipment may be ahead of the journal, so
**dispatch stays off** until a person has settled every unknown outcome and confirmed at the console
that the floor matches. Old epochs and old signatures are refused, so a restored gateway cannot
replay what it already said.

## Updating

By hand: stop the service, replace the binary, start it. The journal migrates itself. Running work is
found again on start, as after a crash.

By itself, with `update_keys` set and `managed: true`. An Edge Release names a version, a file and a
share of gateways. A gateway takes one only if a key it was installed with signed it, it has not
expired and it is newer than what is running, so an old build cannot be replayed. It waits for a moment
with nothing running, checks the file against the signed hash, keeps the old binary beside the new one
and restarts. A new build that keeps dying before it has spoken to the platform is put back, and that
version is refused from then on.

**Robot, PLC and safety firmware stay with their makers' procedures**; nothing here touches them.

## Boxes in cabinets

Equipment is often wired to a small box while authority sits with the site's server. The box runs with
`peer_only: true`, `listen_peers` and the server's public key in `trusted_peers`. The server lists it
under `peers`, and treats it as one more adapter, over a signed link that needs no cloud. Print a
key's public half with `ferrum-site key --config site.yaml`.

A command may be sent twice because a lost answer looks like a lost request, so the box keeps what it
was asked and what it answered. The same command gets the same answer, and one it started and never
finished is unknown rather than started again. Peers are one hop, so that nobody can build a loop.
The link is signed and not encrypted: it belongs on a segmented network or a VPN.

## A closed network

A site with no way to the platform except through one machine runs `ferrum-site relay` on that
machine, with `relay` set, and points its gateways' `server` at it. It passes only what gateways use,
unchanged, since the path and body are what is signed. It holds no keys and can forge nothing.

## Exports

An Edge Export sends what a gateway reads somewhere else as well: `https://…` by batches, signed with
an HMAC of the body when `sign_with` names a secret, or `mqtt://…` by a message per measurement.
What is waiting stays in the journal through an outage, within a budget, and the newest stays when the
budget runs out.

## A broker on site

`deploy/site/mosquitto.conf` is a starting point for Mosquitto beside the runtime, for equipment that
publishes to the gateway itself. It runs as its own process, so a fault in either leaves the other
standing. `deploy/site/docker-compose.yml` runs an anonymous, local one for rehearsing. A real one
needs TLS, users and access lists.

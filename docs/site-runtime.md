# Running a site

`ferrum-site` runs next to the equipment and keeps working without the platform, inside what was
authorized before the link went down. One binary, one SQLite journal, no broker of its own.

**Nothing here has run on physical equipment** ([compatibility](compatibility.md)). Rehearse on the
simulator first.

## Install

Build or take `ferrum-site` for the gateway, then:

```bash
sudo useradd --system --home /var/lib/ferrum-site --create-home ferrum-site
sudo install -m 0755 ferrum-site /usr/local/bin/
sudo install -d -o ferrum-site -m 0750 /etc/ferrum-site
sudo install -m 0640 -g ferrum-site deploy/site/site.yaml /etc/ferrum-site/
sudo install -m 0644 deploy/site/ferrum-site.service /etc/systemd/system/
```

Edit `site.yaml`. Keep `state_dir` on storage that survives power loss and is encrypted at rest: it
holds this gateway's private key and the operators' PIN hashes. The console binds to `127.0.0.1`
unless you say otherwise; anything wider belongs behind a segmented network.

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
| `console` | off | Where the console listens |

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

Stop the service, replace the binary, start it. The journal migrates itself. Running work is found
again on start, as after a crash. An update is not signed by Ferrum yet: take it from a source you
trust, and check its digest. **Robot, PLC and safety firmware stay with their makers' procedures**;
nothing here touches them.

## A broker for rehearsal

`deploy/site/docker-compose.yml` runs Mosquitto for trying the MQTT and VDA 5050 adapters. It is
anonymous and local. A real one needs TLS, users and access lists.

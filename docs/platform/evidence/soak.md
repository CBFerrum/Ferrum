# Soak, 3 November 2026: three hours, every check passing

J12 (docs/platform/journeys.yaml) in software: soak run 41719722922 of `.github/workflows/soak.yml`, on the
image of `main` at 3fe3238 (tagged `latest` when the run began, 21:15 UTC), with PR 45's list
order. Three workspaces of different weight on one Compose stack on one CI runner, through a
database restart, a backup read back while serving, and the server replaced. The run before it,
without PR 45, failed two checks and slowed as the data grew (p95 of the heavy workspace from 347 ms
to 2,315 ms); this one held its pace for the three hours (268 ms to 285 ms) while the database grew
from 111 MiB to 8.8 GB.

What it does not show: a deployment, days rather than hours, real people's work, or more than one
machine. The report below is the workflow's own (`scripts/soak.py`), as it ran.

## The report

Three workspaces at once on one Compose stack on one machine (a CI runner): what it says is about that, not a deployment.

| Check | | |
|---|---|---|
| heavy: errors outside outages | pass | 11 of 2776928 requests |
| steady: errors outside outages | pass | 0 of 1877197 requests |
| light: errors outside outages | pass | 0 of 828314 requests |
| light: never refused | pass | 0 refused |
| light: never queues for a turn | pass | at most 1 ms to take one (2 took a millisecond or more), heavy up to 312 ms |
| heavy: held to its share | pass | 3822 of its requests waited for a turn |
| light: ahead of the flood | pass | its p95 at most 0.18 of heavy's in 15 quiet windows |
| light: answered while heavy floods | pass | p95 49 ms at worst, 13 ms alone |
| recovered from: database restart | pass | back after 0.7 s |
| backup: taken and read back while serving | pass | 1456 tables in the backup, read back, in 108.0 s |
| recovered from: server replaced | pass | back after 1.5 s |
| every incident happened | pass | 3 of 3 |
| server memory does not keep growing | pass | 101 MiB to 109 MiB |
| outbox drains once the load stops | pass | empty 32 s after it stopped |
| nothing more died in the outbox | pass | 0 dead before, 0 after |

What it found, beside the checks:

- The light workspace's p95 went from 13 ms alone to 49 ms at worst while the others worked: the share keeps its turns, and the database's CPU is everyone's.
- The outbox held up to 29 events while the load ran (at 163 min), and was empty 32.20000000000073 s after the load stopped.
- The database grew from 111 MiB to 8810 MiB.

| Incident | At (min) | Back after (s) | |
|---|---|---|---|
| database restart | 45 | 0.7 | Container ferrum-soak-db-1  Restarting
 Container ferrum-soak-db-1  Started |
| backup | 90 | 108.0 | 1456 tables in the backup, read back |
| server replaced | 135 | 1.5 | Container ferrum-soak-ferrum-1  Recreate
 Container ferrum-soak-ferrum-1  Recreated
 Container ferrum-soak-ferrum-1  Starting
 Container ferrum-soak-ferrum-1  Started |

Each workspace: requests / errors / refused / p95 ms.

| From (min) | Heavy | Steady | Light | Server MiB | Database MiB | Outbox |
|---|---|---|---|---|---|---|
| 0 | 157724 / 0 / 0 / 268 | 107021 / 0 / 0 / 79 | 47065 / 0 / 0 / 46 | 102 | 586 | 7.0 |
| 10 | 157292 / 0 / 0 / 268 | 106270 / 0 / 0 / 79 | 46756 / 0 / 0 / 47 | 103 | 1087 | 0.0 |
| 20 | 155711 / 0 / 0 / 271 | 105550 / 0 / 0 / 80 | 46648 / 0 / 0 / 47 | 104 | 1577 | 3.0 |
| 30 | 156608 / 0 / 0 / 271 | 106171 / 0 / 0 / 80 | 46920 / 0 / 0 / 47 | 104 | 2073 | 26.0 |
| 40 | 155120 / 24 / 0 / 272 | 104926 / 8 / 0 / 81 | 46117 / 2 / 0 / 48 | 105 | 2538 | 0.0 |
| 50 | 155007 / 0 / 0 / 273 | 104720 / 0 / 0 / 82 | 46192 / 0 / 0 / 48 | 106 | 3029 | 13.0 |
| 60 | 155422 / 0 / 0 / 272 | 105260 / 0 / 0 / 81 | 46507 / 0 / 0 / 47 | 107 | 3523 | 1.0 |
| 70 | 155229 / 0 / 0 / 273 | 104892 / 0 / 0 / 83 | 46459 / 0 / 0 / 47 | 107 | 4014 | 3.0 |
| 80 | 154189 / 0 / 0 / 275 | 104212 / 0 / 0 / 84 | 46048 / 0 / 0 / 47 | 107 | 4479 | 16.0 |
| 90 | 151739 / 0 / 0 / 282 | 101476 / 0 / 0 / 87 | 44464 / 0 / 0 / 50 | 108 | 4961 | 1.0 |
| 100 | 155067 / 0 / 0 / 274 | 105029 / 0 / 0 / 83 | 46415 / 0 / 0 / 47 | 108 | 5452 | 7.0 |
| 110 | 153769 / 0 / 0 / 276 | 103863 / 0 / 0 / 84 | 46000 / 0 / 0 / 47 | 108 | 5912 | 12.0 |
| 120 | 154763 / 0 / 0 / 276 | 104601 / 0 / 0 / 83 | 46116 / 0 / 0 / 48 | 108 | 6409 | 10.0 |
| 130 | 151873 / 144 / 0 / 282 | 102652 / 37 / 0 / 84 | 45290 / 13 / 0 / 48 | 82 | 6892 | 10.0 |
| 140 | 154337 / 0 / 0 / 277 | 104520 / 0 / 0 / 83 | 46244 / 0 / 0 / 47 | 83 | 7384 | 5.0 |
| 150 | 151440 / 0 / 0 / 281 | 102223 / 0 / 0 / 84 | 45013 / 0 / 0 / 48 | 83 | 7845 | 0.0 |
| 160 | 151419 / 0 / 0 / 283 | 102328 / 0 / 0 / 85 | 45275 / 0 / 0 / 48 | 83 | 8334 | 4.0 |
| 170 | 150219 / 0 / 0 / 285 | 101483 / 0 / 0 / 84 | 44785 / 0 / 0 / 49 | 85 | 8784 | 10.0 |

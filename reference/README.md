# Reference stacks

**These are examples you own, not the product.** Ferrum takes the address of a PostgreSQL and a bucket of an S3-compatible
store, and operates neither: the database's backups, failover and upgrades, and the store's keys and durability, are the
owner's. Each stack here runs both so that you can see the product with its two addresses filled in, and says at its top what it
gives and what you are then responsible for. None of it is needed, and none is assumed: give the product your own database and
bucket and ignore this folder.

| Stack | Runs | Gives | Start |
|---|---|---|---|
| [`single-host/`](single-host/README.md) | Compose: PostgreSQL 17, SeaweedFS, a bucket made once, a daily dump of the database beside the product's `deploy/docker-compose.yml` | the two addresses and a backup you can restore | `reference/single-host/up.sh` |
| [`kubernetes/`](kubernetes/README.md) | CloudNativePG (3 instances, WAL archiving, a daily base backup), SeaweedFS, the product chart installed on them | the two addresses and point-in-time recovery | `reference/kubernetes/install.sh -f my-values.yaml` |

`store/start.sh` is the object store both run: SeaweedFS in one process, given its access key (`STORE_ACCESS_KEY`,
`STORE_SECRET_KEY`), its encryption key made once in its own volume. It makes no bucket: `ferrum task bucket_set_up` does.

`bash reference/test.sh` checks that every path a README here names exists, that the scripts parse, and that the single-host
stack merges with the product's Compose file and names nothing the product's file does not take (it needs `docker compose`
only to validate a file, not a daemon). `bash reference/kubernetes/database/test.sh` renders the database chart.

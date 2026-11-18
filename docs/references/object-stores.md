# Object stores: the one Compose bundles

**Tried:** SeaweedFS 4.53 and Garage 2.5.0, the Homebrew bottles of both, run on one machine (macOS, arm64) on
16 November 2026 for the object-storage slice (docs/plan/stateless.md, D6). MinIO was not tried: both candidates ran,
and one passed.

**Chosen: SeaweedFS**, in `deploy/docker-compose.yml` as `store`, started by `reference/store/start.sh`. Three facts
decided it:

1. Garage answers `PutBucketVersioning` and `ListObjectVersions` with `NotImplemented`, and drops
   `NoncurrentVersionExpiration` from a lifecycle rule without a word: it keeps no versions, so nothing outside Ferrum
   that deletes or writes over a file can be taken back.
2. Garage answers `PutBucketEncryption` with `NotImplemented`: it does not encrypt what it keeps (it takes keys a
   client sends with each request, SSE-C, which nothing here sends).
3. SeaweedFS kept versions (two versions and a delete marker for an object written twice and deleted, the oldest
   readable), kept a lifecycle rule ending old versions, encrypted every object by the bucket's default (`AES256`
   back on each), left no plaintext on disk, and ran as one process of about 90 MB with a health check.

## What was run

Each store in a directory of its own under `ERP/feo/scratch-store/trial`, on ports above 39000, then stopped.

```bash
# SeaweedFS: master, volume server, filer and S3 in one process; an identity file for the keys; the key that encrypts
# what it keeps given as WEED_S3_SSE_KEK (s3.sse.kek in security.toml).
WEED_S3_SSE_KEK=$(openssl rand -hex 32) weed server -dir=sw/data -ip=127.0.0.1 -ip.bind=127.0.0.1 \
  -master.port=39100 -volume.port=39101 -filer.port=39102 -s3 -s3.port=39103 -s3.config=sw/s3.json \
  -filer.encryptVolumeData -master.volumeSizeLimitMB=256 -volume.max=0

# Garage: one process with SQLite metadata, then its layout, a key and a bucket through its own command.
garage -c ga/garage.toml server
garage -c ga/garage.toml layout assign -z dc1 -c 1G <node id>; garage -c ga/garage.toml layout apply --version 1
garage -c ga/garage.toml bucket create ferrum; garage -c ga/garage.toml key import -n ferrum <id> <secret>
garage -c ga/garage.toml bucket allow --read --write --owner ferrum --key ferrum
```

Then the same calls against each, with boto3 (from moto 5.0.28's virtualenv): make a bucket; write, read and delete;
a multipart upload; a presigned read; versioning on, an object written twice and deleted, its versions listed and the
oldest read; lifecycle rules (old versions after 30 days with unfinished uploads after a day, and an expiration on a
prefix) put and read back; default encryption put and read back, an object written and its encryption asked; the
bucket's policy status, public access block and ACL; a read without keys; the `Date` header; the health check. And on
disk: a marker written into an object of an encrypted bucket and of a plain one, looked for in the data directory.

| Check | SeaweedFS 4.53 | Garage 2.5.0 |
|---|---|---|
| Bucket, write, read, delete, multipart upload | pass | pass |
| Presigned read | pass | pass (signed with its own region name; another one is refused) |
| Versioning on, versions kept on overwrite and delete | pass | **fails**: `NotImplemented` |
| Lifecycle rule ending old versions | pass (kept and read back; a daily pass applies it, not watched here) | **fails**: the rule is kept without it |
| Lifecycle: expiration, unfinished uploads | pass | pass |
| Default encryption (SSE-S3) | pass once a key is given; without one, writes fail (`InternalError`, logged as "SSE-S3 encryption is not configured") | **fails**: `NotImplemented` |
| Plaintext on disk | none, with `-filer.encryptVolumeData`, encrypted bucket or not | not looked at (no encryption of its own) |
| Not public: read without keys | refused (403) | refused (403) |
| Policy status, public access block | none set (`NoSuchBucketPolicy`, `NoSuchPublicAccessBlockConfiguration`) | `NotImplemented` (it has no bucket policies) |
| ACL | the owner only | the owner key only |
| Health check | `GET /healthz` on the S3 port: 200 | `GET /health` on its admin port: 200 |
| One small process | one process, about 90 MB at rest | one process, about 38 MB at rest |
| First start | an identity file; `-bucket` exists only in `weed mini` | a layout, a key and an allowance through its command before any request |
| Licence beside AGPL-3.0 | Apache-2.0: may be shipped beside, and combined | AGPL-3.0-or-later |
| Maintained | release 4.53 published on 8 November 2026 | release 2.5.0, active |

## What the trial found about running SeaweedFS

- **Volumes.** `-volume.max` defaults to 8 volumes, and each bucket takes several: a third bucket's writes failed with
  "No writable volumes and no free volumes left". `-volume.max=0` sizes it from the free disk; `start.sh` uses it with
  1 GB volumes.
- **Ports.** Besides its master, volume server, filer and S3, it opens an Iceberg REST catalogue (8181) and a Lance
  namespace server (9101) by default: `-s3.port.iceberg=0 -s3.port.lance=0` turn them off. Only S3 listens on the
  network (`-ip.bind=127.0.0.1 -s3.ip.bind=0.0.0.0`): the filer and the volume server answer without keys, so nothing
  but the container may reach them.
- **Encryption.** `-filer.encryptVolumeData` encrypts each chunk on the volume server with a key kept in the filer's
  metadata (the same volume); SSE-S3 with `WEED_S3_SSE_KEK` encrypts each object's key with one kept apart. `start.sh`
  makes that key at the first start, in the store's own volume, beside its identity file.
- **Keys.** The identity file is read at start; the store's own command (`weed shell`: `s3.user.create`,
  `s3.accesskey.rotate`) changes identities while it runs.

## Not tried

MinIO (both candidates ran; its server's licence is AGPL-3.0 too). Recovery from a lost volume, and lifecycle rules
taking effect (each needs days); `scripts/object_store_rehearsal.sh` runs the chosen store again with
`reference/store/start.sh` on each run (given its key; `ferrum task bucket_set_up` makes the bucket).

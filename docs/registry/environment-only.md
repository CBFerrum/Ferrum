# What stays in the environment

Written by `python3 scripts/settings.py --write`, which refuses to add to it: it may only shrink. Each name a replica still reads from its environment once the release after this one stops reading the settings' registry (server/src/settings.yaml), besides `DATABASE_URL`, with why (docs/operations.md, *What a replica still reads from its environment*). `scripts/one_input.sh` fails a server that reads a name neither the registry nor this list declares.

| Variable | Why |
|---|---|
| `ALL_PROXY` | the HTTP client (reqwest): the system's proxy for the server's outbound requests |
| `BAO_TOKEN` | through R1: the key-management service's token is the file /run/ferrum/kms.token |
| `CLICOLOR` | the terminal's colours (Loco's banner and log) |
| `CLICOLOR_FORCE` | the terminal's colours (Loco's banner and log) |
| `FERRUM_AUTHORITY_WITNESS_URL` | another database's address with its password: a secret the vault does not hold yet |
| `FERRUM_DATABASE_DIRECT_URL` | a whole address with its password: give the host in DATABASE_URL instead (ferrum_direct_host) |
| `FERRUM_DATABASE_READ_URL` | a whole address with its password: give the host in DATABASE_URL instead (ferrum_read_host) |
| `FERRUM_DRIVE_MAX_FILE_MB` | `drive` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_DRIVE_MAX_ZIP_MB` | `drive` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_EDITORS_URL` | `drive` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_EU_TEDB_URL` | `eu` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_EU_VIES_URL` | `eu` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_GB_HMRC_URL` | `gb` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_KMS_CA_FILE` | read by the key-management client (through R1; the file is /run/ferrum/kms-ca.pem from now on) |
| `FERRUM_KMS_KUBERNETES_TOKEN_FILE` | read by the key-management client (through R1; a file the server sends is never named by the database) |
| `FERRUM_KMS_TOKEN_FILE` | read by the key-management client (through R1; the token's file is /run/ferrum/kms.token from now on) |
| `FERRUM_MAIL_ADMIN` | `comms` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_MAIL_ADMIN_SECRET` | `comms`'s own secret, until it keeps it in its own vault |
| `FERRUM_MAIL_HOST` | `comms` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_MAIL_URL` | `comms` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_MEET_API_URL` | `comms` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_MEET_KEY` | `comms`'s own secret, until it keeps it in its own vault |
| `FERRUM_MEET_RECORDINGS` | `comms` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_MEET_SECRET` | `comms`'s own secret, until it keeps it in its own vault |
| `FERRUM_MEET_URL` | `comms` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_METRICS_ENGINE_TOKEN` | `metrics`'s own secret, until it keeps it in its own vault |
| `FERRUM_METRICS_ENGINE_URL` | `metrics` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_MOVE_SOURCE_DATABASE_URL` | an input of `ferrum task move_workspace`, given in its environment as it runs |
| `FERRUM_MOVE_SOURCE_FILES` | an input of `ferrum task move_workspace`, given in its environment as it runs |
| `FERRUM_MOVE_SOURCE_FILES_DIR` | an input of `ferrum task move_workspace`, given in its environment as it runs |
| `FERRUM_MOVE_SOURCE_REGION` | an input of `ferrum task move_workspace`, given in its environment as it runs |
| `FERRUM_MOVE_SOURCE_S3_BUCKET` | an input of `ferrum task move_workspace`, given in its environment as it runs |
| `FERRUM_MOVE_SOURCE_S3_ENDPOINT` | an input of `ferrum task move_workspace`, given in its environment as it runs |
| `FERRUM_MOVE_SOURCE_S3_REGION` | an input of `ferrum task move_workspace`, given in its environment as it runs |
| `FERRUM_MOVE_SOURCE_SECRET_KEYS` | an input of `ferrum task move_workspace`, given in its environment as it runs |
| `FERRUM_MY_MYINVOIS_URL` | `my` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_OFFICE_IPS` | `drive` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_OFFICE_URL` | `drive` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_OFFICE_WOPI_URL` | `drive` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_PT_CERTIFICATE` | `pt` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_PT_KEY_VERSION` | `pt` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_PT_SIGNING_KEY` | `pt`'s own secret, until it keeps it in its own vault |
| `FERRUM_PUBLIC_IP` | `gb` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_RETIRED_SECRET_KEYS` | the old master key's retired ones, until it retires |
| `FERRUM_RO_ANAF_URL` | `ro` reads it with std::env::var until its release declares it (operator_settings) |
| `FERRUM_SECRET_KEY` | the old master key, until it retires (docs/operations.md, Retiring the old key) |
| `FERRUM_SECRET_KEY_COMMAND` | a command line, never read from the database, where whoever writes a row would run a program |
| `FERRUM_TEST_RUN_DIR` | a debug build only: where a test puts the files of /run/ferrum (a release build never reads it) |
| `FERRUM_US_AVATAX_URL` | `us` reads it with std::env::var until its release declares it (operator_settings) |
| `HOME` | PostgreSQL's driver and the object store's client: where a user's own files would be (~/.pgpass, ~/.aws) |
| `HTTPS_PROXY` | the HTTP client (reqwest): the system's proxy for the server's outbound requests |
| `HTTP_PROXY` | the HTTP client (reqwest): the system's proxy for the server's outbound requests |
| `LANG` | the C library: the language of its own messages |
| `LOCO_CONFIG_FOLDER` | Loco: where the configuration files are (the image's /app/config otherwise) |
| `LOCO_ENV` | which configuration file the program reads (the image sets it; `-e production` on the command line does the same) |
| `NODE_ENV` | Loco's other names for LOCO_ENV, asked after it |
| `NO_COLOR` | the terminal's colours (Loco's banner and log) |
| `NO_PROXY` | the HTTP client (reqwest): hosts its outbound requests reach without the proxy |
| `OPENSSL_armcap` | the cryptography library: which processor instructions it uses |
| `PATH` | where a program the server runs is found (sh, for FERRUM_SECRET_KEY_COMMAND) |
| `PGAPPNAME` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGDATABASE` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGHOST` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGHOSTADDR` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGOPTIONS` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGPASSFILE` | PostgreSQL's driver (sqlx): a password file, where DATABASE_URL has no password |
| `PGPASSWORD` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGPORT` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGSSLCERT` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGSSLKEY` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGSSLMODE` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGSSLROOTCERT` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `PGUSER` | PostgreSQL's driver (sqlx): what DATABASE_URL leaves out |
| `RAILS_ENV` | Loco's other names for LOCO_ENV, asked after it |
| `REQUEST_METHOD` | the HTTP client (reqwest): set under CGI, where it ignores HTTP_PROXY (httpoxy) |
| `RUST_BACKTRACE` | the Rust runtime: how a panic is reported |
| `RUST_LIB_BACKTRACE` | the Rust runtime: how a panic is reported |
| `RUST_LOG` | the log's filter, over the log level (a diagnosing aid) |
| `RUST_MIN_STACK` | the Rust runtime: a new thread's stack |
| `SSH_AUTH_SOCK` | a library's SSH agent (libgit2), used only for a repository reached over SSH |
| `TMPDIR` | the Rust runtime: where temporary files go |
| `TOKIO_UNSTABLE_SHARDED_BLOCKING_QUEUE` | the async runtime: how it queues blocking work |
| `TOKIO_WORKER_THREADS` | the async runtime: how many threads it runs |
| `TZ` | the C library: the machine's time zone (the server works in UTC) |
| `USER` | PostgreSQL's driver (sqlx): the role, where DATABASE_URL names none |
| `VAULT_TOKEN` | through R1: the key-management service's token is the file /run/ferrum/kms.token |
| `WASMTIME_BACKTRACE_DETAILS` | the plugin runtime (wasmtime): how a plugin's crash is reported |
| `all_proxy` | the HTTP client (reqwest): the system's proxy, in lower case |
| `http_proxy` | the HTTP client (reqwest): the system's proxy, in lower case |
| `https_proxy` | the HTTP client (reqwest): the system's proxy, in lower case |
| `no_proxy` | the HTTP client (reqwest): hosts reached without the proxy, in lower case |

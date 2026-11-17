{{/* The server's name; the other resources are named from it (-pdf, -db, ...). */}}
{{- define "ferrum.fullname" -}}
{{- .Values.fullnameOverride | default "ferrum" -}}
{{- end -}}

{{/* Labels for a resource's metadata only; selectors use app.kubernetes.io/name alone. Call with (list . "name"). */}}
{{- define "ferrum.labels" -}}
app.kubernetes.io/name: {{ index . 1 }}
app.kubernetes.io/instance: {{ (index . 0).Release.Name }}
app.kubernetes.io/managed-by: {{ (index . 0).Release.Service }}
{{- end -}}

{{/* repository@digest, or repository:tag; fails when neither is set. Call with (list "values.path" imageValues). */}}
{{- define "ferrum.image" -}}
{{- $path := index . 0 -}}{{- $i := index . 1 -}}
{{- if $i.digest -}}{{ $i.repository }}@{{ $i.digest }}
{{- else if $i.tag -}}{{ $i.repository }}:{{ $i.tag }}
{{- else -}}{{ fail (printf "%s: set %s.digest (preferred) or %s.tag; there is no default image version" $path $path) }}
{{- end -}}
{{- end -}}

{{/* A key of the existing Secret. Call with (list . "KEY" optional). */}}
{{- define "ferrum.secretEnv" -}}
- name: {{ index . 1 }}
  valueFrom:
    secretKeyRef:
      name: {{ (index . 0).Values.secrets.existingSecret }}
      key: {{ index . 1 }}
{{- if index . 2 }}
      optional: true
{{- end }}
{{- end -}}

{{/* How far back the database's backups reach, in days: database.backup.retention, CloudNativePG's (30d, 4w, 3m; a month
counted as 31 days, so that deleted files are kept at least as long). */}}
{{- define "ferrum.backupDays" -}}
{{- $r := toString .Values.database.backup.retention -}}
{{- if not (regexMatch "^[1-9][0-9]*[dwm]$" $r) -}}
{{- fail (printf "database.backup.retention: %q is not days, weeks or months (30d, 4w, 3m)" $r) -}}
{{- end -}}
{{- $n := atoi (trimSuffix "d" (trimSuffix "w" (trimSuffix "m" $r))) -}}
{{- if hasSuffix "w" $r }}{{ mul $n 7 }}{{ else if hasSuffix "m" $r }}{{ mul $n 31 }}{{ else }}{{ $n }}{{ end -}}
{{- end -}}

{{/* The server's environment, shared by the Deployment and the migration Job. */}}
{{- define "ferrum.env" -}}
{{- if and .Values.database.enabled .Values.database.pooler.enabled .Values.database.pooler.used (not .direct) }}
{{- /* Through the pooler (database.yaml). The parts come from CloudNativePG's secret; the password it
generates is letters and digits only, so it needs no escaping in the address. */}}
{{- $n := include "ferrum.fullname" . -}}
- name: FERRUM_DB_USER
  valueFrom: {secretKeyRef: {name: {{ $n }}-db-app, key: username}}
- name: FERRUM_DB_PASSWORD
  valueFrom: {secretKeyRef: {name: {{ $n }}-db-app, key: password}}
- name: FERRUM_DB_NAME
  valueFrom: {secretKeyRef: {name: {{ $n }}-db-app, key: dbname}}
- {name: DATABASE_URL, value: "postgresql://$(FERRUM_DB_USER):$(FERRUM_DB_PASSWORD)@{{ $n }}-db-pooler.{{ .Release.Namespace }}:5432/$(FERRUM_DB_NAME)"}
- name: FERRUM_DATABASE_DIRECT_URL
  valueFrom: {secretKeyRef: {name: {{ $n }}-db-app, key: uri}}
{{- else -}}
- name: DATABASE_URL
  valueFrom:
    secretKeyRef:
{{- if .Values.database.enabled }}
      name: {{ include "ferrum.fullname" . }}-db-app
      key: uri
{{- else }}
{{- $s := include "ferrum.externalUrl" . | fromJson }}
      name: {{ $s.name }}
      key: {{ $s.key }}
{{- end }}
{{- end }}
{{- if not .Values.database.enabled }}
{{- with .Values.database.external.directHost }}
- {name: FERRUM_DATABASE_DIRECT_HOST, value: {{ . | quote }}}
{{- end }}
{{- if not .direct }}
{{- with .Values.database.external.readHost }}
- {name: FERRUM_DATABASE_READ_HOST, value: {{ . | quote }}}
{{- end }}
{{- end }}
{{- end }}
{{ include "ferrum.secretEnv" (list . "JWT_SECRET" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_CLAMAV_ADDR" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_CLAMAV_MAX_MB" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_CONNECTION_UPLOAD_MB" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_OPS_TELEMETRY_DAYS" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_AUDIT_SINK_URL" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_AUDIT_SINK_SECRET" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_BOOTSTRAP_TOKEN" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_METRICS_TOKEN" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_OPERATOR_TOKEN" true) }}
{{ include "ferrum.secretEnv" (list . "AWS_ACCESS_KEY_ID" true) }}
{{ include "ferrum.secretEnv" (list . "AWS_SECRET_ACCESS_KEY" true) }}
- {name: FERRUM_S3_BUCKET, value: {{ .Values.storage.bucket | quote }}}
- {name: FERRUM_S3_REGION, value: {{ .Values.storage.region | quote }}}
{{- with .Values.storage.endpoint }}
- {name: FERRUM_S3_ENDPOINT, value: {{ . | quote }}}
{{- end }}
{{- if .Values.database.enabled }}
{{- /* A deleted file stays as long as the database's backups reach (docs/operations.md, Backups of files). */}}
{{- $days := include "ferrum.backupDays" . }}
- {name: FERRUM_BACKUP_REACH_DAYS, value: {{ $days | quote }}}
- {name: FERRUM_FILES_DELETE_AFTER_DAYS, value: {{ $days | quote }}}
{{- end }}
- {name: PUBLIC_URL, value: {{ .Values.publicUrl | quote }}}
{{- with .Values.names.accountsHost }}
- {name: FERRUM_ACCOUNTS_HOST, value: {{ . | quote }}}
{{- end }}
{{- with .Values.names.hooksHost }}
- {name: FERRUM_HOOKS_HOST, value: {{ . | quote }}}
{{- end }}
{{- with .Values.names.passkeyRp }}
- {name: FERRUM_PASSKEY_RP, value: {{ . | quote }}}
{{- end }}
{{- with .Values.names.linkHost }}
- {name: FERRUM_LINK_HOST, value: {{ . | quote }}}
{{- end }}
{{- if ne (toString .Values.names.hostLeavingDays) "" }}
- {name: FERRUM_HOST_LEAVING_DAYS, value: {{ .Values.names.hostLeavingDays | toString | quote }}}
{{- end }}
{{- with .Values.names.domainProof }}
- {name: FERRUM_DOMAIN_PROOF, value: {{ . | quote }}}
{{- end }}
{{- with .Values.names.dnsResolvers }}
- {name: FERRUM_DNS_RESOLVERS, value: {{ . | quote }}}
{{- end }}
{{- if .Values.pdf.enabled }}
- {name: FERRUM_PDF_URL, value: "http://{{ include "ferrum.fullname" . }}-pdf:3000"}
{{- end }}
{{- if .Values.office.enabled }}
- {name: FERRUM_OFFICE_URL, value: "http://{{ include "ferrum.fullname" . }}-office.{{ .Values.office.namespace }}.svc.cluster.local:9980"}
- {name: FERRUM_OFFICE_WOPI_URL, value: "http://{{ include "ferrum.fullname" . }}.{{ .Release.Namespace }}.svc.cluster.local:80"}
- {name: FERRUM_OFFICE_ORIGIN, value: "https://{{ .Values.office.host }}"}
{{- end }}
- {name: FERRUM_DB_MAX_CONNECTIONS, value: {{ .Values.dbMaxConnections | quote }}}
- {name: LOG_LEVEL, value: {{ .Values.logLevel | quote }}}
{{- with .Values.zones }}
- {name: FERRUM_ZONES, value: {{ toJson . | quote }}}
{{- end }}
{{- with .Values.zoneDefault }}
- {name: FERRUM_ZONE_DEFAULT, value: {{ . | quote }}}
{{- end }}
{{- with .Values.zoneMode }}
- {name: FERRUM_ZONE_MODE, value: {{ . | quote }}}
{{- end }}
{{- if .Values.editors.enabled }}
- {name: FERRUM_EDITORS_URL, value: "https://{{ .Values.editors.host }}"}
{{- end }}
{{- if .Values.metricsEngine.enabled }}
- {name: FERRUM_METRICS_ENGINE_URL, value: "http://{{ include "ferrum.fullname" . }}-metrics-engine:5180"}
{{- /* Rendered: Metrics reads the token from its own vault, which the engine's render gets it from. */}}
{{- if ne (include "ferrum.renders" (list . "metrics-engine" .Values.metricsEngine.files)) "true" }}
- name: FERRUM_METRICS_ENGINE_TOKEN
  valueFrom:
    secretKeyRef:
      name: {{ .Values.metricsEngine.tokenSecret.name }}
      key: {{ .Values.metricsEngine.tokenSecret.key }}
{{- end }}
{{- end }}
{{- if and .Values.database.enabled .Values.database.readReplicas.enabled (not .direct) }}
{{- if lt (int .Values.database.instances) 2 }}
{{- fail "database.readReplicas needs database.instances of 2 or more: one instance has no standby to read from" }}
{{- end }}
{{- $n := include "ferrum.fullname" . -}}
{{- if not (and .Values.database.pooler.enabled .Values.database.pooler.used) }}
- name: FERRUM_DB_USER
  valueFrom: {secretKeyRef: {name: {{ $n }}-db-app, key: username}}
- name: FERRUM_DB_PASSWORD
  valueFrom: {secretKeyRef: {name: {{ $n }}-db-app, key: password}}
- name: FERRUM_DB_NAME
  valueFrom: {secretKeyRef: {name: {{ $n }}-db-app, key: dbname}}
{{- end }}
- {name: FERRUM_DATABASE_READ_URL, value: "postgresql://$(FERRUM_DB_USER):$(FERRUM_DB_PASSWORD)@{{ $n }}-db-ro.{{ .Release.Namespace }}:5432/$(FERRUM_DB_NAME)"}
- {name: FERRUM_DATABASE_READ_MAX_LAG_SECS, value: {{ .Values.database.readReplicas.maxLagSeconds | quote }}}
{{- end }}
{{- with .Values.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{/* The services the server's image declares (`ferrum task services`) among the companions this chart bundles: with
`environment: minimal`, these companions take their secrets and settings from files rendered as they start, unless their
`files` value says otherwise. A companion is added here in the change that makes its owner declare its service: until
then its owner reads its secret from the environment, and a render would find no such service. Metrics declares
`metrics-engine` from its 0.9.0. */}}
{{- define "ferrum.declaredServices" -}}
{{- toJson (list "metrics-engine") -}}
{{- end -}}

{{/* "true" when the companion's files are rendered as it starts: its `files` value is `rendered`, or empty with
`environment: minimal` and its service declared (ferrum.declaredServices). Call with (list . "<service>" files). */}}
{{- define "ferrum.renders" -}}
{{- $root := index . 0 -}}{{- $files := index . 2 | default "" -}}
{{- if not (has $files (list "" "secret" "rendered")) -}}
{{- fail (printf "%s's files are secret or rendered, not %q" (index . 1) $files) -}}
{{- end -}}
{{- $minimal := and (eq $root.Values.environment "minimal") (has (index . 1) (include "ferrum.declaredServices" $root | fromJsonArray)) -}}
{{- if or (eq $files "rendered") (and (eq $files "") $minimal) }}true{{ end -}}
{{- end -}}

{{/* The init container that renders a companion's files as its pod starts (docs/operations.md, "Files for the programs
beside the server"): the server's image running `ferrum task render`, with the database's address as its one input
(straight to PostgreSQL, as the Jobs) and /run/ferrum for the vault's key file, writing into the memory-backed volume
`companion`, which the companion mounts read-only at /run/companion. Its pod gets `ferrum.renderVolumes`, and its network
policy `ferrum.databaseEgress` and DNS. Call with (list . "<service>"). */}}
{{- define "ferrum.renderInit" -}}
{{- $root := index . 0 -}}
- name: render
  image: {{ include "ferrum.image" (list "image" $root.Values.image) }}
  args: ["task", "render", "service:{{ index . 1 }}", "to:/run/companion"]
  env:
    {{- range (include "ferrum.env" (dict "Values" $root.Values "Release" $root.Release "Chart" $root.Chart "direct" true) | fromYamlArray) }}
    {{- if has .name (list "DATABASE_URL" "FERRUM_DATABASE_DIRECT_HOST") }}
    - {{ toJson . }}
    {{- end }}
    {{- end }}
  securityContext:
    allowPrivilegeEscalation: false
    readOnlyRootFilesystem: true
    capabilities: {drop: ["ALL"]}
  volumeMounts:
    - {name: render-tmp, mountPath: /tmp}
    {{- include "ferrum.runMount" $root | nindent 4 }}
    - {name: companion, mountPath: /run/companion}
{{- end -}}

{{- define "ferrum.renderVolumes" -}}
# What the render writes, in memory: the companion's secrets are never on a disk.
- name: companion
  emptyDir: {medium: Memory, sizeLimit: 1Mi}
- name: render-tmp
  emptyDir: {sizeLimit: 64Mi}
{{ include "ferrum.runVolume" . }}
{{- end -}}

{{/* The way out to the database: the chart's own cluster, or an outside database's networks (database.external.egress);
nothing when neither is there. */}}
{{- define "ferrum.databaseEgress" -}}
{{- if .Values.database.enabled -}}
- to:
    - podSelector:
        matchLabels: {cnpg.io/cluster: {{ include "ferrum.fullname" . }}-db}
  ports:
    - {port: 5432}
{{- else if .Values.database.external.egress.cidrs -}}
# The outside database and its pooler (database.external.egress).
- to:
    {{- range .Values.database.external.egress.cidrs }}
    - ipBlock: {cidr: {{ . }}}
    {{- end }}
  ports:
    {{- range .Values.database.external.egress.ports }}
    - {port: {{ . }}}
    {{- end }}
{{- end -}}
{{- end -}}

{{/* The Secret holding an outside database's address (database.external.urlSecret, or the older database.urlSecret),
as JSON {name, key}. */}}
{{- define "ferrum.externalUrl" -}}
{{- $e := .Values.database.external.urlSecret | default dict -}}
{{- $old := .Values.database.urlSecret | default dict -}}
{{- $name := required "database.enabled is false: set database.external.urlSecret.name, the Secret holding the database's address" ($e.name | default $old.name) -}}
{{- toJson (dict "name" $name "key" (ternary ($e.key | default "uri") ($old.key | default "uri") (not (empty $e.name)))) -}}
{{- end -}}

{{/* /run/ferrum, the files at fixed paths a pod reads when they are there (docs/plan/stateless.md, D1): the vault's key
file as root.key (vault.keySecret, or the Secret the chart makes, ferrum.vaultKeySecret), the outside database's
certificate authority as db-ca.pem, the key-management service's token and certificate authority as kms.token and
kms-ca.pem. Every source is optional, so a pod starts before the key's Secret exists (a migrate Job on the first upgrade
to this chart). `ferrum.runVolume` goes in a pod's volumes, `ferrum.runMount` in its container's volumeMounts. */}}
{{- define "ferrum.runVolume" -}}
{{- $ca := .Values.database.external.caSecret | default dict -}}
{{- $v := .Values.vault | default dict -}}
{{- $key := $v.keySecret | default dict -}}
{{- $kms := $v.kms | default dict -}}
{{- $token := $kms.tokenSecret | default dict -}}
{{- $kmsCa := $kms.caSecret | default dict -}}
- name: run
  projected:
    sources:
      - secret: {name: {{ include "ferrum.vaultKeySecret" . }}, items: [{key: {{ $key.key | default "root.key" }}, path: root.key}], optional: true}
      {{- if and (not .Values.database.enabled) $ca.name }}
      - secret: {name: {{ $ca.name }}, items: [{key: {{ $ca.key | default "ca.crt" }}, path: db-ca.pem}]}
      {{- end }}
      {{- with $token.name }}
      - secret: {name: {{ . }}, items: [{key: {{ $token.key | default "token" }}, path: kms.token}]}
      {{- end }}
      {{- with $kmsCa.name }}
      - secret: {name: {{ . }}, items: [{key: {{ $kmsCa.key | default "ca.crt" }}, path: kms-ca.pem}]}
      {{- end }}
{{- end -}}

{{- define "ferrum.runMount" -}}
- {name: run, mountPath: /run/ferrum, readOnly: true}
{{- end -}}

{{/* The Secret holding the vault's key file: vault.keySecret.name, or the one the chart makes (vault-key.yaml). */}}
{{- define "ferrum.vaultKeySecret" -}}
{{- $key := (.Values.vault | default dict).keySecret | default dict -}}
{{- $key.name | default (printf "%s-vault-key" (include "ferrum.fullname" .)) -}}
{{- end -}}

{{/* Whether pods mount their service account's token: only to sign in to the key-management service as the pod. */}}
{{- define "ferrum.serviceAccountToken" -}}
{{- ternary "true" "false" (((.Values.vault | default dict).kms | default dict).serviceAccount | default false) -}}
{{- end -}}

{{/* The variables `environment: minimal` keeps besides the database's address and the secrets: what the address is
made of, and what apps compiled into the server read from the environment themselves (server/src/settings.yaml, the
entries that move to their app). The direct and read hosts are not among them: they are the address's own parameters
there (`ferrum_direct_host`, `ferrum_read_host`, which the server takes out before it connects). */}}
{{- define "ferrum.envKept" -}}
{{- toJson (list "FERRUM_DB_USER" "FERRUM_DB_PASSWORD" "FERRUM_DB_NAME" "FERRUM_DATABASE_DIRECT_URL" "FERRUM_DATABASE_READ_URL" "FERRUM_OFFICE_URL" "FERRUM_OFFICE_WOPI_URL" "FERRUM_EDITORS_URL" "FERRUM_METRICS_ENGINE_URL") -}}
{{- end -}}

{{/* A pool's environment with `environment: minimal` (values.yaml): the database's address naming the pool
(`application_name=pool:<name>`; `default` with no pools), the secrets, the variables in ferrum.envKept, extraEnv, the
pool's own `env` and, while the migrate Job runs, migrate.serverEnv. Call with (list . pool). */}}
{{- define "ferrum.minimalEnv" -}}
{{- $root := index . 0 -}}{{- $p := index . 1 -}}
{{- $v := $root.Values -}}
{{- $kept := include "ferrum.envKept" $root | fromJsonArray -}}
{{- $extra := dict -}}{{- range $v.extraEnv }}{{ $_ := set $extra .name true }}{{ end -}}
{{- $pool := printf "application_name=pool:%s" ($p.name | default "default") -}}
{{- if not $v.database.enabled -}}
{{- with $v.database.external.directHost }}{{ $pool = printf "%s&ferrum_direct_host=%s" $pool . }}{{ end -}}
{{- with $v.database.external.readHost }}{{ $pool = printf "%s&ferrum_read_host=%s" $pool . }}{{ end -}}
{{- end -}}
{{- $env := list -}}
{{- range (include "ferrum.env" $root | fromYamlArray) -}}
{{- if and (eq .name "DATABASE_URL") (hasKey . "valueFrom") -}}
{{- $sep := ternary "&" "?" (and (not $v.database.enabled) $v.database.urlHasQuery) -}}
{{- $env = append $env (dict "name" "FERRUM_DATABASE_ADDRESS" "valueFrom" .valueFrom) -}}
{{- $env = append $env (dict "name" "DATABASE_URL" "value" (printf "$(FERRUM_DATABASE_ADDRESS)%s%s" $sep $pool)) -}}
{{- else if eq .name "DATABASE_URL" -}}
{{- $env = append $env (dict "name" "DATABASE_URL" "value" (printf "%s?%s" .value $pool)) -}}
{{- else if or (hasKey . "valueFrom") (has .name $kept) (hasKey $extra .name) -}}
{{- $env = append $env . -}}
{{- end -}}
{{- end -}}
{{- if eq (include "ferrum.migrateJob" $root) "true" }}{{ $env = concat $env $v.migrate.serverEnv }}{{ end -}}
{{- $env = concat $env ($p.env | default list) -}}
{{- toYaml $env -}}
{{- end -}}

{{/* The document the settings Job applies (values.yaml, `settings`): with `environment: minimal`, the values the
full environment would have held, each under its setting's key (its variable in lower case, without FERRUM_), and
each pool's own three; `settings` over them. */}}
{{- define "ferrum.settingsDocument" -}}
{{- $v := .Values -}}
{{- $doc := dict -}}
{{- if eq $v.environment "minimal" -}}
{{- $skip := concat (include "ferrum.envKept" . | fromJsonArray) (list "DATABASE_URL" "FERRUM_DB_MAX_CONNECTIONS" "FERRUM_RUN_BACKGROUND" "FERRUM_ZONE" "FERRUM_DATABASE_DIRECT_HOST" "FERRUM_DATABASE_READ_HOST") -}}
{{- range $v.extraEnv }}{{ $skip = append $skip .name }}{{ end -}}
{{- $installation := dict -}}
{{- range (include "ferrum.env" . | fromYamlArray) -}}
{{- if and (hasKey . "value") (not (has .name $skip)) -}}
{{- $_ := set $installation (.name | lower | trimPrefix "ferrum_") (toString .value) -}}
{{- end -}}
{{- end -}}
{{- $pools := dict -}}
{{- range (include "ferrum.pools" . | fromJson).pools -}}
{{- $_ := set $pools (.name | default "default") (dict "db_max_connections" (toString .dbMaxConnections) "run_background" (ternary "false" "true" (eq .background "off")) "zone" (.zone | default "")) -}}
{{- end -}}
{{- $doc = dict "installation" $installation "pools" $pools -}}
{{- end -}}
{{- toYaml (mergeOverwrite $doc (deepCopy ($v.settings | default dict))) -}}
{{- end -}}

{{/* "true" when the settings Job is rendered: a document to apply, and a database it can reach before the pods start
(as ferrum.migrateJob). `environment: minimal` cannot do without it. */}}
{{- define "ferrum.settingsJob" -}}
{{- if not (has .Values.environment (list "full" "minimal")) -}}
{{- fail (printf "environment is full or minimal, not %q" .Values.environment) -}}
{{- end -}}
{{- $reachable := or (not .Values.database.enabled) .Release.IsUpgrade -}}
{{- if and (eq .Values.environment "minimal") (not $reachable) -}}
{{- fail "environment: minimal takes the settings from the database, which the chart's own database cannot hold before its first install: install with environment: full, then switch" -}}
{{- end -}}
{{- if and $reachable (or .Values.settings (eq .Values.environment "minimal")) }}true{{ end -}}
{{- end -}}

{{/*
How the database starts (values.yaml, database.bootstrap): the Cluster's `bootstrap` and, for a mode that
reads another server or a backup, its `externalClusters`. CloudNativePG reads this once, when it creates the
Cluster. Fields checked against CloudNativePG 1.30 (docs/src/bootstrap.md, database_import.md,
recovery.md) and the Barman Cloud plugin 0.16 (web/docs/usage.md).
  empty     initdb: a new, empty database (today's).
  import    initdb.import, type microservice: pg_dump of one database from a running server, restored here, read
            through row-level security as the system's own session, so the source's own role will do.
            pgDumpExtraOptions is in CloudNativePG's main branch (the release that added it is not checked), and the
            `options` connection parameter is passed to libpq as given (from memory, not checked against the operator).
            The server sees a database it never knew and holds outbound work ("After restoring a backup").
  copy      pg_basebackup: a physical copy of a running server, same PostgreSQL major version. It keeps the
            database's identity, but the copy starts on a new timeline, which the server also holds for
            unless FERRUM_HOLD_ON_TIMELINE=false.
  recovery  recovery from the Barman Cloud plugin's backups, optionally to a point in time: a new timeline,
            so held as after any restore.
*/}}
{{- define "ferrum.dbBootstrap" -}}
{{- $n := include "ferrum.fullname" . -}}
{{- $b := .Values.database.bootstrap | default dict -}}
{{- $mode := $b.mode | default "empty" -}}
{{- $src := $b.source | default dict -}}
{{- if eq $mode "empty" -}}
bootstrap:
  initdb:
    database: ferrum
    owner: ferrum
{{- else if or (eq $mode "import") (eq $mode "copy") -}}
{{- $host := required (printf "database.bootstrap.mode %s copies from another server: set database.bootstrap.source.host" $mode) $src.host -}}
{{- $user := required (printf "database.bootstrap.mode %s: set database.bootstrap.source.user (%s)" $mode (ternary "the role the server runs as there, which owns the database, or a superuser" "a role with REPLICATION and LOGIN" (eq $mode "import"))) $src.user -}}
{{- $pw := $src.passwordSecret | default dict -}}
{{- $pwName := required (printf "database.bootstrap.mode %s: set database.bootstrap.source.passwordSecret.name, a Secret holding that role's password" $mode) $pw.name -}}
bootstrap:
  {{- if eq $mode "import" }}
  initdb:
    database: ferrum
    owner: ferrum
    import:
      type: microservice
      databases: [{{ $src.database | default "ferrum" }}]
      source: {externalCluster: origin}
      # Through row-level security as the system's own session and the vault's (`options` below), so the source's
      # ordinary role reads every workspace's rows and the vault's (docs/operations.md, "Backups and restore");
      # harmless for a superuser.
      pgDumpExtraOptions: ["--enable-row-security"]
  {{- else }}
  pg_basebackup:
    source: origin
    database: {{ $src.database | default "ferrum" }}
    owner: ferrum
  {{- end }}
externalClusters:
  - name: origin
    connectionParameters:
      host: {{ $host | quote }}
      port: {{ $src.port | default 5432 | quote }}
      user: {{ $user | quote }}
      dbname: {{ $src.database | default "ferrum" | quote }}
      sslmode: {{ $src.sslmode | default "prefer" | quote }}
      {{- if eq $mode "import" }}
      options: "-c ferrum.system=on -c ferrum.vault=on"
      {{- end }}
    password: {name: {{ $pwName }}, key: {{ $pw.key | default "password" }}}
{{- else if eq $mode "recovery" -}}
{{- $r := $b.recovery | default dict -}}
{{- $store := $r.objectStore | default (printf "%s-backups" $n) -}}
{{- $server := $r.serverName | default (printf "%s-db" $n) -}}
{{- if and (eq $store (printf "%s-backups" $n)) (eq $server (.Values.database.backup.serverName | default (printf "%s-db" $n))) -}}
{{- fail (printf "database.bootstrap.mode recovery reads the backups of %s in %s, where this cluster would also write its own: set database.backup.serverName to a new name (such as %s-2)" $server $store $server) -}}
{{- end -}}
bootstrap:
  recovery:
    source: origin
    database: ferrum
    owner: ferrum
    {{- with $r.targetTime }}
    recoveryTarget: {targetTime: {{ . | quote }}}
    {{- end }}
externalClusters:
  - name: origin
    plugin:
      name: barman-cloud.cloudnative-pg.io
      parameters:
        barmanObjectName: {{ $store }}
        serverName: {{ $server }}
{{- else -}}
{{- fail (printf "database.bootstrap.mode is empty, import, copy or recovery, not %q" $mode) -}}
{{- end -}}
{{- end -}}

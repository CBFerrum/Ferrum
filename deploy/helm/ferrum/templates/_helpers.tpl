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

{{/* The values that ship as placeholders (a CHANGE-ME bucket, a host on example.com): rendering fails naming each one still
set, so that nothing installs with them. */}}
{{- define "ferrum.placeholders" -}}
{{- $example := "(^|[/.])example[.]com([/:]|$)" -}}
{{- $unset := list -}}
{{- if regexMatch $example .Values.publicUrl }}{{ $unset = append $unset "publicUrl" }}{{ end -}}
{{- if contains "CHANGE-ME" .Values.storage.bucket }}{{ $unset = append $unset "storage.bucket" }}{{ end -}}
{{- if and .Values.gateway.enabled (regexMatch $example .Values.gateway.hostname) }}{{ $unset = append $unset "gateway.hostname" }}{{ end -}}
{{- if and .Values.privateGateway.enabled (regexMatch $example (join " " .Values.privateGateway.hostnames)) }}{{ $unset = append $unset "privateGateway.hostnames" }}{{ end -}}
{{- if and .Values.office.enabled (regexMatch $example .Values.office.host) }}{{ $unset = append $unset "office.host" }}{{ end -}}
{{- if and .Values.editors.enabled (regexMatch $example .Values.editors.host) }}{{ $unset = append $unset "editors.host" }}{{ end -}}
{{- with $unset }}{{ fail (printf "these ship as placeholders, set each to your own: %s" (join ", " .)) }}{{ end -}}
{{- include "ferrum.database" . -}}
{{- end -}}

{{/* The database's values, checked: the three that have no default, and the ones that are gone. The chart takes the address
of a PostgreSQL and runs none (reference/kubernetes/ is an example of one), so each value that made it run one fails with
what to set instead. */}}
{{- define "ferrum.database" -}}
{{- $d := .Values.database -}}
{{- range $gone, $instead := dict
  "enabled" "the chart no longer runs a database: set database.urlSecret.name to the Secret holding the address of yours (reference/kubernetes/ runs one as an example)"
  "external" "database.external.* moved up one level: set database.urlSecret, database.directHost, database.readHost, database.caSecret and database.egress"
  "instances" "the chart no longer runs a database: its instances are the database's own (reference/kubernetes/database/values.yaml)"
  "imageName" "the chart no longer runs a database (reference/kubernetes/database/values.yaml)"
  "storageSize" "the chart no longer runs a database (reference/kubernetes/database/values.yaml)"
  "walStorageSize" "the chart no longer runs a database (reference/kubernetes/database/values.yaml)"
  "parameters" "the chart no longer runs a database (reference/kubernetes/database/values.yaml)"
  "resources" "the chart no longer runs a database (reference/kubernetes/database/values.yaml)"
  "backup" "the chart no longer runs a database or its backups: set database.backupReachDays to how many days back they reach (reference/kubernetes/database/values.yaml, database.backup, is an example)"
  "pooler" "the chart no longer runs a pooler: run yours in front of the database, point database.urlSecret at it and set database.directHost to PostgreSQL itself"
  "readReplicas" "set database.readHost to the host of a read replica"
  "bootstrap" "the chart no longer starts a database: bring the data to yours (docs/operations.md, \"Any PostgreSQL\"; reference/kubernetes/database/values.yaml, database.bootstrap, is an example)"
-}}
{{- if hasKey $d $gone }}{{ fail (printf "database.%s is gone: %s" $gone $instead) }}{{ end -}}
{{- end -}}
{{- $_ := required "database.urlSecret.name is required: the Secret holding the address of your PostgreSQL (DATABASE_URL, the ordinary role's), under the key database.urlSecret.key" $d.urlSecret.name -}}
{{- if or (kindIs "invalid" $d.connectionLimit) (not (regexMatch "^[0-9]+$" (toString $d.connectionLimit))) -}}
{{- fail "database.connectionLimit is required: the connections the role may hold on PostgreSQL for this installation (its rolconnlimit, or max_connections less what you keep for administration and replication)" -}}
{{- end -}}
{{- if kindIs "invalid" $d.backupReachDays -}}
{{- fail "database.backupReachDays is required: how many days back the database's backups reach (0 when there are none), which is also how long a deleted file stays in the bucket" -}}
{{- end -}}
{{- if not (regexMatch "^[0-9]+$" (toString $d.backupReachDays)) -}}
{{- fail (printf "database.backupReachDays is a whole number of days, not %v" $d.backupReachDays) -}}
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

{{/* The server's environment, shared by the Deployment and the migration Job. */}}
{{- define "ferrum.env" -}}
- name: DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ .Values.database.urlSecret.name }}
      key: {{ .Values.database.urlSecret.key | default "uri" }}
{{- with .Values.database.directHost }}
- {name: FERRUM_DATABASE_DIRECT_HOST, value: {{ . | quote }}}
{{- end }}
{{- if not .direct }}
{{- with .Values.database.readHost }}
- {name: FERRUM_DATABASE_READ_HOST, value: {{ . | quote }}}
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
{{- /* A deleted file stays as long as the database's backups reach (docs/operations.md, Backups of files). */}}
{{- $days := toString (int64 .Values.database.backupReachDays) }}
- {name: FERRUM_BACKUP_REACH_DAYS, value: {{ $days | quote }}}
- {name: FERRUM_FILES_DELETE_AFTER_DAYS, value: {{ $days | quote }}}
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

{{/* The way out to the database (database.egress): its peers (an ipBlock for a provider, a podSelector and
namespaceSelector for one in this cluster) and ports; nothing when `to` is empty. */}}
{{- define "ferrum.databaseEgress" -}}
{{- with .Values.database.egress }}
{{- if .to -}}
# The database and its pooler (database.egress).
- to: {{- toYaml .to | nindent 4 }}
  ports:
    {{- range .ports }}
    - {port: {{ . }}}
    {{- end }}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* The way out to the bucket's store (storage.egress), for one inside this cluster or at addresses the HTTPS rule does
not cover; nothing when `to` is empty. */}}
{{- define "ferrum.storageEgress" -}}
{{- with .Values.storage.egress }}
{{- if .to -}}
# The object store (storage.egress).
- to: {{- toYaml .to | nindent 4 }}
  ports:
    {{- range .ports }}
    - {port: {{ . }}}
    {{- end }}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* /run/ferrum, the files at fixed paths a pod reads when they are there (docs/plan/stateless.md, D1): the vault's key
file as root.key (vault.keySecret, or the Secret the chart makes, ferrum.vaultKeySecret), the outside database's
certificate authority as db-ca.pem, the key-management service's token and certificate authority as kms.token and
kms-ca.pem. Every source is optional, so a pod starts before the key's Secret exists (a migrate Job on the first upgrade
to this chart). `ferrum.runVolume` goes in a pod's volumes, `ferrum.runMount` in its container's volumeMounts. */}}
{{- define "ferrum.runVolume" -}}
{{- $ca := .Values.database.caSecret | default dict -}}
{{- $v := .Values.vault | default dict -}}
{{- $key := $v.keySecret | default dict -}}
{{- $kms := $v.kms | default dict -}}
{{- $token := $kms.tokenSecret | default dict -}}
{{- $kmsCa := $kms.caSecret | default dict -}}
- name: run
  projected:
    sources:
      - secret: {name: {{ include "ferrum.vaultKeySecret" . }}, items: [{key: {{ $key.key | default "root.key" }}, path: root.key}], optional: true}
      {{- if $ca.name }}
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
{{- toJson (list "FERRUM_OFFICE_URL" "FERRUM_OFFICE_WOPI_URL" "FERRUM_EDITORS_URL" "FERRUM_METRICS_ENGINE_URL") -}}
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
{{- with $v.database.directHost }}{{ $pool = printf "%s&ferrum_direct_host=%s" $pool . }}{{ end -}}
{{- with $v.database.readHost }}{{ $pool = printf "%s&ferrum_read_host=%s" $pool . }}{{ end -}}
{{- $env := list -}}
{{- range (include "ferrum.env" $root | fromYamlArray) -}}
{{- if and (eq .name "DATABASE_URL") (hasKey . "valueFrom") -}}
{{- $sep := ternary "&" "?" $v.database.urlHasQuery -}}
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

{{/* "true" when the settings Job is rendered: a document to apply. `environment: minimal` cannot do without it. */}}
{{- define "ferrum.settingsJob" -}}
{{- if not (has .Values.environment (list "full" "minimal")) -}}
{{- fail (printf "environment is full or minimal, not %q" .Values.environment) -}}
{{- end -}}
{{- if or .Values.settings (eq .Values.environment "minimal") }}true{{ end -}}
{{- end -}}

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
      name: {{ required "database.urlSecret.name is required when database.enabled is false" .Values.database.urlSecret.name }}
      key: {{ .Values.database.urlSecret.key }}
{{- end }}
{{- end }}
{{ include "ferrum.secretEnv" (list . "JWT_SECRET" false) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_SECRET_KEY" false) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_CLAMAV_ADDR" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_CLAMAV_MAX_MB" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_CONNECTION_UPLOAD_MB" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_OPS_TELEMETRY_DAYS" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_RETIRED_SECRET_KEYS" true) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_BOOTSTRAP_TOKEN" false) }}
{{ include "ferrum.secretEnv" (list . "FERRUM_METRICS_TOKEN" false) }}
{{ include "ferrum.secretEnv" (list . "AWS_ACCESS_KEY_ID" false) }}
{{ include "ferrum.secretEnv" (list . "AWS_SECRET_ACCESS_KEY" false) }}
- {name: FERRUM_S3_BUCKET, value: {{ .Values.storage.bucket | quote }}}
- {name: FERRUM_S3_REGION, value: {{ .Values.storage.region | quote }}}
{{- with .Values.storage.endpoint }}
- {name: FERRUM_S3_ENDPOINT, value: {{ . | quote }}}
{{- end }}
- {name: PUBLIC_URL, value: {{ .Values.publicUrl | quote }}}
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
- name: FERRUM_METRICS_ENGINE_TOKEN
  valueFrom:
    secretKeyRef:
      name: {{ .Values.metricsEngine.tokenSecret.name }}
      key: {{ .Values.metricsEngine.tokenSecret.key }}
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

{{/*
How the database starts (values.yaml, database.bootstrap): the Cluster's `bootstrap` and, for a mode that
reads another server or a backup, its `externalClusters`. CloudNativePG reads this once, when it creates the
Cluster. Fields checked against CloudNativePG 1.30 (docs/src/bootstrap.md, database_import.md,
recovery.md) and the Barman Cloud plugin 0.16 (web/docs/usage.md).
  empty     initdb: a new, empty database (today's).
  import    initdb.import, type microservice: pg_dump of one database from a running server, restored here.
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
{{- $user := required (printf "database.bootstrap.mode %s: set database.bootstrap.source.user (%s)" $mode (ternary "a superuser: row-level security hides every workspace's rows from any other role" "a role with REPLICATION and LOGIN" (eq $mode "import"))) $src.user -}}
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

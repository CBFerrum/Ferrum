{{/* The cluster's name; the other resources are named from it (-db, -backups, -db-pooler). The server chart names its
Deployment the same way (fullnameOverride), so the Secret CloudNativePG makes is "<fullname>-db-app". */}}
{{- define "ferrum.fullname" -}}
{{- .Values.fullnameOverride | default "ferrum" -}}
{{- end -}}

{{/* How far back the backups reach, in days: database.backup.retention, CloudNativePG's (30d, 4w, 3m; a month counted as
31 days, so that the days handed to the server's backupReachDays are never more than the backups keep). */}}
{{- define "ferrum.backupDays" -}}
{{- $r := toString .Values.database.backup.retention -}}
{{- if not (regexMatch "^[1-9][0-9]*[dwm]$" $r) -}}
{{- fail (printf "database.backup.retention: %q is not days, weeks or months (30d, 4w, 3m)" $r) -}}
{{- end -}}
{{- $n := atoi (trimSuffix "d" (trimSuffix "w" (trimSuffix "m" $r))) -}}
{{- if hasSuffix "w" $r }}{{ mul $n 7 }}{{ else if hasSuffix "m" $r }}{{ mul $n 31 }}{{ else }}{{ $n }}{{ end -}}
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

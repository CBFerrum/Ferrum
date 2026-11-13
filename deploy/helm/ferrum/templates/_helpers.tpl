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
{{- with .Values.names.accountsHost }}
- {name: FERRUM_ACCOUNTS_HOST, value: {{ . | quote }}}
{{- end }}
{{- with .Values.names.hooksHost }}
- {name: FERRUM_HOOKS_HOST, value: {{ . | quote }}}
{{- end }}
{{- with .Values.names.passkeyRp }}
- {name: FERRUM_PASSKEY_RP, value: {{ . | quote }}}
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
{{- with .Values.extraEnv }}
{{ toYaml . }}
{{- end }}
{{- end -}}

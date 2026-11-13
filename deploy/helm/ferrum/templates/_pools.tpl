{{/*
The server's pools (values.yaml, `pools`), resolved: JSON {"pools": [...]}. With no `pools` there is one
pool, today's, named as before. The first pool keeps today's names and selector; every other pool is
"<fullname>-<name>". A pool takes every setting it does not give from the top-level values.
Call with the root context: include "ferrum.pools" . | fromJson
*/}}
{{- define "ferrum.pools" -}}
{{- $v := .Values -}}
{{- $n := include "ferrum.fullname" . -}}
{{- $out := list -}}
{{- if not $v.pools -}}
{{- $out = append $out (dict "name" "" "obj" $n "first" true "background" "both" "zone" "" "replicas" $v.replicas "autoscaling" $v.autoscaling "resources" $v.resources "pdb" $v.pdb "dbMaxConnections" $v.dbMaxConnections) -}}
{{- else -}}
{{- range $i, $p := $v.pools -}}
{{- $name := required (printf "pools[%d].name is required" $i) $p.name | toString -}}
{{- if not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$" $name) -}}
{{- fail (printf "pools[%d].name %q: use lower-case letters, digits and dashes" $i $name) -}}
{{- end -}}
{{- $bg := $p.background | default "both" -}}
{{- if not (has $bg (list "both" "off" "only")) -}}
{{- fail (printf "pool %s: background is both, off or only, not %q" $name $bg) -}}
{{- end -}}
{{- /* Booleans set to false are kept: merge would treat them as unset. */ -}}
{{- $as := mergeOverwrite (deepCopy $v.autoscaling) (deepCopy ($p.autoscaling | default dict)) -}}
{{- if and $p.autoscaling (hasKey $p.autoscaling "enabled") -}}{{- $_ := set $as "enabled" $p.autoscaling.enabled -}}{{- end -}}
{{- $pdb := mergeOverwrite (deepCopy $v.pdb) (deepCopy ($p.pdb | default dict)) -}}
{{- if and $p.pdb (hasKey $p.pdb "enabled") -}}{{- $_ := set $pdb "enabled" $p.pdb.enabled -}}{{- end -}}
{{- $out = append $out (dict
  "name" $name
  "obj" (ternary $n (printf "%s-%s" $n $name) (eq $i 0))
  "first" (eq $i 0)
  "background" $bg
  "zone" ($p.zone | default "" | toString)
  "replicas" (hasKey $p "replicas" | ternary $p.replicas $v.replicas)
  "autoscaling" $as
  "resources" ($p.resources | default $v.resources)
  "pdb" $pdb
  "dbMaxConnections" (hasKey $p "dbMaxConnections" | ternary $p.dbMaxConnections $v.dbMaxConnections)
  "env" ($p.env | default list)
  "nodeSelector" ($p.nodeSelector | default dict)
  "tolerations" ($p.tolerations | default list)
  "priorityClassName" ($p.priorityClassName | default "")
) -}}
{{- end -}}
{{- end -}}
{{- toJson (dict "pools" $out) -}}
{{- end -}}

{{/* A selector for every server pod, whatever its pool. Call with the root context. */}}
{{- define "ferrum.serverPods" -}}
{{- $n := include "ferrum.fullname" . -}}
{{- if .Values.pools -}}
{{- $names := list -}}
{{- range (include "ferrum.pools" . | fromJson).pools }}{{ $names = append $names .obj }}{{ end -}}
matchExpressions: [{key: app.kubernetes.io/name, operator: In, values: {{ toJson $names }}}]
{{- else -}}
matchLabels: {app.kubernetes.io/name: {{ $n }}}
{{- end -}}
{{- end -}}

{{/* A pool's environment: the shared one (ferrum.env), with the pool's own settings replacing or added to it.
Call with (list . pool). */}}
{{- define "ferrum.poolEnv" -}}
{{- $root := index . 0 -}}{{- $p := index . 1 -}}
{{- $add := list (dict "name" "FERRUM_DB_MAX_CONNECTIONS" "value" (toString $p.dbMaxConnections)) -}}
{{- if eq $p.background "off" -}}{{- $add = append $add (dict "name" "FERRUM_RUN_BACKGROUND" "value" "false") -}}{{- end -}}
{{- with $p.zone -}}{{- $add = append $add (dict "name" "FERRUM_ZONE" "value" .) -}}{{- end -}}
{{- if eq (include "ferrum.migrateJob" $root) "true" -}}{{- $add = concat $add $root.Values.migrate.serverEnv -}}{{- end -}}
{{- $add = concat $add $p.env -}}
{{- $by := dict -}}
{{- range $add }}{{ $_ := set $by .name . }}{{ end -}}
{{- $env := list -}}
{{- range (include "ferrum.env" $root | fromYamlArray) -}}
{{- if hasKey $by .name }}{{ $env = append $env (get $by .name) }}{{ $_ := unset $by .name }}{{ else }}{{ $env = append $env . }}{{ end -}}
{{- end -}}
{{- range $add }}{{ if hasKey $by .name }}{{ $env = append $env (get $by .name) }}{{ $_ := unset $by .name }}{{ end }}{{ end -}}
{{- toYaml $env -}}
{{- end -}}

{{/* "true" when the migrate Job is rendered. */}}
{{- define "ferrum.migrateJob" -}}
{{- if .Values.migrate.enabled }}true{{ end -}}
{{- end -}}

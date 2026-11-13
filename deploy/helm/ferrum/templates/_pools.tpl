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
{{- $seen := dict -}}
{{- range $i, $p := $v.pools -}}
{{- $name := required (printf "pools[%d].name is required" $i) $p.name | toString -}}
{{- if not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$" $name) -}}
{{- fail (printf "pools[%d].name %q: use lower-case letters, digits and dashes" $i $name) -}}
{{- end -}}
{{- if hasKey $seen $name -}}{{- fail (printf "two pools are named %s: each pool needs a name of its own" $name) -}}{{- end -}}
{{- $_ := set $seen $name true -}}
{{- if has $name (list "pdf" "db" "db-pooler" "office" "editors" "metrics-engine" "migrate" "background" "private") -}}
{{- fail (printf "pool %s: %s-%s is the name of another part of the chart; choose another name" $name $n $name) -}}
{{- end -}}
{{- $bg := $p.background | default "both" -}}
{{- if not (has $bg (list "both" "off" "only")) -}}
{{- fail (printf "pool %s: background is both, off or only, not %q" $name $bg) -}}
{{- end -}}
{{- if and (eq $i 0) (eq $bg "only") -}}
{{- fail (printf "pool %s: the first pool keeps today's Service, which the gateway and the online editor call, so it cannot be background: only; put the background pool after it" $name) -}}
{{- end -}}
{{- if and (eq $bg "only") $p.routes -}}
{{- fail (printf "pool %s: a pool with background: only takes no requests, so it has no routes" $name) -}}
{{- end -}}
{{- with $p.zone -}}
{{- $rule := get ($v.zones | default dict) (toString .) -}}
{{- if not (hasKey ($v.zones | default dict) (toString .)) -}}
{{- fail (printf "pool %s: zone %q is not in zones" $name .) -}}
{{- end -}}
{{- if and $rule (eq (toString $rule.staff) "none") (ne $bg "off") -}}
{{- fail (printf "pool %s: its zone %s admits no staff, so it must not run background work: set background: off" $name .) -}}
{{- end -}}
{{- end -}}
{{- /* Booleans set to false are kept: merge would treat them as unset. */ -}}
{{- $as := mergeOverwrite (deepCopy $v.autoscaling) (deepCopy ($p.autoscaling | default dict)) -}}
{{- if and $p.autoscaling (hasKey $p.autoscaling "enabled") -}}{{- $_ := set $as "enabled" $p.autoscaling.enabled -}}{{- end -}}
{{- $keda := mergeOverwrite (deepCopy ($v.keda | default dict)) (deepCopy ($p.keda | default dict)) -}}
{{- if $keda.enabled -}}
{{- if ne $bg "only" -}}{{- fail (printf "pool %s: keda scales on waiting background work, so it is for a pool with background: only" $name) -}}{{- end -}}
{{- if $as.enabled -}}{{- fail (printf "pool %s: a KEDA ScaledObject and an autoscaler must never both scale one Deployment: set autoscaling: {enabled: false}" $name) -}}{{- end -}}
{{- if lt (int $keda.minReplicas) 1 -}}{{- fail (printf "pool %s: keda.minReplicas must be at least 1: schedules and upkeep show in no gauge, so nothing would wake a worker" $name) -}}{{- end -}}
{{- if and (not $keda.triggers) (not $keda.prometheusAddress) -}}{{- fail (printf "pool %s: set keda.prometheusAddress, the Prometheus that scrapes the server, or give keda.triggers" $name) -}}{{- end -}}
{{- end -}}
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
  "keda" $keda
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
{{- /* The connection budget: every pool at its most replicas, the migrate Job, and what else uses the role
(scripts/check_manifests.py: OTHERS), within the role's limit (database.yaml). With the pooler, each replica
counts only its direct connections and the pooler its own. Checked with pools or the pooler on. */ -}}
{{- $pl := $v.database.pooler -}}
{{- if and $v.database.enabled (or $v.pools $pl.enabled) -}}
{{- $need := 20 -}}
{{- $parts := list -}}
{{- if $pl.enabled -}}
{{- $per := add (int $pl.poolSize) (int (get ($pl.parameters | default dict) "reserve_pool_size" | default 0)) -}}
{{- $need = add $need (mul (int $pl.instances) $per) -}}
{{- $parts = append $parts (printf "the pooler %d x %d" (int $pl.instances) (int $per)) -}}
{{- end -}}
{{- range $out -}}
{{- $r := ternary .autoscaling.maxReplicas .replicas .autoscaling.enabled | int -}}
{{- if and .keda .keda.enabled }}{{ $r = int .keda.maxReplicas }}{{ end -}}
{{- if $pl.enabled -}}
{{- $need = add $need (mul $r (int $pl.directConnectionsPerReplica)) -}}
{{- $parts = append $parts (printf "%s %d x %d direct" (.name | default "ferrum") $r (int $pl.directConnectionsPerReplica)) -}}
{{- else -}}
{{- $need = add $need (mul $r (add (int .dbMaxConnections) 4)) -}}
{{- $parts = append $parts (printf "%s %d x (%d + 4)" .name $r (int .dbMaxConnections)) -}}
{{- end -}}
{{- end -}}
{{- if eq (include "ferrum.migrateJob" .) "true" -}}
{{- $need = add $need (add (int $v.dbMaxConnections) 4) -}}
{{- $parts = append $parts (printf "the migrate Job %d + 4" (int $v.dbMaxConnections)) -}}
{{- end -}}
{{- if gt (int $need) (int $v.database.connectionLimit) -}}
{{- fail (printf "the pools may open %d database connections (%s, and 20 for other users of the role), more than database.connectionLimit %d: lower maxReplicas or dbMaxConnections%s" $need (join ", " $parts) (int $v.database.connectionLimit) (ternary ", or the pooler's instances or poolSize" "" (and $pl.enabled true))) -}}
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

{{/* "true" when the migrate Job is rendered. Not on the first install with the chart's own database: the
hook would run before the database exists, so the pods migrate themselves that once. */}}
{{- define "ferrum.migrateJob" -}}
{{- if and .Values.migrate.enabled (or (not .Values.database.enabled) .Release.IsUpgrade) }}true{{ end -}}
{{- end -}}

{{/*
Gateways and routes, resolved: JSON {"gateways": {key: gateway}, "routes": [...], "admits": {pool name: [gateway name]}}.
A gateway: name, own hostnames, the hostnames pools add, the listener suffix of each hostname ("" for the public
gateway's own `https`/`http`), the hostnames its default route answers and that route's pool. A route: name,
gateway, hostnames, pool, and either every path ("full") or the matches it narrows to.
*/}}
{{- define "ferrum.routing" -}}
{{- $v := .Values -}}
{{- $n := include "ferrum.fullname" . -}}
{{- $pools := (include "ferrum.pools" . | fromJson).pools -}}
{{- $byName := dict -}}
{{- range $pools }}{{ $_ := set $byName .name . }}{{ end -}}
{{- $gws := dict -}}
{{- if $v.gateway.enabled -}}
{{- $_ := set $gws "public" (dict "name" $n "own" (list $v.gateway.hostname) "extra" list "suffix" (dict $v.gateway.hostname "") "claimed" dict "defaultPool" ($v.gateway.defaultPool | default (first $pools).name)) -}}
{{- end -}}
{{- if $v.privateGateway.enabled -}}
{{- $pg := $v.privateGateway -}}
{{- $own := required "privateGateway.hostnames: name at least one host" $pg.hostnames -}}
{{- $suffix := dict -}}
{{- range $own }}{{ $_ := set $suffix . (printf "-%s" (. | replace "*" "wildcard" | replace "." "-")) }}{{ end -}}
{{- $_ := set $gws "private" (dict "name" ($pg.name | default (printf "%s-private" $n)) "own" $own "extra" list "suffix" $suffix "claimed" dict "defaultPool" ($pg.defaultPool | default (first $pools).name)) -}}
{{- end -}}
{{- $admits := dict -}}
{{- range $pools }}{{ $_ := set $admits .name list }}{{ end -}}
{{- range $key, $g := $gws -}}
{{- $d := get $byName $g.defaultPool -}}
{{- if not $d -}}{{- fail (printf "%s gateway: defaultPool %q is not a pool" $key $g.defaultPool) -}}{{- end -}}
{{- if eq $d.background "only" -}}{{- fail (printf "%s gateway: defaultPool %s has background: only and takes no requests" $key $d.name) -}}{{- end -}}
{{- $_ := set $g "defaultObj" $d.obj -}}
{{- $_ := set $admits $d.name (append (get $admits $d.name) $g.name) -}}
{{- end -}}
{{- $routes := list -}}
{{- range $p := $v.pools -}}
{{- range $j, $r := $p.routes -}}
{{- $g := get $gws ($r.gateway | default "public" | toString) -}}
{{- if not $g -}}{{- fail (printf "pool %s: routes[%d] names the gateway %q, which is not enabled (public or private)" $p.name $j ($r.gateway | default "public")) -}}{{- end -}}
{{- $hosts := $r.hostnames | default $g.own -}}
{{- range $hosts -}}
{{- if not (hasKey $g.suffix .) -}}
{{- $_ := set $g.suffix . (printf "-%s" (. | replace "*" "wildcard" | replace "." "-")) -}}
{{- $_ := set $g "extra" (append $g.extra .) -}}
{{- end -}}
{{- end -}}
{{- $route := dict "name" (printf "%s-%s-%d" $g.name $p.name $j) "gateway" $g.name "hostnames" $hosts "obj" (get $byName $p.name).obj -}}
{{- if or $r.paths $r.apps -}}
{{- $matches := list -}}
{{- range ($r.paths | default (list "/api" "/")) -}}
{{- $m := dict "path" (dict "type" "PathPrefix" "value" .) -}}
{{- if $r.apps -}}
{{- $apps := list -}}{{- range $r.apps }}{{ $apps = append $apps (regexQuoteMeta (toString .)) }}{{ end -}}
{{- $_ := set $m "headers" (list (dict "type" "RegularExpression" "name" "x-ferrum-ui" "value" (printf "^(%s)@[^@]+$" (join "|" $apps)))) -}}
{{- end -}}
{{- $matches = append $matches $m -}}
{{- end -}}
{{- $_ := set $route "matches" $matches -}}
{{- else -}}
{{- if not $r.hostnames -}}{{- fail (printf "pool %s: routes[%d] takes all of the gateway's own hosts: make it the gateway's defaultPool instead" $p.name $j) -}}{{- end -}}
{{- range $hosts -}}
{{- if hasKey $g.claimed . -}}{{- fail (printf "pools %s and %s both take all of host %s" (get $g.claimed .) $p.name .) -}}{{- end -}}
{{- $_ := set $g.claimed . $p.name -}}
{{- end -}}
{{- end -}}
{{- $routes = append $routes $route -}}
{{- if not (has $g.name (get $admits $p.name)) }}{{ $_ := set $admits $p.name (append (get $admits $p.name) $g.name) }}{{ end -}}
{{- end -}}
{{- end -}}
{{- range $key, $g := $gws -}}
{{- $hosts := list -}}
{{- range (concat $g.own $g.extra) }}{{ if not (hasKey $g.claimed .) }}{{ $hosts = append $hosts . }}{{ end }}{{ end -}}
{{- $_ := set $g "defaultHosts" $hosts -}}
{{- end -}}
{{- toJson (dict "gateways" $gws "routes" $routes "admits" $admits) -}}
{{- end -}}

{{/* The rules of a route that takes every path of its hosts: today's three. Call with the backend's name. */}}
{{- define "ferrum.fullRules" -}}
# Long-lived connections: WebSockets and MCP streams have no overall time limit.
- matches:
    - path: {type: PathPrefix, value: /ws}
    - path: {type: PathPrefix, value: /mcp}
  backendRefs: [{name: {{ . }}, port: 80}]
  timeouts: {request: "0s"}
# The rest of the server: large exports and imports may take minutes.
- matches:
    - path: {type: PathPrefix, value: /api}
    - path: {type: PathPrefix, value: /files}
    - path: {type: PathPrefix, value: /oauth}
    - path: {type: PathPrefix, value: /.well-known/oauth-protected-resource}
    - path: {type: Exact, value: /.well-known/oauth-authorization-server}
    - path: {type: Exact, value: /.well-known/caldav}
    - path: {type: Exact, value: /.well-known/carddav}
  backendRefs: [{name: {{ . }}, port: 80}]
  timeouts: {request: 600s}
# Everything else: Home, apps' interfaces, the shell's files and pages.
- backendRefs: [{name: {{ . }}, port: 80}]
{{- end -}}

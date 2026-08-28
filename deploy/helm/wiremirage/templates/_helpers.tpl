{{/*
Name helpers, standard shape.
*/}}
{{- define "wiremirage.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "wiremirage.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "wiremirage.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "wiremirage.labels" -}}
helm.sh/chart: {{ include "wiremirage.chart" . }}
{{ include "wiremirage.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "wiremirage.selectorLabels" -}}
app.kubernetes.io/name: {{ include "wiremirage.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "wiremirage.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "wiremirage.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Validation.

The host fails fast on missing or contradictory configuration rather
than starting in a degraded state. These checks apply the same rule one
layer earlier, at render time, so a broken release is refused before it
reaches the cluster instead of crashlooping there.
*/}}
{{- define "wiremirage.validate" -}}

{{- if not .Values.apexHost }}
{{- fail "apexHost is required: it defines the control-plane origin, and mock traffic is served on {group}.{apexHost} subdomains (ADR-0030). Set it to the hostname this instance is reachable at, e.g. wm.example.com." }}
{{- end }}

{{- if not .Values.existingSecret }}
{{- fail "existingSecret is required: create a Secret holding SESSION_SECRET (and WM_BOOTSTRAP_TOKEN on first boot, plus any OIDC/GitHub client secret) and name it here. The chart does not template secrets from values, so they never land in `helm get values` or a git-tracked values file." }}
{{- end }}

{{- if and (not .Values.valkey.url) (not .Values.valkey.urlSecretKey) }}
{{- fail "valkey.url (or valkey.urlSecretKey) is required: this chart deploys no storage. Point it at a Valkey/Redis instance, e.g. redis://valkey.default.svc:6379." }}
{{- end }}

{{/*
The chart cannot express in-memory storage at all, and that is
deliberate: every cross-replica mechanism short-circuits on it, so a
multi-replica release over in-memory storage would look healthy while
each pod served a private, divergent view of the world.
*/}}
{{- if or (hasPrefix "memory" .Values.valkey.url) (eq .Values.valkey.url "memory") }}
{{- fail "valkey.url must be a redis:// or rediss:// URL. In-memory storage is single-process by construction: route invalidation, journal fan-out, the shared login throttle and the sweep lease all short-circuit on it, so replicas would silently diverge." }}
{{- end }}

{{- if and (gt (int .Values.replicaCount) 1) (not .Values.ingress.enabled) }}
{{- fail "replicaCount > 1 without an ingress leaves nothing routing across pods. Enable ingress, or run a single replica." }}
{{- end }}

{{- if and .Values.ingress.enabled .Values.ingress.tls.enabled (not .Values.ingress.tls.secretName) }}
{{- fail "ingress.tls.secretName is required when TLS is enabled. Mock traffic is served on *.{apexHost}, so this must be a *wildcard* certificate; HTTP-01 cannot issue one, so provision it with cert-manager plus a DNS-01 solver and name the secret here. Set ingress.tls.enabled=false only if TLS terminates upstream of this ingress." }}
{{- end }}

{{- if and .Values.auth.oidc.enabled (or (not .Values.auth.oidc.issuer) (not .Values.auth.oidc.clientId)) }}
{{- fail "auth.oidc.enabled requires auth.oidc.issuer and auth.oidc.clientId (and WM_OIDC_CLIENT_SECRET in existingSecret)." }}
{{- end }}

{{/*
The host requires exactly one OIDC allow posture and refuses to start
without one, so rendering a release that could only crash-loop would be
the chart failing at its job.
*/}}
{{- if .Values.auth.oidc.enabled }}
{{- $perIdentity := or .Values.auth.oidc.allowEmails .Values.auth.oidc.allowDomains .Values.auth.oidc.allowGroups }}
{{- if and (not .Values.auth.oidc.allowAll) (not $perIdentity) }}
{{- fail "OIDC needs exactly one allow posture: set auth.oidc.allowAll=true (right for a private IdP, where account existence is the authorization decision), or one or more of auth.oidc.allowEmails / allowDomains / allowGroups. The host refuses to start without one." }}
{{- end }}
{{- if and .Values.auth.oidc.allowAll $perIdentity }}
{{- fail "auth.oidc.allowAll and the per-identity allow rules are mutually exclusive, and the host refuses to start with both — the combination usually means the rules are believed to restrict something they don't. Pick one." }}
{{- end }}
{{- if and .Values.auth.oidc.allowGroups (not .Values.auth.oidc.groupsClaim) }}
{{- fail "auth.oidc.allowGroups needs auth.oidc.groupsClaim, which names the claim the IdP puts groups in." }}
{{- end }}
{{- end }}

{{- if and .Values.auth.github.enabled (not .Values.auth.github.clientId) }}
{{- fail "auth.github.enabled requires auth.github.clientId (and WM_GITHUB_CLIENT_SECRET in existingSecret)." }}
{{- end }}

{{- if and .Values.auth.github.enabled (not (or .Values.auth.github.allowUsers .Values.auth.github.allowOrgs)) }}
{{- fail "GitHub OAuth needs an allow rule: set auth.github.allowUsers and/or auth.github.allowOrgs. Without one, any GitHub account could sign in." }}
{{- end }}

{{- end }}

{{/*
The trusted-proxy value. Behind an ingress the host must know which
forwarded headers to believe, or secure-cookie and Host handling
misbehave silently (ADR-0027 collapsed three knobs into this one).
Derived from apexHost so it cannot drift from what the ingress routes.
*/}}
{{- define "wiremirage.trustedProxy" -}}
{{- .Values.apexHost }}
{{- end }}

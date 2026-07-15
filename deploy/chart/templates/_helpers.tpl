{{- define "dora.labels" -}}
app.kubernetes.io/part-of: usg-dora
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "dora.componentLabels" -}}
{{ include "dora.labels" . }}
app.kubernetes.io/name: {{ .name }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{- define "dora.selectorLabels" -}}
app.kubernetes.io/name: {{ .name }}
{{- end -}}

{{- define "dora.image" -}}
{{ .root.Values.image.registry }}/{{ .image }}:{{ .root.Values.image.tag }}
{{- end -}}

{{- define "dora.migrateImage" -}}
{{ .Values.image.registry }}/{{ .Values.image.migrate }}:{{ .Values.image.tag }}
{{- end -}}

{{- define "dora.databaseUrl" -}}
postgres://{{ .Values.db.user }}:{{ .Values.db.password | urlquery }}@usg-dora-db:5432/{{ .Values.db.name }}
{{- end -}}

{{- define "dora.doraConfig" -}}
{{- if .Values.doraConfig -}}
{{ .Values.doraConfig }}
{{- else if .Values.site -}}
{{ .Files.Get (printf "sites/%s/config.yaml" .Values.site) }}
{{- else -}}
{{- fail "set either 'site' or 'doraConfig' (or pass --set-file doraConfig=path/to/config.yaml)" -}}
{{- end -}}
{{- end -}}

{{- define "dora.initMigrate" -}}
- name: migrate
  image: {{ include "dora.migrateImage" . }}
  args: ["--dora-log", "info"]
  env:
    - name: DATABASE_URL
      valueFrom:
        secretKeyRef:
          name: dora-db
          key: DATABASE_URL
  resources:
    {{- toYaml .Values.resources.migrate | nindent 4 }}
{{- end -}}

{{- define "dora.envCommon" -}}
- name: DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: dora-db
      key: DATABASE_URL
- name: DORA_LOG
  value: info
{{- if .Values.forensicLog.path }}
- name: DORA_FORENSIC_LOG_PATH
  value: {{ .Values.forensicLog.path | quote }}
{{- end }}
{{- end -}}

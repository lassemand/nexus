# Vendored Kubernetes dashboards

| File | Source | Pin | License |
|---|---|---|---|
| `k8s-views-global.json` | https://github.com/dotdc/grafana-dashboards-kubernetes `dashboards/k8s-views-global.json` | tag `v3.0.8` (commit `e4faf59e017a18e1b06fa64f11623bef15ce7507`) | Apache-2.0 |
| `k8s-views-namespaces.json` | same, `dashboards/k8s-views-namespaces.json` | `v3.0.8` | Apache-2.0 |
| `k8s-views-nodes.json` | same, `dashboards/k8s-views-nodes.json` | `v3.0.8` | Apache-2.0 |
| `k8s-views-pods.json` | same, `dashboards/k8s-views-pods.json` | `v3.0.8` | Apache-2.0 |
| `node-exporter-full.json` | https://grafana.com/api/dashboards/1860/revisions/45/download (rfmoz/grafana-dashboards) | revision `45` | Apache-2.0 (per rfmoz/grafana-dashboards) |

`k8s-system-coredns.json` is left out: nothing in this repo's Prometheus config scrapes CoreDNS.

## Local modifications
- All files are minified with `jq -c` (keeps `node-exporter-full.json` under the 262144-byte client-side-apply annotation limit).
- The datasource template variable (`datasource` for dotdc, `ds_prometheus` for 1860) has its `current` value set to `Prometheus` / `nexus-prometheus`.
- No query changes.

## Updating
Bump the pin above, re-download, and re-apply both local modifications:
`jq -c '(.templating.list[]|select(.type=="datasource")|.current)={"selected":true,"text":"Prometheus","value":"nexus-prometheus"}' in.json > out.json`

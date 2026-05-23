# Kafka — deployment guide

Nexus uses Kafka as the message bus between the `chronicle` producers and the
`signal` consumer.  This directory documents the Kafka deployment for the
Nexus Kubernetes cluster.

## Distribution

[Bitnami `kafka` Helm chart](https://github.com/bitnami/charts/tree/main/bitnami/kafka),
version **31.0.0**, configured in **KRaft mode** (ZooKeeper-free).

## Manifests

| File | Purpose |
|---|---|
| `infra/argocd/root-app.yaml` | One-time bootstrap: registers the App of Apps with ArgoCD |
| `infra/argocd/apps/nexus-kafka.yaml` | ArgoCD Application — installs the Bitnami Kafka chart |

ArgoCD manages the full lifecycle after bootstrap.  Re-applying the manifests
is idempotent.

## Target namespace

`nexus`

## Internal DNS

| Address | Port | Protocol |
|---|---|---|
| `kafka.nexus.svc.cluster.local` | `9092` | PLAINTEXT (within-cluster) |

Use this address in application configuration (`KAFKA_BROKERS`,
`KAFKA_BOOTSTRAP_SERVERS`, etc.).

## Storage

A **5 Gi** PersistentVolumeClaim is created on the cluster's default
StorageClass.  Broker data survives pod restarts and rescheduling.

To use a specific StorageClass, set `persistence.storageClass` in the Helm
values block inside `nexus-kafka.yaml`.

## Resources

| | CPU | Memory |
|---|---|---|
| Request | 250 m | 512 Mi |
| Limit | 500 m | 1 Gi |

## Bootstrap steps

These steps are performed **once** per cluster.  They are not automated
because they require access to the cluster before ArgoCD is managing it.

### 1 — Install ArgoCD (if not already present)

```bash
kubectl create namespace argocd
kubectl apply -n argocd \
  -f https://raw.githubusercontent.com/argoproj/argo-cd/stable/manifests/install.yaml
```

### 2 — Set the repository URL

Edit `infra/argocd/root-app.yaml` and replace `<YOUR_REPO_URL>` with the
actual Git remote, e.g. `https://github.com/your-org/nexus.git`.  Commit and
push.

### 3 — Apply the root application

```bash
kubectl apply -f infra/argocd/root-app.yaml
```

ArgoCD will discover `infra/argocd/apps/` and reconcile all child Applications,
including `nexus-kafka`, automatically.

### 4 — Verify Kafka is healthy

```bash
# Wait for the StatefulSet to roll out
kubectl rollout status statefulset/kafka-broker -n nexus

# Check the broker pod is Running and both probes pass
kubectl get pods -n nexus -l app.kubernetes.io/name=kafka

# Confirm the ClusterIP service exists
kubectl get svc kafka -n nexus
```

Expected output for the service:
```
NAME    TYPE        CLUSTER-IP      EXTERNAL-IP   PORT(S)    AGE
kafka   ClusterIP   10.x.x.x        <none>        9092/TCP   ...
```

## Upgrading

To upgrade the chart version, change `targetRevision` in
`infra/argocd/apps/nexus-kafka.yaml`, commit, and push.  ArgoCD will
reconcile the change automatically.

## Out of scope (follow-on tasks)

- **Topic creation** — handled in NEX-29 via the Bitnami provisioning Job or
  a separate init manifest.
- **Application wiring** — `KAFKA_BROKERS` env vars for `chronicle` and
  `signal` are configured in NEX-30.
- **TLS / SASL** — not enabled; internal cluster traffic is trusted.
- **Monitoring / alerting** — separate concern.

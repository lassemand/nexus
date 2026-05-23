# Kafka — deployment guide

Nexus uses Kafka as the message bus between the `chronicle` producers and the
`signal` consumer. This document covers the Kafka deployment for the Nexus
Kubernetes cluster.

## Distribution

[Strimzi](https://strimzi.io/) **1.0.0** — the Kubernetes-native Kafka
operator. Kafka version **4.1.0** in **KRaft mode** (ZooKeeper-free).

## Repository layout

```
infra/kafka/
  README.md                       — this file
  manifests/
    00-namespace.yaml             — nexus Namespace
    01-kafka-node-pool.yaml       — KafkaNodePool CR (broker + controller, 1 replica)
    02-kafka.yaml                 — Kafka CR (cluster definition)
infra/argocd/apps/nexus-kafka.yaml — ArgoCD Application pointing at manifests/
```

## Target namespace

`nexus`

## Internal bootstrap address

```
nexus-kafka-bootstrap.nexus.svc.cluster.local:9092   (PLAINTEXT, within-cluster only)
```

Use this as `KAFKA_BROKERS` / `KAFKA_BOOTSTRAP_SERVERS` in application config.

Individual broker DNS (Strimzi headless service):
```
nexus-combined-{id}.nexus-combined-brokers.nexus.svc.cluster.local:9092
```

## Storage

A **5 Gi** PersistentVolumeClaim is provisioned per broker on the cluster's
default StorageClass (`deleteClaim: false` — data is retained on pod deletion).

To use a specific StorageClass, set `storage.storageClass` in
`infra/kafka/manifests/01-kafka-node-pool.yaml`.

## Resources

| | CPU | Memory |
|---|---|---|
| Request | 250 m | 512 Mi |
| Limit | 500 m | 1 Gi |

## Health probes

Both liveness and readiness probes are configured in `02-kafka.yaml`:

| Probe | Initial delay | Timeout |
|---|---|---|
| Liveness | 15 s | 5 s |
| Readiness | 15 s | 5 s |

Strimzi's operator manages the actual probe implementation (TCP socket on
port 9092 + JMX checks).

## Bootstrap steps

These steps are performed **once** per cluster before ArgoCD takes over.

### 1 — Install the Strimzi operator

```bash
kubectl create namespace nexus --dry-run=client -o yaml | kubectl apply -f -

kubectl apply -f 'https://strimzi.io/install/latest?namespace=nexus'

# Wait for the operator to be ready
kubectl rollout status deployment/strimzi-cluster-operator -n nexus
```

> **Pinned version:** the URL above resolves to the latest Strimzi release.
> To pin to a specific version (e.g. 1.0.0) use:
> `https://github.com/strimzi/strimzi-kafka-operator/releases/download/1.0.0/strimzi-1.0.0.yaml`

### 2 — Apply the Kafka manifests

```bash
kubectl apply -f infra/kafka/manifests/
```

### 3 — (Optional) Bootstrap ArgoCD App-of-Apps

Once ArgoCD is installed, edit `infra/argocd/root-app.yaml` to set
`<YOUR_REPO_URL>`, then:

```bash
kubectl apply -f infra/argocd/root-app.yaml
```

ArgoCD will then manage all subsequent reconciliations declaratively.

## Verifying Kafka is healthy

```bash
# Operator ready?
kubectl rollout status deployment/strimzi-cluster-operator -n nexus

# Kafka cluster ready?
kubectl get kafka nexus -n nexus

# Broker pod running?
kubectl get pods -n nexus -l strimzi.io/cluster=nexus

# Bootstrap service exists?
kubectl get svc nexus-kafka-bootstrap -n nexus
```

Expected service output:
```
NAME                    TYPE        CLUSTER-IP      EXTERNAL-IP   PORT(S)    AGE
nexus-kafka-bootstrap   ClusterIP   10.x.x.x        <none>        9092/TCP   ...
```

## Upgrading Kafka version

Supported versions for Strimzi 1.0.0: `4.1.0`, `4.1.1`, `4.1.2`, `4.2.0`.

1. Update `spec.kafka.version` in `infra/kafka/manifests/02-kafka.yaml`
2. Commit and push — ArgoCD reconciles automatically (or `kubectl apply -f infra/kafka/manifests/`)

## Out of scope (follow-on tasks)

- **Topic creation** — NEX-29: use `KafkaTopic` CRs managed by Strimzi Entity Operator
- **Application wiring** — NEX-30: set `KAFKA_BROKERS` env vars for `chronicle` and `signal`
- **TLS / SASL** — not enabled; internal cluster traffic is trusted
- **Monitoring / alerting** — separate concern

#!/bin/bash
# nexus-kafka-forward.sh
#
# Persistent kubectl port-forward for the Kafka external NodePort listener.
#
# Binds both forwards to 192.168.49.2 (loopback alias created by the
# io.nexus.minikube-tunnel LaunchDaemon) so that Kafka clients connecting to
# the bootstrap and then receiving broker metadata pointing at 192.168.49.2
# can complete their connection entirely through port-forward.
#
# Ports forwarded:
#   192.168.49.2:32092 → nexus-kafka-external-bootstrap:9093  (bootstrap)
#   192.168.49.2:32093 → nexus-combined-external-0:9093       (broker 0)
#
# Runs as the agent user via the io.nexus.kafka-port-forward LaunchAgent.
# The LaunchAgent restarts this script whenever it exits.

export HOME="/Users/agent"
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

NAMESPACE="nexus"

echo "$(date): starting Kafka port-forwards on 127.0.0.1"

kubectl port-forward svc/nexus-kafka-external-bootstrap \
    32092:9093 -n "$NAMESPACE" &
BOOTSTRAP_PID=$!

kubectl port-forward svc/nexus-combined-external-0 \
    32093:9093 -n "$NAMESPACE" &
BROKER_PID=$!

echo "$(date): bootstrap PID=$BOOTSTRAP_PID  broker PID=$BROKER_PID"

# Poll until either port-forward exits, then kill the other and exit cleanly.
# The LaunchAgent (KeepAlive=true) restarts this script automatically.
while kill -0 "$BOOTSTRAP_PID" 2>/dev/null && kill -0 "$BROKER_PID" 2>/dev/null; do
    sleep 5
done

echo "$(date): a port-forward exited — restarting both"
kill "$BOOTSTRAP_PID" "$BROKER_PID" 2>/dev/null
wait "$BOOTSTRAP_PID" "$BROKER_PID" 2>/dev/null
exit 0

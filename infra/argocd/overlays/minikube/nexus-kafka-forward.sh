#!/bin/bash
# nexus-kafka-forward.sh
#
# Persistent kubectl port-forward for the Kafka external NodePort listener.
#
# Both forwards bind to 127.0.0.1 (localhost), matching the advertisedHost
# set on the Strimzi external listener so that Kafka clients get back a
# broker address they can actually reach from the macOS host.
#
# Ports forwarded:
#   localhost:32092 → nexus-kafka-external-bootstrap:9093  (bootstrap)
#   localhost:32093 → nexus-combined-external-0:9093       (broker 0)
#
# Runs as the agent user via the io.nexus.kafka-port-forward LaunchAgent.
# The LaunchAgent (KeepAlive=true) restarts this script whenever it exits,
# so if either port-forward dies (e.g. after a pod restart) both are
# restarted within ~10 seconds.

export HOME="/Users/agent"
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

NAMESPACE="nexus"

echo "$(date): starting Kafka port-forwards on 127.0.0.1"

kubectl port-forward svc/nexus-kafka-external-bootstrap \
    --address 127.0.0.1 32092:9093 -n "$NAMESPACE" &
BOOTSTRAP_PID=$!

kubectl port-forward svc/nexus-combined-external-0 \
    --address 127.0.0.1 32093:9093 -n "$NAMESPACE" &
BROKER_PID=$!

echo "$(date): bootstrap PID=$BOOTSTRAP_PID  broker PID=$BROKER_PID"

# Poll until either port-forward exits, then kill the other and exit cleanly.
# The LaunchAgent restarts this script automatically.
while kill -0 "$BOOTSTRAP_PID" 2>/dev/null && kill -0 "$BROKER_PID" 2>/dev/null; do
    sleep 5
done

echo "$(date): a port-forward exited — restarting both"
kill "$BOOTSTRAP_PID" "$BROKER_PID" 2>/dev/null
wait "$BOOTSTRAP_PID" "$BROKER_PID" 2>/dev/null
exit 0

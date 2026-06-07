#!/bin/bash
# nexus-minikube-alias.sh
#
# Adds 192.168.49.2 as a loopback alias on lo0 so that kubectl port-forward
# can bind to it and Kafka clients that receive broker metadata advertising
# 192.168.49.x addresses can connect via port-forward — even on macOS with
# Docker Desktop where the Docker bridge network is not routable from the host.
#
# Runs as root via the io.nexus.minikube-tunnel LaunchDaemon.
# Refreshes the alias every 30 s in case it is cleared by a network change.

set -euo pipefail

ALIAS_IP="192.168.49.2"
NETMASK="255.255.255.255"

add_alias() {
    ifconfig lo0 alias "$ALIAS_IP" netmask "$NETMASK" 2>/dev/null && \
        echo "$(date): alias $ALIAS_IP added to lo0" || true
}

add_alias

while true; do
    ifconfig lo0 | grep -q "$ALIAS_IP" || add_alias
    sleep 30
done

# nexus-read policy
# Grants read-only access to all secrets under secret/nexus/.
# Applied to the Kubernetes auth role "nexus" so that nexus service
# account tokens can retrieve API keys and broker credentials.

path "secret/data/nexus/*" {
  capabilities = ["read"]
}

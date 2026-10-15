terraform {
  required_version = ">= 1.6"

  # Layer 0 of 3 — applied before `orbstack/`, because the SSH credential this
  # layer issues is how later layers are expected to reach the host at all.
  #
  # State is local, like the other two. The reasons differ slightly: for
  # `orbstack/` local state avoids a circular dependency on the VM it manages,
  # whereas here it is that Vault runs *inside* that VM's cluster, so a Vault
  # or `pg` backend would make this layer unable to plan exactly when the
  # cluster is unhealthy — the moment the SSH access is most needed.
  backend "local" {
    path = "terraform.tfstate"
  }

  required_providers {
    vault = {
      source  = "hashicorp/vault"
      version = "~> 5.4"
    }
  }
}

# Address and token come from the environment (VAULT_ADDR, VAULT_TOKEN) rather
# than being written here: the token is the operator's own, short-lived, and
# must not land in state or in a .tfvars file. README documents the
# port-forward this needs, since Vault's NodePort is not reachable from the
# host.
provider "vault" {}

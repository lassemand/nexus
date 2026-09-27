terraform {
  required_version = ">= 1.6"

  # Layer 1 of 2. Bootstrap layer — state is deliberately LOCAL, not in the pg
  # backend the tmux layer uses.
  #
  # This layer configures OrbStack. The pg backend lives in Postgres, which runs
  # in Kubernetes, which runs inside OrbStack — so storing this layer's state
  # there is circular: planning or applying the thing that manages the VM would
  # require the VM to already be up and healthy, with k8s, Postgres and a
  # port-forward all working. That is exactly the situation where you most need
  # Terraform to work, and it is not hypothetical: an OrbStack restart during
  # development made `terraform init` impossible for over an hour.
  #
  # Local state has real downsides — it is not shared, it is absent in a fresh
  # clone, and it has no backup yet (see ../README.md). Accepted for now because
  # a bootstrap layer that cannot reach its own state is worse.
  backend "local" {
    path = "terraform.tfstate"
  }
}

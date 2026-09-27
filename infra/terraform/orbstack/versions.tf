terraform {
  required_version = ">= 1.6"

  # Layer 1 of 2. Own state, separate from the tmux layer — see ../README.md.
  #
  # OrbStack has to be up before anything inside it (the cluster, and therefore
  # Vault) can be reached, so it cannot share a state with things that depend on
  # it: a single state would let a partially-built OrbStack and a tmux config
  # that assumes a working one be applied in the same run.
  backend "pg" {
    schema_name = "terraform_state_orbstack"
  }
}

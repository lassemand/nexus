terraform {
  required_version = ">= 1.6"

  # Layer 2 of 2. State is local, as in layer 1 — see ../README.md.
  #
  # This originally used the pg backend on nexus-postgres, on the grounds that
  # this layer requires a running OrbStack anyway so the backend's dependency on
  # it cost nothing. That was true about cost but implied a durability benefit
  # that does not exist: the cluster runs a single instance on node-local storage
  # with archive_command set to /bin/true, so state there is exactly as durable
  # as a file on disk — and it sits inside the VM, so `orb reset` destroys it
  # just the same. Locking was the only real feature, and advisory locks are moot
  # for a single operator on a single machine.
  backend "local" {
    path = "terraform.tfstate"
  }
}

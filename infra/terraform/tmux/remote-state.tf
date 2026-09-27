# Reads the orbstack layer's state, which does two things:
#
#   1. Makes the dependency explicit and machine-enforced rather than a
#      convention in a README. Reading a state that does not exist is an error,
#      so `terraform plan` here fails outright until the orbstack layer has been
#      applied at least once. That failure is the desired behaviour — tmux
#      configuration is meaningless if OrbStack was never brought up.
#   2. Gives this layer a way to consume orbstack outputs as they are added.
#
# `conn_str` is deliberately omitted so the pg backend resolves it from the same
# PG_CONN_STR used to init this layer, keeping the credential out of the repo.
data "terraform_remote_state" "orbstack" {
  backend = "pg"

  config = {
    schema_name = "terraform_state_orbstack"
  }
}

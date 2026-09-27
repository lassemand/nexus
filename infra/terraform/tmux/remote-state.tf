# Reads the orbstack layer's state, which does two things:
#
#   1. Makes the layer dependency machine-enforced rather than a README
#      convention. Reading a state that does not exist is an error, so a plan
#      here fails until the orbstack layer has been applied at least once. That
#      failure is intended — tmux configuration is meaningless if OrbStack was
#      never brought up.
#   2. Gives this layer a way to consume orbstack outputs as they are added.
#
# The orbstack layer keeps its state locally (see ../orbstack/versions.tf for
# why), so this reads a file path rather than the pg backend. The path is
# relative to this module directory.
data "terraform_remote_state" "orbstack" {
  backend = "local"

  config = {
    path = "../orbstack/terraform.tfstate"
  }
}

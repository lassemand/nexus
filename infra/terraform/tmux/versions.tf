terraform {
  required_version = ">= 1.6"

  # Layer 2 of 2. Own state, applied only after the orbstack layer — see
  # ../README.md for the ordering and why it is enforced rather than documented.
  backend "pg" {
    schema_name = "terraform_state_tmux"
  }
}

terraform {
  required_version = ">= 1.6"

  # Remote state lives in the dedicated `terraform` database on the existing
  # nexus-postgres cluster (declared in infra/charts/nexus/postgres).
  #
  # Connection details are deliberately absent here: the pg backend reads them
  # from the PG_CONN_STR environment variable at init time, so the
  # operator-managed credentials are never written into a .tf file or committed.
  # See README.md for the init procedure.
  backend "pg" {
    schema_name = "terraform_state"
  }
}

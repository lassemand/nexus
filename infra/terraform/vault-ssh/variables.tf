variable "host_user" {
  description = <<-DESC
    The one macOS account certificates may log in as. Least privilege: the role
    below allows exactly this name, so a signed certificate cannot be used to
    reach any other account, including an admin one.
  DESC
  type        = string
  default     = "agent"

  validation {
    condition     = can(regex("^[a-z_][a-z0-9_-]*$", var.host_user))
    error_message = "host_user must be a plain lowercase Unix account name."
  }
}

variable "certificate_ttl" {
  description = <<-DESC
    Lifetime of an issued certificate. Short on purpose — the point of the SSH
    secrets engine over a stored keypair is that a leaked credential expires by
    itself. Long enough for a `terraform apply` or a debugging session, not long
    enough to be worth stashing.
  DESC
  type        = string
  default     = "30m"
}

variable "certificate_max_ttl" {
  description = "Ceiling for an explicitly requested longer TTL."
  type        = string
  default     = "2h"
}

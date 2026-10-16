# Guard against `terraform apply` silently turning Kubernetes off.
#
# Everything under infra/ assumes a running cluster, so disabling the engine is
# destructive. Whatever mechanism ends up setting it (to be confirmed by the
# on-host inventory, NEX-124 — see ../README.md), the change must be a deliberate,
# two-step act: ask for it AND acknowledge it.
#
#   orbstack_kubernetes_enabled = null   (default)  unmanaged, Terraform does not touch it
#                               = true              keep it on
#                               = false             turn it off — refused unless
#                                                   allow_kubernetes_disable = true
#
# The default is a no-op, so applying this layer cannot disable Kubernetes.

variable "orbstack_kubernetes_enabled" {
  description = "Desired state of OrbStack's Kubernetes engine. null leaves it unmanaged."
  type        = bool
  default     = null
}

variable "allow_kubernetes_disable" {
  description = "Must be true, in addition to orbstack_kubernetes_enabled = false, for Terraform to disable Kubernetes."
  type        = bool
  default     = false
}

resource "terraform_data" "kubernetes_guard" {
  input = var.orbstack_kubernetes_enabled

  lifecycle {
    precondition {
      condition     = var.orbstack_kubernetes_enabled != false || var.allow_kubernetes_disable
      error_message = "Refusing to disable OrbStack's Kubernetes engine: the rest of infra/ assumes a running cluster. If you really mean it, also set allow_kubernetes_disable = true."
    }
  }
}

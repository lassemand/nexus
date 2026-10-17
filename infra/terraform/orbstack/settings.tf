# OrbStack application settings, declared.
#
# READ FIRST: this applies to the OrbStack of the account that runs it, and
# `make apply` runs as `agent`. The `nexus` cluster runs in `lasse`'s OrbStack —
# port 26443 is served by lasse's OrbStack Helper — so as written this never
# configures the cluster's VM. See the correction in ../README.md, which also
# records the open decision on which instance this should manage.
#
# There is no Terraform provider for OrbStack. Settings persist in
# ~/.orbstack/vmconfig.json, which holds only overrides — it reads `{}` until
# something is set — and are changed through `orbctl config set`, which is what
# this uses through `terraform_data` plus `local-exec`. orbctl is preferred over
# writing the JSON because it validates the value and reports when a restart is
# needed.
#
# Limitation, stated plainly: `local-exec` runs on create and on replace, so
# this enforces a value when the declaration changes. It does not notice drift
# made elsewhere — if someone changes a setting in the GUI, a plan here is
# still empty. Closing that would need an `external` data source shelling out
# to `orbctl config get`, which is a separate piece of work.

variable "vm_cpu" {
  description = <<-DESC
    CPU cores for the VM. The host has 10 (4 performance), and OrbStack's
    default is all of them, which is what this records.

    Changing a VM's cpu or memory needs an OrbStack restart to take effect
    (orbctl says so). As applied by `agent` this restarts `agent`'s VM, which
    does not run the cluster; aimed at `lasse`'s, it would take the cluster and
    every running agent down. Declared at the current value so an apply is a
    no-op.
  DESC
  type        = number
  default     = 10
}

variable "vm_memory_mib" {
  description = <<-DESC
    Memory for the VM, in MiB. 8192 is OrbStack's default on this 16 GiB host
    and is `agent`'s value. The cluster's VM is `lasse`'s, raised to 10240 on
    2026-10-16 (node allocatable 7.8 -> 9.8 GiB) without this value changing —
    which is how the two instances were told apart. Conductor's memory budget is
    sized against the cluster VM in its chart, not here.
  DESC
  type        = number
  default     = 8192
}

variable "rosetta" {
  description = "x86 emulation for amd64 images. Default true; the host is arm64."
  type        = bool
  default     = true
}

variable "pause_in_sleep" {
  description = <<-DESC
    Pause the VM while the Mac sleeps. Default true, and relevant beyond
    power: with it on, the guest clock stops during sleep and comes back
    behind, which is how every GitHub App JWT the conductor pod signed came to
    be rejected as expired. Keeping the host awake is the actual remedy; this
    is recorded so the interaction is not rediscovered.
  DESC
  type        = bool
  default     = true
}

# ── the Kubernetes safeguard ────────────────────────────────────────────────
#
# `k8s.enable` is NOT managed by default.
#
# This originally claimed the key "does not describe whether Kubernetes is
# running", because it read `false` beside a cluster that had been Ready for 117
# days. That was two instances compared as one: the key is accurate for
# `agent`'s OrbStack, where Kubernetes really is off, and the cluster is
# `lasse`'s.
#
# The safeguard stays for the corrected reason. Run as `agent`, writing the key
# looks like managing the cluster's Kubernetes while doing nothing of the kind;
# pointed at `lasse`'s instance, writing `false` would tear down everything
# infra/ depends on.
#
# Managing it requires opting in explicitly, which is the deliberate act the
# task asks for.

variable "manage_k8s_enable" {
  description = <<-DESC
    Whether Terraform may write `k8s.enable` at all. Default false, which makes
    this a no-op regardless of `k8s_enable`.

    Before setting this true, decide which OrbStack this layer manages (see the
    README correction), and test the change somewhere the cluster is
    expendable.
  DESC
  type        = bool
  default     = false
}

variable "k8s_enable" {
  description = "Desired `k8s.enable`, applied only when `manage_k8s_enable` is true."
  type        = bool
  default     = true
}

locals {
  # Settings safe to assert: each is already at this value, so the first apply
  # changes nothing and proves the mechanism rather than the host.
  settings = {
    "cpu"                  = tostring(var.vm_cpu)
    "memory_mib"           = tostring(var.vm_memory_mib)
    "rosetta"              = tostring(var.rosetta)
    "power.pause_in_sleep" = tostring(var.pause_in_sleep)
  }

  k8s_settings = var.manage_k8s_enable ? { "k8s.enable" = tostring(var.k8s_enable) } : {}

  managed = merge(local.settings, local.k8s_settings)
}

resource "terraform_data" "orb_config" {
  for_each = local.managed

  # Replaced when the declared value changes, which is what re-runs the setter.
  triggers_replace = {
    key   = each.key
    value = each.value
  }

  provisioner "local-exec" {
    # Reads before writing, so applying a value that is already set does
    # nothing at all. That matters for `cpu` and `memory_mib`: it is not known
    # whether `orbctl config set` restarts the VM when the value is unchanged,
    # and a restart tears down the cluster. Not writing is the only certain way
    # not to find out the hard way.
    #
    # `orbctl` rather than `orb`: same binary, but `orb` with no subcommand
    # opens a Linux shell, and a typo there would hang an apply on a TTY.
    interpreter = ["/bin/bash", "-c"]
    command     = <<-CMD
      set -euo pipefail
      current="$(orbctl config get ${each.key})"
      if [ "$current" = "${each.value}" ]; then
        echo "${each.key} is already ${each.value}; leaving it alone"
      else
        echo "${each.key}: $current -> ${each.value}"
        orbctl config set ${each.key} ${each.value}
      fi
    CMD
  }
}

output "managed_settings" {
  description = "Settings this layer asserts, and the values it asserts."
  value       = local.managed
}

output "k8s_enable_managed" {
  description = "Whether `k8s.enable` is under Terraform's control here."
  value       = var.manage_k8s_enable
}

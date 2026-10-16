# OrbStack application settings, declared.
#
# There is no Terraform provider for OrbStack, and — verified on this host, not
# assumed — no user-writable settings file either:
#
#   ~/.orbstack/config/docker.json      {}        (2 bytes)
#   ~/.orbstack/vmconfig.json           {}        (2 bytes)
#   ~/Library/Preferences/dev.kdrag0n.MacVirt.plist
#                                       Sparkle updater keys only
#   ~/Library/Group Containers/HUAQ24HBR6.dev.orbstack/
#                                       VM disk images, no configuration
#
# So `local_file` has nothing to write. The only programmatic surface is
# `orbctl config get|set`, which is what this uses through `terraform_data`
# plus `local-exec`. See ../README.md for the full inventory.
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

    Changing this almost certainly restarts the VM, which tears down the
    Kubernetes cluster and every running agent with it. Declared at the current
    value so an apply is a no-op; change it deliberately, in a window where
    losing the cluster is acceptable.
  DESC
  type        = number
  default     = 10
}

variable "vm_memory_mib" {
  description = <<-DESC
    Memory for the VM, in MiB. OrbStack's default on this 16 GiB host is 8192,
    which is what the VM reports (allocatable 8196572Ki).

    This is currently too small for the conductor chart's own stated invariant:
    `maxSessions` (5) x ~3 GiB per concurrent `cargo build` needs ~15 GiB, and
    the chart's comment says that has to fit inside both the container limit
    (16Gi, which it does) and the VM's memory (8 GiB, which it does not). The
    chart's own remedy is to lower `maxSessions` rather than raise this past
    what the host can give. Left at the measured value here, because changing
    it restarts the VM and because the fix may belong in the chart instead.
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
# `k8s.enable` is NOT managed by default, and the reason is measured rather
# than theoretical: on this host it reads `false` while the cluster has been
# Ready for 117 days (v1.35.6+orb1, `orbctl status: Running`). The setting
# therefore does not describe whether Kubernetes is running, so writing it from
# a declared value could disable the engine the whole of infra/ depends on —
# with the config offering no way to tell that it had.
#
# Managing it requires opting in explicitly, which is the deliberate act the
# task asks for.

variable "manage_k8s_enable" {
  description = <<-DESC
    Whether Terraform may write `k8s.enable` at all. Default false, which makes
    this a no-op regardless of `k8s_enable`.

    Before setting this true, establish what `k8s.enable` actually controls on
    a host where it disagrees with reality, and do it somewhere the cluster is
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

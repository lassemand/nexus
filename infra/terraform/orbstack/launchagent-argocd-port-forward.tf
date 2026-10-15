# ArgoCD UI port-forward as a macOS LaunchAgent (https://localhost:8080).
#
# Terraform runs on the host itself (see ../README.md "Applying"), so these are
# plain local resources — no SSH, no provisioner connection.
#
# Two resources, split so that an unchanged plist is never reloaded:
#   - local_file writes the plist; it changes only when the content does.
#   - terraform_data (re)loads the agent, and is replaced only when the plist's
#     hash changes. Re-applying with no change therefore touches neither.
#
# Known gap: a change made out of band to launchd (the agent unloaded by hand) is
# not detected, since Terraform has no launchd resource to refresh. Re-load with
# `terraform apply -replace=terraform_data.argocd_port_forward_load`.

variable "launch_agents_dir" {
  description = "Directory the LaunchAgent plist is installed into. Defaults to the current user's."
  type        = string
  default     = "~/Library/LaunchAgents"
}

locals {
  argocd_port_forward_label = "io.nexus.argocd-port-forward"
}

resource "local_file" "argocd_port_forward_plist" {
  # The plist stays where the ArgoCD overlay documents it; Terraform installs it.
  content         = file("${path.module}/../../argocd/overlays/orbstack/${local.argocd_port_forward_label}.plist")
  filename        = pathexpand("${var.launch_agents_dir}/${local.argocd_port_forward_label}.plist")
  file_permission = "0644"
}

resource "terraform_data" "argocd_port_forward_load" {
  triggers_replace = [local_file.argocd_port_forward_plist.content_sha256]

  # Destroy provisioners may only reference `self`, hence the copies here.
  input = {
    label = local.argocd_port_forward_label
    plist = local_file.argocd_port_forward_plist.filename
  }

  # bootout first so a changed plist, or an agent loaded by the old manual
  # `launchctl load -w`, is replaced rather than failing with "already loaded".
  # `enable` undoes a previous `unload -w`, which persists a disabled flag.
  provisioner "local-exec" {
    environment = {
      LABEL = self.input.label
      PLIST = self.input.plist
    }
    command = <<-EOT
      set -eu
      domain="gui/$(id -u)"
      launchctl bootout "$domain/$LABEL" 2>/dev/null || true
      launchctl enable "$domain/$LABEL"
      launchctl bootstrap "$domain" "$PLIST"
    EOT
  }

  provisioner "local-exec" {
    when = destroy
    environment = {
      LABEL = self.input.label
    }
    command = "launchctl bootout \"gui/$(id -u)/$LABEL\" 2>/dev/null || true"
  }
}

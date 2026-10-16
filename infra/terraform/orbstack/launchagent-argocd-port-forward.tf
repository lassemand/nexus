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

  # A LaunchAgent can only be loaded from the user's GUI (Aqua) session. The
  # documented way to apply this is `ssh -t agent@host 'make apply'`, and an SSH
  # session is in the *Background* domain, where every load path fails —
  # measured on macOS 26.4:
  #
  #   launchctl enable    gui/<uid>/<label>   125 Domain does not support ...
  #   launchctl bootstrap gui/<uid> <plist>   125 Domain does not support ...
  #   launchctl bootstrap user/<uid> <plist>    5 Input/output error
  #   launchctl load -w <plist>                 0 and loads nothing at all
  #
  # So this installs the plist and loads it only when it can. Out of a GUI
  # session it says so and succeeds, rather than failing an apply that did
  # everything it was actually able to do. The agent has RunAtLoad, so it
  # starts by itself at the next login regardless.
  #
  # bootout first so a changed plist, or an agent loaded by the old manual
  # `launchctl load -w`, is replaced rather than failing with "already loaded".
  # `enable` undoes a previous `unload -w`, which persists a disabled flag —
  # and it takes `user/<uid>`, not `gui/<uid>`, which is what produced the 125
  # above even before the bootstrap was reached.
  provisioner "local-exec" {
    environment = {
      LABEL = self.input.label
      PLIST = self.input.plist
    }
    command = <<-EOT
      set -eu
      uid="$(id -u)"
      manager="$(launchctl managername 2>/dev/null || echo unknown)"
      if [ "$manager" != "Aqua" ]; then
        echo "launchctl manager is '$manager', not Aqua: the plist is installed but not loaded."
        echo "To load it now, from Terminal.app on the host:"
        echo "    launchctl bootout gui/$uid/$LABEL 2>/dev/null || true"
        echo "    launchctl enable user/$uid/$LABEL"
        echo "    launchctl bootstrap gui/$uid $PLIST"
        echo "Otherwise it starts by itself at the next login (RunAtLoad)."
        exit 0
      fi
      launchctl bootout "gui/$uid/$LABEL" 2>/dev/null || true
      launchctl enable "user/$uid/$LABEL"
      launchctl bootstrap "gui/$uid" "$PLIST"
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

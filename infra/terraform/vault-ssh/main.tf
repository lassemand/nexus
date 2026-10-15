# Vault's SSH secrets engine in CA mode: Vault holds a signing key and stamps
# short-lived certificates on demand. The host trusts the CA, not any individual
# key, so no credential is ever stored anywhere.
#
# Chosen over a static keypair in KV (NEX-120's first criterion). The tradeoff,
# recorded in ../README.md:
#
#   * KV is simpler — one `vault kv get`, no host configuration beyond
#     authorized_keys — but the private key is long-lived, is copied onto every
#     machine that uses it, and revoking it means editing authorized_keys on the
#     host. That is the "static key in a file" this repo already avoids for
#     every other credential.
#   * The engine costs one-time host setup (TrustedUserCAKeys) and a Vault
#     round-trip per session, and buys expiry by default: a leaked certificate
#     stops working on its own, and revoking access is a Vault policy change
#     rather than a host edit.
#
# The engine is also the pattern already established here — ADR-0003's Saxo
# refresh tokens, the GitHub App's minted installation tokens — where nothing
# long-lived sits on disk.

resource "vault_mount" "ssh_client_signer" {
  path        = "ssh-client-signer"
  type        = "ssh"
  description = "Signs short-lived SSH client certificates for host access (NEX-120)"
}

# `generate_signing_key` keeps the private half inside Vault: it is created
# there and never leaves, so there is no moment where a signing key exists on an
# operator's disk to be lost or copied.
resource "vault_ssh_secret_backend_ca" "signer" {
  backend              = vault_mount.ssh_client_signer.path
  generate_signing_key = true
}

resource "vault_ssh_secret_backend_role" "host" {
  name     = "host"
  backend  = vault_mount.ssh_client_signer.path
  key_type = "ca"

  allow_user_certificates = true
  # Host certificates are a different job (proving the *server's* identity) and
  # nothing here needs them.
  allow_host_certificates = false

  # One name, not a wildcard: a certificate from this role can only be used to
  # log in as this account.
  allowed_users = var.host_user
  default_user  = var.host_user

  # permit-pty alone. An interactive shell needs it; agent forwarding,
  # port forwarding and X11 are not needed to run configuration commands, and
  # each would widen what a leaked certificate could do.
  default_extensions = {
    permit-pty = ""
  }
  allowed_extensions = "permit-pty"

  ttl     = var.certificate_ttl
  max_ttl = var.certificate_max_ttl
}

# What an operator needs, and nothing else: sign with this one role. Notably it
# does not grant read on the mount's config, so holding this policy does not let
# you read or replace the CA.
resource "vault_policy" "ssh_host_signer" {
  name = "nexus-ssh-host"

  policy = <<-HCL
    # Sign a client certificate for the single host role.
    path "${vault_mount.ssh_client_signer.path}/sign/${vault_ssh_secret_backend_role.host.name}" {
      capabilities = ["create", "update"]
    }

    # `vault ssh` reads the role to learn the default user before signing.
    path "${vault_mount.ssh_client_signer.path}/roles/${vault_ssh_secret_backend_role.host.name}" {
      capabilities = ["read"]
    }
  HCL
}

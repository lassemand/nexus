output "ca_public_key" {
  description = <<-DESC
    The CA's public half, to be installed on the host and referenced by
    sshd_config's TrustedUserCAKeys. Public by nature, so not marked sensitive —
    it is meant to be copied onto the machine. ../README.md has the exact steps.
  DESC
  value       = vault_ssh_secret_backend_ca.signer.public_key
}

output "sign_path" {
  description = "Vault path an operator signs against."
  value       = "${vault_mount.ssh_client_signer.path}/sign/${vault_ssh_secret_backend_role.host.name}"
}

output "policy_name" {
  description = "Policy to attach to whoever is allowed to request a certificate."
  value       = vault_policy.ssh_host_signer.name
}

output "host_user" {
  description = "The only account certificates from this role may log in as."
  value       = vault_ssh_secret_backend_role.host.allowed_users
}

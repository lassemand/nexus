# Thin wrapper around bin/terraform-run, which holds the guards and the layer
# ordering. Kept thin on purpose: a future self-hosted runner on the host can
# call the script directly and get identical behaviour.
#
# macOS ships GNU Make 3.81, so nothing here uses anything newer.
#
#   make plan                 # both layers, in order
#   make apply                # both layers, in order, interactive
#   make plan  LAYER=tmux     # one layer
#   make apply LAYER=orbstack
#   make apply FORCE=1        # skip the clean-tree / on-main checks
#   make apply HOST_NAME=...  # override the expected host
#
# LAYER, FORCE, HOST_NAME and HOST_USER are exported so the script sees them
# whether they arrive from the environment or from the command line.
export LAYER
export FORCE
export HOST_NAME
export HOST_USER

RUN := bin/terraform-run

.PHONY: plan apply help

help:
	@echo "make plan [LAYER=orbstack|tmux]"
	@echo "make apply [LAYER=orbstack|tmux] [FORCE=1]"
	@echo ""
	@echo "Runs on the host only, so Terraform state stays there. See README.md."

plan:
	@$(RUN) plan

apply:
	@$(RUN) apply

## mkpm bootstrap
ifndef mkpm_included
ifeq ($(filter grouped-target,$(.FEATURES)),)
$(error mkpm requires GNU Make >= 4.3 (found $(MAKE_VERSION)). On macOS: brew install make, then use 'gmake')
endif
mkpm_dir := $(subst mkpm_dir=,,$(filter mkpm_dir=%,$(or $(file < .mkpmrc.local),$(file < .mkpmrc))))
ifneq ($(mkpm_dir),)
include $(if $(filter /%,$(mkpm_dir)),$(mkpm_dir),$(abspath $(mkpm_dir)))/Makefile
else
mkpm: REMOTE ?= https://mkpm.io/Makefile
mkpm:
	@curl -fsSL $(REMOTE) -o $@ || { echo "Failed to download $@ from $(REMOTE)" >&2; exit 1; }
include mkpm
endif
endif
## /mkpm bootstrap

$(call mkpm_load,docker)
$(call mkpm_load,docker_compose)

# The Dockerfile lives at the workspace root, shared with the transcoder.
docker_compose_main_image_hash = $(shell cat ../Dockerfile | sha256sum | cut -c1-12)

# This one is a daemon, so the docker_compose targets all make sense as-is:
#   make up          start polling
#   make logs-f      follow it
#   make down        stop it (SIGTERM, 600s grace)
#   make image-build rebuild

CARGO ?= cargo

# No apostrophes in any mkpm_help text; the help renderer cannot survive one.

$(call mkpm_help,help-runner,Show the runner command-line help and every environment variable)
help-runner:
	$(call docker_compose_run,runner,--help)

$(call mkpm_help,test,Run the runner tests (needs a local cargo))
test:
	@$(CARGO) test -p vidra-runner

$(call mkpm_help,lint,Clippy over the runner (needs a local cargo))
lint:
	@$(CARGO) clippy -p vidra-runner --all-targets -- -D warnings

$(call mkpm_help,env-example,Write runner/.env.local from the documented template if it does not exist)
env-example:
	@test ! -f .env.local || { echo '.env.local already exists; not overwriting' >&2; exit 1; }
	@cp .env.example .env.local
	@echo 'wrote .env.local — fill in the credentials, it is gitignored'

.PHONY: help-runner test lint env-example

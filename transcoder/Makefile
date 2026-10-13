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

# The Dockerfile lives at the workspace root (it is shared with the runner), so
# point the image hash at it. Without this, docker_compose hashes a Dockerfile
# that is not here and every build looks changed.
docker_compose_main_image_hash = $(shell cat ../Dockerfile | sha256sum | cut -c1-12)

# docker_compose brings in up/down/image-build/image-push/logs-f/shell/exec.
# `up` is of limited use here: vidra transcodes one video and exits, so
# `make run` (docker compose run) is the real entry point.

CARGO ?= cargo

# The default output directory for `make run`, derived from the input filename
# the same way vidra itself derives --id.
run_id = $(or $(id),$(basename $(file)))

# No apostrophes in any mkpm_help text; the help renderer cannot survive one.

$(call mkpm_help,run,Transcode in/<file> into out/. E.g. `make run file=clip.mp4` or `make run file=clip.mp4 id=my-video opts="--no-transcribe"`)
run: file ?=
run: id ?=
run: opts ?=
run:
	@test -n "$(file)" || { \
	  echo 'usage: make run file=<name under ./in> [id=<id>] [opts="<vidra flags>"]' >&2; \
	  exit 1; }
	@test -f "in/$(file)" || { echo "in/$(file) does not exist" >&2; exit 1; }
	@mkdir -p out/$(run_id)
	$(call docker_compose_run,transcoder,/in/$(file) --out /out/$(run_id) --force $(opts))

$(call mkpm_help,help-vidra,Show the vidra command-line help)
help-vidra:
	$(call docker_compose_run,transcoder,--help)

$(call mkpm_help,test,Run the transcoder tests (needs a local cargo))
test:
	@$(CARGO) test -p vidra

$(call mkpm_help,lint,Clippy over the transcoder (needs a local cargo))
lint:
	@$(CARGO) clippy -p vidra --all-targets -- -D warnings

.PHONY: run help-vidra test lint

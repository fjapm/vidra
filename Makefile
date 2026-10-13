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

$(call mkpm_load,monorepo)
$(call monorepo_set_dirs,transcoder runner)

# Fan-out: `make transcoder/run file=clip.mp4`, `make runner/up`, and so on.
#
# `make help` here lists only this file's targets. Each package has its own:
# `make transcoder/help`, `make runner/help`. (The monorepo package does ship
# a help_hook meant to aggregate them, but the bundled oras plugin defines a
# help_hook too and the last one loaded wins — so the aggregation never
# happens. Not worth fighting; the per-package targets are one keystroke away.)
#
# Note the function is `monorepo_set_dirs`, with no `@` prefix. `$(call
# @monorepo_set_dirs,...)` is an undefined variable that expands to nothing,
# which registers no rules at all and fails silently — every `<dir>/<target>`
# then reports "No rule to make target".
#
# Cargo targets run against the whole workspace from here, because one
# lockfile and one target directory means there is no reason to split them.

CARGO ?= cargo

# No apostrophes in any mkpm_help text. The help renderer interpolates these
# into a single-quoted shell printf, so one apostrophe anywhere breaks
# `make help` for the whole project.

$(call mkpm_help,test,Run every test in the workspace)
test:
	@$(CARGO) test

$(call mkpm_help,lint,Clippy over the workspace with warnings as errors)
lint:
	@$(CARGO) clippy --all-targets -- -D warnings

$(call mkpm_help,fmt,Format the whole workspace)
fmt:
	@$(CARGO) fmt --all

$(call mkpm_help,fmt-check,Fail if the workspace is not formatted)
fmt-check:
	@$(CARGO) fmt --all --check

$(call mkpm_help,check,fmt-check + lint + test)
check: fmt-check lint test
	@printf '\033[32mAll checks passed.\033[0m\n'

$(call mkpm_help,images,Build both container images)
images:
	@$(MAKE) -C transcoder image-build
	@$(MAKE) -C runner image-build

$(call mkpm_help,clean,Remove build output and local transcode results)
clean:
	@rm -rf target
	@rm -rf transcoder/out/*

.PHONY: test lint fmt fmt-check check images clean

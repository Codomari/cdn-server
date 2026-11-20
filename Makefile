.DEFAULT_GOAL := build

CARGO ?= cargo
SUDO ?= sudo
IS_ROOT := $(filter 0,$(shell id -u))

APP_DIR := $(DESTDIR)/etc/cdn-server
SYSTEMD_DIR := $(DESTDIR)/etc/systemd/system
PROGRAM := bin/cdn-server

ifeq ($(strip $(DESTDIR)),)
ADMIN := $(if $(IS_ROOT),,$(SUDO))
ROOT_OWNER := -o root -g root
CONFIG_OWNER := -o root -g cdn
DATA_OWNER := -o cdn -g cdn
endif

.PHONY: build install check-install-privileges

build:
	$(CARGO) build --locked --release --target-dir target
	install -D -m 0755 target/release/cdn-server "$(PROGRAM)"

check-install-privileges:
	@if [ -z "$(DESTDIR)" ] && [ "$$(id -u)" -ne 0 ]; then \
		if [ -z "$(strip $(SUDO))" ]; then \
			printf '%s\n' 'Installing system-wide requires root privileges or sudo.' >&2; exit 1; \
		fi; \
		printf '%s\n' 'Requesting sudo privileges to install cdn-server.'; \
		$(SUDO) -v; \
	fi

install: check-install-privileges build
ifeq ($(strip $(DESTDIR)),)
	@getent group cdn >/dev/null || $(ADMIN) groupadd --system cdn || getent group cdn >/dev/null
	@getent passwd cdn >/dev/null || $(ADMIN) useradd --system --gid cdn --no-create-home --home-dir /etc/cdn-server --shell /usr/sbin/nologin cdn || getent passwd cdn >/dev/null
endif
	$(ADMIN) install -d -m 0755 $(ROOT_OWNER) "$(APP_DIR)" "$(APP_DIR)/bin"
	$(ADMIN) install -d -m 0750 $(CONFIG_OWNER) "$(APP_DIR)/certs"
	$(ADMIN) install -d -m 0750 $(DATA_OWNER) "$(APP_DIR)/storage" "$(APP_DIR)/certbot"
	$(ADMIN) install -d "$(SYSTEMD_DIR)"
	@set -eu; \
	temp=; \
	trap 'if [ -n "$$temp" ]; then $(ADMIN) unlink "$$temp"; fi' EXIT; \
	put() { \
		mode=$$1; source=$$2; dest=$$3; policy=$$4; shift 4; \
		if [ "$$policy" = seed ] && { [ -e "$$dest" ] || [ -L "$$dest" ]; }; then return; fi; \
		temp=$$($(ADMIN) mktemp "$$dest.XXXXXX"); \
		$(ADMIN) install -m "$$mode" "$$@" "$$source" "$$temp"; \
		if [ "$$policy" = seed ]; then \
			if ! $(ADMIN) ln -T -- "$$temp" "$$dest"; then \
				[ -e "$$dest" ] || [ -L "$$dest" ] || return 1; \
			fi; \
			$(ADMIN) unlink "$$temp"; \
		else \
			$(ADMIN) mv -fT -- "$$temp" "$$dest"; \
		fi; \
		temp=; \
	}; \
	put 0755 "$(PROGRAM)" "$(APP_DIR)/bin/cdn-server" replace $(ROOT_OWNER); \
	put 0644 config.example.yaml "$(APP_DIR)/config.example.yaml" replace $(ROOT_OWNER); \
	put 0640 config.example.yaml "$(APP_DIR)/config.yaml" seed $(CONFIG_OWNER); \
	put 0644 cdn-server.service "$(SYSTEMD_DIR)/cdn-server.service" replace $(ROOT_OWNER)
ifeq ($(strip $(DESTDIR)),)
	$(ADMIN) systemctl daemon-reload
	$(ADMIN) systemctl enable cdn-server.service
	@printf '%s\n' 'Configure /etc/cdn-server/config.yaml for your environment.' \
		'Start or restart the service with sudo systemctl restart cdn-server.service.'
else
	@printf '%s\n' 'Installation staged under $(DESTDIR).'
endif

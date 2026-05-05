.PHONY: help check check-agents fmt clippy server bridge bot kill restart \
	stage-static linux-release docker-image docker-run docker-save deploy deploy-image deploy-binary deploy-status deploy-logs

HOST ?= 127.0.0.1
PORT ?= 3001
STATIC_DIR ?= .

EKZA_URL ?= http://$(HOST):$(PORT)
BRIDGE_BIND ?= 127.0.0.1:5055
BRIDGE_URL ?= http://$(BRIDGE_BIND)

FRONTEND_DIST ?= ../core/dist
LINUX_TARGET ?= x86_64-unknown-linux-musl
DIST_DIR ?= dist
DIST_BIN ?= $(DIST_DIR)/server-linux-amd64
LINUX_RUSTFLAGS ?= -C linker=rust-lld
IMAGE_NAME ?= ekza-rust-server
IMAGE_TAG ?= latest
IMAGE_PLATFORM ?= linux/amd64
PLATFORM_SLUG := $(subst /,-,$(IMAGE_PLATFORM))
ARCHIVE_NAME ?= $(IMAGE_NAME)-$(IMAGE_TAG)-$(PLATFORM_SLUG).tar.gz
CONTAINER_NAME ?= ekza-rust-server
EXTRA_CONTAINER_NAMES ?= ekza-server-rust

DEPLOY_HOST ?= vds-eternal
DEPLOY_DIR ?= ~/ekza-server-rust
REMOTE_PORT ?= 3001
REMOTE_HEALTH_URL ?= http://127.0.0.1:$(REMOTE_PORT)/health
CORS_ALLOWED_ORIGINS ?= *
DOCKER_BUILD_FLAGS ?=
DEPLOY_STRATEGY ?= binary
DEPLOY_RUNTIME_IMAGE ?= $(IMAGE_NAME):$(IMAGE_TAG)
DEPLOY_BINARY_PATH ?= $(DIST_BIN)
DEPLOY_BINARY_NAME ?= $(notdir $(DEPLOY_BINARY_PATH))

help:
	@echo "Targets:"
	@echo "  make check        - cargo check --bin server"
	@echo "  make check-agents - cargo check --bins --features agents"
	@echo "  make fmt          - cargo fmt"
	@echo "  make clippy       - cargo clippy"
	@echo "  make kill         - kill server/bridge/bot processes"
	@echo "  make restart      - kill then start server+bridge"
	@echo "  make server       - run ekza server (HOST/PORT/STATIC_DIR)"
	@echo "  make bridge       - run agent_bridge (EKZA_URL/BRIDGE_BIND)"
	@echo "  make bot          - run agent_bot (BRIDGE_URL, reads .env)"
	@echo "  make stage-static - copy FRONTEND_DIST into ./build for Docker static serving"
	@echo "  make linux-release - build static Linux release binary into DIST_BIN"
	@echo "  make docker-image - package DIST_BIN into Alpine image for IMAGE_PLATFORM"
	@echo "  make docker-run   - run image locally on PORT"
	@echo "  make docker-save  - save Docker image tar.gz for SSH deploy"
	@echo "  make deploy       - deploy using DEPLOY_STRATEGY (binary|image), default: $(DEPLOY_STRATEGY)"
	@echo "  make deploy-binary - build binary, upload it, restart container with new binary"
	@echo "  make deploy-image - build image tarball, upload, restart container, and verify /health over SSH"
	@echo "  make deploy-status - show remote container status and /health"
	@echo "  make deploy-logs  - tail remote container logs"
	@echo ""
	@echo "Vars (override like: make server PORT=3002):"
	@echo "  HOST PORT STATIC_DIR EKZA_URL BRIDGE_BIND BRIDGE_URL"
	@echo "  FRONTEND_DIST LINUX_TARGET LINUX_RUSTFLAGS DIST_BIN IMAGE_NAME IMAGE_TAG IMAGE_PLATFORM DEPLOY_HOST DEPLOY_DIR DEPLOY_STRATEGY DEPLOY_RUNTIME_IMAGE REMOTE_PORT"

check:
	cargo check --bin server

check-agents:
	cargo check --bins --features agents

fmt:
	cargo fmt

clippy:
	cargo clippy

server:
	HOST=$(HOST) PORT=$(PORT) STATIC_DIR=$(STATIC_DIR) cargo run --bin server

bridge:
	EKZA_URL=$(EKZA_URL) BRIDGE_BIND=$(BRIDGE_BIND) cargo run --features agents --bin agent_bridge -- --ekza-url $(EKZA_URL) --bind $(BRIDGE_BIND)

bot:
	BRIDGE_URL=$(BRIDGE_URL) cargo run --features agents --bin agent_bot -- --bridge $(BRIDGE_URL)

kill:
	@echo "Killing anything on PORT=$(PORT) and bridge port $$(echo $(BRIDGE_BIND) | awk -F: '{print $$2}')"
	@lsof -tiTCP:$(PORT) -sTCP:LISTEN | xargs -r kill || true
	@lsof -tiTCP:$$(echo $(BRIDGE_BIND) | awk -F: '{print $$2}') -sTCP:LISTEN | xargs -r kill || true
	@pkill -f "target/debug/server" || true
	@pkill -f "target/debug/agent_bridge" || true
	@pkill -f "target/debug/agent_bot" || true

restart: kill
	@echo "Starting server+bridge..."
	@$(MAKE) server &
	@sleep 0.5
	@$(MAKE) bridge

stage-static:
	@if [ -f "$(FRONTEND_DIST)/index.html" ]; then \
		echo "Staging frontend from $(FRONTEND_DIST) -> build"; \
		rm -rf build; \
		mkdir -p build; \
		cp -R "$(FRONTEND_DIST)/." build/; \
	else \
		echo "No $(FRONTEND_DIST)/index.html found; creating minimal static health page"; \
		mkdir -p build; \
		printf '%s\n' '<!doctype html><title>Ekza Server</title><h1>Ekza Server</h1>' > build/index.html; \
	fi

linux-release:
	rustup target add $(LINUX_TARGET)
	RUSTFLAGS="$(LINUX_RUSTFLAGS)" cargo build --locked --release --bin server --target $(LINUX_TARGET)
	@mkdir -p "$(DIST_DIR)"
	cp "target/$(LINUX_TARGET)/release/server" "$(DIST_BIN)"
	chmod 755 "$(DIST_BIN)"
	@echo "Linux release binary: $(DIST_BIN)"

docker-image: stage-static linux-release
	docker buildx build \
		--platform $(IMAGE_PLATFORM) \
		--load \
		-t $(IMAGE_NAME):$(IMAGE_TAG) \
		$(DOCKER_BUILD_FLAGS) \
		.

docker-run:
	docker run --rm \
		--name $(CONTAINER_NAME)-local \
		-p $(PORT):3001 \
		-e CORS_ALLOWED_ORIGINS='$(CORS_ALLOWED_ORIGINS)' \
		$(IMAGE_NAME):$(IMAGE_TAG)

docker-save: docker-image
	docker save $(IMAGE_NAME):$(IMAGE_TAG) | gzip > $(ARCHIVE_NAME)
	@echo "Docker image archive: $(ARCHIVE_NAME)"

deploy:
	@if [ "$(DEPLOY_STRATEGY)" = "binary" ]; then \
		$(MAKE) deploy-binary; \
	else \
		$(MAKE) deploy-image; \
	fi

deploy-image: docker-save
	@test -n "$(DEPLOY_HOST)" || (echo "Set DEPLOY_HOST=user@host"; exit 1)
	ssh -o BatchMode=yes "$(DEPLOY_HOST)" "mkdir -p $(DEPLOY_DIR)"
	scp "$(ARCHIVE_NAME)" "$(DEPLOY_HOST):$(DEPLOY_DIR)/"
	ssh "$(DEPLOY_HOST)" "set -e; \
		cd $(DEPLOY_DIR); \
		gunzip -c $(ARCHIVE_NAME) | docker load; \
		docker stop $(CONTAINER_NAME) $(EXTRA_CONTAINER_NAMES) 2>/dev/null || true; \
		docker rm $(CONTAINER_NAME) $(EXTRA_CONTAINER_NAMES) 2>/dev/null || true; \
		docker run -d \
			--restart unless-stopped \
			--name $(CONTAINER_NAME) \
			-p $(REMOTE_PORT):3001 \
			-e HOST=0.0.0.0 \
			-e PORT=3001 \
			-e LOG_LEVEL=info \
			-e STATIC_DIR=build \
			-e CORS_ALLOWED_ORIGINS='$(CORS_ALLOWED_ORIGINS)' \
			$(IMAGE_NAME):$(IMAGE_TAG); \
		sleep 2; \
		curl -fsS $(REMOTE_HEALTH_URL); \
		docker ps --filter name=$(CONTAINER_NAME)"
	@echo "Deploy verified: $(DEPLOY_HOST) $(REMOTE_HEALTH_URL)"

deploy-binary: stage-static linux-release
	@test -n "$(DEPLOY_HOST)" || (echo "Set DEPLOY_HOST=user@host"; exit 1)
	@test -n "$(DEPLOY_BINARY_PATH)" || (echo "Set DEPLOY_BINARY_PATH"; exit 1)
	ssh -o BatchMode=yes "$(DEPLOY_HOST)" "mkdir -p $(DEPLOY_DIR)"
	scp "$(DEPLOY_BINARY_PATH)" "$(DEPLOY_HOST):$(DEPLOY_DIR)/$(notdir $(DEPLOY_BINARY_PATH))"
	scp -r build "$(DEPLOY_HOST):$(DEPLOY_DIR)/"
	ssh "$(DEPLOY_HOST)" "set -e; \
		docker image inspect $(DEPLOY_RUNTIME_IMAGE) >/dev/null 2>&1 || { echo \"runtime image $(DEPLOY_RUNTIME_IMAGE) is not available on $(DEPLOY_HOST)\"; exit 1; }; \
		docker stop $(CONTAINER_NAME) $(EXTRA_CONTAINER_NAMES) 2>/dev/null || true; \
		docker rm $(CONTAINER_NAME) $(EXTRA_CONTAINER_NAMES) 2>/dev/null || true; \
		docker run -d \
			--restart unless-stopped \
			--name $(CONTAINER_NAME) \
			-p $(REMOTE_PORT):3001 \
			-v $(DEPLOY_DIR)/$(notdir $(DEPLOY_BINARY_PATH)):/app/server \
			-v $(DEPLOY_DIR)/build:/app/build:ro \
			-e HOST=0.0.0.0 \
			-e PORT=3001 \
			-e LOG_LEVEL=info \
			-e STATIC_DIR=build \
			-e CORS_ALLOWED_ORIGINS='$(CORS_ALLOWED_ORIGINS)' \
			$(DEPLOY_RUNTIME_IMAGE) \
			/bin/sh -c 'chmod +x /app/server && /app/server'; \
		sleep 2; \
		curl -fsS $(REMOTE_HEALTH_URL); \
		docker ps --filter name=$(CONTAINER_NAME)"
	@echo "Binary deploy verified: $(DEPLOY_HOST) $(REMOTE_HEALTH_URL)"

deploy-status:
	ssh "$(DEPLOY_HOST)" "set -e; curl -fsS $(REMOTE_HEALTH_URL); echo; docker ps --filter name=$(CONTAINER_NAME)"

deploy-logs:
	ssh "$(DEPLOY_HOST)" "docker logs --tail=120 -f $(CONTAINER_NAME)"

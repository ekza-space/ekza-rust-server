.PHONY: build run dev check test fmt lint install-watch \
	linux-release dist-bin docker-image docker-run docker-up docker-save deploy

# --- локальная разработка ---
build:
	cargo build

run:
	cargo run

# Hot-reload dev server (restarts on code changes). Requires cargo-watch.
install-watch:
	cargo install cargo-watch

dev:
	@command -v cargo-watch >/dev/null 2>&1 || (echo "cargo-watch not installed. Run: make install-watch"; exit 1)
	cargo watch -w src -w Cargo.toml -w Cargo.lock -x run

check:
	cargo check

test:
	cargo test

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings

# --- Docker: сборка release внутри docker build (rust:alpine), без локального musl-таргета ---
# Опционально — кросс-компиляция на хосте в dist/ (cross / cargo-zigbuild):
LINUX_TARGET ?= x86_64-unknown-linux-musl
DIST_DIR := dist
DIST_BIN := $(DIST_DIR)/ekza-rust-server
RUST_RELEASE_BIN := target/$(LINUX_TARGET)/release/ekza-rust-server

IMAGE_NAME ?= ekza-rust-server
IMAGE_TAG ?= latest
CONTAINER_NAME ?= ekza-rust-server

# SSH-деплой: пример — make deploy DEPLOY_HOST=vds-eternal DEPLOY_DIR=/root/ekza-rust-server
DEPLOY_HOST ?=
DEPLOY_DIR ?= ~/ekza
# Должен совпадать с Origin фронта (например https://space.ekza.io). Для превью — перечислить через запятую.
CORS_ALLOWED_ORIGINS ?= https://space.ekza.io
ARCHIVE_NAME ?= $(IMAGE_NAME)-$(IMAGE_TAG).tar.gz

linux-release:
	@mkdir -p "$(DIST_DIR)" build
	@if command -v cross >/dev/null 2>&1; then \
		echo "Using cross for $(LINUX_TARGET)"; \
		cross build --release --target $(LINUX_TARGET); \
	elif command -v cargo-zigbuild >/dev/null 2>&1; then \
		echo "Using cargo-zigbuild for $(LINUX_TARGET)"; \
		cargo zigbuild --release --target $(LINUX_TARGET); \
	else \
		cargo build --release --target $(LINUX_TARGET); \
	fi

$(DIST_BIN): linux-release
	@mkdir -p "$(DIST_DIR)"
	@test -f "$(RUST_RELEASE_BIN)" || (echo "Нет $(RUST_RELEASE_BIN). Установите cross или cargo-zigbuild (см. комментарии в Makefile)."; exit 1)
	cp "$(RUST_RELEASE_BIN)" "$(DIST_BIN)"
	chmod 755 "$(DIST_BIN)"

dist-bin: $(DIST_BIN)

docker-image:
	docker build -t $(IMAGE_NAME):$(IMAGE_TAG) .

# Локально: только запуск уже собранного образа
docker-run:
	docker run --rm -p 3001:3001 $(IMAGE_NAME):$(IMAGE_TAG)

# Сборка в Docker и сразу запуск (проверка в одну команду)
docker-up: docker-image
	docker run --rm -p 3001:3001 $(IMAGE_NAME):$(IMAGE_TAG)

docker-save: docker-image
	docker save $(IMAGE_NAME):$(IMAGE_TAG) | gzip > $(ARCHIVE_NAME)
	@echo "Архив образа: $(ARCHIVE_NAME)"

deploy: docker-save
	@test -n "$(DEPLOY_HOST)" || (echo "Задайте DEPLOY_HOST=user@host"; exit 1)
	ssh -o BatchMode=yes "$(DEPLOY_HOST)" "mkdir -p $(DEPLOY_DIR)"
	scp "$(ARCHIVE_NAME)" "$(DEPLOY_HOST):$(DEPLOY_DIR)/"
	ssh "$(DEPLOY_HOST)" "set -e; cd $(DEPLOY_DIR); gunzip -c $(ARCHIVE_NAME) | docker load; docker stop $(CONTAINER_NAME) 2>/dev/null || true; docker rm $(CONTAINER_NAME) 2>/dev/null || true; docker run -d --restart unless-stopped --name $(CONTAINER_NAME) -p 3001:3001 -e CORS_ALLOWED_ORIGINS='$(CORS_ALLOWED_ORIGINS)' $(IMAGE_NAME):$(IMAGE_TAG)"
	@echo "Сервер слушает порт 3001 на $(DEPLOY_HOST)"

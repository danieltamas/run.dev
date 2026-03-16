.PHONY: install build clean test

CYAN  := \033[0;36m
GREEN := \033[0;32m
RED   := \033[0;31m
DIM   := \033[2m
BOLD  := \033[1m
NC    := \033[0m

LOG := /tmp/rundev-build.log

install:
	@printf "\n"
	@printf "  $(CYAN)$(BOLD) ██████╗ ██╗   ██╗███╗   ██╗   ██████╗ ███████╗██╗   ██╗$(NC)\n"
	@printf "  $(CYAN)$(BOLD) ██╔══██╗██║   ██║████╗  ██║   ██╔══██╗██╔════╝██║   ██║$(NC)\n"
	@printf "  $(CYAN)$(BOLD) ██████╔╝██║   ██║██╔██╗ ██║   ██║  ██║█████╗  ██║   ██║$(NC)\n"
	@printf "  $(CYAN)$(BOLD) ██╔══██╗██║   ██║██║╚██╗██║██╗██║  ██║██╔══╝  ╚██╗ ██╔╝$(NC)\n"
	@printf "  $(CYAN)$(BOLD) ██║  ██║╚██████╔╝██║ ╚████║╚═╝██████╔╝███████╗ ╚████╔╝ $(NC)\n"
	@printf "  $(CYAN)$(BOLD) ╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝   ╚═════╝ ╚══════╝  ╚═══╝  $(NC)\n"
	@printf "\n"
	@printf "  $(BOLD)AI-native local dev environment$(NC)\n"
	@printf "  $(DIM)Replaces MAMP/nginx — manages services, reverse proxy,$(NC)\n"
	@printf "  $(DIM)SSL certs, and live AI crash diagnosis from one dashboard.$(NC)\n"
	@printf "\n"
	@printf "  $(DIM)by Daniel Tamas  •  getrun.dev$(NC)\n"
	@printf "\n"
	@printf "  ────────────────────────────────────────\n"
	@printf "\n"
	@cargo install --path . > $(LOG) 2>&1 & \
		PID=$$!; \
		FRAMES="⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏"; \
		I=0; \
		while kill -0 $$PID 2>/dev/null; do \
			F=$$(echo $$FRAMES | cut -d' ' -f$$((I % 10 + 1))); \
			printf "\r  $(CYAN)$$F$(NC)  Building..."; \
			I=$$((I + 1)); \
			sleep 0.1; \
		done; \
		wait $$PID; EXIT=$$?; \
		printf "\r"; \
		if [ $$EXIT -ne 0 ]; then \
			printf "  $(RED)✗$(NC)  Build failed:\n\n"; \
			cat $(LOG); \
			exit 1; \
		fi
	@printf "  $(GREEN)✓$(NC)  run.dev installed — run $(BOLD)rundev$(NC) to start\n\n"

build:
	@printf "\n  $(CYAN)→$(NC)  Building run.dev (debug)...\n"
	@cargo build > $(LOG) 2>&1 & \
		PID=$$!; \
		FRAMES="⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏"; \
		I=0; \
		while kill -0 $$PID 2>/dev/null; do \
			F=$$(echo $$FRAMES | cut -d' ' -f$$((I % 10 + 1))); \
			printf "\r  $(CYAN)$$F$(NC)  Building..."; \
			I=$$((I + 1)); \
			sleep 0.1; \
		done; \
		wait $$PID; EXIT=$$?; \
		printf "\r"; \
		if [ $$EXIT -ne 0 ]; then \
			printf "  $(RED)✗$(NC)  Build failed:\n\n"; \
			cat $(LOG); \
			exit 1; \
		fi
	@printf "  $(GREEN)✓$(NC)  Built — ./target/debug/rundev\n\n"

test:
	@printf "\n  $(CYAN)→$(NC)  Running tests...\n"
	@cargo test 2>&1 | tail -3
	@printf "\n"

clean:
	@cargo clean -q
	@printf "  $(GREEN)✓$(NC)  Cleaned\n"

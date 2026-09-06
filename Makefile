# OS System - 本地构建一键脚本（devops-agent C4 构建脚本）
# ============================================================
# 与 .github/workflows/ci.yml 保持一致的本地复现命令。
# 用法见底部 `make help`。
#
# 红线（呼应规格书 §9）：
#   - clippy 一律 -D warnings（不得本地放行后再推 CI）
#   - 测试一律带 --features mock（解锁下游 Mock 注入路径）
# ============================================================

# ---- 可调参数（环境变量覆盖）----
FEATURES       ?= mock
CARGO          ?= cargo
CLIPPY_FLAGS   := -D warnings
# 带 criterion bench 的 crate 列表（与 ci.yml bench job 一致）。
BENCH_CRATES   := os-storage os-meta osd os-services os-api

# ---- 默认目标 ----
.DEFAULT_GOAL := help
.PHONY: help check clippy fmt fmt-check test doc all clean install-hooks \
        bench bench-pkg bench-save bench-baseline bench-check iso web

help: ## 显示本帮助
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

check: ## cargo check（workspace + mock）
	$(CARGO) check --workspace --features $(FEATURES) --all-targets

clippy: ## cargo clippy（-D warnings，与 CI 同门）
	$(CARGO) clippy --workspace --all-targets --features $(FEATURES) -- $(CLIPPY_FLAGS)

fmt: ## cargo fmt（自动格式化）
	$(CARGO) fmt --all

fmt-check: ## cargo fmt --check（CI 风格校验，不写盘）
	$(CARGO) fmt --all -- --check

test: ## cargo test（workspace + mock）
	$(CARGO) test --workspace --features $(FEATURES)

doc: ## cargo doc（workspace + mock，不构建依赖文档）
	$(CARGO) doc --workspace --features $(FEATURES) --no-deps

all: check clippy test ## 三道门全跑（check + clippy + test）
	@echo "✓ check + clippy + test 全绿"

install-hooks: ## 安装 git hooks（pre-commit -> scripts/pre-commit.sh）
	@if [ -d .git ]; then \
		ln -sf ../../scripts/pre-commit.sh .git/hooks/pre-commit; \
		chmod +x scripts/pre-commit.sh; \
		echo "✓ pre-commit hook 已安装到 .git/hooks/pre-commit"; \
	else \
		echo "✗ 未找到 .git 目录（非 git 仓库根？）" >&2; exit 1; \
	fi

# ---- criterion 微基准（devops-agent §5 性能门）----
# 注意：bench 走 release profile，耗时长，与三道门（check/clippy/test）解耦，不进 `all`。
bench: ## cargo bench（workspace + mock，跑全部 criterion 微基准）
	$(CARGO) bench --workspace --features $(FEATURES)

bench-pkg: ## cargo bench 单 crate（用法：make bench-pkg PKG=os-storage）
	@if [ -z "$(PKG)" ]; then \
		echo "✗ 缺 PKG：make bench-pkg PKG=<crate>" >&2; exit 2; \
	fi
	$(CARGO) bench -p $(PKG) --features $(FEATURES)

bench-save: ## cargo bench 并保存为基线（用法：make bench-save TAG=before-refactor）
	@if [ -z "$(TAG)" ]; then \
		echo "✗ 缺 TAG：make bench-save TAG=<name>（criterion --save-baseline）" >&2; exit 2; \
	fi
	$(CARGO) bench --workspace --features $(FEATURES) -- --save-baseline $(TAG)

# ---- criterion 回归门控（feature/bench-regression-ci，配合 ci.yml bench job）----
# criterion 0.5 检测到回归时仍 exit 0，故用 scripts/ci/bench-regression-gate.sh
# 解析输出 + 分桶阈值判定。默认 BASELINE_TAG=os-baseline（与 ci.yml env 一致）。
# 阈值可经环境变量覆盖：STRICT_THRESHOLD=15 LOOSE_THRESHOLD=30。
BASELINE_TAG    ?= os-baseline
GATE_SCRIPT     := scripts/ci/bench-regression-gate.sh

bench-baseline: ## 保存/更新回归基线（用法：make bench-baseline [TAG=os-baseline]）
	@echo "→ cargo bench --save-baseline $(BASELINE_TAG)（首次建基线 / 算法变更后更新）"
	$(CARGO) bench --workspace --features $(FEATURES) -- --save-baseline $(BASELINE_TAG)
	@echo "✓ 基线已保存为 $(BASELINE_TAG)（target/criterion）。"

bench-check: ## 比对回归基线 + 门控（用法：make bench-check [TAG=os-baseline]）
	@command -v awk >/dev/null || { echo "✗ 需要 awk" >&2; exit 2; }
	@if [ ! -x "$(GATE_SCRIPT)" ]; then \
		echo "✗ 找不到/不可执行: $(GATE_SCRIPT)" >&2; exit 2; \
	fi
	@echo "→ cargo bench --baseline $(BASELINE_TAG)（比对模式）"
	$(CARGO) bench --workspace --features $(FEATURES) -- --baseline $(BASELINE_TAG) 2>&1 | tee criterion-bench.log
	@echo "→ 回归门控判定（scripts/ci/bench-regression-gate.sh）"
	$(GATE_SCRIPT) criterion-bench.log

iso: ## 构建 OS ISO 安装包（cargo build --release + mksquashfs + xorriso）
	@bash scripts/build-iso.sh

web: ## 构建 Vue3 前端（npm run build → crates/os-api/static-dist/，rust-embed 内嵌）
	@echo "→ 构建 Vue3 前端（crates/os-api/web）"
	@cd crates/os-api/web && npm install && npm run build
	@echo "✓ Vue3 前端产物已输出到 crates/os-api/static-dist/"

clean: ## cargo clean（清 target/）
	$(CARGO) clean

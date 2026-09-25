# i18n-agent 进度日志

## 当前状态
- 阶段：完成（待评审）—— P2 toml crate 接通完成
- 最后更新：2026-08-05

## DoD 状态（规格书 §5.2）
- [x] `Translator` 有 `BundleTranslator` 具体实现（非 `todo!()`）—— `src/impl_translator.rs`
- [x] `cargo check -p os-i18n` 通过
- [x] `cargo test -p os-i18n` 通过（**19 测试全绿**；含 fallback 链 / 占位符 / reload / 并发读 / 嵌套表 / 多行字符串 / 内联表 / 数组 / 转义）
- [x] `cargo clippy -p os-i18n --features mock --all-targets -- -D warnings` 无警告
- [x] 三语资源文件骨架（`locales/{en,zh_cn,zh_tw}.toml`）已提交
- [x] `MockTranslator`（`src/mock.rs`，feature gate `mock`）已提交
- [x] `cargo doc -p os-i18n --features mock` 无警告
- [x] PROGRESS.md 已更新

## 已完成
- [x] 三语 TOML 资源骨架（en/zh_cn/zh_tw，覆盖 error.*/common.*/pool.* 共 35 键 × 3 语）
- [x] `BundleTranslator`：嵌入式 TOML + `toml` crate 真实解析 + `RwLock` 并发索引
- [x] `MockTranslator`（feature `mock`），`with(key, locale, msg)` 链式构造器
- [x] 单元/集成测试 19 个（fallback 链、占位符、reload、并发读、bundle/all_bundles、解析失败、
      嵌套表、多行字符串、内联表+数组跳过、转义序列、端到端高级 TOML）

## 决策与说明
### 后端选择：toml crate 真实解析（接通 ADR-DEPS-002）
ADR-DEPS-002 已在 workspace `[workspace.dependencies]` 注册 `toml = "1"`（锁定 1.1.4+spec-1.1.0），
现正式接通：crate 级 `Cargo.toml` 加 `toml.workspace = true`，`impl_translator.rs` 用
`toml::from_str` 替换早期自写的极简「扁平 TOML 子集」解析器 `parse_flat_toml`。
- `crates/os-i18n/locales/{en,zh_cn,zh_tw}.toml` —— 真实 TOML 文件（满足 DoD 的 TOML 骨架要求）
- `include_str!` 在编译期嵌入三语资源（部署零额外文件）
- `impl_translator.rs` 解析入口 `parse_toml`：
  - 用 `toml::from_str::<toml::Table>(input)` 做完整 TOML 解析
  - `flatten_table` 递归把嵌套表扁平化为 `full_key -> 文案模板`（如 `pool.created`）
  - 支持完整 TOML 语法：行注释 / 节头 / 嵌套表（`[a.b.c]`）/ 内联表（`k = {...}`）/
    数组 / 多行字符串（`"""..."""`）/ 基本与字面字符串 / 全部转义
  - 仅字符串叶子作为文案收集；非字符串叶子（整数/布尔/数组/日期）跳过
  - 解析失败返回 `I18nError::ParseFailed`，附带 toml crate 原始错误信息

trait 签名零修改；`from_toml`/`new`/`reload`/`t` 入口语义不变；fallback 链不变（绝不 panic）。
解析入口仍为单一 `Self::from_toml`，平滑切换无业务侧侵入。

### fallback 链（规格书 §3 / §5.1）
`t(key, locale, args)`：
1. 查 `locale` → 命中返回填占位符结果
2. 查 `DEFAULT_LOCALE`（en）→ 命中返回
3. 全部缺失 → 返回 key 本身（**绝不 panic / 空串**）

### 并发
`BundleTranslator.inner: RwLock<HashMap<Locale, LocaleTable>>`，`t()` 走读锁，`reload()` 走写锁；
集成测 8 线程 × 200 次（其中 1 线程并发 reload）通过。

## 阻塞
- 无。

## 待办（可选增强，非本次 DoD）
- 业务 crate 的键由各 crate 自行贡献到 `locales/*.toml`（本 agent 不替其枚举，见规格书 §2 边界）。

## Commit 列表
（见下方 git log；均在 `agent/i18n-agent` 分支，未 push）

// =============================================================================
// 联邦条目一键导入·已登记行的升级决策（纯函数，无 Vue/网络依赖）
// LlmModels.vue mcImportFed「已登记过」分支使用；抽出为纯函数以便直测决策
// 矩阵（旧行+新凭据→更新字段集；旧行+脱敏→不覆盖 key）。
//
// 背景（2026-09-03 修复）：0.1.16 之前从联邦大厅导入的登记行没有 via_node
// （直连语义）；升级后再导入同名条目时旧逻辑只切换目标、旧行原样保留，
// 对话仍走直连报错。修复：用条目携带的新凭据 PUT 升级旧行后再切换。
// =============================================================================

/** 旧登记行的决策所需子集（视图传 LlmExternalApi，测试可传最小字面量）。 */
export interface FedUpgradeExistingRow {
  /** 来源 NodeID（空/缺失 = 直连语义的旧行）。 */
  via_node?: string;
  /** 是否已配置 key（脱敏列表视角唯一可知的 key 事实）。 */
  has_api_key?: boolean;
  /** 现有模型清单。 */
  models?: string[];
}

/** 联邦条目的凭据来源子集（视图传 McFedListing，结构兼容）。 */
export interface FedListingCredentialSource {
  /** 来源 NodeID（0x+66hex；非空写入 → chat/test 经 overlay 中继源节点代发）。 */
  source_node_id?: string;
  server_config?: { model_name?: string | null } | null;
  /** key 仅明文视角可带（脱敏态 `前4***后4` 含 *；短 key 全掩 ****）。 */
  access_info?: { api_key?: string } | null;
}

/** PUT /llm/external-apis/:id 升级补丁（部分更新语义：未提供字段保留原值）。 */
export interface FedUpgradePatch {
  via_node?: string;
  api_key?: string;
  models?: string[];
}

/** 升级决策结果。 */
export interface FedUpgradeDecision {
  /** PUT 请求体（空对象 = 无可升级字段，跳过 PUT 只切换）。 */
  patch: FedUpgradePatch;
  /** 是否执行升级 PUT（patch 含 ≥1 字段）。 */
  upgraded: boolean;
  /** 条目 key 处于脱敏视角（不可据此覆盖登记已有 key）。 */
  keyMasked: boolean;
  /** 脱敏且旧行无 key → 提示用户手动补填（消息附带 fedKeyMasked 指引）。 */
  needsManualKey: boolean;
}

/**
 * 计算已登记行的联邦升级补丁：
 *
 * - via_node：条目带来源节点且与旧行不同 → 写入（升级为 overlay 中继语义；
 *   相同则跳过——避免无变化却报「已升级」）。
 * - api_key：仅明文视角写（脱敏含 `*` / 空串都不写——绝不覆盖登记已有 key；
 *   明文无法与旧行比对，提供即覆盖，幂等）。
 * - models：条目声明模型名且旧行不等于 `[model_name]` → 置为 `[model_name]`
 *   （与一键新建导入同形）。
 */
export function fedUpgradeDecision(
  existing: FedUpgradeExistingRow,
  listing: FedListingCredentialSource,
): FedUpgradeDecision {
  const patch: FedUpgradePatch = {};
  const viaNode = (listing.source_node_id ?? '').trim();
  if (viaNode && viaNode !== (existing.via_node ?? '')) patch.via_node = viaNode;

  const rawKey = (listing.access_info?.api_key ?? '').trim();
  const keyMasked = rawKey.includes('*');
  if (rawKey && !keyMasked) patch.api_key = rawKey;

  const modelName = (listing.server_config?.model_name ?? '').trim();
  const existingModels = existing.models ?? [];
  if (modelName && !(existingModels.length === 1 && existingModels[0] === modelName)) {
    patch.models = [modelName];
  }

  return {
    patch,
    upgraded: patch.via_node !== undefined || patch.api_key !== undefined || patch.models !== undefined,
    keyMasked,
    needsManualKey: keyMasked && !existing.has_api_key,
  };
}
